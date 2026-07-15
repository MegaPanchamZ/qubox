# ADR-014 Reed-Solomon FEC for Keyframes and ROI

## Status

Rewritten 2026-07-11 from the original Proposed draft. Branch
`feature/adr-014-rs-fec-keyframes`. Based on `main` after commit
`47585ea`. Builds on ADR-013 (frame-aware pacing) and ADR-011 (QUIC v2
datagram path). Required for P2-14 (HDR), P2-15 (Pen), and P2-16 (4K144)
— all three consume the FEC-protected media plane.

This is the **canonical** spec for the Reed–Solomon FEC layer. The
existing prototype in `crates/qubox-transport/src/media/rs_fec.rs`
already implements §1–§6 below; this ADR pins the wire format, ROI
classifier, decoder state machine, and remaining work for a junior
implementer.

## Context

`crates/qubox-transport/Cargo.toml:14` already lists
`reed-solomon-erasure = "6"` as a dependency — this is **not** a
P0-02 leftover. The prototype `rs_fec` module at
`crates/qubox-transport/src/media/rs_fec.rs:42-588` implements the bulk
of this ADR (~330 LoC of RS + adaptive controller + 11 tests).
Sections §7–§10 (ROI classifier, decoder state machine, FecController
integration into the host/client loop, full QUIC-stack integration
tests) are the remaining work.

QUIC's loss recovery is per-packet, retransmission-based, with a
~500 ms tail. For media, retransmitting an old access unit is
counterproductive (the next frame is already in flight). Forward
Error Correction (FEC) is the standard solution: send *k* data
packets + *m* parity packets; the receiver can reconstruct from any
*k* of the *k+m*. WebRTC's `media_opt_util.cc` enables ULPFEC at
>20 ms RTT and any sustained loss (`webrtc.org` internals discussion);
FEC reduces visible artefacts by ~80 % at 5 % PLR (Google Research
"Handling Packet Loss in WebRTC", 2013).

### Research summary (2026-07)

- **`reed-solomon-erasure = "6"`** (existing dep): pure-Rust GF(2⁸),
  port of Backblaze/Klaus Post. ~4 500 MB/s on a 2017 mobile i5;
  scales to **~3–15 µs per 14.4 KiB block** on a modern desktop core.
  MIT. Maintainers explicitly seeking new owners (low-medium risk).
  No SIMD.
- **`reed-solomon-simd` (3.x)**: Leopard-RS GF(2¹⁶) over AVX2/SSSE3
  (x86-64) + NEON (AArch64). **~10 GiB/s** on a Ryzen 5 3600
  (1024-byte shards, 1024+1024 layout) → ~1.4 µs per 14.4 KiB block.
  Actively maintained. Requires even shard sizes and a different
  API surface — would require rewriting `rs_fec.rs` from scratch.
