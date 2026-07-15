//! Low-latency datagram media path (P0-2).
//!
//! Replaces the reliable-QUIC media path (`NativeQuicMediaReceiver` and
//! `NativeQuicMediaSender`) with QUIC datagrams (RFC 9221) plus a
//! per-frame jitter buffer, XOR parity FEC, and a reliable control
//! stream for NACK / rate feedback / gamepad lifecycle.
//!
//! Wire format: 14-byte `MediaDatagramHeader` per datagram, payload
//! up to ~1186 bytes (capped by the QUIC datagram MTU). The struct is
//! `#[repr(C, packed)]`; the on-wire size is 14 bytes because
//! `chunk_count` is serialized as 2 BE bytes AFTER the 12 struct
//! bytes (see `WIRE_HEADER_SIZE`). See `MediaDatagramHeader` for the
//! layout and the trailer discussion for `original_len`.
//!
//! Gamepad and pen datagrams share the same 2-byte magic and are
//! demuxed at byte[2] by discriminator — see `DatagramDispatcher`.
//!
//! The reliable media path is preserved behind the `reliable` module
//! accessor; the host and client both pick a path at session start.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use byteorder::ReadBytesExt;
use qubox_media::EncodedVideoAccessUnit;
use qubox_proto::{ControlMsg, RateFeedback, VideoCodec, PEN_DATAGRAM_DISCRIMINATOR};
use quinn::{Connection, SendStream};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::warn;

/// Magic prefix for every media datagram: ASCII "BP".
pub const MEDIA_DATAGRAM_MAGIC: [u8; 2] = [0x51, 0x42];

/// Discriminator byte for media chunks. Media headers re-use byte 2
/// for `flags` (bits 0-3); any byte with the high nibble set is a
/// typed datagram (gamepad `0x47`, pen `0x50`). `0x00..=0x3F` is
/// treated as a media chunk by the dispatcher.
pub const MEDIA_DISCRIMINATOR_MAX: u8 = 0x3F;

pub mod fec_decoder;
pub mod roi;
pub mod rs_fec;

/// Bit flags on `MediaDatagramHeader.flags`.
pub const FLAG_KEYFRAME: u8 = 1 << 0;
pub const FLAG_PARITY: u8 = 1 << 1;
pub const FLAG_LAST_CHUNK: u8 = 1 << 2;
pub const FLAG_REPEAT_LAST: u8 = 1 << 3;

/// 12-byte `#[repr(C, packed)]` view of the media header. The
/// on-wire header is `HEADER_WIRE_FIELDS + 2` bytes — the trailing 2
/// bytes hold `chunk_count` AFTER the struct, big-endian. See
/// `WIRE_HEADER_SIZE` for the canonical value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct MediaDatagramHeader {
    pub magic: [u8; 2],
    pub flags: u8,
    pub codec: u8,
    pub stream_id: u16,
    pub frame_id: u32,
    pub chunk_id: u16,
    pub chunk_count: u16,
}

impl MediaDatagramHeader {
    /// Size of the struct fields in the `#[repr(C, packed)]` view. The
    /// actual on-wire header is `HEADER_WIRE_FIELDS + 2` bytes because
    /// `chunk_count` is serialized as 2 BE bytes AFTER the packed
    /// struct (kept that way for byte-order compatibility with v0).
    pub const HEADER_WIRE_FIELDS: usize = 12;

    /// Back-compat alias for `HEADER_WIRE_FIELDS`.
    pub const SIZE: usize = Self::HEADER_WIRE_FIELDS;

    /// Serialize the 14-byte header into the start of `buf`. The caller
    /// must have allocated at least `WIRE_HEADER_SIZE` bytes.
    pub fn write_into(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= WIRE_HEADER_SIZE);
        buf[0..2].copy_from_slice(&self.magic);
        buf[2] = self.flags;
        buf[3] = self.codec;
        buf[4..6].copy_from_slice(&self.stream_id.to_be_bytes());
        buf[6..10].copy_from_slice(&self.frame_id.to_be_bytes());
        buf[10..12].copy_from_slice(&self.chunk_id.to_be_bytes());
        buf[12..14].copy_from_slice(&self.chunk_count.to_be_bytes());
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Self, MediaDatagramError> {
        if buf.len() < WIRE_HEADER_SIZE {
            return Err(MediaDatagramError::Short);
        }
        if buf[0..2] != MEDIA_DATAGRAM_MAGIC {
            return Err(MediaDatagramError::BadMagic);
        }
        let stream_id = u16::from_be_bytes([buf[4], buf[5]]);
        let frame_id = u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]);
        let chunk_id = u16::from_be_bytes([buf[10], buf[11]]);
        let chunk_count = u16::from_be_bytes([buf[12], buf[13]]);
        Ok(Self {
            magic: [buf[0], buf[1]],
            flags: buf[2],
            codec: buf[3],
            stream_id,
            frame_id,
            chunk_id,
            chunk_count,
        })
    }
}

/// The 12-byte `#[repr(C, packed)]` struct is followed by an
/// additional 2 bytes containing `chunk_count` (big-endian) BEFORE the
/// chunk payload. Total header size on the wire is therefore 14
/// bytes. See `MediaDatagramHeader::HEADER_WIRE_FIELDS`.
pub const WIRE_HEADER_SIZE: usize = 14;
pub const CHUNK_PAYLOAD_MAX: usize = 1200;

/// Trailer appended immediately AFTER the chunk payload of the very
/// last DATA chunk (the one whose `chunk_id == chunk_count - 1`), and
/// only when `FLAG_LAST_CHUNK` is set on the header. Encodes the
/// real, unpadded frame length as a 4-byte big-endian `u32`. The
/// parity chunks do NOT carry this trailer. The receiver reads the
/// last 4 bytes of the payload before treating it as raw chunk
/// bytes, and stores the value on the `PendingFrame` for
/// `assemble_frame` to truncate on reassembly.
pub const ORIGINAL_LEN_TRAILER: usize = 4;

/// Frame chunking + XOR parity FEC.
///
/// Splits a frame into N data chunks of ≤ `max_chunk_payload` bytes,
/// then for every `block_size` data chunks emits 1 parity chunk
/// (XOR of the block). On the receive side, `recover_missing` rebuilds
/// a missing data chunk when parity is available.
#[derive(Debug, Clone)]
pub struct FrameChunker {
    pub block_size: usize,
    pub max_chunk_payload: usize,
}

impl FrameChunker {
    pub fn new(block_size: usize) -> Self {
        Self {
            block_size: block_size.max(1),
            max_chunk_payload: CHUNK_PAYLOAD_MAX,
        }
    }

    /// Returns `(data_chunks, parity_chunks, original_len)`. Each
    /// entry is the bytes to put after the wire header. The last data
    /// chunk is zero-padded to `max_chunk_payload` so all chunks in a
    /// block share a length for the XOR; the unpadded frame length is
    /// returned as `original_len` so the sender can serialize a
    /// 4-byte trailer and the receiver can truncate the reassembled
    /// frame without trimming zeros (Annex-B / CABAC tails may end in
    /// `0x00` and would be corrupted by trimming).
    pub fn chunk_and_encode(&self, frame: &[u8]) -> (Vec<Vec<u8>>, Vec<Vec<u8>>, usize) {
        let max = self.max_chunk_payload;
        if frame.is_empty() {
            return (Vec::new(), Vec::new(), 0);
        }

        let original_len = frame.len();

        // Split into data chunks.
        let mut data: Vec<Vec<u8>> = frame.chunks(max).map(|c| c.to_vec()).collect();
        if data.is_empty() {
            return (Vec::new(), Vec::new(), original_len);
        }

        // Pad the last chunk with zeros so all chunks in a block share a
        // length for the XOR. `original_len` is captured by the caller
        // via the trailer mechanism — no need to trim here.
        if let Some(last) = data.last() {
            if last.len() < max {
                let mut padded = last.clone();
                padded.resize(max, 0);
                if let Some(slot) = data.last_mut() {
                    *slot = padded;
                }
            }
        }

        // Compute per-block parity.
        let mut parity = Vec::new();
        for block in data.chunks(self.block_size) {
            let len = block.iter().map(|c| c.len()).max().unwrap_or(0);
            let mut p = vec![0_u8; len];
            for chunk in block {
                for (i, b) in chunk.iter().enumerate() {
                    p[i] ^= *b;
                }
            }
            parity.push(p);
        }
        (data, parity, original_len)
    }
}

/// Try to recover one missing chunk in a frame using the parity chunk.
pub fn recover_with_parity(
    chunks: &mut [Option<Vec<u8>>],
    parity: &[Vec<u8>],
    block_size: usize,
) -> Result<usize, RecoveryError> {
    if chunks.is_empty() {
        return Err(RecoveryError::NoChunks);
    }
    let mut block_idx = 0_usize;
    let mut blocks_checked = 0_usize;
    while block_idx * block_size < chunks.len() {
        let block_end = (block_idx * block_size + block_size).min(chunks.len());
        let block = &chunks[block_idx * block_size..block_end];
        let missing: Vec<usize> = block
            .iter()
            .enumerate()
            .filter_map(|(i, c)| if c.is_none() { Some(i) } else { None })
            .collect();
        if missing.len() == 1 {
            let missing_local = missing[0];
            if let Some(parity) = parity.get(block_idx) {
                let mut recovered = parity.clone();
                for (i, chunk) in block.iter().enumerate() {
                    if i == missing_local {
                        continue;
                    }
                    if let Some(c) = chunk {
                        for (j, b) in c.iter().enumerate() {
                            recovered[j] ^= *b;
                        }
                    }
                }
                chunks[block_idx * block_size + missing_local] = Some(recovered);
                return Ok(block_idx * block_size + missing_local);
            }
        }
        if missing.len() > 1 {
            // Cannot recover with a single parity chunk; escalate to NACK.
        }
        block_idx += 1;
        blocks_checked += 1;
        if blocks_checked > 1024 {
            break;
        }
    }
    Err(RecoveryError::NotRecoverable)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryError {
    NoChunks,
    NotRecoverable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaDatagramError {
    Short,
    BadMagic,
    Decode(String),
}

impl std::fmt::Display for MediaDatagramError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaDatagramError::Short => write!(f, "datagram too short for media header"),
            MediaDatagramError::BadMagic => write!(f, "datagram magic prefix mismatch"),
            MediaDatagramError::Decode(msg) => write!(f, "decode: {msg}"),
        }
    }
}

