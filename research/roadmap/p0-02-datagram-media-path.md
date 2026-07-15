# P0-2: Datagram Media Path (QUIC Datagrams + Jitter Buffer + FEC)

Status: **complete** (commits `db0492d`, `51557a4`, `6cacfc6`; PR https://github.com/MegaPanchamZ/qubox/pull/1). Datagram path is on by default; reliable uni-stream is the opt-in fallback via `--no-datagram-media`.
Owner: `qubox-transport` crate, with a new `media-path` module.
Depends on: current `NativeQuicMediaReceiver`/`NativeQuicMediaSender` (reliable streams).
Blockers: none. The current `quinn 0.10` workspace dependency supports RFC 9221 DATAGRAM.

## Goal

Replace the reliable-stream media path (`NativeQuicMediaReceiver` and friends) with a low-latency **QUIC datagram** media path that adds <5 ms of transport latency on top of the encoder's frame interval, while keeping the control path on reliable QUIC streams for NACKs, rate feedback, and pairing. Add a 5-10 ms adaptive jitter buffer client-side and an XOR-parity FEC scheme for 1-3% residential packet loss. Target end-to-end capture-to-display latency: **<60 ms** (Parsec) and **<40 ms** (Moonlight-class).

## Research Summary

### Quinn datagram API (quinn 0.10/0.11, 2024-2026)

QUIC DATAGRAM (RFC 9221) is enabled via the `TransportConfig` and is congestion-controlled but unretransmitted. The high-level API is `Connection::send_datagram(&[u8])` and `Connection::read_datagram()`.

```rust
use quinn::{ClientConfig, Endpoint, TransportConfig};
use std::sync::Arc;

fn make_client_endpoint(bind_addr: std::net::SocketAddr) -> anyhow::Result<Endpoint> {
    let mut endpoint = Endpoint::client(bind_addr)?;
    let mut transport_config = TransportConfig::default();
    transport_config.datagram_send_buffer_size(1 << 20);   // 1 MiB
    transport_config.datagram_receive_buffer_size(1 << 20); // 1 MiB
    transport_config.min_mtu(1200);                          // QUIC spec floor
    let mut client_config = ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::default()));
    client_config.transport_config(Arc::new(transport_config));
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

async fn send_media(conn: &quinn::Connection, payload: &[u8]) -> Result<(), quinn::SendDatagramError> {
    conn.send_datagram(payload.to_vec())?;
    Ok(())
}
```

Key constraints from quinn 0.10 docs:
- `max_datagram_size` is the per-datagram payload cap (~1200 bytes on typical Ethernet after QUIC+UDP+IP overhead).
- `datagram_send_buffer_size` and `datagram_receive_buffer_size` (1 MiB each is a sensible default for 4 Mbps / 60 fps).
- `send_datagram` returns `SendDatagramError::Full` if the send buffer is full; the caller must back off (drop frames, not error the connection).
- ECN / CongestionFeedback is automatic; the datagram path exposes the same `ConnectionStats` as the stream path.

### MTU-aware frame chunking

A 4 Mbps H.264 stream at 60 fps produces ~5 KB access units on average (one frame per AU for CBR at 60 fps). QUIC datagrams cap payloads at ~1200 bytes, so each frame must be fragmented across multiple datagrams.

Custom header per datagram (12 bytes):

```rust
#[repr(C, packed)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct MediaDatagramHeader {
    pub magic:        [u8; 2], // 0xB2 0x16 ("BP")
    pub flags:        u8,      // bit 0: keyframe, bit 1: parity, bit 2: last chunk
    pub codec:        u8,      // VideoCodec discriminant
    pub stream_id:    u16,     // which video stream
    pub frame_id:     u32,     // monotonically increasing per stream
    pub chunk_id:     u16,     // 0-based fragment index
    pub chunk_count:  u16,     // total fragments for this frame
}
```

The header is 12 bytes; the chunk payload is `~1188 bytes`. A 5 KB frame becomes 5 chunks. The `flags.last_chunk` bit lets the client identify when a frame is complete (without waiting for the per-frame deadline).

### Jitter buffer design (5-10 ms target)

The jitter buffer is **per-frame, per-stream** (BTreeMap keyed by `frame_id`). The deadline is `first_chunk_arrival + target_delay`. When a frame is complete or its deadline expires, it pops and is fed to the decoder.

```rust
pub struct JitterBuffer {
    frames: BTreeMap<u32, FrameBuffer>,
    target_delay: Duration,        // 5 ms on LAN, 10 ms on WAN
    max_inflight: usize,           // bound memory; ~30 frames @ 60 fps
}

impl JitterBuffer {
    pub fn push_chunk(&mut self, hdr: MediaDatagramHeader, chunk: &[u8], now: Instant) {
        let fb = self.frames.entry(hdr.frame_id).or_insert_with(|| FrameBuffer {
            chunks: vec![None; hdr.chunk_count as usize],
            first_arrival: now,
        });
        fb.chunks[hdr.chunk_id as usize] = Some(chunk.to_vec());
    }

    pub fn pop_ready(&mut self, now: Instant) -> Vec<ReassembledFrame> {
        // Iterate from oldest frame_id. If complete OR deadline passed, pop.
    }
}
```

**Adaptive target_delay**: track a 1-second exponential moving average of the inter-arrival jitter (the variance of `(chunk_arrival - capture_time)`). If jitter > 5 ms, bump target_delay by 1 ms (cap 15 ms). If jitter < 1 ms sustained for 5 s, decrement by 1 ms (floor 3 ms).

### Forward Error Correction (FEC) — XOR parity

For Parsec/Moonlight-class residential networks with 1-3% packet loss, **XOR block parity** is the right trade-off: trivial CPU, recovers single-packet loss per block, latency cost is zero (parity chunks are sent inline with data chunks, not on a delay).

Encoding (per frame, after chunking):

1. Split `N` data chunks into blocks of `B` chunks (e.g. `B = 5`).
2. For each block, compute one parity chunk = `XOR(data_chunks)`.
3. Send the `B + 1` chunks (5 data + 1 parity) with a `flags.parity = 1` bit on the parity chunk.

Decoding (per frame, on receive):

1. Collect chunks for the frame.
2. If exactly 1 chunk is missing and its block has the parity chunk, recover: `missing = XOR(known_chunks_of_block)`.
3. If 2+ chunks are missing, give up and NACK (control stream).

A typical 5 KB frame is 5 data chunks; with 1 parity chunk that's 6 chunks per frame — a 20% overhead that recovers any 1-packet-per-block loss. For residential 1% loss this is sufficient. For 3-5% loss, move to Reed-Solomon with `data=5, parity=2` (40% overhead).

```rust
fn make_xor_parity(chunks: &[Vec<u8>]) -> Vec<u8> {
    let max = chunks.iter().map(|c| c.len()).max().unwrap();
    let mut parity = vec![0u8; max];
    for c in chunks {
        for (i, &b) in c.iter().enumerate() { parity[i] ^= b; }
    }
    parity
}
```

### Loss concealment (H.264/H.265/AV1)

When a frame is missing 1+ chunks that FEC cannot recover:

- **Default**: drop the entire frame, but instruct the decoder to **repeat the previous frame** (or use a frozen-IDR technique: send a small `flags.repeat_last_frame` datagram that the decoder converts to a copy of the previous decoded frame).
- **Alternative**: send an intra-refresh IDR every ~30 ms (not a full IDR, just enough macroblocks to refresh the entire frame in 30 ms). This is what WebRTC's `intra-refresh` mode does. Pairs well with P0-1 (HW encoders all support intra-refresh on VAAPI/QSV/AMF; NVENC needs `repeat-seq-headers` or `-idr_freq` with `intra-refresh=1`).
- For game streaming, **frame repeat** is the right default. WebRTC research shows frame-repeat with intra-refresh gives 90th-percentile PSNR within 2 dB of no-loss at 1% loss.

### Control channel (NACK + rate feedback)

A single bidirectional QUIC stream for all control traffic. Message types (bincode/serde):

```rust
#[derive(Serialize, Deserialize)]
pub enum ControlMsg {
    Nack { stream_id: u16, frame_id: u32, missing_chunks: Vec<u16> },
    RateFeedback { stream_id: u16, target_bitrate_kbps: u32, max_au_bytes: u32, rtt_ms: u16, loss_x1000: u16, jitter_ms: u16 },
    KeyframeRequest { stream_id: u16 },
    StreamStats { stream_id: u16, frames_decoded: u32, frames_dropped: u32, frames_recovered: u32 },
}
```

Frequencies:
- **NACK**: emitted as soon as the jitter buffer's deadline passes with chunks missing. One NACK per missing frame (typically <5% of frames on a healthy network).
- **Rate feedback**: 4 Hz (every 250 ms). Smooths rate oscillations and is well below the rate controller's own 1 Hz tick.
- **Keyframe request**: client-side, emitted if frames_decoded - frames_received > 2 (i.e. 2+ consecutive frame drops).
- **Stream stats**: 1 Hz. For the stats overlay (P1-12) and the daemon (P1-13).

The server **does not** retransmit non-keyframe chunks over the reliable stream — the cost (RTT × 1+ for retransmit, plus reordering) is worse than just dropping the frame and continuing. Only keyframe chunks are retransmitted (on keyframe request, server sends a fresh keyframe; the client doesn't have to wait for the original).

### Rust crate recommendations

- **quinn** (already in workspace, 0.10): the QUIC stack. Use the high-level Connection API; don't touch the low-level streams for the media path.
- **reed-solomon-erasure** (or **reed-solomon-simd** for AVX2): mature Reed-Solomon over GF(256). Use this when 1% XOR-parity is not enough (residential WANs > 3% loss).
- **raptorq** (or **raptorq-rs**): RaptorQ fountain coding. Not needed for the first release — RS over small blocks is simpler and lower-latency.
- **bytemuck**: for the `MediaDatagramHeader` Pod/Zeroable derive (already transitively available; add explicitly if not).
- **bincode** or **postcard**: control message serialization. **postcard** is preferred for no-std and serde-derive compatibility, but bincode is fine here.
- **No mature jitter buffer crate** in the Rust ecosystem. Roll our own per the design above. (GStreamer's `webrtcbin` has a jitter buffer in C; integrating GStreamer is too heavy for this codebase.)

### 2024-2026 status

- **RFC 9221 (QUIC DATAGRAM)** is finalized and supported in quinn 0.10+.
- **RFC 9297 (HTTP Datagrams and Capsule Protocol)** is the HTTP/3 framing for unreliable media. We can ride this on top of h3-quinn if we want HTTP/3 semantics; not needed for the first release.
- **Media over QUIC (MoQ, draft-ietf-moq-transport)**: IETF is standardizing a publish-subscribe media architecture on QUIC. Our design (datagram media + stream control) is MoQ-aligned. Don't wait for MoQ to ship; ship raw QUIC now and migrate to MoQ in a follow-up.
- **quiche (Cloudflare) vs quinn (Mozilla)**: quiche's datagram API is `Connection::datagram_send` and the framing is similar. We're already on quinn, so no migration.
- **SRT vs QUIC**: SRT (Secure Reliable Transport) is the de-facto standard for low-latency video contribution (broadcast). ARQ-based, 1-RTT retransmit, 120-150 ms latency. For game streaming (<60 ms), QUIC datagrams with our own jitter buffer + FEC is faster than SRT.
- **RIST vs QUIC**: RIST (Reliable Internet Stream Transport) is the broadcast-grade ARQ protocol with FEC. Too heavy and too slow for game streaming.

## Implementation Plan

### Step 1: New `media-path` module in `qubox-transport`

`crates/qubox-transport/src/media/mod.rs`:
- `pub mod chunker;` (frame → datagram chunks + XOR parity)
- `pub mod jitter;` (per-frame BTreeMap with deadline)
- `pub mod datagram;` (send/recv datagram task on an existing quinn Connection)
- `pub mod control;` (NACK + rate feedback over a reliable stream)
- `pub mod fec;` (XOR parity, RS as a future)

`crates/qubox-transport/src/media/datagram.rs`:
- `pub struct MediaDatagramSender { conn: quinn::Connection }` with `pub fn send_frame(&self, frame_id: u32, codec: VideoCodec, stream_id: u16, frame: &[u8])`.
- `pub struct MediaDatagramReceiver { conn: quinn::Connection, chunk_rx: tokio::sync::mpsc::UnboundedReceiver<(MediaDatagramHeader, Vec<u8>)> }` with `pub async fn recv_chunk(&mut self) -> Option<(MediaDatagramHeader, Vec<u8>)>`.
- Spawn a worker task that calls `conn.read_datagram()` in a loop and parses the header.

### Step 2: Reassembler and jitter buffer

`crates/qubox-transport/src/media/jitter.rs`:
- `pub struct JitterBuffer` as above.
- `pub fn push_chunk(&mut self, hdr: MediaDatagramHeader, chunk: Vec<u8>, now: Instant)`.
- `pub fn pop_ready(&mut self, now: Instant) -> Vec<ReassembledFrame>`.
- `pub fn pop_deadline_passed(&mut self, now: Instant) -> Vec<ReassembledFrame>` for the per-frame timeout.
- `pub fn max_inflight_frames(&self) -> usize` to bound memory.

### Step 3: FEC chunker

`crates/qubox-transport/src/media/fec.rs`:
- `pub fn chunk_and_encode(frame: &[u8], block_size: usize) -> (Vec<MediaChunk>, Vec<MediaChunk>)` returns `(data_chunks, parity_chunks)`.
- `pub fn recover(missing: &mut FrameBuffer) -> Result<(), RecoveryError>` attempts XOR-parity recovery.

### Step 4: Control message types

`crates/qubox-transport/src/media/control.rs`:
- `#[derive(Serialize, Deserialize)] pub enum ControlMsg { Nack, RateFeedback, KeyframeRequest, StreamStats }`.
- `pub struct ControlChannel { send: quinn::SendStream, recv: quinn::RecvStream }` with `pub async fn send(&mut self, msg: &ControlMsg) -> Result<()>` and `pub async fn recv(&mut self) -> Result<ControlMsg>`.
- Frame each message with a 4-byte length prefix (`u32 LE`) so partial reads don't desync.

### Step 5: Replace `NativeQuicMediaReceiver`

In `crates/qubox-transport/src/lib.rs`:
- `pub struct NativeQuicMediaReceiver` becomes a thin wrapper around `MediaDatagramReceiver + JitterBuffer + ControlChannel`.
- `pub fn next_access_unit(&mut self) -> impl Future<Output = Result<Option<WireAccessUnit>>>` pops the next ready frame from the jitter buffer; on deadline, returns `Ok(None)` and emits a NACK over the control channel.
- `pub fn emit_rate_feedback(&mut self)` builds and sends the rate feedback message.
- The existing wire format `WireAccessUnit { payload: Vec<u8>, codec: VideoCodec }` is unchanged — the datagram header is stripped before the AU is exposed.

### Step 6: Encoder-side integration

In `apps/host-agent/src/encoder/pipeline.rs`:
- After the encoder subprocess produces an AU, call `MediaDatagramSender::send_frame(frame_id, codec, stream_id, &au_bytes)`.
- `frame_id` is a `u32` counter per stream, wrapping at `u32::MAX`.
- Keyframes are flagged with `flags.keyframe = 1`; the client requests a keyframe on `KeyframeRequest`.

### Step 7: Adaptive target_delay

`crates/qubox-transport/src/media/jitter.rs`:
- Track `jitter_ewma: f32` (exponential moving average of the chunk-to-chunk interval variance, in milliseconds).
- Every 1 second, re-evaluate `target_delay`:
  - `jitter_ewma > 5.0` → `target_delay = min(target_delay + 1, 15)`.
  - `jitter_ewma < 1.0` for 5 s → `target_delay = max(target_delay - 1, 3)`.
- Expose `pub fn current_target_delay(&self) -> Duration` so the stats overlay (P1-12) can display it.

### Step 8: Tests

- Unit test: `chunk_and_encode` round-trip for a 5 KB frame with 0, 1, 2 lost chunks (FEC recovery at 1, drop at 2+).
- Unit test: `JitterBuffer::pop_ready` with a synthetic arrival sequence.
- Integration test: spin up a `quinn` server and client on `127.0.0.1`, send 100 frames at 60 fps, count drops and recoveries.
- Soak test: 10 minutes of synthetic 1% loss (drop random chunks in the receiver), verify <1% frame drop rate.
- Latency test: round-trip `std::time::Instant::now()` from encoder to decoder across loopback.

## Risks and Open Questions

- **Reordering under congestion**: QUIC datagrams can arrive out of order relative to the `frame_id` sequence. Our reassembler handles this (chunks are placed by `chunk_id`), but if a single frame's chunks are widely reordered (e.g. 1st and 5th swapped), the frame is held until either both arrive or the deadline expires. For residential networks this is rare; for WAN with deep reordering, increase the jitter buffer target.
- **Head-of-line blocking on the control stream**: NACKs and rate feedback share a single reliable stream. A stalled control stream (because of a stuck TCP-like retransmit) blocks rate feedback. Mitigation: open a new control stream for every NACK burst, or use a short message timeout on `send` and emit an in-band `Urgent` flag.
- **FEC overhead vs encoder bitrate**: 20% FEC overhead means the encoder gets 20% less effective bandwidth. Either increase `-b:v 5000k` to compensate, or tell the user "your bandwidth is 4 Mbps *with* FEC, expect ~3.2 Mbps of video at 20% parity".
- **AV1 + FEC interaction**: AV1 has larger access units than H.264 (more chunks per frame). The FEC block size should be adjusted (B=4 instead of B=5).
- **Reed-Solomon vs XOR at the same overhead**: RS can recover any 2/8 (25% loss) at parity=2; XOR can only recover 1/N per block. RS is strictly better at the same overhead; the only reason to prefer XOR is CPU. Use RS for >=3% loss networks, XOR otherwise. A `pub enum FecMode { Xor, ReedSolomon { parity: u8 } }` is the right abstraction.
- **Media-over-QUIC migration**: when MoQ ships (probably 2026-2027 IETF finalization), the datagram+stream split maps cleanly to MoQ's `Track` + `Object` model. Migration is local to the `media-path` module; the encoder pipeline and decoder are unaffected.
- **QUIC datagram ECN feedback**: the QUIC stack will signal ECN-CE on a congested path. Our rate controller (P0-4) should treat ECN-CE as an additional loss signal and back off the bitrate. Wire this through the `RateFeedback` message.

## References

- Quinn TransportConfig: https://docs.rs/quinn/latest/quinn/struct.TransportConfig.html
- Quinn issue #1154 (datagram sizing): https://github.com/quinn-rs/quinn/issues/1154
- quic-go datagrams reference: https://quic-go.net/docs/quic/datagrams/
- RFC 9221: QUIC DATAGRAM extension (final).
- RFC 9297: HTTP Datagrams and the Capsule Protocol (final).
- RFC 3984: RTP Payload Format for H.264 (NAL fragmentation, FU-A).
- RFC 6330: RaptorQ (informative; we may not need it).
- RFC 5053: Raptor (informative).
- h3-quinn changelog (datagram feature): https://github.com/hyperium/h3/blob/master/changelog-h3-quinn.md
- MASQUE/H3 datagrams draft: https://www.ietf.org/archive/id/draft-ietf-masque-h3-datagram-01.html
- Perplexity research, 2026-07-02: Quinn API, FEC tradeoffs, MoQ status.