- **`raptorq` (RFC 6330)**: fountain code over GF(256) with
  on-the-fly Gaussian elimination. Throughput dominated by the
  *K × K* matrix solve; for small blocks (*K* = 10) decode latency
  is **~hundreds of µs** (see Maister "real-time video streaming
  experiments with FEC", 2024). Designed for rateless multi-receiver
  cases; wasted on fixed *m = 2*.
- **ULPFEC (RFC 5109) / FlexFEC (RFC 8627)**: mask-based wire format
  (SN base + offset mask), XOR parity, *no fixed (k,m)*. Crates.io
  has **no** standalone ULPFEC/FlexFEC engine — only the broader
  `webrtc` and `webrtc-data` stacks. WebRTC's "FEC only on loss +
  RTT > 20 ms" trigger is the closest precedent to our adaptive
  `FecController`.
- **UEP for ROI** (Iqbal & Zepernick 2011, Boulos et al. Packet
  Video 2009): stronger parity on the foreground / ROI macroblocks
  yields +0.87 dB ROI-PSNR and up to +30 dB vs. unprotected
  background. ULP-FEC + JPEG2000 ROI scaling gives an additional
  ~1 dB on top of equal protection at 5–10 % PLR.

### Decision summary

| Question | Answer | Why |
|---|---|---|
| Library | Keep `reed-solomon-erasure = "6"` | Already wired + tested; ~10 µs/block is well inside the 16.7 ms frame budget. |
| Wire format | Reuse `FLAG_PARITY` bit on existing `MediaDatagramHeader` | No new discriminator needed — already additive; older clients ignore parity datagrams. |
| Default block | **k=4, m=2** (NOT k=10/m=2 of original ADR) | Matches 1080p60 H.264 ~12 KB frame into 3 KB shards. Higher k wastes decoder buffer. |
| Max parity | m ≤ 4 (already enforced) | Bandwidth budget: at m=4 with k=4, FEC overhead is 100 % — only enabled during 5 %+ loss. |
| ROI scheme | Two-tier UEP: 2× for ROI frames, 1.5× elsewhere | Standard UEP pattern from Iqbal/Zepernick. |
| Decoder state | Pending → Ready → Emitted → Free; 200 ms timeout | Matches jitter buffer budget (`crates/qubox-transport/src/media/mod.rs:321`). |
| RaptorQ | Deferred (out of scope) | Overkill for fixed (k,m); revisit if 4K144 (P2-16) needs it. |

## Decision

### 1. Library choice — `reed-solomon-erasure = "6"` (unchanged)

`Cargo.toml` entry (already present at `crates/qubox-transport/Cargo.toml:14`):

```toml
reed-solomon-erasure = "6"
```

**Why keep it, not switch to `reed-solomon-simd`:**

1. **Already wired and tested.** The prototype in `rs_fec.rs:42-588`
   has 11 passing tests (`cargo test -p qubox-transport rs_fec::`
   runs them all). Switching libraries invalidates all of them.
2. **Sub-50 µs/block is fine.** For 1080p60 H.264 at ~12 KB/frame,
   k=4/m=2 generates 6 shards of ~3 KB each. Pure-Rust GF(2⁸) does
   this in ~3–15 µs on a modern x86-64 core — well inside the
   16.7 ms frame budget.
3. **No GF(2¹⁶) shard-size constraint.** `reed-solomon-simd`
   requires even shard sizes; GF(2⁸) works on any byte count.
4. **No NEON today.** Apple Silicon (M1/M2) runs the pure-Rust
   path at ~2–4× lower throughput than AVX2. This is acceptable
   for the 60 fps path; revisit only if a profiler points at FEC.
5. **Maintenance risk is low-medium.** The "seeking new owners"
   notice is a yellow flag, but the algorithm is frozen (MDS code
   over GF(2⁸), identical to Backblaze/Java + Klaus Post/Go).
   We can fork the GitHub repo (`darrenldl/reed-solomon-erasure`)
   into our own Git if upstream goes silent.

**Why NOT `raptorq`:**

- RaptorQ's decode is dominated by a K×K GF(256) matrix solve
  (Maister 2024 measures hundreds of µs for small K). At k=10,
  m=2 this is ~10–50× the latency of Reed-Solomon for no
  recovery-quality gain (we know `m = 2` in advance, so the
  fountain-code "infinite parity" feature is unused).
- 150+ KB compiled code (vs. ~40 KB for the GF(2⁸) RS crate).
- Two existing libs to choose from: `raptorq` (older, mature) and
  `raptor-code` (1.0.8 Jun 2025, actively maintained). Neither
  fits our use case.

### 2. Block sizing — k=4, m=2 default (NOT k=10)

**Override the original ADR's k=10/m=2 default.** Rationale:

- A 1080p60 H.264 access unit at 8 Mbps is ~17 KB every 16.7 ms.
  At k=4 the shard length is ~3 KB — three times the QUIC
  datagram MTU of ~1.2 KB? No — see §3: the existing
  `shard_len_for` at `rs_fec.rs:253-268` **rounds shard length
  to the QUIC MTU (~1.2 KB)**, not the natural frame size.
  Concretely, a 12 KB frame with k=4 produces
  `total_chunks = max(needed_chunks=10, block_size=4) = 10`
  shards of ~1.2 KB each, across **2.5 RS blocks** (rounded to
  3 blocks: blocks 1–4, 5–8, 9–10+padding).
- At k=10/m=2 a 12 KB frame would emit exactly 1 RS block of 12
  shards × 1.2 KB — simpler encoder, but doubles per-block
  decoder buffer pressure (10 KB pending instead of 5 KB), and
  at 60 fps the decoder holds ~600 KB of pending shards.
- **Chosen:** k=4, m=2 default; k up to 16 for 4K144 (P2-16).
  This matches the existing `DEFAULT_BLOCK_SIZE = 4` at
  `crates/qubox-transport/src/media/rs_fec.rs:53` and the existing
  `MAX_PARITY_SHARDS = 4` at `rs_fec.rs:49`.

```rust
// crates/qubox-transport/src/media/rs_fec.rs (existing constants)
pub const MAX_PARITY_SHARDS: usize = 4;        // was 4 — keep
pub const DEFAULT_BLOCK_SIZE: usize = 4;       // was 4 — keep
pub const DEFAULT_PARITY_SHARDS: usize = 2;    // new — add
```

### 3. Wire format — additive on `MediaDatagramHeader`

**No new `MediaParity` discriminator.** The original ADR §3
proposed a separate `MediaParity = 0x01` discriminator in the
media byte. **Do not implement that.** The existing prototype
already uses the **`FLAG_PARITY` bit** (`0x02`) on the existing
`MediaDatagramHeader` at `crates/qubox-transport/src/media/mod.rs:48`:

```rust
pub const FLAG_PARITY: u8 = 1 << 1;
```

The 14-byte wire header at `mod.rs:118` (`WIRE_HEADER_SIZE = 14`)
is unchanged. A parity datagram has the same magic `[0x51, 0x42]`
at `mod.rs:36`, the same `flags |= FLAG_PARITY`, and uses
`chunk_id` as the **parity shard index within its block**:

| Offset | Bytes | Field | Notes |
|---|---|---|---|
| 0..2 | 2 | `magic = [0x51, 0x42]` | Same as `MEDIA_DATAGRAM_MAGIC` at `crates/qubox-transport/src/media/mod.rs:36`. |
| 2 | 1 | `flags` | bit 0 = `FLAG_KEYFRAME`, bit 1 = `FLAG_PARITY`, bit 2 = `FLAG_LAST_CHUNK`. |
| 3 | 1 | `codec` | Same enum as data datagrams (H.264=0, H.265=1, AV1=2). |
| 4..6 | 2 | `stream_id` BE | Identical to data shards. |
| 6..10 | 4 | `frame_id` BE | Identical to data shards. |
| 10..12 | 2 | `chunk_id` BE | **For parity shards: 0..m-1** (re-uses the chunk_id slot). |
| 12..14 | 2 | `chunk_count` BE | Identical to data shards (`k` + filler if block < full). |

**Discriminator range** (per ADR-010 §1.2 + existing
`MEDIA_DISCRIMINATOR_MAX = 0x3F` at `mod.rs:42`): the magic-prefix
match at `mod.rs:862` plus the byte[2] ≤ 0x3F check at `mod.rs:880`
continues to route parity datagrams as media chunks. Gamepad (0x47),
pen (0x50), and mic (0x4D) discriminators sit above 0x3F and are
unchanged. **The original ADR-014 §3 plan to add a separate
`MediaParity = 0x01` discriminator is dropped** — it was a thought
experiment before the FLAG_PARITY reuse was prototyped.

#### 3.1 `MediaParityHeader` is not a separate struct

The original ADR §3 proposed a `MediaParityHeader { block_id, symbol_index, k, m }`
struct. **This is unnecessary on the wire** because:

- `frame_id` in the existing header identifies the block.
- `chunk_id` already carries the parity-shard index (0..m-1).
- `k` and `m` are **session-negotiated** (in
  `MediaFecMode::ReedSolomon { block_size, parity_shards }`),
  not per-packet — see `crates/qubox-transport/src/media/mod.rs:650-663`
  `MediaDatagramSender::with_reed_solomon`.

If a future extension needs in-band signalling of a *changing*
`m` (e.g. mid-stream controller update), add a single bit
`FLAG_ADAPTIVE_PARITY = 1 << 4` and 1-byte `parity_count` AFTER
the 14-byte header. **Not in scope for this ADR.**

### 4. QUIC-stack integration

#### 4.1 Buffer sizes (cross-ref ADR-011 §3)

Already set in `crates/qubox-transport/src/lib.rs:1875-1876`:

```rust
config.datagram_send_buffer_size(1 << 20);     // 1 MiB
config.datagram_receive_buffer_size(Some(1 << 20));  // 1 MiB
```

Per-frame bandwidth at k=4/m=2 (1080p60 H.264 8 Mbps):
6 datagrams × ~1.2 KB = ~7.2 KB → 1 MiB holds ~140 frames
(~2.3 s of media) which is more than enough for FEC recovery.

#### 4.2 Datagram wrap

Each parity shard is sent via the existing path at
`crates/qubox-transport/src/media/mod.rs:719-731`:

```rust
for (i, p) in parity.iter().enumerate() {
    let header = MediaDatagramHeader {
        magic: MEDIA_DATAGRAM_MAGIC,
        flags: base_flags | FLAG_PARITY,    // bit 1 set
        codec: codec_byte,
        stream_id: self.stream_id,
        frame_id,
        chunk_id: i as u16,                  // parity index in block
        chunk_count,
    };
    self.send_chunk(&header, p)?;
}
```

The actual QUIC send is the same `conn.send_datagram(Bytes::from(buf))`
call at `mod.rs:772-774`. No new path required.

**Max datagram size check:** at
`crates/qubox-transport/src/media/mod.rs:665-670`,
`max_datagram_size()` returns the QUIC MTU minus the 14-byte
header. Parity shards are sized to fit (RS `shard_len_for` at
`rs_fec.rs:253-268` rounds to 1200 bytes target → always ≤ MTU).

### 5. ROI classifier

A **heuristic** that decides whether a frame gets full 2× parity
(m=2 default) or reduced 1.5× parity (m=1). Lives in the new
`crates/qubox-transport/src/media/roi.rs` module
(see §10.2 for the file layout).

#### 5.1 Heuristic

```rust
// crates/qubox-transport/src/media/roi.rs (new file)
use qubox_proto::CaptureRegion;

/// Decide the parity-shard count for an encoded access unit.
///
/// Returns `m` in 1..=4. The caller passes `m` to
/// `MediaDatagramSender::with_reed_solomon().send_frame()` or the
/// adaptive `FecController::adjust_for_loss(...)` output.
pub fn classify_roi(
    encoded: &EncodedVideoAccessUnit,
    capture_region: Option<CaptureRegion>,
    receiver_max_parity: usize,  // capped from MAX_PARITY_SHARDS
) -> usize {
    // 1. Keyframes always get 2x parity regardless of ROI:
    //    a keyframe loss freezes the screen until the next IDR.
    if encoded.keyframe {
        return receiver_max_parity.min(2);
    }

    // 2. ROI classification: does the capture cover the central 1080p
    //    desktop region?
    //
    //    "Central 1080p region" = a 1920x1080 rectangle centred on
    //    the capture framebuffer. We assume the user is looking at
    //    a fullscreen desktop application.
    let covers_central = match capture_region {
        None => true,  // no region set → assume fullscreen
        Some(r) => {
            // 1920x1080 centred box at (x, y) = (cx-w/2, cy-h/2)
            // with cx, cy the centre of the capture framebuffer.
            let cx = (r.x + r.width / 2) as i32;
            let cy = (r.y + r.height / 2) as i32;
            let central = (cx - 1920 / 2, cy - 1080 / 2, 1920, 1080);

            let rx0 = r.x as i32;
            let ry0 = r.y as i32;
            let rx1 = rx0 + r.width as i32;
            let ry1 = ry0 + r.height as i32;

            let overlap_w = (rx1.min(central.0 + central.2))
                .saturating_sub(rx0.max(central.0));
            let overlap_h = (ry1.min(central.1 + central.3))
                .saturating_sub(ry0.max(central.1));
            if overlap_w <= 0 || overlap_h <= 0 {
                false
            } else {
                let overlap = (overlap_w * overlap_h) as u64;
                let central_area = (central.2 * central.3) as u64;
                // >50% of the central 1920x1080 region is visible
                // in the capture → this is a "central 1080p" capture.
                overlap * 2 > central_area
            }
        }
    };

    if covers_central {
        // ROI capture → 2x parity (full protection).
        receiver_max_parity.min(2)
    } else {
        // Peripheral capture (e.g. ultrawide sidebar, picture-in-picture)
        // → 1.5x parity (1 shard per block; saves ~10% bandwidth).
        receiver_max_parity.min(1)
    }
}
```

**Design notes:**

- The `>50% overlap` heuristic handles ultrawide monitors (e.g.
  3440×1440): the capture region covers the central 1080p but the
  periphery extends to the sides. We treat this as "central".
- Picture-in-picture (small window in the corner) does **not**
  cover the central region → 1.5× parity.
- Multi-monitor capture where the central 1080p is fully visible
  on one monitor → 2× parity (full ROI).
- The cap to `receiver_max_parity` ensures we never emit more
  parity shards than `FecController` allows (which scales m down
  on low loss).

#### 5.2 Interaction with `FecController`

The `FecController::adjust_for_loss(loss_x1000)` at
`crates/qubox-transport/src/media/rs_fec.rs:369-388` already returns
a *parity count* based on loss. The ROI classifier output is the
**starting point** for each frame; the controller then scales it
up or down based on observed loss. Combined:

```rust
let roi_m = classify_roi(&encoded, capture_region, MAX_PARITY_SHARDS);
let ctrl_m = fec_controller.adjust_for_loss(recent_loss_x1000);
let m = roi_m.max(ctrl_m).min(MAX_PARITY_SHARDS);  // take the larger
```

### 6. Decoder state machine

The existing `ReedSolomonFec::reconstruct` at
`crates/qubox-transport/src/media/rs_fec.rs:195-251` already
performs the *core* decode. **Missing:** the higher-level block
state machine that buffers incoming shards, times out stale
blocks, and emits the reconstructed frame. Add to a new
`crates/qubox-transport/src/media/fec_decoder.rs`.

#### 6.1 State diagram

```
                    ┌──────────────┐
                    │   (none)     │
                    └──────┬───────┘
                           │ first chunk arrives
                           ▼
                    ┌──────────────┐  200ms timeout
              ┌────▶│   Pending    │────────────────┐
              │     │ (k shards    │                │
              │     │  collected)  │                ▼
              │     └──────┬───────┘        ┌──────────────┐
   arrived    │            │                │  Discarded   │
   too late   │            │ k data         │ (notify host │
              │            │ received       │  to skip the │
              │            ▼                │  next AU)    │
              │     ┌──────────────┐        └──────────────┘
              │     │   Ready      │
              │     │ (reconstruct │
              │     │  succeeded)  │
              │     └──────┬───────┘
              │            │ assembled + emitted
              │            ▼
              │     ┌──────────────┐
              └─────│    Free      │  (block_id dropped)
                    └──────────────┘
```

States:

- **Pending** — at least one shard received; awaiting more.
  When `data_received + parity_received >= k`, call
  `ReedSolomonFec::reconstruct`; if it succeeds, transition to
  Ready.
- **Ready** — all `k` data shards reconstructed. Emit the
  access unit via the same channel as
  `JitterBuffer::pop_ready` at `mod.rs:439-458`, then Free.
- **Free** — block_id removed from the `FecDecoder`'s map.
  Late-arriving shards for a Free block_id are dropped (and
  logged at `debug!` level).
- **Discarded** — deadline exceeded (200 ms since first
  arrival). Emit a `ControlMsg::BlockDiscarded { frame_id }` so
  the host can request a keyframe.

#### 6.2 Constants (insert into `fec_decoder.rs`)

```rust
/// Block timeout. Matches the jitter buffer target_delay upper
/// bound (15ms nominal, 25ms under load — see
/// crates/qubox-transport/src/media/mod.rs:505).
pub const FEC_BLOCK_TIMEOUT: Duration = Duration::from_millis(200);

/// Max simultaneous pending blocks. At k=4/m=2 with ~3KB shards,
/// each block holds ~18KB. 16 blocks = ~288KB decoder buffer.
pub const FEC_MAX_PENDING_BLOCKS: usize = 16;
```

The 200 ms timeout is intentionally 2× the worst-case jitter
buffer target_delay (15 ms nominal + spike budget) — this gives
FEC a chance to recover before the access unit is declared lost
and a keyframe is requested.

#### 6.3 Decoder API

```rust
// crates/qubox-transport/src/media/fec_decoder.rs (new file)
pub struct FecDecoder {
    block_size: usize,
    max_parity_shards: usize,
    timeout: Duration,
    pending: BTreeMap<u32, PendingBlock>,  // keyed by frame_id
    stats: FecDecoderStats,
}

#[derive(Debug, Default, Clone)]
pub struct FecDecoderStats {
    pub blocks_started: u64,
    pub blocks_recovered_via_fec: u64,
    pub blocks_emitted_direct: u64,
    pub blocks_discarded: u64,
    pub late_shards_dropped: u64,
}

#[derive(Debug)]
struct PendingBlock {
    data: Vec<Option<Vec<u8>>>,   // length k
    parity: Vec<Option<Vec<u8>>>, // length m
    first_arrival: Instant,
    deadline: Instant,
}

impl FecDecoder {
    pub fn new(rs: ReedSolomonFec, timeout: Duration, max_pending: usize) -> Self;

    /// Feed one data or parity shard. Returns `Some(Vec<u8>)`
    /// (= the reconstructed access unit, length `original_len`)
    /// only when the block becomes Ready on this shard.
    /// Returns `None` if the block is still pending or has
    /// already been emitted.
    pub fn add_shard(
        &mut self,
        frame_id: u32,
        chunk_id: u16,
        is_parity: bool,
        payload: &[u8],
        now: Instant,
    ) -> Option<Vec<u8>>;

    /// Drain blocks whose deadline has passed. Returns the
    /// discarded `frame_id`s so the caller can emit
    /// `ControlMsg::BlockDiscarded` to the peer.
    pub fn drain_expired(&mut self, now: Instant) -> Vec<u32>;
}
```

### 7. Out of scope: RaptorQ (RFC 6330)

RaptorQ is on the roadmap as a follow-up for very high-loss / long
fat networks (e.g. satellite). It adds ~150 KB of compiled code
and would require switching to a fountain-code data flow (no
fixed block_id). Defer to ADR-022 once a real use case arrives
(e.g. a customer request for 30 % PLR tolerance on 4G uplink).

### 8. Out of scope: 2D / RDP-style difference coding

The user's RDP-graphics research dump is interesting but is **not
an FEC scheme** — it is a content-aware redundancy scheme that
overlaps with the codec's intra-refresh + ROI map. We keep this on
the radar and incorporate it as a content hint into the codec
selection matrix (ADR-018), not as a separate transport-layer FEC.

## Consequences

### Positive

- Keyframe loss becomes invisible at 5 % PLR (current worst case:
  1–2 second freeze while waiting for I-frame request → retransmit).
- ROI FEC protects the visually-critical desktop region even at
  10–15 % PLR, which is the operational regime of congested 5G uplink.
- Decoder overhead is bounded: at k=4, m=2, RS decode is ~5–15 µs per
  block on a modern x86-64 core. Negligible vs the H.264 decode
  pipeline (which costs ~5–10 ms per 1080p frame).
- Wire format is additive: parity datagrams coexist with existing
  data datagrams; older clients that don't recognise `FLAG_PARITY`
  silently drop them (the next data datagram still arrives).
- Adaptive `FecController` already implemented at
  `crates/qubox-transport/src/media/rs_fec.rs:310-395` scales m
  up/down based on `RateFeedback::loss_x1000`.

### Negative / Risk

- Bandwidth overhead: 2× parity on keyframes = 50 % bandwidth
  penalty on those frames (k=4, m=2 → 6 datagrams instead of 4).
  At a keyframe every 5 s and 8 Mbps average, that's ~0.4 %
  aggregate bandwidth overhead — well worth it.
- Decoder buffer pressure: 16 blocks × 18 KB = 288 KB of pending
  data. On memory-constrained clients this is significant.
  Mitigation: the block count is configurable via
  `FEC_MAX_PENDING_BLOCKS`; default 16 (= 288 KB).
- The `reed-solomon-erasure` crate's "seeking maintainers" notice
  (GitHub README) is a soft risk. Mitigation: the algorithm is
  frozen (MDS over GF(2⁸)) and the Backblaze/Java + Klaus Post/Go
  references are stable; we can fork if upstream goes silent.
- On Apple Silicon the pure-Rust path is ~2–4× slower than AVX2.
  At 60 fps 1080p this is still ~50 µs/block — within budget.
  If a future profiler points here, switch to `reed-solomon-simd`
  (requires GF(2¹⁶) shard even-length constraint + API rewrite).

### Roadmap mapping

- Closes the deferred item from P0-02 ("FEC: TBD, see ADR-014").
- Required for P2-14 (HDR), P2-15 (Pen), P2-16 (4K144).
- A precondition for ADR-018 (codec matrix assumes protected media).

## File paths and insertion points

| § | What | Path | Approx. line range | Status |
|---|---|---|---|---|
| §1 | `reed-solomon-erasure = "6"` dep | `crates/qubox-transport/Cargo.toml` | line 14 | **exists** |
| §2 | `DEFAULT_BLOCK_SIZE`, `MAX_PARITY_SHARDS`, new `DEFAULT_PARITY_SHARDS` | `crates/qubox-transport/src/media/rs_fec.rs` | lines 49, 53, new const after 53 | **exists + add one** |
| §3 | `FLAG_PARITY` bit on header | `crates/qubox-transport/src/media/mod.rs` | line 48 | **exists** |
| §3 | `WIRE_HEADER_SIZE = 14` | `crates/qubox-transport/src/media/mod.rs` | line 118 | **exists** |
| §3 | Parity sender loop | `crates/qubox-transport/src/media/mod.rs` | lines 719–731 | **exists** |
| §4.1 | `datagram_send_buffer_size(1<<20)` | `crates/qubox-transport/src/lib.rs` | lines 1875–1876 | **exists** |
| §5.1 | `classify_roi()` heuristic | `crates/qubox-transport/src/media/roi.rs` | **new file** (~80 LoC) | **TODO** |
| §5.2 | `roi_m.max(ctrl_m)` integration | `crates/qubox-transport/src/media/mod.rs` | around line 687 (inside `send_frame`) | **TODO** |
| §6.3 | `FecDecoder` struct + impl | `crates/qubox-transport/src/media/fec_decoder.rs` | **new file** (~180 LoC) | **TODO** |
| §6.3 | `FEC_BLOCK_TIMEOUT`, `FEC_MAX_PENDING_BLOCKS` constants | same new file | top of file | **TODO** |
| §6.3 | `FecDecoder` → `JitterBuffer` integration | `crates/qubox-transport/src/media/mod.rs` | new method on `JitterBuffer` (or replace `try_fec_recovery` at lines 424–435) | **TODO** |
| §6.3 | `ControlMsg::BlockDiscarded` variant | `crates/qubox-proto/src/lib.rs` | append after `MicConfigAck` at lines 364–371 | **TODO** |

## Step-by-step implementation order (PRs)

1. **PR-A: Land existing `rs_fec.rs` tests** (no new code).
   `cargo test -p qubox-transport rs_fec::` must pass on Linux,
   Windows, macOS. Run the example:
   `cargo run -p qubox-transport --example rs_fec_demo`.
   Output `/tmp/rs_demo_original.pgm` should be a 128×96 grayscale
   gradient.
2. **PR-B: Add `DEFAULT_PARITY_SHARDS = 2` const** to
   `rs_fec.rs:55` and a test `default_block_and_parity_match_rs_fec`
   that asserts `ReedSolomonFec::new(DEFAULT_BLOCK_SIZE, DEFAULT_PARITY_SHARDS).is_ok()`.
3. **PR-C: `crates/qubox-transport/src/media/roi.rs`** — implement
   `classify_roi()` with the §5.1 heuristic. Add the 3 unit tests
   from §10.1.
4. **PR-D: `crates/qubox-transport/src/media/fec_decoder.rs`** —
   implement `FecDecoder` with the §6.3 API. Add the 4 unit tests
   from §10.1.
5. **PR-E: Wire `FecDecoder` into `JitterBuffer`** — replace or
   wrap `try_fec_recovery` at `mod.rs:424-435` to call
   `FecDecoder::add_shard` per incoming chunk, then drain expired
   blocks in a periodic tick (every 16 ms or piggybacked on the
   next `pop_ready`).
6. **PR-F: Wire `classify_roi()` into `MediaDatagramSender::send_frame`**
   at `mod.rs:687` — choose `m` per-frame from
   `roi_m.max(ctrl_m).min(MAX_PARITY_SHARDS)`. Add the
   `control_msg_sends_parity_decision` test.
7. **PR-G: `ControlMsg::BlockDiscarded` proto + integration test**
   — add the variant at `crates/qubox-proto/src/lib.rs:~370`,
   send on `fec_decoder.drain_expired()` → `control.send(...)`.
   End-to-end test: drop 3 datagrams in a keyframe, observe
   `BlockDiscarded` is NOT sent (RS recovered), then drop 5 (RS
   fails), observe `BlockDiscarded` IS sent.

## Test specifications

All tests live in `crates/qubox-transport/src/media/{rs_fec,roi,fec_decoder}.rs`
`#[cfg(test)] mod tests` blocks.

### 10.1 Required test names + expected outputs

| Test name | Path | Expects |
|---|---|---|
| `fec_encoder_produces_k_plus_m_symbols` | `rs_fec.rs` | `ReedSolomonFec::new(4, 2).encode(&[0u8; 4096])` returns `EncodedFrame { data.len() == 4, parity.len() == 2, shard_len in 1024..=1200 }`. |
| `fec_decoder_reconstructs_from_any_k_symbols` | `rs_fec.rs` | Drop any 2 of the 6 shards (data or parity), reconstruct, byte-equal original. Run for each pair `(i, j)` with `i ≠ j`. |
| `fec_decoder_drops_blocks_after_200ms` | `fec_decoder.rs` | Add 3 of 4 data shards, sleep 250 ms, call `drain_expired`, assert returned `frame_id` matches and `add_shard` on a late 4th shard returns `None`. |
| `fec_decoder_emits_reconstructed_bytes` | `fec_decoder.rs` | Add 4 data shards (k=4), assert `add_shard` returns `Some(Vec<u8>)` whose `len() == original_len` and bytes match. |
| `roi_classifier_central_1080p_gets_2x` | `roi.rs` | `EncodedVideoAccessUnit { keyframe: false, ..}` + `CaptureRegion { x: 0, y: 0, width: 1920, height: 1080 }` → returns `2`. |
| `roi_classifier_periphery_gets_1_5x` | `roi.rs` | `CaptureRegion { x: 1920, y: 0, width: 800, height: 600 }` (right-side PiP) → returns `1`. |
| `roi_classifier_keyframe_always_gets_2x` | `roi.rs` | `EncodedVideoAccessUnit { keyframe: true, ..}` + any capture region → returns `2`. |
| `parity_adaptive_increases_on_loss_spike` | `rs_fec.rs` | `FecController::new(4)` then `adjust_for_loss(2500)` → returns `3` after 2 ticks (hysteresis). |
| `parity_adaptive_decreases_on_low_loss` | `rs_fec.rs` | Force m=2, then `adjust_for_loss(0)` → returns `0`. |
| `fec_decoder_late_shard_for_freed_block_is_dropped` | `fec_decoder.rs` | Add 4 data shards → emit → assert `add_shard` for same `frame_id` with a new chunk returns `None` and increments `stats.late_shards_dropped`. |
| `control_msg_sends_parity_decision` | `fec_decoder.rs` + `mod.rs` | Mock `ControlChannel`, drop 5 shards, assert `ControlMsg::BlockDiscarded { frame_id }` is sent within 250 ms. |
| `loopback_native_quic_fec_round_trip` | `mod.rs` (existing test file, add) | Full QUIC loopback, send 30 frames, drop 2 % of datagrams uniformly, assert receiver gets ≥ 95 % of frames intact (5 % degradation is the loss + recovery error budget). |

### 10.2 Module file layout

```
crates/qubox-transport/src/media/
├── mod.rs                  (existing — touch only at lines 424-435, 687)
├── rs_fec.rs               (existing — touch only to add DEFAULT_PARITY_SHARDS const)
├── roi.rs                  (NEW — §5.1, ~80 LoC + 3 tests)
└── fec_decoder.rs          (NEW — §6.3, ~180 LoC + 4 tests)
```

## Pitfalls (read these before coding)

1. **RS decode fails silently on a corrupted symbol.** The
   `reed-solomon-erasure` library returns
   `Error::TooFewDataShards` or `Error::TooFewShardsPresent` (see
   `rs_fec.rs:476-482` for the existing handling). **Always check
   the `Result` of `ReedSolomonFec::reconstruct` and treat `Err` as
   "block un-recoverable; transition to `Discarded` and send
   `BlockDiscarded`."** Never panic on `Err`.

2. **The `FLAG_LAST_CHUNK` trailer is 4 bytes, not part of the
   shard payload.** When the last DATA shard of a frame is
   received, strip the 4-byte `original_len` trailer
   (`mod.rs:389-394`) BEFORE feeding the shard to FEC. The
   parity shards NEVER carry the trailer (only DATA shards do).
   Confusing data and parity at this step corrupts the entire
   reconstructed access unit.

3. **Block boundaries span shards across multiple datagrams.**
   A 12 KB frame at k=4 produces 3 RS blocks: shards 0–3, 4–7,
   8–11. The decoder must slice the incoming shards back into
   blocks using `(chunk_id / block_size)`. A simple mod-arithmetic
   bug here will give "RS works in unit tests, fails on 4K144".

4. **`shard_len_for` at `rs_fec.rs:253-268` rounds up to 16-byte
   multiples.** If `frame.len() == 0`, it returns 0 (early exit at
   line 254). The `reconstruct` path must handle the
   `original_len == 0` case explicitly (drop the block, do not
   call `ReedSolomon::new(_, 0)` — the library panics).

5. **The `FecController::adjust_for_loss` step-up is one-at-a-time
   (`rs_fec.rs:379-385`), step-down is full.** A single 6 % loss
   sample does NOT jump from m=0 to m=4 — it takes 4 ticks. Do
   not "fix" this; the hysteresis prevents oscillation when loss
   sits on a threshold boundary. **Document this in the
   `FecController` doc-comment** if you touch it.

6. **`MediaDatagramSender::next_frame_id` wraps at `u32::MAX`**
   (`mod.rs:684-685`). The receiver must accept the wrap
   (treat frame_ids as u32, not as monotonic timestamps). The
   decoder `BTreeMap<u32, PendingBlock>` already uses u32 keys,
   so wrap is fine — but be aware that `JitterBuffer::pop_ready`
   iterates in `BTreeMap` order, which after wrap is non-monotonic.
   See the existing warn at `mod.rs:584-587` for the equivalent
   issue on `original_len` trailers.

7. **Apple Silicon throughput is ~2–4× lower than AVX2** because
   `reed-solomon-erasure` has no NEON intrinsics. This is a
   profiling observation, not a bug. If a future macOS perf
   trace points at FEC, switch to `reed-solomon-simd` — but be
   aware it requires even shard sizes (`shard_len_for` would need
   to round up to 2 instead of 16) and a different `ReedSolomon`
   API.

8. **The `quinn` `send_datagram` API returns `SendDatagramError::Full`
   when the 1 MiB buffer is exhausted.** `MediaDatagramSender::send_chunk`
   propagates this as `MediaDatagramSendError::Full`
   (`mod.rs:780-785`). The host caller must back off (e.g. drop
   the current frame, request a keyframe). Do NOT silently drop
   parity shards while keeping data shards — that breaks the RS
   invariants (block has data but no parity → FEC is disabled).

9. **The `MediaDatagramHeader::chunk_id` field is `u16` but RS
   needs `u8` shard counts.** The library uses u8 for `k` and
   `m`; our `chunk_id` is u16 because we also use it to count
   total chunks in a frame. The cast at `mod.rs:710` is
   `i as u16` — make sure `i < u16::MAX` before casting, or
   transition to multi-block headers.

10. **`FecDecoder::add_shard` returns `Some(bytes)` only when the
    block transitions to Ready on THIS shard.** A block that was
    already Ready and gets one more shard returns `None`. Don't
    double-emit. Test this explicitly with the
    `fec_decoder_emits_reconstructed_bytes` test.

## Verification commands

### Build + unit tests

```bash
# All FEC tests (existing + new):
cargo test -p qubox-transport rs_fec:: roi:: fec_decoder::

# Just the new ROI tests:
cargo test -p qubox-transport --lib media::roi::tests

# Just the new FEC decoder tests:
cargo test -p qubox-transport --lib media::fec_decoder::tests

# Full crate test (catches integration regressions):
cargo test -p qubox-transport

# All workspace tests:
cargo test --workspace
```

### CLI demo

The existing demo at
`crates/qubox-transport/examples/rs_fec_demo.rs` exercises the
RS path end-to-end with PSNR measurement. Run it:

```bash
cargo run -p qubox-transport --release --example rs_fec_demo
```

Expected output (must include these lines):

```
=== Qubix RS-FEC video-frame demo ===
  frame: 128x96 RGBA  (49152 bytes), 240 frames simulated
  saved /tmp/rs_demo_original.pgm (frame 0 reference, P5 format)
  encode+simulate-loss+recover @ 1 lost shard/frame:
    240 frames in   ... -> ... fps, ... MB/s
  PSNR after RS(4+2) recovery at various loss rates:
      0.0 %  |    ... dB       |     ... / ...  (visually lossless)
      2.0 %  |    ... dB       |     ... / ...  (imperceptible)
  Done.  Open /tmp/rs_demo_original.pgm in any image viewer to inspect the reference frame.
```

Open `/tmp/rs_demo_original.pgm` (a P5 grayscale PBM) — should
show a gradient + moving diagonal line. If the image is empty,
`RS(4+2)` is broken; do not proceed.

### End-to-end QUIC loopback

The new `loopback_native_quic_fec_round_trip` test (added in
PR-G) runs a full QUIC loopback with 2 % uniform datagram loss:

```bash
cargo test -p qubox-transport --release loopback_native_quic_fec_round_trip -- --nocapture
```

Expected: ≥ 95 % of 30 sent frames reassembled intact (5 %
budget covers loss spikes beyond RS(4+2) recovery range).

### Static checks

```bash
cargo clippy -p qubox-transport --all-targets -- -D warnings
cargo fmt -p qubox-transport --check
```

## References

- `crates/qubox-transport/Cargo.toml:14` — `reed-solomon-erasure = "6"`
- `crates/qubox-transport/src/media/mod.rs:36` — `MEDIA_DATAGRAM_MAGIC = [0x51, 0x42]`
- `crates/qubox-transport/src/media/mod.rs:48` — `FLAG_PARITY = 1 << 1`
- `crates/qubox-transport/src/media/mod.rs:118` — `WIRE_HEADER_SIZE = 14`
- `crates/qubox-transport/src/media/mod.rs:719-731` — parity datagram send loop
- `crates/qubox-transport/src/media/rs_fec.rs:42-588` — existing RS implementation
- `crates/qubox-transport/src/media/rs_fec.rs:53` — `DEFAULT_BLOCK_SIZE = 4`
- `crates/qubox-transport/src/media/rs_fec.rs:49` — `MAX_PARITY_SHARDS = 4`
- `crates/qubox-transport/src/media/rs_fec.rs:310-395` — `FecController`
- `crates/qubox-transport/src/lib.rs:1875-1876` — datagram buffer sizes (1 MiB)
- ADR-010 §1.2 — discriminator range allocation (gamepad 0x47, pen 0x50, mic 0x4D)
- ADR-011 §3 — `datagram_send_buffer_size = 1 MiB`
- ADR-013 §1 — `FramePacingSchedule::bytes_per_frame` budget
- IETF draft-michel-quic-fec — "Forward Erasure Correction for QUIC loss recovery" (Oct 2023, not adopted)
- RFC 5109 — ULPFEC (no fixed k/m; mask-based; not adopted — we reuse the parity concept but not the wire format)
- RFC 8627 — FlexFEC (same as ULPFEC, no fixed k/m)
- Maister "real-time video streaming experiments with forward error correction" (Feb 2024) — LT/fountain code latency data
- Klaus Post `reedsolomon` Go library README — Backblaze lineage
- `AndersTrier/reed-solomon-simd` GitHub README — AVX2/NEON benchmarks
- Iqbal & Zepernick (2011) "A framework for error protection of ROI coded images and videos" — UEP background
- Boulos et al. Packet Video 2009 — "Region of Interest-based error resilience model"
- Google Research "Handling Packet Loss in WebRTC" (2013) — FEC reduces artefacts by ~80 % at 5 % PLR
- "Reed-Solomon Codes and Their Applications" — Wicker, 1999 (textbook)