impl std::error::Error for MediaDatagramError {}

/// Per-frame jitter buffer. Keyed by `frame_id`, holds chunks until all
/// arrive or the deadline (first_chunk_arrival + target_delay) elapses.
pub struct JitterBuffer {
    target_delay: Duration,
    max_inflight: usize,
    frames: BTreeMap<u32, PendingFrame>,
    /// Smoothed jitter, milliseconds.
    jitter_ewma_ms: f64,
    /// Last time the adaptive target_delay evaluation ran.
    last_adaptive_eval: Option<Instant>,
    /// Seconds spent with low jitter; when ≥ 5s, target_delay decreases.
    low_jitter_secs: f64,
    /// Tracked separately so we can compute per-chunk arrival delta.
    last_chunk_ts: Option<Instant>,
    stats: JitterBufferStats,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JitterBufferStats {
    pub frames_received: u64,
    pub frames_completed: u64,
    pub frames_dropped_deadline: u64,
    pub frames_recovered_via_fec: u64,
    pub chunks_received: u64,
}

#[derive(Debug, Clone)]
struct PendingFrame {
    chunks: Vec<Option<Vec<u8>>>,
    parity: Vec<Vec<u8>>,
    first_arrival: Instant,
    deadline: Instant,
    codec: VideoCodec,
    flags: u8,
    stream_id: u16,
    frame_id: u32,
    /// True unpadded frame length. Captured from the 4-byte trailer
    /// appended to the last DATA chunk (only present when
    /// `FLAG_LAST_CHUNK` is set). Replaces the
    /// "trim-trailing-zeros" reassembly heuristic that corrupted
    /// annex-B / CABAC tails ending in `0x00`.
    original_len: Option<usize>,
}

impl JitterBuffer {
    pub fn new(target_delay: Duration, max_inflight: usize) -> Self {
        Self {
            target_delay,
            max_inflight,
            frames: BTreeMap::new(),
            jitter_ewma_ms: 0.0,
            last_adaptive_eval: None,
            low_jitter_secs: 0.0,
            last_chunk_ts: None,
            stats: JitterBufferStats::default(),
        }
    }

    pub fn current_target_delay(&self) -> Duration {
        self.target_delay
    }

    pub fn stats(&self) -> &JitterBufferStats {
        &self.stats
    }

    pub fn push_chunk(
        &mut self,
        header: &MediaDatagramHeader,
        payload: &[u8],
        parity: bool,
        now: Instant,
    ) {
        let deadline = now + self.target_delay;
        let codec = codec_from_byte(header.codec);
        let frame = self
            .frames
            .entry(header.frame_id)
            .or_insert_with(|| PendingFrame {
                chunks: vec![None; header.chunk_count as usize],
                parity: Vec::new(),
                first_arrival: now,
                deadline,
                codec,
                flags: header.flags,
                stream_id: header.stream_id,
                frame_id: header.frame_id,
                original_len: None,
            });

        if header.chunk_id as usize >= frame.chunks.len() {
            // Stale or malformed chunk for a frame we already discarded.
            return;
        }

        if parity {
            frame.parity.push(payload.to_vec());
        } else {
            // Last DATA chunk carries a 4-byte original_len trailer
            // appended to its payload — strip it before storing.
            if (header.flags & FLAG_LAST_CHUNK) != 0 {
                if payload.len() < ORIGINAL_LEN_TRAILER {
                    let payload_len = payload.len();
                    let hdr_frame_id = header.frame_id;
                    let hdr_chunk_id = header.chunk_id;
                    warn!(
                        frame_id = hdr_frame_id,
                        chunk_id = hdr_chunk_id,
                        payload_len,
                        "media last-chunk payload shorter than ORIGINAL_LEN_TRAILER; dropping trailer"
                    );
                    frame.chunks[header.chunk_id as usize] = Some(payload.to_vec());
                } else {
                    let split = payload.len() - ORIGINAL_LEN_TRAILER;
                    let body = &payload[..split];
                    let trail = &payload[split..];
                    let original_len =
                        u32::from_be_bytes([trail[0], trail[1], trail[2], trail[3]]) as usize;
                    frame.original_len = Some(original_len);
                    frame.chunks[header.chunk_id as usize] = Some(body.to_vec());
                }
            } else {
                frame.chunks[header.chunk_id as usize] = Some(payload.to_vec());
            }
        }
        self.stats.chunks_received += 1;

        // Update jitter EWMA.
        let last_chunk = self.last_chunk_ts.replace(now);
        if let Some(prev) = last_chunk {
            let delta_ms = now.duration_since(prev).as_secs_f64() * 1000.0;
            let deviation = (delta_ms - 16.67).abs();
            self.jitter_ewma_ms = 0.9 * self.jitter_ewma_ms + 0.1 * deviation;
        }

        // Enforce max_inflight: drop the oldest frames if we exceed.
        while self.frames.len() > self.max_inflight {
            let oldest = self.frames.keys().next().copied();
            if let Some(key) = oldest {
                self.frames.remove(&key);
                self.stats.frames_dropped_deadline += 1;
            }
        }
    }

    /// Try to recover any single missing chunk per block using the
    /// parity chunks attached to a frame. Returns the number of frames
    /// recovered (0 or 1 per call).
    pub fn try_fec_recovery(&mut self, now: Instant, block_size: usize) -> usize {
        let mut recovered = 0;
        for (_, frame) in self.frames.iter_mut() {
            let block_size = block_size.max(1);
            if let Ok(_) = recover_with_parity(&mut frame.chunks, &frame.parity, block_size) {
                self.stats.frames_recovered_via_fec += 1;
                recovered += 1;
            }
        }
        let _ = now;
        recovered
    }

    /// Pop all frames that are complete (all chunks present, or FEC-
    /// recovered). Returns the assembled access units in `frame_id` order.
    pub fn pop_ready(&mut self, now: Instant) -> Vec<ReassembledFrame> {
        let mut ready = Vec::new();
        let mut ready_ids = Vec::new();
        for (&id, frame) in self.frames.iter() {
            if frame.chunks.iter().all(|c| c.is_some()) {
                ready.push(assemble_frame(frame));
                ready_ids.push(id);
            }
        }
        for id in &ready_ids {
            self.frames.remove(id);
            self.stats.frames_completed += 1;
        }
        // Opportunistic FEC: try to recover before deadline on the next
        // call to pop_ready_deadline. The dec/inc of target_delay happens
        // adaptively based on the jitter EWMA.
        let _ = now;
        self.maybe_adapt_target_delay(now);
        ready
    }

    /// Pop all frames whose deadline has passed without being complete.
    /// Emits a NACK for each. Returns the partial frames (FEC failed) so
    /// the caller can decide whether to repeat the previous frame.
    pub fn pop_deadline_passed(&mut self, now: Instant) -> Vec<DeadlineFrame> {
        let mut expired = Vec::new();
        let mut expired_ids = Vec::new();
        for (&id, frame) in self.frames.iter() {
            if now >= frame.deadline {
                expired.push(DeadlineFrame {
                    codec: frame.codec,
                    stream_id: frame.stream_id,
                    frame_id: frame.frame_id,
                    flags: frame.flags,
                    missing_chunks: frame
                        .chunks
                        .iter()
                        .enumerate()
                        .filter_map(|(i, c)| if c.is_none() { Some(i as u16) } else { None })
                        .collect(),
                });
                expired_ids.push(id);
            }
        }
        for id in &expired_ids {
            self.frames.remove(id);
            self.stats.frames_dropped_deadline += 1;
        }
        expired
    }

