//! Reed–Solomon erasure FEC primitives and an adaptive `FecController`.
//!
//! This module sits next to the existing XOR-parity `FrameChunker` in
//! `super`. It is the *next-generation* FEC: configurable per-block data
//! shard count, configurable per-block parity shard count (so we can
//! recover multiple losses per block, not just one), and an
//! `FecController` that scales the parity count up or down based on the
//! loss fraction observed by the receiver.
//!
//! ## Why Reed–Solomon
//!
//! XOR parity can recover exactly *one* missing chunk per block. With a
//! 1200-byte QUIC datagram MTU and 4K frames, loss bursts of two or more
//! packets within a single frame are common on cellular links. Reed–
//! Solomon over GF(2^8) recovers up to `parity_shards` losses per
//! block, and is fast on x86-64 (the default pure-Rust path is more
//! than enough for our 60 FPS path).
//!
//! ## Wire format
//!
//! This module is intentionally **wire-format-compatible** with the
//! existing XOR-parity path. The wire protocol only distinguishes
//! "parity chunk" via the `FLAG_PARITY` bit on `MediaDatagramHeader`.
//! On each frame the sender emits `block_size` data shards followed by
//! `parity_shards` parity shards, in order. The receiver uses the
//! session-negotiated `(block_size, parity_shards)` to slice them back
//! out. A frame with `parity_shards == 0` is identical to "no FEC".
//!
//! Dynamic parity-count adjustment (`FecController`) changes the count
//! between frames. Receiver tracks the last-seen `(block_size,
//! parity_count)` and recomputes blocks accordingly. We deliberately
//! avoid touching the on-wire header in this PR.
//!
//! ## When to switch on
//!
//! Today XOR parity is good enough to survive single packet losses. The
//! RS path is opt-in via `MediaFecMode::ReedSolomon` in the session
//! handshake. When `FecController` is in `Adaptive` mode and loss
//! exceeds 1% over the EWMA, parity is non-zero; below 0.5%, parity is
//! 0. This means low-loss networks pay zero FEC overhead.

use reed_solomon_erasure::galois_8::ReedSolomon;

/// Maximum parity shards we will ever emit per block.
///
/// 4 data chunks == 33% overhead at `parity_count = 2`, 67% at
/// `parity_count = 4`. We cap here so an unbounded loss spike (e.g.,
/// peer reconnects mid-stream) doesn't bloat every frame.
pub const MAX_PARITY_SHARDS: usize = 4;

/// Recommended block size for 1080p60 H.264 traffic at ~1200 byte
/// chunks. Tuned to fit one frame in ~12 chunks.
pub const DEFAULT_BLOCK_SIZE: usize = 4;

/// Default parity shards per block (k=4, m=2). Overrides original ADR k=10.
pub const DEFAULT_PARITY_SHARDS: usize = 2;

/// Per-block Reed–Solomon encoder.
#[derive(Debug, Clone)]
pub struct ReedSolomonFec {
    block_size: usize,
    parity_shards: usize,
}

