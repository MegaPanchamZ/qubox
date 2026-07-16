//! FEC block decoder state machine.
//!
//! Coordinates shard accumulation, Reed–Solomon reconstruction, and
//! timeout-based discard for incoming media datagrams. Wraps the
//! lower-level `ReedSolomonFec` with a state machine:
//!
//!   Pending → Ready → Emitted → (pruned on drain)

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::rs_fec::ReedSolomonFec;

/// Block timeout. Matches the jitter buffer target_delay upper bound.
pub const FEC_BLOCK_TIMEOUT: Duration = Duration::from_millis(200);

/// Max simultaneous pending blocks. At k=4/m=2 with ~3KB shards,
/// each block holds ~18KB. 16 blocks = ~288KB decoder buffer.
pub const FEC_MAX_PENDING_BLOCKS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockState {
    Pending,
    Emitted,
}

#[derive(Debug)]
struct PendingBlock {
    data: Vec<Option<Vec<u8>>>,
    parity: Vec<Option<Vec<u8>>>,
    chunk_count: u16,
    first_arrival: Instant,
    deadline: Instant,
    state: BlockState,
}

/// Per-decoder statistics counters.
#[derive(Debug, Default, Clone)]
pub struct FecDecoderStats {
    pub blocks_started: u64,
    pub blocks_recovered_via_fec: u64,
    pub blocks_emitted_direct: u64,
    pub blocks_discarded: u64,
    pub late_shards_dropped: u64,
}

/// FEC block decoder with timeout-based discard.
///
/// Tracks pending blocks keyed by `frame_id`. Each block accumulates
/// data and parity shards. When enough shards arrive (`≥ k`), the block
/// is either emitted directly (all data present) or recovered via FEC.
/// Stale blocks exceeding the timeout are reported via `drain_expired`.
pub struct FecDecoder {
    rs: ReedSolomonFec,
    timeout: Duration,
    pending: BTreeMap<u32, PendingBlock>,
    stats: FecDecoderStats,
}

impl FecDecoder {
    /// Construct a new decoder wrapping the given `ReedSolomonFec`.
    ///
    /// `timeout` is the per-block deadline from first shard arrival.
    /// `_max_pending` reserves the max simultaneous pending blocks
    /// (reserved for future eviction logic).
    pub fn new(rs: ReedSolomonFec, timeout: Duration, _max_pending: usize) -> Self {
        FecDecoder {
            rs,
            timeout,
            pending: BTreeMap::new(),
            stats: FecDecoderStats::default(),
        }
    }

    /// Feed one data or parity shard.
    ///
    /// Returns `Some(Vec<u8>)` (= the reconstructed access unit) only
    /// when the block becomes Ready on this shard. Returns `None` if
    /// the block is still pending, has already been emitted, or
    /// reconstruction fails.
    pub fn add_shard(
        &mut self,
        frame_id: u32,
        chunk_id: u16,
        chunk_count: u16,
        is_parity: bool,
        payload: &[u8],
        now: Instant,
    ) -> Option<Vec<u8>> {
        let block = self.pending.entry(frame_id).or_insert_with(|| {
            self.stats.blocks_started += 1;
            PendingBlock {
                data: Vec::new(),
                parity: Vec::new(),
                chunk_count,
                first_arrival: now,
                deadline: now + self.timeout,
                state: BlockState::Pending,
            }
        });

        // Late shard for an already-emitted block.
        if block.state != BlockState::Pending {
            self.stats.late_shards_dropped += 1;
            return None;
        }

        // Update chunk_count (all data shards carry the true value).
        if !is_parity && chunk_count > block.chunk_count {
            block.chunk_count = chunk_count;
        }

        let shard = payload.to_vec();
        if is_parity {
            let idx = chunk_id as usize;
            if idx >= block.parity.len() {
                block.parity.resize(idx + 1, None);
            }
            if block.parity[idx].is_some() {
                return None; // duplicate
            }
            block.parity[idx] = Some(shard);
        } else {
            let idx = chunk_id as usize;
            if idx >= block.data.len() {
                block.data.resize(idx + 1, None);
            }
            if block.data[idx].is_some() {
                return None;
            }
            block.data[idx] = Some(shard);
        }

        let k = self.rs.block_size();
        let data_count = block.data.iter().filter(|s| s.is_some()).count();
        let parity_count = block.parity.iter().filter(|s| s.is_some()).count();
        let total = data_count + parity_count;
        let cc = block.chunk_count as usize;

        if total >= k && data_count > 0 && cc > 0 {
            // Ensure vecs are fully sized.
            while block.data.len() < cc {
                block.data.push(None);
            }
            let num_blocks = (cc + k - 1) / k;
            let m = self.rs.parity_shards();
            let parity_len = num_blocks * m;
            while block.parity.len() < parity_len {
                block.parity.push(None);
            }

            if data_count == cc {
                // All data present → emit directly.
                block.state = BlockState::Emitted;
                self.stats.blocks_emitted_direct += 1;
                return Some(Self::assemble(&block.data));
            }

            // Try FEC recovery on a clone (safe on failure).
            let mut data_try = block.data.clone();
            let mut parity_try = block.parity.clone();
            match self.rs.reconstruct(&mut data_try, &mut parity_try) {
                Ok(_) => {
                    block.data = data_try;
                    block.parity = parity_try;
                    block.state = BlockState::Emitted;
                    self.stats.blocks_recovered_via_fec += 1;
                    Some(Self::assemble(&block.data))
                }
                Err(_) => None,
            }
        } else {
            None
        }
    }