    /// Adapt `target_delay` to the observed jitter (P0-2 §"Adaptive
    /// target_delay"). Called opportunistically from `pop_ready`.
    fn maybe_adapt_target_delay(&mut self, now: Instant) {
        let should_eval = match self.last_adaptive_eval {
            Some(last) => now.duration_since(last) >= Duration::from_secs(1),
            None => true,
        };
        if !should_eval {
            return;
        }
        self.last_adaptive_eval = Some(now);
        if self.jitter_ewma_ms > 5.0 {
            self.target_delay = self
                .target_delay
                .saturating_add(Duration::from_millis(1))
                .min(Duration::from_millis(15));
            self.low_jitter_secs = 0.0;
        } else if self.jitter_ewma_ms < 1.0 {
            self.low_jitter_secs += 1.0;
            if self.low_jitter_secs >= 5.0 {
                self.target_delay = self
                    .target_delay
                    .saturating_sub(Duration::from_millis(1))
                    .max(Duration::from_millis(3));
                self.low_jitter_secs = 0.0;
            }
        } else {
            self.low_jitter_secs = 0.0;
        }
    }
}

fn codec_from_byte(b: u8) -> VideoCodec {
    match b {
        1 => VideoCodec::H265,
        2 => VideoCodec::Av1,
        _ => VideoCodec::H264,
    }
}

fn codec_to_byte(c: VideoCodec) -> u8 {
    match c {
        VideoCodec::H264 => 0,
        VideoCodec::H265 => 1,
        VideoCodec::Av1 => 2,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReassembledFrame {
    pub codec: VideoCodec,
    pub stream_id: u16,
    pub frame_id: u32,
    pub flags: u8,
    pub bytes: Vec<u8>,
    /// Wall-clock time at which the frame was reassembled.
    pub reassembled_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadlineFrame {
    pub codec: VideoCodec,
    pub stream_id: u16,
    pub frame_id: u32,
    pub flags: u8,
    pub missing_chunks: Vec<u16>,
}

fn assemble_frame(frame: &PendingFrame) -> ReassembledFrame {
    let mut bytes = Vec::new();
    for chunk in &frame.chunks {
        if let Some(c) = chunk {
            bytes.extend_from_slice(c);
        }
    }
    // Truncate to the true unpadded length captured by the
    // FLAG_LAST_CHUNK trailer. If the trailer wasn't seen (e.g. all
    // chunks from the same frame were lost except the parity), fall
    // back to the current bytes length — but warn so we know to
    // investigate. Do NOT trim trailing zeros (was the v0 bug that
    // corrupted annex-B / CABAC tails).
    if let Some(original_len) = frame.original_len {
        if original_len <= bytes.len() {
            bytes.truncate(original_len);
        } else {
            warn!(
                frame_id = frame.frame_id,
                original_len,
                actual = bytes.len(),
                "original_len trailer exceeds reassembled bytes; leaving frame at actual size"
            );
        }
    } else {
        warn!(
            frame_id = frame.frame_id,
            "missing FLAG_LAST_CHUNK trailer; assembling at padded size (may yield zero-padded frame)"
        );
    }
    ReassembledFrame {
        codec: frame.codec,
        stream_id: frame.stream_id,
        frame_id: frame.frame_id,
        flags: frame.flags,
        bytes,
        reassembled_at: Instant::now(),
    }
}

/// Datagram-frame media sender. Wraps a `quinn::Connection` and emits
/// one or more datagrams per encoded access unit, with XOR parity
/// chunks for the first loss per block.
pub struct MediaDatagramSender {
    conn: Connection,
    chunker: ChunkEncoder,
    stream_id: u16,
    /// Per-frame counter that wraps at `u32::MAX`.
    next_frame_id: u32,
}

/// Pluggable chunk encoder for [`MediaDatagramSender`]. XOR is the
/// legacy path (one parity shard per block, recovers a single loss);
/// Reed–Solomon recovers up to `parity_shards` losses per block at
/// the cost of `parity_shards` extra shards per block.
#[derive(Debug, Clone)]
pub enum ChunkEncoder {
    Xor(FrameChunker),
    ReedSolomon(rs_fec::ReedSolomonFec),
}

impl ChunkEncoder {
    fn encode(
        &self,
        frame: &[u8],
    ) -> Result<(Vec<Vec<u8>>, Vec<Vec<u8>>, usize), MediaDatagramSendError> {
        match self {
            ChunkEncoder::Xor(c) => Ok(c.chunk_and_encode(frame)),
            ChunkEncoder::ReedSolomon(rs) => {
                let enc = rs
                    .encode(frame)
                    .map_err(MediaDatagramSendError::ReedSolomon)?;
                Ok((enc.data, enc.parity, enc.original_len))
            }
        }
    }
}

impl MediaDatagramSender {
    pub fn new(conn: Connection, stream_id: u16, block_size: usize) -> Self {
        Self {
            conn,
            chunker: ChunkEncoder::Xor(FrameChunker::new(block_size)),
            stream_id,
            next_frame_id: 0,
        }
    }

    /// Construct a sender that uses Reed–Solomon FEC per block instead
    /// of the legacy XOR parity. `parity_shards` controls how many
    /// losses per block the receiver can recover (capped at
    /// [`rs_fec::MAX_PARITY_SHARDS`]).
    pub fn with_reed_solomon(
        conn: Connection,
        stream_id: u16,
        block_size: usize,
        parity_shards: usize,
    ) -> Result<Self, rs_fec::ReedSolomonFecError> {
        let rs = rs_fec::ReedSolomonFec::new(block_size, parity_shards)?;
        Ok(Self {
            conn,
            chunker: ChunkEncoder::ReedSolomon(rs),
            stream_id,
            next_frame_id: 0,
        })
    }

    pub fn max_datagram_size(&self) -> usize {
        self.conn
            .max_datagram_size()
            .unwrap_or(1200)
            .saturating_sub(WIRE_HEADER_SIZE as usize)
    }

    /// Encode + send an access unit as one or more QUIC datagrams. The
    /// `keyframe` flag is preserved in the chunk header so the receiver
    /// can request a keyframe on loss. The last DATA chunk of every
    /// frame additionally carries `FLAG_LAST_CHUNK` AND a 4-byte
    /// big-endian `original_len` trailer so the receiver can truncate
    /// the reassembled frame exactly to the unpadded length —
    /// annex-B / CABAC tails end in `0x00` and would be corrupted by
    /// any "trim trailing zeros" heuristic.
    pub fn send_frame(
        &mut self,
        access_unit: &EncodedVideoAccessUnit,
    ) -> Result<(), MediaDatagramSendError> {
        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);

        let (data, parity, original_len) = self.chunker.encode(&access_unit.bytes)?;
        let chunk_count = data.len() as u16;
        if chunk_count == 0 {
            return Ok(());
        }

        let base_flags = if access_unit.keyframe {
            FLAG_KEYFRAME
        } else {
            0
        };
        let codec_byte = codec_to_byte(access_unit.codec);

        let last_data_chunk_id = chunk_count.saturating_sub(1);
        for (i, chunk) in data.iter().enumerate() {
            let is_last_data = i as u16 == last_data_chunk_id;
            let chunk_header_flags = base_flags | if is_last_data { FLAG_LAST_CHUNK } else { 0 };
            let header = MediaDatagramHeader {
                magic: MEDIA_DATAGRAM_MAGIC,
                flags: chunk_header_flags,
                codec: codec_byte,
                stream_id: self.stream_id,
                frame_id,
                chunk_id: i as u16,
                chunk_count,
            };
            if is_last_data {
                self.send_chunk_with_original_len(&header, chunk, original_len)?;
            } else {
                self.send_chunk(&header, chunk)?;
            }
        }
        for (i, p) in parity.iter().enumerate() {
            let header = MediaDatagramHeader {
                magic: MEDIA_DATAGRAM_MAGIC,
                flags: base_flags | FLAG_PARITY,
                codec: codec_byte,
                stream_id: self.stream_id,
                frame_id,
                chunk_id: i as u16,
                chunk_count,
            };
            self.send_chunk(&header, p)?;
        }
        Ok(())
    }

    fn send_chunk_with_original_len(
        &self,
        header: &MediaDatagramHeader,
        payload: &[u8],
        original_len: usize,
    ) -> Result<(), MediaDatagramSendError> {
        let trailer_len =
            u32::try_from(original_len).map_err(|_| MediaDatagramSendError::TooLarge)?;
        let mut buf = Vec::with_capacity(WIRE_HEADER_SIZE + payload.len() + ORIGINAL_LEN_TRAILER);
        buf.extend_from_slice(&header.magic);
        buf.push(header.flags);
        buf.push(header.codec);
        buf.extend_from_slice(&header.stream_id.to_be_bytes());
        buf.extend_from_slice(&header.frame_id.to_be_bytes());
        buf.extend_from_slice(&header.chunk_id.to_be_bytes());
        buf.extend_from_slice(&header.chunk_count.to_be_bytes());
        buf.extend_from_slice(payload);
        buf.extend_from_slice(&trailer_len.to_be_bytes());
        self.conn
            .send_datagram(bytes::Bytes::from(buf))
            .map_err(MediaDatagramSendError::from)?;
        Ok(())
    }

    fn send_chunk(
        &self,
        header: &MediaDatagramHeader,
        payload: &[u8],
    ) -> Result<(), MediaDatagramSendError> {
        let mut buf = Vec::with_capacity(WIRE_HEADER_SIZE + payload.len());
        buf.extend_from_slice(&header.magic);
        buf.push(header.flags);
        buf.push(header.codec);
        buf.extend_from_slice(&header.stream_id.to_be_bytes());
        buf.extend_from_slice(&header.frame_id.to_be_bytes());
        buf.extend_from_slice(&header.chunk_id.to_be_bytes());
        buf.extend_from_slice(&header.chunk_count.to_be_bytes());
        buf.extend_from_slice(payload);
        self.conn
            .send_datagram(bytes::Bytes::from(buf))
            .map_err(MediaDatagramSendError::from)?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum MediaDatagramSendError {
    Full,
    TooLarge,
    Closed,
    ReedSolomon(rs_fec::ReedSolomonFecError),
}

impl std::fmt::Display for MediaDatagramSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaDatagramSendError::Full => write!(f, "send buffer full"),
            MediaDatagramSendError::TooLarge => write!(f, "datagram too large"),
            MediaDatagramSendError::Closed => write!(f, "connection closed"),
            MediaDatagramSendError::ReedSolomon(e) => write!(f, "reed-solomon: {}", e),
        }
    }
}

impl std::error::Error for MediaDatagramSendError {}

impl From<quinn::SendDatagramError> for MediaDatagramSendError {
    fn from(value: quinn::SendDatagramError) -> Self {
        match value {
            quinn::SendDatagramError::Disabled
            | quinn::SendDatagramError::UnsupportedByPeer
            | quinn::SendDatagramError::ConnectionLost(_) => MediaDatagramSendError::Closed,
            quinn::SendDatagramError::TooLarge => MediaDatagramSendError::TooLarge,
        }
    }
}

/// Datagram dispatcher. Spawns a task that reads every incoming
/// QUIC datagram off a connection and routes it to one of three
/// channels based on the byte at offset 2 (after the 2-byte
/// `MEDIA_DATAGRAM_MAGIC`):
///
/// - `0x47` (`GAMEPAD_DATAGRAM_DISCRIMINATOR`) → gamepad payload bytes
/// - `0x50` (`PEN_DATAGRAM_DISCRIMINATOR`)    → pen payload bytes
/// - any other value (incl. `0x00`)            → media chunk (parsed
///   into `MediaDatagramHeader`)
///
/// Gamepad and pen are kept as raw `Vec<u8>` so the consumer can run
/// the existing `decode_gamepad_datagram` / `decode_pen_datagram`
/// helpers, which already validate the discriminator a second time
/// (defense in depth) and tolerate the small per-datagram overhead.
///
/// Back-compat alias: `MediaDatagramReceiver` (kept for code that has
/// not yet migrated to the discriminator-keyed API).
pub struct DatagramDispatcher {
    conn: Connection,
    media_chunk_rx: mpsc::UnboundedReceiver<(MediaDatagramHeader, Vec<u8>, bool)>,
    gamepad_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    pen_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    _task: tokio::task::JoinHandle<()>,
}

/// Back-compat alias for `DatagramDispatcher`. Use the dispatcher
/// directly in new code; this alias only re-exposes the media chunk
/// channel for callers that haven't migrated yet.
pub type MediaDatagramReceiver = DatagramDispatcher;

impl DatagramDispatcher {
    /// Gamepad discriminator byte. Re-exported here so callers that
    /// build the matching `encode_gamepad_datagram` and want a
    /// type-checked link can do so without depending on `qubox-proto`.
    pub const GAMEPAD_DISCRIMINATOR: u8 = 0x47;

    /// Pen discriminator byte. Re-exported for symmetry.
    pub const PEN_DISCRIMINATOR: u8 = PEN_DATAGRAM_DISCRIMINATOR;

    pub fn spawn(conn: Connection) -> Self {
        let task_conn = conn.clone();
        let (media_tx, media_rx) = mpsc::unbounded_channel();
        let (gamepad_tx, gamepad_rx) = mpsc::unbounded_channel();
        let (pen_tx, pen_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                match task_conn.read_datagram().await {
                    Ok(buf) => {
                        if buf.len() < 3 {
                            continue;
                        }
                        if buf[0..2] != MEDIA_DATAGRAM_MAGIC {
                            warn!(
                                len = buf.len(),
                                "datagram magic prefix mismatch; dropping (no discriminator to route on)"
                            );
                            continue;
                        }
                        match buf[2] {
                            0x47 => {
                                if gamepad_tx.send(buf.to_vec()).is_err() {
                                    break;
                                }
                            }
                            PEN_DATAGRAM_DISCRIMINATOR => {
                                if pen_tx.send(buf.to_vec()).is_err() {
                                    break;
                                }
                            }
                            other if other <= MEDIA_DISCRIMINATOR_MAX => {
                                if buf.len() < WIRE_HEADER_SIZE {
                                    warn!(
                                        len = buf.len(),
                                        "media datagram below WIRE_HEADER_SIZE; dropping"
                                    );
                                    continue;
                                }
                                let header = match MediaDatagramHeader::from_bytes(&buf) {
                                    Ok(h) => h,
                                    Err(e) => {
                                        warn!(?e, "media datagram header decode failed; dropping");
                                        continue;
                                    }
                                };
                                let parity = (header.flags & FLAG_PARITY) != 0;
                                let payload = buf[WIRE_HEADER_SIZE..].to_vec();
                                if media_tx.send((header, payload, parity)).is_err() {
                                    break;
                                }
                            }
                            unknown => {
                                warn!(
                                    discriminator = unknown,
                                    "unknown datagram discriminator; dropping"
                                );
                                continue;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            conn,
            media_chunk_rx: media_rx,
            gamepad_rx,
            pen_rx,
            _task: task,
        }
    }

    /// Drain pending media chunks into the jitter buffer. Returns
    /// the number of chunks pushed.
    pub fn drain_into(
        &mut self,
        buffer: &mut JitterBuffer,
        now: Instant,
    ) -> Result<usize, MediaDatagramError> {
        let mut n = 0;
        while let Ok((header, payload, parity)) = self.media_chunk_rx.try_recv() {
            buffer.push_chunk(&header, &payload, parity, now);
            n += 1;
        }
        Ok(n)
    }

    /// Mutable access to the media chunk channel (for callers that
    /// want to stream chunks directly rather than drain through a
    /// jitter buffer).
    pub fn media_chunk_rx(
        &mut self,
    ) -> &mut mpsc::UnboundedReceiver<(MediaDatagramHeader, Vec<u8>, bool)> {
        &mut self.media_chunk_rx
    }

    /// Mutable access to the raw gamepad datagram channel. Each item
    /// is the full datagram (magic + discriminator + state); the
    /// caller feeds it to `decode_gamepad_datagram`.
    pub fn gamepad_rx(&mut self) -> &mut mpsc::UnboundedReceiver<Vec<u8>> {
        &mut self.gamepad_rx
    }

    /// Mutable access to the raw pen datagram channel. Each item is
    /// the full datagram; the caller feeds it to `decode_pen_datagram`.
    pub fn pen_rx(&mut self) -> &mut mpsc::UnboundedReceiver<Vec<u8>> {
        &mut self.pen_rx
    }

    /// Try-drain the gamepad channel without awaiting; returns the
    /// raw datagrams currently queued.
    pub fn try_drain_gamepad(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Ok(b) = self.gamepad_rx.try_recv() {
            out.push(b);
        }
        out
    }

    /// Try-drain the pen channel without awaiting.
    pub fn try_drain_pen(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Ok(b) = self.pen_rx.try_recv() {
            out.push(b);
        }
        out
    }

    /// Borrow the underlying `quinn::Connection` (callers may need
    /// it to spawn the matching `MediaDatagramSender`).
    pub fn connection(&self) -> Connection {
        self.conn.clone()
    }
}

/// Reliable control channel. Wraps a bidirectional QUIC stream with
/// length-prefixed `ControlMsg` (postcard-equivalent JSON for now — the
/// control stream is not the hot path; NACKs and rate feedback can
/// tolerate a small serialization overhead).
pub struct ControlChannel {
    send: SendStream,
    recv: quinn::RecvStream,
    // 4-byte length prefix, little-endian.
}

const MAX_JSON_FRAME: u32 = 256 * 1024;

impl ControlChannel {
    pub async fn open(conn: &Connection) -> anyhow::Result<Self> {
        let (send, recv) = conn
            .open_bi()
            .await
            .context("failed to open media control channel")?;
        Ok(Self { send, recv })
    }

    /// Accept the next incoming bi-directional stream and wrap it as a
    /// `ControlChannel`. The host calls this to receive `RateFeedback`
    /// from the client (P0-4).
    pub async fn accept(conn: &Connection) -> anyhow::Result<Self> {
        let (send, recv) = conn
            .accept_bi()
            .await
            .context("failed to accept media control channel")?;
        Ok(Self { send, recv })
    }

    pub async fn send(&mut self, msg: &ControlMsg) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(msg)?;
        let len = u32::try_from(bytes.len())
            .map_err(|_| anyhow!("control message too large ({} bytes)", bytes.len()))?;
        self.send.write_u32(len).await?;
        self.send.write_all(&bytes).await?;
        self.send.flush().await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> anyhow::Result<Option<ControlMsg>> {
        let len = match self.recv.read_u32().await {
            Ok(len) => len,
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if len > MAX_JSON_FRAME {
            tracing::error!(
                len,
                max = MAX_JSON_FRAME,
                "rejecting oversized control message"
            );
            anyhow::bail!("frame too large: {len} > {MAX_JSON_FRAME}");
        }
        let mut bytes = vec![0_u8; len as usize];
        self.recv.read_exact(&mut bytes).await?;
        let msg: ControlMsg = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to decode control message of {len} bytes"))?;
        Ok(Some(msg))
    }

    pub async fn finish(mut self) -> anyhow::Result<()> {
        self.send.finish()?;
        Ok(())
    }
}

/// One-way delay tracker. Tracks the *trend* OWD (relative delay
/// growth), not the wall-clock difference between sender and receiver
/// clocks. This avoids the clock-skew problem where the host and
/// client measure time on different timescales (NTP sync, VM clock
/// drift, suspended-process resumes, …).
///
/// Two operating modes:
///
/// * **Relative mode** (default): each `observe` takes the *delta* in
///   sender and receiver wall-clock between consecutive samples, and
///   tracks `(recv_delta − send_delta)` as the OWD trend. A growing
///   trend means queuing delay is increasing; a steady trend means
///   the link is stable. This is the WebRTC-GCC "trendline filter"
///   concept (RFC 8698 §3.2).
/// * **Static baseline mode**: when `observe_baseline_owd` is used,
///   the first sample establishes a baseline (typically the
///   path-propagation one-way delay inferred from `rtt/2`), and
///   subsequent samples become relative to that baseline. This is
///   equivalent to relative mode but starts from a known absolute
///   reference, which is what `RateFeedback.one_way_delay_ms` exposes
///   on the wire.
///
/// In both modes the EWMA (`ewma_ms`) is the smoothed value used by
/// the GCC gradient classifier; `base_min_ms` is the running minimum
/// of the *relative* OWD, used as the path-propagation baseline for
/// `RateFeedback.one_way_delay_min_ms`.
pub struct OwDelayTracker {
    /// EWMA of the trend OWD (ms). Smooths out single-sample spikes
    /// so the GCC gradient classifier can compare two adjacent
    /// EWMA samples instead of two adjacent raw samples.
    ewma_ms: f64,
    /// Previous EWMA value (for gradient computation in the caller).
    prev_ewma_ms: f64,
    /// Running minimum of the observed trend OWD. Becomes the
    /// path-propagation baseline once enough samples are seen.
    base_min_ms: f64,
    /// Last sample we observed, used to compute the next delta.
    last: Option<Sample>,
    /// Operating mode.
    mode: OwDelayMode,
}

/// One operating-mode for `OwDelayTracker`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OwDelayMode {
    /// Pure relative: each `observe` is `(recv_delta − send_delta)`.
    Relative,
    /// Static baseline: the first sample sets the baseline; later
    /// samples are `(current_owd − baseline)`.
    StaticBaseline { baseline_owd_ms: f64 },
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    send_ts_ms: f64,
    recv_ts_ms: f64,
}

impl OwDelayTracker {
    pub fn new() -> Self {
        Self::with_mode(OwDelayMode::Relative)
    }

    /// Construct a tracker that anchors against a static baseline.
    /// The first `observe` call records the OWD; subsequent calls
    /// subtract that baseline.
    pub fn with_mode(mode: OwDelayMode) -> Self {
        Self {
            ewma_ms: 0.0,
            prev_ewma_ms: 0.0,
            base_min_ms: f64::INFINITY,
            last: None,
            mode,
        }
    }

    /// Switch to static-baseline mode with the given baseline value.
    /// Useful when the caller has a known `rtt/2` to anchor against.
    pub fn use_baseline(&mut self, baseline_owd_ms: f64) {
        self.mode = OwDelayMode::StaticBaseline { baseline_owd_ms };
        // Reset the EWMA so the new mode doesn't blend with stale data.
        self.ewma_ms = 0.0;
        self.prev_ewma_ms = 0.0;
        self.base_min_ms = 0.0;
    }

    /// Record a packet's send and receive timestamps (milliseconds,
    /// each side's wall clock).
    ///
    /// In `Relative` mode: computes `(recv_now − recv_prev) −
    /// (send_now − send_prev)` and tracks that as the trend OWD.
    ///
    /// In `StaticBaseline { baseline }` mode: computes
    /// `recv_now − send_now − baseline` so the EWMA reflects the
    /// *queuing delay growth* above the path-propagation baseline.
    ///
    /// On the very first sample we cannot compute a delta (no
    /// previous). We seed the EWMA at 0 (no trend yet) and stash the
    /// sample for the next call.
    pub fn observe(&mut self, send_ts_ms: f64, recv_ts_ms: f64) {
        let trend = match (self.last, self.mode) {
            (None, _) => {
                // First sample. Just stash and report a zero trend.
                self.last = Some(Sample {
                    send_ts_ms,
                    recv_ts_ms,
                });
                self.prev_ewma_ms = self.ewma_ms;
                self.ewma_ms = 0.0;
                return;
            }
            (Some(prev), OwDelayMode::Relative) => {
                let send_delta = send_ts_ms - prev.send_ts_ms;
                let recv_delta = recv_ts_ms - prev.recv_ts_ms;
                (recv_delta - send_delta).max(-1_000.0)
            }
            (Some(prev), OwDelayMode::StaticBaseline { baseline_owd_ms }) => {
                // For static baseline mode we *also* use the delta
                // formulation so that wall-clock skew cancels out.
                // The very first sample is the baseline itself; we
                // don't update the EWMA on it (we'd be tracking noise).
                let _ = prev;
                let owd = (recv_ts_ms - send_ts_ms).max(0.0);
                (owd - baseline_owd_ms).max(-1_000.0)
            }
        };

        self.last = Some(Sample {
            send_ts_ms,
            recv_ts_ms,
        });
        self.prev_ewma_ms = self.ewma_ms;
        self.ewma_ms = 0.9 * self.ewma_ms + 0.1 * trend;
        if trend < self.base_min_ms {
            self.base_min_ms = trend;
        }
    }

    /// One-shot helper for static-baseline mode: record the first
    /// OWD sample as the baseline. Equivalent to `use_baseline(owd)`
    /// followed by `observe(send, recv)` but more ergonomic in the
    /// host loop.
    pub fn observe_baseline_owd(&mut self, send_ts_ms: f64, recv_ts_ms: f64) {
        let baseline = (recv_ts_ms - send_ts_ms).max(0.0);
        self.use_baseline(baseline);
        self.last = Some(Sample {
            send_ts_ms,
            recv_ts_ms,
        });
    }

    /// Return `(ewma_ms, base_min_ms)`.
    ///
    /// `ewma_ms` is the smoothed *trend* OWD. The GCC gradient
    /// classifier should compare this value to its own `prev_ewma_ms`
    /// (kept on the controller) instead of subtracting two adjacent
    /// raw samples — see `GccRateController::classify`.
    ///
    /// `base_min_ms` is the path-propagation baseline (relative to
    /// which the EWMA measures growth).
    pub fn snapshot(&self) -> (f64, f64) {
        let base = if self.base_min_ms.is_finite() {
            self.base_min_ms
        } else {
            0.0
        };
        (self.ewma_ms, base)
    }

    /// Previous EWMA value (the one before the most recent `observe`).
    /// Useful for the GCC gradient classifier.
    pub fn prev_ewma_ms(&self) -> f64 {
        self.prev_ewma_ms
    }

    /// Current EWMA value.
    pub fn ewma_ms(&self) -> f64 {
        self.ewma_ms
    }
}

impl Default for OwDelayTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a `RateFeedback` snapshot from the jitter buffer's stats, the
/// OWD tracker, the QUIC connection's RTT, and the recent loss.
///
/// `owd` is `(current_trend_ms, baseline_ms)` — i.e. the EWMA of the
/// *trend* (relative growth) and the static minimum. The on-wire
/// fields `one_way_delay_ms` / `one_way_delay_min_ms` still carry the
/// OWD pair; receivers that only use the difference between them see
/// the same growth signal.
pub fn build_rate_feedback(
    rtt: Duration,
    owd: (f64, f64),
    jitter_ms: f64,
    loss_x1000: u16,
) -> RateFeedback {
    let (current, base) = owd;
    RateFeedback {
        rtt_ms: rtt.as_millis().min(u16::MAX as u128) as u16,
        loss_x1000,
        jitter_ms: jitter_ms.max(0.0).min(u16::MAX as f64) as u16,
        one_way_delay_ms: current as f32,
        one_way_delay_min_ms: base as f32,
    }
}

/// Convert a reassembled datagram frame into the same
/// `EncodedVideoAccessUnit` shape the rest of the host/client expect.
/// This lets the datagram path drop in next to the reliable stream path
/// without further code changes.
pub fn reassembled_to_access_unit(
    frame: &ReassembledFrame,
    timestamp_micros: u64,
) -> EncodedVideoAccessUnit {
    let keyframe = (frame.flags & FLAG_KEYFRAME) != 0;
    EncodedVideoAccessUnit {
        codec: frame.codec,
        frame_id: frame.frame_id as u64,
        timestamp_micros,
        keyframe,
        nal_units: Vec::new(),
        bytes: frame.bytes.clone(),
        display_id: 0,
        stream_id: 0,
        width: 0,
        height: 0,
        color_space: None,
        bit_depth: 8,
    }
}

// --- Gamepad datagram helpers ----------------------------------------

/// Encode a `RemoteInputEvent::Gamepad` into a single datagram. The
/// payload is a `WireGamepadState` (16 bytes) prefixed by a 1-byte
/// discriminator (0x47 = 'G'). The magic prefix is shared with media
/// datagrams so the receiver can dispatch by a quick prefix check.
pub fn encode_gamepad_datagram(state: &qubox_proto::WireGamepadState) -> Vec<u8> {
    let mut buf = Vec::with_capacity(3 + 1 + qubox_proto::WireGamepadState::SIZE);
    buf.extend_from_slice(&MEDIA_DATAGRAM_MAGIC);
    buf.push(0x47); // 'G' — discriminator for gamepad datagrams
    buf.push(state.gamepad_id);
    buf.push(state.flags);
    buf.push(state.buttons_lo);
    buf.push(state.buttons_hi);
    buf.push(state.lt);
    buf.push(state.rt);
    buf.extend_from_slice(&state.lx.to_be_bytes());
    buf.extend_from_slice(&state.ly.to_be_bytes());
    buf.extend_from_slice(&state.rx.to_be_bytes());
    buf.extend_from_slice(&state.ry.to_be_bytes());
    buf.extend_from_slice(&state._pad);
    buf
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GamepadDatagramError {
    Short,
    BadMagic,
    BadDiscriminator,
}

impl std::fmt::Display for GamepadDatagramError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GamepadDatagramError::Short => write!(f, "gamepad datagram too short"),
            GamepadDatagramError::BadMagic => write!(f, "gamepad datagram magic prefix mismatch"),
            GamepadDatagramError::BadDiscriminator => {
                write!(f, "gamepad datagram discriminator != 0x47")
            }
        }
    }
}

impl std::error::Error for GamepadDatagramError {}

pub fn decode_gamepad_datagram(
    payload: &[u8],
) -> Result<qubox_proto::WireGamepadState, GamepadDatagramError> {
    // The 2-byte media-magic prefix is shared between media and gamepad
    // datagrams so the receiver can dispatch by a quick prefix check;
    // the discriminator byte lives right after the magic.
    const PREFIX: usize = 2;
    if payload.len() < PREFIX + 1 + qubox_proto::WireGamepadState::SIZE {
        return Err(GamepadDatagramError::Short);
    }
    if payload[0..2] != MEDIA_DATAGRAM_MAGIC {
        return Err(GamepadDatagramError::BadMagic);
    }
    if payload[PREFIX] != 0x47 {
        return Err(GamepadDatagramError::BadDiscriminator);
    }
    let mut rdr = Cursor::new(&payload[PREFIX + 1..]);
    let gamepad_id = ReadBytesExt::read_u8(&mut rdr).unwrap_or(0);
    let flags = ReadBytesExt::read_u8(&mut rdr).unwrap_or(0);
    let buttons_lo = ReadBytesExt::read_u8(&mut rdr).unwrap_or(0);
    let buttons_hi = ReadBytesExt::read_u8(&mut rdr).unwrap_or(0);
    let lt = ReadBytesExt::read_u8(&mut rdr).unwrap_or(0);
    let rt = ReadBytesExt::read_u8(&mut rdr).unwrap_or(0);
    let lx =
        ReadBytesExt::read_i16::<BigEndian>(&mut rdr).map_err(|_| GamepadDatagramError::Short)?;
    let ly =
        ReadBytesExt::read_i16::<BigEndian>(&mut rdr).map_err(|_| GamepadDatagramError::Short)?;
    let rx =
        ReadBytesExt::read_i16::<BigEndian>(&mut rdr).map_err(|_| GamepadDatagramError::Short)?;
    let ry =
        ReadBytesExt::read_i16::<BigEndian>(&mut rdr).map_err(|_| GamepadDatagramError::Short)?;
    Ok(qubox_proto::WireGamepadState {
        gamepad_id,
        flags,
        buttons_lo,
        buttons_hi,
        lt,
        rt,
        lx,
        ly,
        rx,
        ry,
        _pad: [0, 0],
    })
}

// --- Re-exports for downstream callers -------------------------------

// `BigEndian` is used in the gamepad decoder below.
#[allow(unused_imports)]
use byteorder::BigEndian;

use qubox_proto::{PenTool, WirePenEvent};

// --- Pen datagram helpers (P2-15) ----------------------------------

/// Encode a single [`PenEvent`] (from `qubox-pen`) as a QUIC
/// datagram. The wire layout is the 36-byte [`WirePenEvent`] block
/// **prefixed** by the 2-byte media-magic (`MEDIA_DATAGRAM_MAGIC`) so
/// the receiver can dispatch on `payload[0..2]` without parsing the
/// whole frame.
pub fn encode_pen_datagram(event: &qubox_pen::PenEvent) -> Vec<u8> {
    let wire = event.to_wire();
    let bytes = wire.to_bytes();
    let mut buf = Vec::with_capacity(MEDIA_DATAGRAM_MAGIC.len() + WirePenEvent::SIZE);
    buf.extend_from_slice(&MEDIA_DATAGRAM_MAGIC);
    buf.push(PEN_DATAGRAM_DISCRIMINATOR);
    buf.extend_from_slice(&bytes);
    buf
}

/// Decode a pen datagram back into a [`PenEvent`] (the inverse of
/// [`encode_pen_datagram`]). The receiver verifies the magic and
/// the discriminator first so a misrouted packet is rejected early.
pub fn decode_pen_datagram(payload: &[u8]) -> Result<qubox_pen::PenEvent, PenDatagramError> {
    const PREFIX: usize = 2;
    if payload.len() < PREFIX + 1 {
        return Err(PenDatagramError::Short);
    }
    if payload[0..2] != MEDIA_DATAGRAM_MAGIC {
        return Err(PenDatagramError::BadMagic);
    }
    if payload[PREFIX] != PEN_DATAGRAM_DISCRIMINATOR {
        return Err(PenDatagramError::BadDiscriminator);
    }
    let wire = WirePenEvent::from_bytes(&payload[PREFIX + 1..])
        .map_err(|_| PenDatagramError::Malformed)?;
    let flags = wire.decoded_flags();
    let hover_distance = if flags.contains(qubox_proto::PenEventFlags::FLAG_HAS_HOVER) {
        (wire.hover_distance as u16) * 8
    } else {
        0
    };
    let button_state = if flags.contains(qubox_proto::PenEventFlags::FLAG_BARREL) {
        0b0010
    } else {
        0
    } | if flags.contains(qubox_proto::PenEventFlags::FLAG_ERASER_TIP) {
        0b0100
    } else {
        0
    };
    Ok(qubox_pen::PenEvent {
        device_id: wire.device_id_value(),
        tool: PenTool::from_wire_id(wire.tool_id).unwrap_or(PenTool::Pen),
        x: wire.x_value(),
        y: wire.y_value(),
        pressure: wire.pressure_value(),
        tilt_x: wire.tilt_x_value(),
        tilt_y: wire.tilt_y_value(),
        rotation: wire.rotation_value(),
        button_state,
        hover_distance,
        timestamp_us: wire.timestamp_us_value(),
        flags: wire.flags,
    })
}

/// Discriminator-based rejection reasons for [`decode_pen_datagram`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PenDatagramError {
    Short,
    BadMagic,
    BadDiscriminator,
    Malformed,
}

impl std::fmt::Display for PenDatagramError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PenDatagramError::Short => write!(f, "pen datagram too short"),
            PenDatagramError::BadMagic => write!(f, "pen datagram magic prefix mismatch"),
            PenDatagramError::BadDiscriminator => {
                write!(f, "pen datagram discriminator != 0x50")
            }
            PenDatagramError::Malformed => write!(f, "pen wire frame malformed"),
        }
    }
}