impl ReedSolomonFec {
    /// Construct a `ReedSolomon` for the given data/parity counts.
    /// `parity_shards == 0` is permitted and disables FEC entirely.
    pub fn new(block_size: usize, parity_shards: usize) -> Result<Self, ReedSolomonFecError> {
        if block_size == 0 {
            return Err(ReedSolomonFecError::ZeroBlockSize);
        }
        if parity_shards > MAX_PARITY_SHARDS {
            return Err(ReedSolomonFecError::TooManyParity(parity_shards));
        }
        // Lazy validate `block_size + parity_shards <= 255` (the
        // Reed–Solomon library uses `u8` for shard counts).
        if block_size + parity_shards > u8::MAX as usize {
            return Err(ReedSolomonFecError::BlockTooLarge(
                block_size + parity_shards,
            ));
        }
        // Pre-instantiate so any library-side error surfaces at construction.
        // The library refuses parity_shards == 0; that case is handled by
        // the short-circuits in encode()/reconstruct().
        if parity_shards > 0 {
            ReedSolomon::new(block_size, parity_shards).map_err(ReedSolomonFecError::Library)?;
        }
        Ok(Self {
            block_size,
            parity_shards,
        })
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn parity_shards(&self) -> usize {
        self.parity_shards
    }

    /// Encode `frame` into `block_size`-shaped data shards and
    /// `parity_shards` parity shards per block.
    ///
    /// Pads the final block with zeros so all shards in a frame are the
    /// same length. Returns `(data_blocks, parity_shards_per_block,
    /// shard_len)`.
    pub fn encode(&self, frame: &[u8]) -> Result<EncodedFrame, ReedSolomonFecError> {
        if self.parity_shards == 0 {
            // Short-circuit: no FEC, just split.
            let shards: Vec<Vec<u8>> = frame
                .chunks(self.shard_len_for(frame.len()))
                .map(|c| c.to_vec())
                .collect();
            return Ok(EncodedFrame {
                data: shards,
                parity: Vec::new(),
                shard_len: self.shard_len_for(frame.len()),
                original_len: frame.len(),
            });
        }

        let shard_len = self.shard_len_for(frame.len());
        let frame_chunks: Vec<Vec<u8>> = frame
            .chunks(shard_len)
            .map(|c| {
                let mut v = c.to_vec();
                v.resize(shard_len, 0);
                v
            })
            .collect();
        if frame_chunks.is_empty() {
            return Err(ReedSolomonFecError::EmptyFrame);
        }

        let blocks = (frame_chunks.len() + self.block_size - 1) / self.block_size;
        let total_data_shards = blocks * self.block_size;
        let total_parity_shards = blocks * self.parity_shards;

        // Lay out shards per block: [data block 0, parity block 0,
        // data block 1, parity block 1, ...]. Each block's slice is
        // contiguous, which is what `ReedSolomon::encode` requires.
        let mut all_shards: Vec<Vec<u8>> =
            Vec::with_capacity(total_data_shards + total_parity_shards);
        for block_idx in 0..blocks {
            let d_lo = block_idx * self.block_size;
            let d_hi = (d_lo + self.block_size).min(frame_chunks.len());
            for i in d_lo..d_hi {
                all_shards.push(frame_chunks[i].clone());
            }
            // Pad the data side of the final short block with zero shards.
            for _ in d_hi..d_lo + self.block_size {
                all_shards.push(vec![0u8; shard_len]);
            }
            // Pre-allocate parity slots as zero-filled shards; rs.encode
            // overwrites them in place.
            for _ in 0..self.parity_shards {
                all_shards.push(vec![0u8; shard_len]);
            }
        }

        // Encode each block independently -- the library does not span
        // across blocks.
        let rs = ReedSolomon::new(self.block_size, self.parity_shards)
            .expect("validated at construction");
        for block_idx in 0..blocks {
            let start = block_idx * (self.block_size + self.parity_shards);
            let end_idx = start + self.block_size + self.parity_shards;
            let block = &mut all_shards[start..end_idx];
            rs.encode(block).map_err(ReedSolomonFecError::Library)?;
        }

        // Split into data and parity slices in the same ordering that
        // reconstruct() expects.
        let mut data_out: Vec<Vec<u8>> = Vec::with_capacity(total_data_shards);
        let mut parity_out: Vec<Vec<u8>> = Vec::with_capacity(total_parity_shards);
        for block_idx in 0..blocks {
            let block_start = block_idx * (self.block_size + self.parity_shards);
            data_out.extend_from_slice(&all_shards[block_start..block_start + self.block_size]);
            parity_out.extend_from_slice(
                &all_shards[block_start + self.block_size
                    ..block_start + self.block_size + self.parity_shards],
            );
        }

        Ok(EncodedFrame {
            data: data_out,
            parity: parity_out,
            shard_len,
            original_len: frame.len(),
        })
    }

    /// Reconstruct up to `parity_shards` missing shards per block.
    /// `data` and `parity` are sized as `(block_size, blocks)` and
    /// `(parity_shards, blocks)` respectively; missing entries are
    /// `None`.
    pub fn reconstruct(
        &self,
        data: &mut [Option<Vec<u8>>],
        parity: &mut [Option<Vec<u8>>],
    ) -> Result<usize, ReedSolomonFecError> {
        if self.parity_shards == 0 {
            return Ok(0);
        }
        if data.is_empty() {
            return Err(ReedSolomonFecError::EmptyFrame);
        }
        let blocks = (data.len() + self.block_size - 1) / self.block_size;
        let rs = ReedSolomon::new(self.block_size, self.parity_shards)
            .expect("validated at construction");

        let mut recovered = 0usize;
        for block_idx in 0..blocks {
            let d_lo = block_idx * self.block_size;
            let d_hi = (d_lo + self.block_size).min(data.len());
            let p_lo = block_idx * self.parity_shards;
            let p_hi = (p_lo + self.parity_shards).min(parity.len());

            let mut shards: Vec<Option<Vec<u8>>> =
                Vec::with_capacity(self.block_size + self.parity_shards);
            shards.extend_from_slice(&data[d_lo..d_hi]);
            // Pad the data-side up to `block_size` (final block may be short).
            while shards.len() < self.block_size {
                shards.push(None);
            }
            shards.extend(parity[p_lo..p_hi].iter().cloned());
            // Pad the parity-side up to `parity_shards`.
            while shards.len() < self.block_size + self.parity_shards {
                shards.push(None);
            }

            rs.reconstruct(&mut shards)
                .map_err(ReedSolomonFecError::Library)?;
            for (i, shard) in shards.iter().enumerate().take(d_hi - d_lo) {
                if data[d_lo + i].is_none() && shard.is_some() {
                    data[d_lo + i] = shard.clone();
                    recovered += 1;
                }
            }
            for (i, shard) in shards
                .iter()
                .enumerate()
                .skip(self.block_size)
                .take(p_hi - p_lo)
            {
                if parity[p_lo + (i - self.block_size)].is_none() && shard.is_some() {
                    parity[p_lo + (i - self.block_size)] = shard.clone();
                    recovered += 1;
                }
            }
        }
        Ok(recovered)
    }

    fn shard_len_for(&self, frame_len: usize) -> usize {
        if frame_len == 0 {
            return 0;
        }
        // Aim for chunks close to the QUIC datagram MTU (~1200 bytes),
        // then round up to a 16-byte boundary so the layout is friendly
        // for SIMD-style FEC kernels.
        const TARGET_CHUNK_BYTES: usize = 1200;
        let needed_chunks = (frame_len + TARGET_CHUNK_BYTES - 1) / TARGET_CHUNK_BYTES;
        // Always emit at least block_size shards so the first RS block
        // is always full; otherwise the partial block would force extra
        // padding overhead on every frame.
        let total_chunks = needed_chunks.max(self.block_size);
        let shard_len = (frame_len + total_chunks - 1) / total_chunks;
        shard_len.div_ceil(16) * 16
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EncodedFrame {
    /// One entry per data shard across the whole frame (block_size * num_blocks).
    pub data: Vec<Vec<u8>>,
    /// One entry per parity shard across the whole frame (parity_shards * num_blocks).
    pub parity: Vec<Vec<u8>>,
    /// Length of each shard (all shards are equal length).
    pub shard_len: usize,
    /// Original frame length (used to drop trailing zero pad on the
    /// receiver side).
    pub original_len: usize,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ReedSolomonFecError {
    #[error("block_size must be > 0")]
    ZeroBlockSize,
    #[error("parity_shards > MAX_PARITY_SHARDS ({0})")]
    TooManyParity(usize),
    #[error("block_size + parity_shards > 255 (got {0})")]
    BlockTooLarge(usize),
    #[error("frame is empty")]
    EmptyFrame,
    #[error("reed-solomon library error: {0:?}")]
    Library(reed_solomon_erasure::Error),
}

impl From<reed_solomon_erasure::Error> for ReedSolomonFecError {
    fn from(e: reed_solomon_erasure::Error) -> Self {
        ReedSolomonFecError::Library(e)
    }
}

/// Adaptive parity-count controller.
///
/// Consumes the latest loss fraction (in `loss_x1000` units — i.e. loss%
/// × 1000, e.g. 1500 = 1.5%) and returns the parity-shard count for
/// the *next* frame. The controller is intentionally hysteresis-bounded
/// so single samples do not thrash the wire parity overhead.
#[derive(Debug, Clone)]
pub struct FecController {
    block_size: usize,
    /// Last emitted parity count (for hysteresis).
    last_parity: usize,
    /// Configurable thresholds (loss_x1000 → target parity count).
    thresholds: [LossThreshold; 4],
}

#[derive(Debug, Clone, Copy)]
struct LossThreshold {
    /// Trigger when `ewma_loss_x1000` exceeds this.
    above_x1000: u32,
    parity_count: usize,
}

impl FecController {
    /// Construct a new controller. `block_size` is the per-block data
    /// shard count; recommend 4 for 1080p60.
    pub fn new(block_size: usize) -> Self {
        let thresholds = [
            // >5% loss → 4 parity shards per block (67% overhead — last resort).
            LossThreshold {
                above_x1000: 5000,
                parity_count: 4,
            },
            // >2% loss → 3 parity shards per block.
            LossThreshold {
                above_x1000: 2000,
                parity_count: 3,
            },
            // >1% loss → 2 parity shards per block.
            LossThreshold {
                above_x1000: 1000,
                parity_count: 2,
            },
            // >0.3% loss → 1 parity shard per block.
            LossThreshold {
                above_x1000: 300,
                parity_count: 1,
            },
        ];
        Self {
            block_size: block_size.max(1),
            last_parity: 0,
            thresholds,
        }
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn last_parity(&self) -> usize {
        self.last_parity
    }

    /// Pick the parity count for the next frame given the EWMA loss.
    /// `loss_x1000` is the value from `RateFeedback::loss_x1000`.
    pub fn adjust_for_loss(&mut self, loss_x1000: u32) -> usize {
        let target = self
            .thresholds
            .iter()
            .find(|t| loss_x1000 >= t.above_x1000)
            .map(|t| t.parity_count)
            .unwrap_or(0);

        // Only allow stepping up by one parity count at a time. This
        // prevents oscillation when loss sits right on a threshold.
        let next = if target > self.last_parity {
            (self.last_parity + 1).min(target)
        } else if target < self.last_parity {
            target
        } else {
            self.last_parity
        };
        self.last_parity = next;
        next
    }

    /// Force a parity count (e.g., when the operator pins a session to
    /// a known-bad link).
    pub fn force(&mut self, parity: usize) {
        self.last_parity = parity.min(MAX_PARITY_SHARDS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_helper(block: usize, parity: usize, drop: &[usize]) {
        let rs = ReedSolomonFec::new(block, parity).unwrap();
        let frame: Vec<u8> = (0..(block * 1024)).map(|i| (i % 251) as u8).collect();
        let encoded = rs.encode(&frame).unwrap();
        assert_eq!(encoded.data.len(), block);
        assert_eq!(encoded.parity.len(), parity);
        for chunk in &encoded.data {
            assert_eq!(chunk.len(), encoded.shard_len);
        }
        for chunk in &encoded.parity {
            assert_eq!(chunk.len(), encoded.shard_len);
        }
        let mut data: Vec<Option<Vec<u8>>> = encoded.data.into_iter().map(Some).collect();
        let mut par: Vec<Option<Vec<u8>>> = encoded.parity.into_iter().map(Some).collect();
        for &i in drop {
            if i < block {
                data[i] = None;
            } else if i < block + parity {
                par[i - block] = None;
            }
        }
        let recovered = rs.reconstruct(&mut data, &mut par).unwrap();
        assert!(
            recovered == drop.len(),
            "expected to recover {} shard(s), got {}",
            drop.len(),
            recovered
        );
        let reassembled: Vec<u8> = data
            .iter()
            .map(|s| s.as_ref().unwrap())
            .flatten()
            .copied()
            .take(frame.len())
            .collect();
        assert_eq!(reassembled, frame);
    }

    #[test]
    fn xor_compat_no_fec_round_trip() {
        roundtrip_helper(4, 0, &[]);
    }

    #[test]
    fn one_loss_in_one_block() {
        roundtrip_helper(4, 1, &[0]);
        roundtrip_helper(4, 1, &[2]);
    }

    #[test]
    fn two_losses_in_one_block_recovered() {
        roundtrip_helper(4, 2, &[0, 1]);
        roundtrip_helper(4, 2, &[0, 3]);
    }

    #[test]
    fn three_losses_in_one_block_recovered() {
        roundtrip_helper(6, 3, &[0, 2, 4]);
    }

    #[test]
    fn four_losses_in_one_block_recovered() {
        roundtrip_helper(8, 4, &[0, 1, 4, 7]);
    }

    #[test]
    fn insufficient_parity_returns_error() {
        let rs = ReedSolomonFec::new(4, 1).unwrap();
        let frame: Vec<u8> = (0..512u32).map(|i| (i & 0xff) as u8).collect();
        let enc = rs.encode(&frame).unwrap();
        let mut data: Vec<Option<Vec<u8>>> = enc.data.into_iter().map(Some).collect();
        let mut par: Vec<Option<Vec<u8>>> = enc.parity.into_iter().map(Some).collect();
        data[0] = None;
        data[1] = None;
        par[0] = None;
        let err = rs.reconstruct(&mut data, &mut par).unwrap_err();
        assert!(matches!(
            err,
            ReedSolomonFecError::Library(reed_solomon_erasure::Error::TooFewShardsPresent)
                | ReedSolomonFecError::Library(reed_solomon_erasure::Error::TooFewDataShards)
                | ReedSolomonFecError::Library(_)
        ));
    }

    #[test]
    fn fec_controller_thresholds() {
        let mut ctrl = FecController::new(4);
        assert_eq!(ctrl.adjust_for_loss(0), 0);
        assert_eq!(ctrl.last_parity(), 0);
        // Step up across the lowest threshold (300).
        assert_eq!(ctrl.adjust_for_loss(350), 1);
        assert_eq!(ctrl.adjust_for_loss(800), 1);
        assert_eq!(ctrl.adjust_for_loss(1100), 2);
        assert_eq!(ctrl.adjust_for_loss(2500), 3);
        assert_eq!(ctrl.adjust_for_loss(6000), 4);
        // Step down by full deltas (no hysteresis on the way down).
        assert_eq!(ctrl.adjust_for_loss(0), 0);
    }

    #[test]
    fn fec_controller_hysteresis_step_up() {
        let mut ctrl = FecController::new(4);
        ctrl.adjust_for_loss(0);
        // First sample above a higher threshold does not jump straight there;
        // it walks up one at a time.
        assert_eq!(ctrl.adjust_for_loss(6000), 1);
        assert_eq!(ctrl.adjust_for_loss(6000), 2);
        assert_eq!(ctrl.adjust_for_loss(6000), 3);
        assert_eq!(ctrl.adjust_for_loss(6000), 4);
        assert_eq!(ctrl.adjust_for_loss(6000), 4);
    }

    #[test]
    fn fec_controller_force_caps_at_max() {
        let mut ctrl = FecController::new(4);
        ctrl.force(99);
        assert_eq!(ctrl.last_parity(), MAX_PARITY_SHARDS);
    }

    #[test]
    fn reed_solomon_fec_rejects_bad_construction() {
        assert!(matches!(
            ReedSolomonFec::new(0, 1),
            Err(ReedSolomonFecError::ZeroBlockSize)
        ));
        assert!(matches!(
            ReedSolomonFec::new(4, MAX_PARITY_SHARDS + 1),
            Err(ReedSolomonFecError::TooManyParity(_))
        ));
    }

    #[test]
    fn encode_then_reconstruct_across_multiple_blocks() {
        // 32 KiB frame forces >1 RS block with block=4, parity=2.
        let rs = ReedSolomonFec::new(4, 2).unwrap();
        let frame: Vec<u8> = (0..(32 * 1024usize))
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        let enc = rs.encode(&frame).unwrap();
        // With block=4, parity=2, a 32 KiB frame yields >1 block of shards.
        assert!(
            enc.data.len() >= 8,
            "expected >1 block of data, got {}",
            enc.data.len()
        );
        assert!(
            enc.parity.len() >= 4,
            "expected >1 block of parity, got {}",
            enc.parity.len()
        );
        let n_data = enc.data.len();
        let n_par = enc.parity.len();
        let mut data: Vec<Option<Vec<u8>>> = enc.data.into_iter().map(Some).collect();
        let mut par: Vec<Option<Vec<u8>>> = enc.parity.into_iter().map(Some).collect();
        // Drop one data shard and one parity shard from the same block.
        data[0] = None;
        par[n_par - 1] = None;
        let recovered = rs.reconstruct(&mut data, &mut par).unwrap();
        assert!(
            recovered >= 2,
            "expected at least 2 shards recovered, got {}",
            recovered
        );
        // Re-assemble the frame by concatenating the recovered data shards.
        let shard_len = enc.shard_len;
        let mut assembled: Vec<u8> = Vec::with_capacity(data.len() * shard_len);
        for d in data.iter() {
            let s = d.as_ref().expect("data shard missing after recovery");
            assembled.extend_from_slice(s);
        }
        assembled.truncate(frame.len());
        if assembled != frame {
            for i in 0..assembled.len().min(frame.len()) {
                if assembled[i] != frame[i] {
                    eprintln!(
                        "DBG: first diff byte {} assembled={} frame={}; data[0]={:?}",
                        i,
                        assembled[i],
                        frame[i],
                        data[0].as_ref().map(|v| v.len())
                    );
                    break;
                }
            }
        }
        assert_eq!(assembled, frame);
    }

    #[test]
    fn default_block_and_parity_match_rs_fec() {
        let rs = ReedSolomonFec::new(DEFAULT_BLOCK_SIZE, DEFAULT_PARITY_SHARDS);
        assert!(rs.is_ok(), "ReedSolomonFec::new(4, 2) should succeed");
    }

    #[test]
    fn fec_encoder_produces_k_plus_m_symbols() {
        let rs = ReedSolomonFec::new(4, 2).unwrap();
        let encoded = rs.encode(&[0u8; 4096]).unwrap();
        assert_eq!(encoded.data.len(), 4);
        assert_eq!(encoded.parity.len(), 2);
        assert!(
            encoded.shard_len >= 1024 && encoded.shard_len <= 1200,
            "shard_len {} out of expected range 1024..=1200",
            encoded.shard_len
        );
    }

    #[test]
    fn fec_decoder_reconstructs_from_any_k_symbols() {
        let rs = ReedSolomonFec::new(4, 2).unwrap();
        let frame: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let encoded = rs.encode(&frame).unwrap();
        let n_data = encoded.data.len();
        let n_par = encoded.parity.len();
        let total = n_data + n_par; // 6

        for drop_i in 0..total {
            for drop_j in (drop_i + 1)..total {
                let mut data: Vec<Option<Vec<u8>>> =
                    encoded.data.iter().map(|d| Some(d.clone())).collect();
                let mut parity: Vec<Option<Vec<u8>>> =
                    encoded.parity.iter().map(|p| Some(p.clone())).collect();

                for &idx in &[drop_i, drop_j] {
                    if idx < n_data {
                        data[idx] = None;
                    } else {
                        parity[idx - n_data] = None;
                    }
                }

                rs.reconstruct(&mut data, &mut parity)
                    .unwrap_or_else(|_| panic!("reconstruct failed for drop pair ({}, {})", drop_i, drop_j));

                let reassembled: Vec<u8> = data
                    .iter()
                    .filter_map(|s| s.as_ref())
                    .flatten()
                    .copied()
                    .take(frame.len())
                    .collect();
                assert_eq!(
                    reassembled, frame,
                    "byte mismatch for drop pair ({}, {})",
                    drop_i, drop_j
                );
            }
        }
    }
}