    /// Drain blocks whose deadline has passed.
    ///
    /// Returns the discarded `frame_id`s so the caller can emit
    /// `ControlMsg::BlockDiscarded` to the peer.
    pub fn drain_expired(&mut self, now: Instant) -> Vec<u32> {
        let mut expired = Vec::new();
        let mut to_remove = Vec::new();

        for (&frame_id, block) in &self.pending {
            match block.state {
                BlockState::Emitted => {
                    to_remove.push(frame_id);
                }
                BlockState::Pending if now >= block.deadline => {
                    self.stats.blocks_discarded += 1;
                    expired.push(frame_id);
                    to_remove.push(frame_id);
                }
                _ => {}
            }
        }

        for id in to_remove {
            self.pending.remove(&id);
        }

        expired
    }

    /// Immutable reference to the decoder statistics.
    pub fn stats(&self) -> &FecDecoderStats {
        &self.stats
    }

    fn assemble(data: &[Option<Vec<u8>>]) -> Vec<u8> {
        let mut out = Vec::new();
        for s in data.iter().flatten() {
            out.extend_from_slice(s);
        }
        out
    }
}

// TODO(adr-014): raptorq-deferred — RaptorQ (RFC 6330) is on the
// roadmap for very high-loss / fat networks. Requires fountain-code
// data flow (no fixed block_id). Defer to ADR-022.

#[cfg(test)]
mod tests {
    use super::*;

    fn build_encoded(
        block_size: usize,
        parity_shards: usize,
        data_len: usize,
    ) -> (ReedSolomonFec, Vec<u8>, super::super::rs_fec::EncodedFrame) {
        let rs = ReedSolomonFec::new(block_size, parity_shards).unwrap();
        let frame: Vec<u8> = (0..data_len as u32).map(|i| (i % 251) as u8).collect();
        let encoded = rs.encode(&frame).unwrap();
        (rs, frame, encoded)
    }

    #[test]
    fn fec_decoder_emits_reconstructed_bytes() {
        let (rs, frame, encoded) = build_encoded(4, 2, 4096);
        let mut decoder = FecDecoder::new(rs, FEC_BLOCK_TIMEOUT, FEC_MAX_PENDING_BLOCKS);
        let now = Instant::now();

        let mut result = None;
        let chunk_count = encoded.data.len() as u16;
        for (i, shard) in encoded.data.iter().enumerate() {
            let ret = decoder.add_shard(0, i as u16, chunk_count, false, shard, now);
            if i == 3 {
                result = ret;
            }
        }

        let decoded = result.expect("4th data shard should trigger emit");
        assert_eq!(decoded.len(), encoded.shard_len * encoded.data.len());
        assert_eq!(&decoded[..frame.len()], &frame[..]);
    }

    #[test]
    fn fec_decoder_drops_blocks_after_200ms() {
        let (rs, _frame, encoded) = build_encoded(4, 2, 4096);
        let mut decoder = FecDecoder::new(rs, Duration::from_millis(200), 16);
        let start = Instant::now();
        let chunk_count = encoded.data.len() as u16;

        // Add 3 of 4 data shards — not enough to reconstruct.
        for i in 0..3 {
            decoder.add_shard(0, i as u16, chunk_count, false, &encoded.data[i], start);
        }

        // Drain expired at start + 250 ms (past the 200 ms deadline).
        let expired = decoder.drain_expired(start + Duration::from_millis(250));
        assert!(expired.contains(&0), "frame 0 should be expired");

        // Late 4th shard: block was already freed → creates new block
        // with only 1 shard, returns None.
        let late = decoder.add_shard(
            0,
            3,
            chunk_count,
            false,
            &encoded.data[3],
            start + Duration::from_millis(250),
        );
        assert!(late.is_none(), "late shard should return None");
    }