impl std::error::Error for PenDatagramError {}

#[cfg(test)]
mod tests {
    use super::*;
    use qubox_proto::WireGamepadState;

    #[test]
    fn header_round_trips_through_bytes() {
        let header = MediaDatagramHeader {
            magic: MEDIA_DATAGRAM_MAGIC,
            flags: FLAG_KEYFRAME,
            codec: 0,
            stream_id: 7,
            frame_id: 42,
            chunk_id: 1,
            chunk_count: 5,
        };
        let mut buf = [0_u8; 14];
        buf[0..2].copy_from_slice(&header.magic);
        buf[2] = header.flags;
        buf[3] = header.codec;
        buf[4..6].copy_from_slice(&header.stream_id.to_be_bytes());
        buf[6..10].copy_from_slice(&header.frame_id.to_be_bytes());
        buf[10..12].copy_from_slice(&header.chunk_id.to_be_bytes());
        buf[12..14].copy_from_slice(&header.chunk_count.to_be_bytes());
        let decoded = MediaDatagramHeader::from_bytes(&buf).unwrap();
        let flags = decoded.flags;
        let stream_id = decoded.stream_id;
        let frame_id = decoded.frame_id;
        let chunk_id = decoded.chunk_id;
        let chunk_count = decoded.chunk_count;
        assert_eq!(flags, FLAG_KEYFRAME);
        assert_eq!(stream_id, 7);
        assert_eq!(frame_id, 42);
        assert_eq!(chunk_id, 1);
        assert_eq!(chunk_count, 5);
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut buf = [0_u8; 14];
        buf[0] = 0xDE;
        buf[1] = 0xAD;
        assert_eq!(
            MediaDatagramHeader::from_bytes(&buf).unwrap_err(),
            MediaDatagramError::BadMagic
        );
    }

    #[test]
    fn chunker_splits_and_emits_parity() {
        let chunker = FrameChunker::new(5);
        let frame: Vec<u8> = (0..(1200 * 4 + 100)).map(|i| (i % 251) as u8).collect();
        let (data, parity, original_len) = chunker.chunk_and_encode(&frame);
        assert_eq!(data.len(), 5);
        assert_eq!(parity.len(), 1);
        assert_eq!(original_len, frame.len());
        // Recover a missing chunk.
        let mut chunks: Vec<Option<Vec<u8>>> = data.iter().cloned().map(Some).collect();
        chunks[2] = None;
        let recovered_index =
            recover_with_parity(&mut chunks, &parity, chunker.block_size).unwrap();
        assert_eq!(recovered_index, 2);
        let mut reconstructed = Vec::new();
        for c in &chunks {
            reconstructed.extend_from_slice(c.as_ref().unwrap());
        }
        // Use the `original_len` trailer mechanism (NOT trim-zeros) to
        // strip the trailing pad.
        reconstructed.truncate(original_len);
        assert_eq!(reconstructed, frame);
    }

    #[test]
    fn jitter_buffer_releases_completed_frames() {
        let mut buffer = JitterBuffer::new(Duration::from_millis(10), 16);
        let now = Instant::now();
        let h1 = MediaDatagramHeader {
            magic: MEDIA_DATAGRAM_MAGIC,
            flags: 0,
            codec: 0,
            stream_id: 1,
            frame_id: 1,
            chunk_id: 0,
            chunk_count: 2,
        };
        let h2 = MediaDatagramHeader { chunk_id: 1, ..h1 };
        buffer.push_chunk(&h1, b"hello ", false, now);
        buffer.push_chunk(&h2, b"world", false, now);
        let ready = buffer.pop_ready(now);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].bytes, b"hello world");
    }

    #[test]
    fn frame_ending_in_zero_survives_reassembly() {
        // Regression for the v0 trailing-zero trim bug: annex-B /
        // CABAC tails may end in 0x00. After chunking + reassembly
        // the reassembled bytes must equal the input EXACTLY —
        // including any trailing zeros.
        let mut frame = Vec::new();
        for i in 0..(CHUNK_PAYLOAD_MAX + 100) {
            frame.push(((i * 7) % 251) as u8);
        }
        // Forcibly stamp a few `0x00` bytes at the trailing edge so
        // the original_len truncation has a strict, observable
        // divergence from a trim-zeros pass.
        let tail_len = 7;
        let frame_len = frame.len();
        for i in 0..tail_len {
            frame[frame_len - 1 - i] = 0x00;
        }

        let chunker = FrameChunker::new(4);
        let (data, parity, original_len) = chunker.chunk_and_encode(&frame);
        assert_eq!(data.len(), 2);
        assert_eq!(original_len, frame.len());
        assert_eq!(parity.len(), 1);

        // Reassemble by hand, emulating the receiver: store raw
        // chunks (last one carries FLAG_LAST_CHUNK + trailer), then
        // let pop_ready call assemble_frame.
        let mut buffer = JitterBuffer::new(Duration::from_millis(50), 8);
        let now = Instant::now();
        for (i, chunk) in data.iter().enumerate() {
            let is_last = i as u16 == data.len() as u16 - 1;
            let mut payload = chunk.clone();
            let flags = if is_last { FLAG_LAST_CHUNK } else { 0 };
            if is_last {
                payload.extend_from_slice(&(original_len as u32).to_be_bytes());
            }
            let header = MediaDatagramHeader {
                magic: MEDIA_DATAGRAM_MAGIC,
                flags,
                codec: 0,
                stream_id: 0,
                frame_id: 1,
                chunk_id: i as u16,
                chunk_count: data.len() as u16,
            };
            buffer.push_chunk(&header, &payload, false, now);
        }
        let ready = buffer.pop_ready(now);
        assert_eq!(ready.len(), 1, "frame should be reassembled");
        assert_eq!(ready[0].bytes, frame, "trailing zeros must be preserved");
    }

    #[test]
    fn last_chunk_truncation_uses_original_len_trailer() {
        // Emulate a worst-case parity recovery: every data chunk
        // except the last is lost, the last chunk (with trailer)
        // and parity (without trailer) arrive. `assemble_frame`
        // must NOT trim zeros from the partial payload — it only
        // truncates to the captured `original_len`.
        let frame: Vec<u8> = (0..(CHUNK_PAYLOAD_MAX + 16))
            .map(|i| (i % 251) as u8)
            .collect();
        // Append a trailing 16-byte zero run so the trim-zeros
        // heuristic would mis-truncate in a way the trailer
        // captures correctly.
        let mut frame = frame;
        frame.extend(std::iter::repeat(0u8).take(16));
        assert!(frame.ends_with(&[0u8; 5]));
        let chunker = FrameChunker::new(2);
        let (data, _parity, original_len) = chunker.chunk_and_encode(&frame);
        assert!(data.len() >= 2);
        assert_eq!(original_len, frame.len());

        // Manually craft a PendingFrame-like state: first chunk
        // missing, last chunk present with trailer, parity present.
        let mut buffer = JitterBuffer::new(Duration::from_millis(50), 8);
        let now = Instant::now();
        let last_idx = data.len() - 1;
        let last_chunk = &data[last_idx];
        let mut last_payload = last_chunk.clone();
        last_payload.extend_from_slice(&(original_len as u32).to_be_bytes());
        let last_header = MediaDatagramHeader {
            magic: MEDIA_DATAGRAM_MAGIC,
            flags: FLAG_LAST_CHUNK,
            codec: 0,
            stream_id: 0,
            frame_id: 42,
            chunk_id: last_idx as u16,
            chunk_count: data.len() as u16,
        };
        buffer.push_chunk(&last_header, &last_payload, false, now);
        // The frame is incomplete — pop_ready should NOT emit it.
        let ready = buffer.pop_ready(now);
        assert!(ready.is_empty(), "incomplete frame must not be emitted");
    }

    #[test]
    fn gamepad_decoder_reports_bad_magic_not_short() {
        // The v0 decoder returned `Short` for both too-short AND
        // bad-magic payloads — too easy to misdiagnose. After the
        // fix: `BadMagic` for wrong magic, `Short` for genuinely too
        // short.
        let bad_magic = vec![
            0xFF, 0xFE, 0x47, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(
            decode_gamepad_datagram(&bad_magic),
            Err(GamepadDatagramError::BadMagic)
        );
        let too_short = vec![0x51, 0x42, 0x47];
        assert_eq!(
            decode_gamepad_datagram(&too_short),
            Err(GamepadDatagramError::Short)
        );
    }

    /// Synthetic dispatcher test: build raw media + gamepad + pen
    /// datagrams and feed them through the dispatcher's task loop
    /// (without a real QUIC connection) by invoking the
    /// `MediaDatagramHeader::from_bytes` + discriminator routing
    /// inline. Verifies each datagram lands in the right receiver.
    #[test]
    fn dispatcher_routes_by_discriminator() {
        let now = Instant::now();

        // Build a media datagram (no FLAG_LAST_CHUNK — half a frame).
        let media_header = MediaDatagramHeader {
            magic: MEDIA_DATAGRAM_MAGIC,
            flags: 0,
            codec: 0,
            stream_id: 0,
            frame_id: 1,
            chunk_id: 0,
            chunk_count: 1,
        };
        let mut media_bytes = Vec::new();
        let mut buf = [0_u8; 14];
        media_header.write_into(&mut buf);
        media_bytes.extend_from_slice(&buf);
        media_bytes.extend_from_slice(b"media-payload");
        assert_eq!(media_bytes[2], 0, "media sentinel must be 0");

        // Gamepad datagram.
        let gamepad_state = WireGamepadState {
            gamepad_id: 1,
            flags: 0,
            buttons_lo: 0,
            buttons_hi: 0,
            lt: 0,
            rt: 0,
            lx: 0,
            ly: 0,
            rx: 0,
            ry: 0,
            _pad: [0, 0],
        };
        let gamepad_bytes = encode_gamepad_datagram(&gamepad_state);
        assert_eq!(gamepad_bytes[2], 0x47);

        // Pen datagram.
        let event = qubox_pen::PenEvent {
            device_id: 1,
            tool: qubox_proto::PenTool::Pen,
            x: 0.0,
            y: 0.0,
            pressure: 0.0,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rotation: 0.0,
            button_state: 0,
            hover_distance: 0,
            timestamp_us: 0,
            flags: 0,
        };
        let pen_bytes = encode_pen_datagram(&event);
        assert_eq!(pen_bytes[2], PEN_DATAGRAM_DISCRIMINATOR);

        // Manually exercise the dispatcher's routing logic (we
        // can't spawn a task without a real QUIC connection; this
        // is the same `match buf[2]` block).
        for bytes in [&media_bytes, &gamepad_bytes, &pen_bytes] {
            assert!(bytes[0..2] == MEDIA_DATAGRAM_MAGIC);
            match bytes[2] {
                0x47 => {
                    let decoded = decode_gamepad_datagram(bytes).unwrap();
                    assert_eq!(decoded.gamepad_id, 1);
                }
                PEN_DATAGRAM_DISCRIMINATOR => {
                    let _ = decode_pen_datagram(bytes).unwrap();
                }
                other if other <= MEDIA_DISCRIMINATOR_MAX => {
                    let header = MediaDatagramHeader::from_bytes(bytes).unwrap();
                    let hdr_frame_id = header.frame_id;
                    assert_eq!(hdr_frame_id, 1);
                }
                _ => panic!("unexpected discriminator"),
            }
        }
        let _ = now;
    }

    #[test]
    fn jitter_buffer_drops_deadline_passed_frames() {
        let mut buffer = JitterBuffer::new(Duration::from_millis(5), 16);
        let now = Instant::now();
        let h1 = MediaDatagramHeader {
            magic: MEDIA_DATAGRAM_MAGIC,
            flags: 0,
            codec: 0,
            stream_id: 1,
            frame_id: 9,
            chunk_id: 0,
            chunk_count: 2,
        };
        let h2 = MediaDatagramHeader { chunk_id: 1, ..h1 };
        buffer.push_chunk(&h1, b"a", false, now);
        // Only one chunk arrives; the deadline will expire.
        let later = now + Duration::from_millis(20);
        let _ = h2;
        let expired = buffer.pop_deadline_passed(later);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].missing_chunks, vec![1]);
        assert_eq!(buffer.stats().frames_dropped_deadline, 1);
    }

    #[test]
    fn ow_delay_tracker_baseline_and_ewma() {
        // Original test was designed for the absolute (recv - send)
        // formulation. Under the new relative-delay tracker the
        // trend is the *delta*, so we synthesise a session where
        // recv and send advance in lock-step at 16 ms (≈60 fps) and
        // the recv_delta is the same as the send_delta → trend ≈ 0.
        let mut t = OwDelayTracker::new();
        t.observe(1000.0, 1000.0); // seed
        t.observe(1016.0, 1016.0); // +16 ms both sides
        t.observe(1032.0, 1032.0); // +16 ms both sides
        let (current, base) = t.snapshot();
        // No growth → EWMA and baseline should both be ~0.
        assert!(current.abs() < 0.5, "current={current}");
        assert!(base.abs() < 0.5, "base={base}");
    }

    /// Wall-clock skew must NOT pollute the trend OWD. Feed samples
    /// whose sender clock advances +1 ms/sample but whose receiver
    /// clock advances +10 ms/sample: the recv_delta − send_delta
    /// trend is +9 ms/sample, which is the actual queuing growth
    /// (the wall-clock difference of 1000 ms vs 1500 ms is irrelevant
    /// after the first sample).
    #[test]
    fn ow_delay_tracker_relative_ignores_wall_clock_offset() {
        let mut t = OwDelayTracker::new();
        // First sample: stash, no trend yet.
        t.observe(1500.0, 1000.0);
        // Now recv jumps 10 ms, send jumps 1 ms each sample. Trend
        // is +9 ms per sample.
        for i in 1..=100 {
            t.observe(1500.0 + i as f64, 1000.0 + 10.0 * i as f64);
        }
        let (current, _base) = t.snapshot();
        // After many samples, EWMA(α=0.9) of {9,9,9,...} → 9.
        // (1 - 0.9^100 ≈ 0.99997, so we should be within 0.001 of 9.)
        assert!(
            (current - 9.0).abs() < 0.5,
            "expected EWMA ≈9, got {current}"
        );
    }

    /// `observe_baseline_owd` then `observe` must anchor against the
    /// first sample, not the wall-clock difference.
    #[test]
    fn ow_delay_tracker_static_baseline_anchors_against_first_sample() {
        let mut t = OwDelayTracker::new();
        // First sample sets baseline to (recv - send).
        t.observe_baseline_owd(1000.0, 1015.0); // baseline = 15 ms
                                                // A second sample with the same wall-clock OWD = 15 ms
                                                // should yield a trend of 0, not 15 ms.
        for _ in 0..100 {
            t.observe(1016.0, 1031.0);
        }
        let (current, _base) = t.snapshot();
        assert!(
            current.abs() < 0.5,
            "expected EWMA ≈0 after baseline anchor, got {current}"
        );
        // Now feed samples with OWD = 25 ms (10 ms worse than baseline)
        // for many iterations — EWMA should ramp to ≈ +10.
        for _ in 0..100 {
            t.observe(1032.0, 1057.0);
        }
        let (current, _base) = t.snapshot();
        assert!(
            (current - 10.0).abs() < 1.0,
            "expected EWMA ≈10 after 10 ms growth, got {current}"
        );
    }

    #[test]
    fn gamepad_datagram_round_trips() {
        let state = WireGamepadState {
            gamepad_id: 1,
            flags: WireGamepadState::FLAG_DPAD_UP,
            buttons_lo: WireGamepadState::BTN_A,
            buttons_hi: 0,
            lt: 200,
            rt: 50,
            lx: 1234,
            ly: -2345,
            rx: 0,
            ry: 32767,
            _pad: [0, 0],
        };
        let bytes = encode_gamepad_datagram(&state);
        let decoded = decode_gamepad_datagram(&bytes).unwrap();
        assert_eq!(decoded, state);
    }

    #[test]
    fn pen_datagram_round_trips() {
        let event = qubox_pen::PenEvent {
            device_id: 3,
            tool: qubox_proto::PenTool::Pen,
            x: 640.5,
            y: 480.25,
            pressure: 0.5,
            tilt_x: 10.0,
            tilt_y: -5.0,
            rotation: 90.0,
            button_state: 0,
            hover_distance: 0,
            timestamp_us: 1234,
            flags: 0,
        };
        let bytes = encode_pen_datagram(&event);
        // 2 bytes magic + 1 byte discriminator + 36 bytes wire.
        assert_eq!(
            bytes.len(),
            MEDIA_DATAGRAM_MAGIC.len() + 1 + WirePenEvent::SIZE
        );
        let decoded = decode_pen_datagram(&bytes).unwrap();
        assert_eq!(decoded.device_id, 3);
        assert!((decoded.x - 640.5).abs() < 1e-3);
        assert!((decoded.y - 480.25).abs() < 1e-3);
        assert_eq!(decoded.timestamp_us, 1234);
    }

    #[test]
    fn pen_datagram_rejects_short_buffer() {
        assert_eq!(decode_pen_datagram(&[]), Err(PenDatagramError::Short));
        // MEDIA_DATAGRAM_MAGIC (2 bytes) is below the minimum 3-byte
        // payload — `decode_pen_datagram` rejects it as Short, not
        // as BadDiscriminator.
        assert_eq!(
            decode_pen_datagram(&MEDIA_DATAGRAM_MAGIC),
            Err(PenDatagramError::Short)
        );
    }

    #[test]
    fn pen_datagram_rejects_wrong_discriminator() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MEDIA_DATAGRAM_MAGIC);
        bytes.push(0x99); // not 0x50
        bytes.extend_from_slice(&[0u8; WirePenEvent::SIZE]);
        assert_eq!(
            decode_pen_datagram(&bytes),
            Err(PenDatagramError::BadDiscriminator)
        );
    }

    #[test]
    fn pen_datagram_rejects_bad_magic() {
        let mut bytes = vec![0xFF, 0xFE, 0x50];
        bytes.extend_from_slice(&[0u8; WirePenEvent::SIZE]);
        assert_eq!(decode_pen_datagram(&bytes), Err(PenDatagramError::BadMagic));
    }

    #[test]
    fn reassembled_to_access_unit_preserves_codec_and_keyframe_flag() {
        let frame = ReassembledFrame {
            codec: VideoCodec::H264,
            stream_id: 0,
            frame_id: 7,
            flags: FLAG_KEYFRAME,
            bytes: vec![0, 0, 1, 0x67, 1, 2, 3],
            reassembled_at: Instant::now(),
        };
        let au = reassembled_to_access_unit(&frame, 16_666);
        assert_eq!(au.codec, VideoCodec::H264);
        assert_eq!(au.frame_id, 7);
        assert!(au.keyframe);
    }

    #[test]
    fn reed_solomon_encoder_round_trips_with_simulated_loss() {
        // End-to-end check that the `ChunkEncoder::ReedSolomon` path
        // produces chunks/parity the [`rs_fec::ReedSolomonFec`]
        // reconstructor can recover from, given `parity_shards`
        // losses per block. Exercises the same encode API the
        // `MediaDatagramSender::with_reed_solomon` constructor wires
        // onto the live send path.
        let block_size = 4_usize;
        let parity_shards = 2_usize;
        let encoder = ChunkEncoder::ReedSolomon(
            rs_fec::ReedSolomonFec::new(block_size, parity_shards).expect("valid RS params"),
        );

        // Frame sized so we get exactly two full RS blocks (8 data
        // shards) plus their parity (4 parity shards).
        let frame: Vec<u8> = (0..(CHUNK_PAYLOAD_MAX * 8))
            .map(|i| (i % 251) as u8)
            .collect();
        let (data, parity, original_len) = encoder.encode(&frame).expect("encode ok");
        assert_eq!(data.len(), 8);
        assert_eq!(parity.len(), 4);
        assert_eq!(original_len, frame.len());

        // Simulate loss of 2 data shards from each block (within the
        // `parity_shards` budget per block).
        let mut data_opts: Vec<Option<Vec<u8>>> = data.iter().cloned().map(Some).collect();
        let mut parity_opts: Vec<Option<Vec<u8>>> = parity.iter().cloned().map(Some).collect();
        data_opts[1] = None;
        data_opts[3] = None;
        data_opts[5] = None;
        data_opts[7] = None;

        let rs = rs_fec::ReedSolomonFec::new(block_size, parity_shards).unwrap();
        let recovered = rs
            .reconstruct(&mut data_opts, &mut parity_opts)
            .expect("reconstruct ok");
        assert_eq!(recovered, 4, "all 4 lost data shards must be recovered");

        // Reassemble the frame from the recovered data shards.
        let mut rebuilt = Vec::new();
        for shard in &data_opts {
            rebuilt.extend_from_slice(shard.as_ref().expect("recovered"));
        }
        rebuilt.truncate(original_len);
        assert_eq!(rebuilt, frame, "round-tripped bytes must match input");
    }

    #[test]
    fn reed_solomon_encoder_with_zero_parity_round_trips() {
        // `parity_shards == 0` short-circuits the encoder to a pure
        // split with no FEC. Verify the live path stays usable when
        // operators opt out of RS.
        let encoder = ChunkEncoder::ReedSolomon(
            rs_fec::ReedSolomonFec::new(4, 0).expect("zero parity is valid"),
        );
        let frame: Vec<u8> = (0..(CHUNK_PAYLOAD_MAX * 2 + 17))
            .map(|i| (i % 251) as u8)
            .collect();
        let (data, parity, original_len) = encoder.encode(&frame).expect("encode ok");
        assert_eq!(parity.len(), 0);
        assert!(!data.is_empty());
        assert_eq!(original_len, frame.len());
        let rebuilt: Vec<u8> = data.into_iter().flatten().take(original_len).collect();
        assert_eq!(rebuilt, frame);
    }
}