    #[test]
    fn fec_decoder_late_shard_for_freed_block_is_dropped() {
        let (rs, _frame, encoded) = build_encoded(4, 2, 4096);
        let mut decoder = FecDecoder::new(rs, FEC_BLOCK_TIMEOUT, FEC_MAX_PENDING_BLOCKS);
        let now = Instant::now();
        let chunk_count = encoded.data.len() as u16;

        // Emit the block by providing all 4 data shards.
        for (i, shard) in encoded.data.iter().enumerate() {
            decoder.add_shard(0, i as u16, chunk_count, false, shard, now);
        }

        // Late shard for the same frame_id.
        let late = decoder.add_shard(0, 4, chunk_count, true, &encoded.parity[0], now);
        assert!(late.is_none());
        assert_eq!(decoder.stats.late_shards_dropped, 1);
    }

    #[test]
    fn old_client_drops_parity() {
        // Simulate old client: receives only data shards (ignores
        // parity with FLAG_PARITY). With all data shards present,
        // the decoder emits without FEC.
        let (rs, frame, encoded) = build_encoded(4, 2, 4096);
        let mut decoder = FecDecoder::new(rs, FEC_BLOCK_TIMEOUT, FEC_MAX_PENDING_BLOCKS);
        let now = Instant::now();
        let chunk_count = encoded.data.len() as u16;

        // Feed only data shards, skip parity.
        let mut result = None;
        for (i, shard) in encoded.data.iter().enumerate() {
            let ret = decoder.add_shard(0, i as u16, chunk_count, false, shard, now);
            if i == 3 {
                result = ret;
            }
        }

        let decoded = result.expect("all data shards should emit directly");
        assert_eq!(&decoded[..frame.len()], &frame[..]);
        assert_eq!(decoder.stats.blocks_emitted_direct, 1);
    }

    #[test]
    fn fec_decoder_recovery_via_parity() {
        // Feed 3 data + 1 parity → should recover the missing data shard.
        let (rs, frame, encoded) = build_encoded(4, 2, 4096);
        let mut decoder = FecDecoder::new(rs, FEC_BLOCK_TIMEOUT, FEC_MAX_PENDING_BLOCKS);
        let now = Instant::now();
        let chunk_count = encoded.data.len() as u16;
        let shard_len = encoded.shard_len;

        // Drop data[3], feed data[0..3) + parity[0].
        let mut result = None;
        for i in 0..3 {
            decoder.add_shard(0, i as u16, chunk_count, false, &encoded.data[i], now);
        }
        let ret = decoder.add_shard(0, 0, chunk_count, true, &encoded.parity[0], now);
        result = ret;

        let decoded = result.expect("3 data + 1 parity should trigger FEC recovery");
        assert!(
            decoded.len() >= shard_len * data_chunks(&encoded),
            "decoded len {} >= {}",
            decoded.len(),
            shard_len * data_chunks(&encoded)
        );
        assert_eq!(&decoded[..frame.len()], &frame[..]);
        assert_eq!(decoder.stats.blocks_recovered_via_fec, 1);
    }

    #[test]
    fn fec_decoder_parity_insufficient_returns_none() {
        // Only 1 data + 1 parity = 2 total < k=4 → can't reconstruct.
        let (rs, _frame, encoded) = build_encoded(4, 2, 4096);
        let mut decoder = FecDecoder::new(rs, FEC_BLOCK_TIMEOUT, FEC_MAX_PENDING_BLOCKS);
        let now = Instant::now();
        let chunk_count = encoded.data.len() as u16;

        decoder.add_shard(0, 0, chunk_count, false, &encoded.data[0], now);
        let ret = decoder.add_shard(0, 0, chunk_count, true, &encoded.parity[0], now);
        assert!(ret.is_none(), "not enough shards should return None");
    }

    fn data_chunks(encoded: &super::super::rs_fec::EncodedFrame) -> usize {
        encoded.data.len()
    }
}
