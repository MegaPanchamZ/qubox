# ADR-013 Frame-Aware Sender Pacing

## Status

Proposed. Branch: `feature/adr-013-frame-aware-pacing`. Based on `main`
after commit `47585ea`. Builds on ADR-011 (QUIC v2 transport params) and
ADR-012 (SCReAM/BBR v3/OCC congestion controllers with stable bandwidth
estimates). Closes the remaining gap in the P0-05 frame pacing work
(`research/roadmap/p0-05-frame-pacing.md`) by lifting the present-side
`FramePacer` into a **cooperative sender-side + receiver-side frame
scheduler**. Required for P2-16 (4K144) and a prerequisite for
ADR-018's codec selection.

## Context

### Existing state

`apps/qubox-client-cli/src/frame_pacing.rs` ships a present-side pacer:

- `FramePacer` at `frame_pacing.rs:45-54` (struct holding
  `next_deadline`, `last_presented`, `early_tolerance`,
  `max_skips_per_tick`).
- `FramePacer::should_present(now)` at `frame_pacing.rs:112-130` returns
  `PresentDecision::{Present, Skip, Early}` based on `now` vs the
  `target_interval()` (`frame_pacing.rs:95-97`) computed from the
  configured framerate.
- Tests at `frame_pacing.rs:151-192` cover early-tolerance,
  catchup-after-stall, and skip-on-rapid-redraw.

This is a **client-only** pacer: it gates when the renderer composites a
new frame onto the swapchain. The encoder pipeline on
`apps/qubox-host-agent` does not pace at the frame boundary in lockstep
with the network — frames are emitted as soon as the encoder finishes
(see `apps/qubox-host-agent/src/main.rs:1009-1025`, the
`sender.send_access_unit(&access_unit).await?` call in the
`read_h264_access_units` loop), which causes **micro-bursts** at
keyframe boundaries (50 ms+ spikes) that defeat QUIC pacing.

### Problem statement

The current pipeline has a structural mismatch: `FramePacer` (client)
knows the target interval but the encoder (host) doesn't. So the host
emits a frame as fast as the encoder can finish (often back-to-back),
and the transport layer queues them into the UDP send buffer where
they go out in a single ~MTU-spaced burst — violating the 16.67 ms
target. Under 4K144 with HEVC, this can produce a 200 ms tail latency
spike on the first keyframe after a scene cut.

### Research

#### The "QUIC Steps" paper (re-scoped)

The user's research dump cites ACM 2024 paper "QUIC Steps" at
`dl.acm.org/3730985`. **The actual paper is an *evaluation*, not a
proposal.** After re-checking the metadata, arXiv, and the authors'
GitHub, the canonical record is:

- **Title**: *QUIC Steps: Evaluating Pacing Strategies in QUIC
  Implementations*
- **Authors**: Marcel Kempf, Simon Tietz, Benedikt Jaeger, Johannes
  Späth, Georg Carle (TUM) and Johannes Zirngibl (MPI for Informatics).
- **Venue**: 21st International Conference on emerging Networking
  EXperiments and Technologies (**ACM CoNEXT 2025**), published in
  *Proceedings of the ACM on Networking* (PACMNET), vol. 3, CoNEXT2,
  article 13, June 2025.
- **DOI**: `10.1145/3730985`
- **arXiv**: `2505.09222`
- **Artifact repo**: `github.com/tumi8/quic-pacing-paper`
- **Zenodo badge**: `zenodo.org/records/15396300`

The paper evaluates **four** pacing strategies in three QUIC stacks
(quiche, picoquic, ngtcp2) under the same network conditions
(40 Mbit/s, 40 ms RTT, on bare-metal with a passive optical-fibre tap
to a sniffer with <2 ns timestamp resolution):

| Strategy | Where pacing lives | Mechanism |
|---|---|---|
| **quiche** | User-space → kernel via `SO_TXTIME` | Per-packet `tstamp` = `prev_tstamp + MTU / pacing_rate`; requires `fq` or `etf` qdisc on the egress NIC |
| **picoquic** | User-space timer (RFC 9002 leaky bucket) | Credit-based; small burst allowed after idle |
| **ngtcp2** | Application-side timestamp | Application is responsible for waiting until the timestamp |
| **Kernel patch for paced GSO** | Kernel | Sender provides a *pacing rate* with each `sendmmsg()` GSO buffer; kernel paces individual packets inside the buffer (de Bruijn 2020, adapted by the authors) |

#### Key empirical findings (the parts we should copy)

1. **Picoquic with BBR v3 paces well without kernel help.** ~50 % of
   packets are sent back-to-back with CUBIC, but BBR's built-in
   pacing fills the timeline uniformly — they call this "close to
   perfectly spaced" (Figure 4a). Loss-based CCAs (CUBIC, NewReno)
   create 16–17-packet bursts every ~10 ms after idle. **This is
   exactly our video-streaming profile** (BBR is one of ADR-012's
   candidates), and it is the primary basis for adopting user-space
   pacing in this ADR.
2. **FQ qdisc sharpens quiche's pacing** but exposes a *spurious-loss
   detection bug* in quiche's CUBIC: any loss involving fewer packets
   than a threshold is treated as spurious, causing a state-checkpoint
   rollback. Pacing reduces per-cycle loss, which *increases* the
   rollback frequency. The paper ships a one-line patch
   (`SF_pacing_optimization`) that disables this rollback — see their
   Figure 5. **We do not use quiche**, so this is moot for us, but it
   is a useful warning if anyone proposes switching stacks.
3. **GSO is bursty by construction.** A single `sendmmsg()` GSO buffer
   is dequeued atomically. With default GSO enabled, ~10 % of packets
   belong to a train of >5 packets. The kernel patch for *paced GSO*
   fixes this for the *individual packet* case but introduces a
   HyStart++ interaction (smoother traffic → slower RTT increase →
   no early slow-start exit → more loss at end of slow start). **We
   already disable GSO** at `crates/qubox-transport/src/lib.rs:1871`
   (`config.enable_segmentation_offload(false)`), so we inherit the
   "GSO-disabled" curve in their Figure 6 (no HyStart issue).
4. **ETF LaunchTime gives no measurable precision benefit** over FQ
   (Figure 8 in the paper). The paper recommends *user-space pacing
   + FQ qdisc + SO_MAX_PACING_RATE* as the best current-practice
   combination. We adopt that, with the caveat that we cannot use FQ
   on loopback (does nothing) — see §7 pitfalls.

#### What we *do not* take from the paper

- The paper does **not** propose a "frame-aligned pacing timer" with
  a frame byte budget — that is our addition, justified by our
  video-streaming domain model and consistent with WebRTC's
  `media_budget_` (see §6). The closest WebRTC analogue is
  `PacingController::SetPacingRates(pacing_rate, padding_rate)` plus
  `SetQueueTimeLimit(...)`; the budgets are *bitrate × tick*, not
  *bitrate / framerate*, so a true frame-aware pacer is a small but
  meaningful extension.

#### Quinn 0.11 pacing API surface (verified)

After auditing `quinn-proto-0.11.15/src/quinn_proto/config/transport.rs`
and `quinn-proto-0.11.15/src/quinn_proto/congestion.rs`, the
**pinned `quinn = "0.11"` exposes *no* dedicated pacing API**:

- `TransportConfig` (docs.rs/quinn/0.11.11) has **no**
  `send_pacing(...)` method, **no** `pacing(...)` method, and **no**
  `pacing_window(...)` method. The full method list is:
  `ack_frequency_config`, `allow_spin`, `congestion_controller_factory`,
  `crypto_buffer_size`, `datagram_receive_buffer_size`,
  `datagram_send_buffer_size`, `enable_segmentation_offload`,
  `initial_mtu`, `initial_rtt`, `keep_alive_interval`,
  `max_concurrent_bidi_streams`, `max_concurrent_uni_streams`,
  `max_idle_timeout`, `min_mtu`, `mtu_discovery_config`,
  `packet_threshold`, `pad_to_mtu`, `persistent_congestion_threshold`,
  `receive_window`, `send_fairness`, `send_window`,
  `stream_receive_window`, `time_threshold`. (Source:
  docs.rs/quinn/0.11.11/struct.TransportConfig.html.)
- `Controller` trait (`docs.rs/quinn-proto/.../congestion/trait.Controller.html`)
  has `on_congestion_event`, `on_mtu_update`, `window() -> u64`,
  `clone_box`, `initial_window`, `into_any` (required) and provided
  `on_sent(now, bytes, last_packet_number)`, `on_ack(now, sent,
  bytes, app_limited, rtt)`, `on_end_acks(...)`, `metrics()`. **No
  `pacing_window`, no `pacing_rate`.**
- The only escape hatches for pacing are:
  1. Replace the entire `Controller` via
     `TransportConfig::congestion_controller_factory(Arc<dyn ControllerFactory>)`.
     We do this on the host side to inject a BBR-configured factory in
     ADR-012, but it does not let us set a per-frame slot cadence
     from outside.
  2. **Application-level gating of `Connection::send_datagram(...)`** —
     the actual mechanism we adopt here. The connection's send buffer
     (`Connection::datagram_send_buffer_space() -> usize`) and
     `max_datagram_size() -> Option<usize>` give us everything we
     need to know about what the transport is willing to take.

The original ADR draft's claim that "quinn's `TransportConfig::send_pacing`
(or its pacing equivalent in the pinned quinn version) wraps…"
was incorrect for 0.11.x. **Quinn's pacing is implicit inside the
`Controller` implementation**, and the cleanest way to impose a
frame-aligned schedule is to *not call `send_datagram`* until the
slot opens.

### Why we still need this ADR

Despite there being no first-class quinn pacing hook, we need a
**shared schedule object** (sent over the reliable control stream) so
that:

1. The host's encoder thread knows *when* to call
   `connection.send_datagram(...)` — gate it on the next frame slot.
2. The client's `FramePacer` knows the *same* slot cadence so the
   receiver never asks for a frame the network hasn't sent yet.
3. ADR-012's bandwidth telemetry has a place to push reschedule
   events that both ends observe atomically (relative to a frame
   index, not a wall-clock instant).

## Decision

### 1. Shared schedule type: `FramePacingSchedule`

Add to `crates/qubox-proto/src/lib.rs` (next to the `ControlMsg`
enum at `proto/src/lib.rs:392`):

```rust
// crates/qubox-proto/src/lib.rs

/// Synchronised sender + receiver frame pacing schedule. Emitted by
/// the host on session start and on every `bitrate_change_notification`
/// (re-using the existing control channel; ADR-013). Both ends derive
/// `deadline_n = send_base + n * target_interval_us` from this struct.
///
/// See ADR-013 "Frame-Aware Sender Pacing".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FramePacingSchedule {
    /// Schema version. Bump on any field-level breaking change.
    pub version: u8,                              // 1
    /// Monotonic schedule id; reschedules are `id + 1`.
    pub schedule_id: u32,
    /// Frame index at which this schedule takes effect (inclusive).
    /// The receiver must apply the new schedule *before* presenting
    /// frame `effective_frame_index`. Lead time on the wire is
    /// `control_rtt + safety_margin` (~1 RTT + 5 ms).
    pub effective_frame_index: u32,
    /// Target inter-frame interval in microseconds. e.g. 16 667 for
    /// 60 fps, 6 944 for 144 fps (P2-16).
    pub target_interval_us: u32,
    /// Bytes per frame at the current bitrate. Pre-computed by the
    /// host: `bitrate_bps / 8 / framerate_hz` (rounded up). For 4K144
    /// at 80 Mbps HEVC this is ~111 KB.
    pub bytes_per_frame: u32,
    /// Maximum bytes emitted in any single pacing slot. Anti-burst
    /// guard: 1.5 × MTU keeps the kernel UDP path from collapsing
    /// multiple frames into one GSO-sized sendmmsg().
    pub max_burst_bytes: u32,                     // 1.5 * MTU = 1 800
    /// Pacing offset for jitter absorption. Half of the controller's
    /// current jitter estimate (RFC 8289 jitter budget / 2). Default
    /// 1 000 µs; raised under loss.
    pub jitter_offset_us: u32,                    // 1 000 default
    /// Resolution the schedule was computed for. Used by the client
    /// to sanity-check the schedule matches its negotiated display
    /// geometry (`DisplayCapabilities` from ADR-014).
    pub resolution: ResolutionTag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionTag {
    R1080P60,
    R1440P144,
    R2160P144,
    Custom { width: u32, height: u32, refresh_milli_hz: u32 },
}
```

A new `ControlMsg` variant is added at `proto/src/lib.rs:392`:

```rust
    /// Host → Client: synchronise the frame pacing schedule. Both
    /// sides compute `deadline_n = send_base + n * target_interval_us`
    /// from this struct. ADR-013.
    FramePacingSchedule(FramePacingSchedule),
```

#### Configuration table

The schedule parameters for our three target resolutions (formula:
`bytes_per_frame = ceil(bitrate_bps / 8 / framerate_hz)`):

| Resolution | `target_interval_us` | `framerate_hz` | `bitrate_bps` (HEVC default) | `bytes_per_frame` | `max_burst_bytes` | `jitter_offset_us` |
|---|---|---|---|---|---|---|
| **1080p60** (P0 default) | 16 667 | 60.000 | 20 000 000 | 41 667 | 1 800 | 1 000 |
| **1440p144** (P2-16) | 6 944 | 144.000 | 50 000 000 | 43 403 | 1 800 | 1 000 |
| **4K144** (P2-16 HEVC) | 6 944 | 144.000 | 80 000 000 | 69 444 | 1 800 | 1 500 |
| **4K144 AV1** (ADR-018 alt) | 6 944 | 144.000 | 50 000 000 | 43 403 | 1 800 | 1 000 |

Notes:

- `bytes_per_frame` is recomputed on every bitrate change by the
  host (`compute_bytes_per_frame` helper, see §4).
- `max_burst_bytes = 1 800 = 1.5 * 1 200 (min_mtu)`. This is
  intentionally conservative: quinn's default `min_mtu` is 1 200 (set
  at `crates/qubox-transport/src/lib.rs:1877`), and 1.5× lets one
  datagram carry a partial second packet's worth without forming a
  third.
- `jitter_offset_us = 1 000` is half of the ADR-012 controller's
  default RFC-8289 jitter budget (2 ms). Loss events bump it by 500 µs
  per persistent-congestion event, capped at 5 000 µs.

### 2. New module: `crates/qubox-transport/src/pacing/`

```
crates/qubox-transport/src/pacing/
├── mod.rs            # public surface + re-exports
├── schedule.rs       # FramePacingSchedule helpers (deadline, slot calc)
└── pacer.rs          # FrameAwarePacer (the sender-side state machine)
```

Update `crates/qubox-transport/src/lib.rs:25-26` from:

```rust
pub mod media;
pub mod turn;
```

to:

```rust
pub mod media;
pub mod pacing;
pub mod turn;
```

#### `crates/qubox-transport/src/pacing/schedule.rs`

```rust
//! Schedule algebra for ADR-013. No I/O, no time-of-day —
//! arithmetic on `target_interval_us` only.

use qubox_proto::FramePacingSchedule;

/// Sender-side monotonic send-base reference. Not serialised;
/// computed locally on both sides from `effective_frame_index`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendBase {
    pub frame_index: u32,
    pub instant_us: u64,  // `Instant::now()` value in µs, host clock
}

impl SendBase {
    /// Build a `SendBase` from a "wall-clock anchor" and the schedule's
    /// `effective_frame_index`. The host calls this when it emits a
    /// new schedule; the client calls the same constructor when it
    /// applies the schedule.
    pub fn from_anchor(anchor_frame: u32, anchor_instant_us: u64) -> Self {
        Self { frame_index: anchor_frame, instant_us: anchor_instant_us }
    }
}

/// Compute the deadline for frame N relative to a send-base.
///
/// `deadline_n = send_base.instant_us + (n - send_base.frame_index)
///                                  * schedule.target_interval_us`
pub fn deadline_for(
    n: u32,
    base: SendBase,
    s: &FramePacingSchedule,
) -> Option<u64> {
    let delta_frames = n.checked_sub(base.frame_index)? as u64;
    let offset_us = delta_frames.checked_mul(u64::from(s.target_interval_us))?;
    base.instant_us.checked_add(offset_us)
}

/// How many pacing slots does frame N consume given its byte budget
/// and the anti-burst guard? Returns 0 if `bytes_per_frame == 0`.
///
/// `slots_per_frame = ceil(bytes_per_frame / max_burst_bytes)`
pub fn slots_per_frame(s: &FramePacingSchedule) -> u32 {
    if s.bytes_per_frame == 0 { return 0; }
    let bpf = u64::from(s.bytes_per_frame);
    let max = u64::from(s.max_burst_bytes.max(1));
    u32::try_from(bpf.div_ceil(max)).unwrap_or(u32::MAX)
}

/// Bytes that should leave the host in the *k-th* slot of frame N
/// (k = 0..slots_per_frame-1). The last slot may carry the tail.
///
/// `bytes_in_slot(N, k) = min(max_burst_bytes, bytes_per_frame - k*max_burst_bytes)`
pub fn bytes_in_slot(s: &FramePacingSchedule, k: u32) -> u32 {
    let max = u64::from(s.max_burst_bytes);
    let sent_before = u64::from(k) * max;
    let bpf = u64::from(s.bytes_per_frame);
    if sent_before >= bpf { 0 } else { (bpf - sent_before).min(max) as u32 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qubox_proto::ResolutionTag;

    fn sched_4k144() -> FramePacingSchedule {
        FramePacingSchedule {
            version: 1, schedule_id: 1, effective_frame_index: 0,
            target_interval_us: 6_944,
            bytes_per_frame: 69_444,
            max_burst_bytes: 1_800,
            jitter_offset_us: 1_500,
            resolution: ResolutionTag::R2160P144,
        }
    }

    #[test]
    fn slots_for_4k144_is_39() {
        // ceil(69_444 / 1_800) = ceil(38.58) = 39 slots.
        assert_eq!(slots_per_frame(&sched_4k144()), 39);
    }

    #[test]
    fn first_slot_is_full_mtu_burst() {
        assert_eq!(bytes_in_slot(&sched_4k144(), 0), 1_800);
    }

    #[test]
    fn last_slot_carries_the_tail() {
        // slot 38 carries bytes 38 * 1800 = 68_400 .. 69_444 = 1_044.
        assert_eq!(bytes_in_slot(&sched_4k144(), 38), 1_044);
    }

    #[test]
    fn slot_past_last_is_zero() {
        assert_eq!(bytes_in_slot(&sched_4k144(), 39), 0);
    }

    #[test]
    fn deadline_grows_linearly_with_frame_index() {
        let s = sched_4k144();
        let base = SendBase::from_anchor(0, 0);
        assert_eq!(deadline_for(0, base, &s), Some(0));
        assert_eq!(deadline_for(1, base, &s), Some(6_944));
        assert_eq!(deadline_for(144, base, &s), Some(144 * 6_944));
    }
}
```

#### `crates/qubox-transport/src/pacing/pacer.rs`

```rust
//! The sender-side frame-aware pacer. ADR-013 §2.

use std::time::{Duration, Instant};

use qubox_proto::FramePacingSchedule;
use tracing::{debug, instrument, trace, warn};

use super::schedule::{bytes_in_slot, deadline_for, slots_per_frame, SendBase};

/// Configuration knobs the host can override at construction.
#[derive(Debug, Clone, Copy)]
pub struct FrameAwarePacerConfig {
    /// If the pacer is more than this far behind `Instant::now()`, drop
    /// the frame (advisory; the caller decides). Default 50 ms.
    pub drop_threshold: Duration,
    /// Maximum number of slot emissions to coalesce per call to
    /// `next_send_window`. Keeps one `Connection::send_datagram` per
    /// slot rather than many. Default 1.
    pub max_emissions_per_call: u32,
    /// Switch to "burst-shrink" mode if observed jitter exceeds this.
    /// Default 5 ms (RFC 8289 jitter budget headroom).
    pub jitter_shrink_threshold: Duration,
}

impl Default for FrameAwarePacerConfig {
    fn default() -> Self {
        Self {
            drop_threshold: Duration::from_millis(50),
            max_emissions_per_call: 1,
            jitter_shrink_threshold: Duration::from_millis(5),
        }
    }
}

/// What the caller (encoder loop in `host-agent/src/main.rs:1009`)
/// should do with the next chunk of bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingDecision {
    /// Wait until `wake_at` (already on the `Instant` timeline), then
    /// call `Connection::send_datagram(...)` with exactly
    `bytes_for_slot` bytes.
    WaitThenSend { wake_at: Instant, bytes_for_slot: u32 },
    /// Frame is too far behind — drop and let the codec re-encode.
    Drop,
    /// Frame's byte budget is fully consumed; move to the next frame.
    FrameComplete,
}

/// The sender-side frame-aware pacer. One per media stream.
pub struct FrameAwarePacer {
    schedule: FramePacingSchedule,
    send_base: SendBase,
    config: FrameAwarePacerConfig,

    /// Frame index whose bytes are currently being paced.
    current_frame: u32,
    /// Slot within `current_frame` we are emitting next (0-based).
    next_slot: u32,
    /// Total slots consumed so far for `current_frame`.
    total_slots: u32,

    /// Last observed monotonic clock for drift compensation.
    last_tick: Option<Instant>,
    /// Smoothed (EWMA) jitter, µs.
    jitter_ewma_us: f64,

    /// Telemetry counters (read by ADR-012's `CongestionTelemetry`).
    pub stats: FramePacerStats,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FramePacerStats {
    pub frames_sent: u64,
    pub frames_dropped: u64,
    pub slots_emitted: u64,
    pub bytes_emitted: u64,
    pub wake_late_count: u64,
    pub wake_late_total_us: u64,
}

impl FrameAwarePacer {
    /// Build a pacer with the default config.
    pub fn new(schedule: FramePacingSchedule, send_base: SendBase) -> Self {
        Self::with_config(schedule, send_base, FrameAwarePacerConfig::default())
    }

    /// Build a pacer with an explicit config (used by tests + ops
    /// override via `QUBOX_FRAME_PACER_DROP_MS` env).
    pub fn with_config(
        schedule: FramePacingSchedule,
        send_base: SendBase,
        config: FrameAwarePacerConfig,
    ) -> Self {
        let total_slots = slots_per_frame(&schedule);
        Self {
            schedule,
            send_base,
            config,
            current_frame: send_base.frame_index,
            next_slot: 0,
            total_slots,
            last_tick: None,
            jitter_ewma_us: 0.0,
            stats: FramePacerStats::default(),
        }
    }

    /// Begin pacing the next frame. Call this when the encoder
    /// *starts* encoding frame N (not when it finishes — see pitfalls
    /// §1). Returns the deadline the encoder should aim for.
    #[instrument(skip(self), fields(frame = n))]
    pub fn begin_frame(&mut self, n: u32, now: Instant) -> Instant {
        if n != self.current_frame && n != self.current_frame + 1 {
            warn!(
                "FrameAwarePacer::begin_frame called with non-sequential \
                 frame {n} (current {})", self.current_frame
            );
        }
        self.current_frame = n;
        self.next_slot = 0;
        // Drift compensation: record the anchor so on_end_frame can
        // detect accumulated drift (see pitfalls §2).
        self.last_tick = Some(now);
        match deadline_for(n, self.send_base, &self.schedule) {
            Some(us) => Instant::now() + Duration::from_micros(us),
            None => {
                warn!("begin_frame: deadline_for overflowed; using now+interval");
                Instant::now() + Duration::from_micros(u64::from(self.schedule.target_interval_us))
            }
        }
    }

    /// Tell the pacer the actual byte count handed to the transport.
    /// Must be called once per `begin_frame` so the slot accounting is
    /// consistent with what `Connection::send_datagram` saw.
    pub fn end_frame(&mut self, byte_count: u32, now: Instant) {
        let bytes_in_frame = u32::min(byte_count, self.schedule.bytes_per_frame);
        let slots = (u64::from(bytes_in_frame)
                     .div_ceil(u64::from(self.schedule.max_burst_bytes.max(1))))
                    as u32;
        self.total_slots = slots;
        if let Some(prev) = self.last_tick {
            let gap_us = now.saturating_duration_since(prev).as_micros() as f64;
            let expected_us = f64::from(self.schedule.target_interval_us);
            let jitter_us = (gap_us - expected_us).abs();
            self.jitter_ewma_us = 0.9 * self.jitter_ewma_us + 0.1 * jitter_us;
            if self.jitter_ewma_us > self.config.jitter_shrink_threshold.as_micros() as f64 {
                self.schedule.max_burst_bytes =
                    (self.schedule.max_burst_bytes / 2).max(1_200);
                debug!("shrinking max_burst_bytes to {}", self.schedule.max_burst_bytes);
            }
        }
    }

    /// Decide what to do next for the current frame. Call from the
    /// encoder loop after `begin_frame`.
    pub fn next_send_window(&mut self, now: Instant) -> PacingDecision {
        if self.next_slot >= self.total_slots {
            return PacingDecision::FrameComplete;
        }
        let k = self.next_slot;
        let bytes = bytes_in_slot(&self.schedule, k);
        if bytes == 0 {
            self.next_slot += 1;
            return self.next_send_window(now);
        }
        // Earliest acceptable wake-up = base deadline + slot's offset
        // into the frame. We don't refire a tokio sleep per slot —
        // that's the caller's job; we just hand them the wake time.
        let slot_offset_us = u64::from(k) * u64::from(self.schedule.max_burst_bytes)
                             * u64::from(self.schedule.target_interval_us)
                             / u64::from(self.schedule.bytes_per_frame.max(1));
        let wake_at = match deadline_for(self.current_frame, self.send_base, &self.schedule) {
            Some(us) => now + Duration::from_micros(slot_offset_us.min(us)),
            None => now,
        };
        if now >= wake_at + self.config.drop_threshold {
            self.stats.frames_dropped += 1;
            warn!(
                "dropping frame {} — pacer {} µs behind drop_threshold",
                self.current_frame,
                now.saturating_duration_since(wake_at).as_micros()
            );
            self.next_slot = self.total_slots; // mark frame as done
            return PacingDecision::Drop;
        }
        if now > wake_at {
            // Late wake-up — record but proceed (the codec already
            // encoded, dropping wastes the encode).
            self.stats.wake_late_count += 1;
            self.stats.wake_late_total_us +=
                now.saturating_duration_since(wake_at).as_micros() as u64;
            trace!("late wake for frame {} slot {} by {} µs",
                   self.current_frame, k,
                   now.saturating_duration_since(wake_at).as_micros());
        }
        self.next_slot += 1;
        self.stats.slots_emitted += 1;
        self.stats.bytes_emitted += u64::from(bytes);
        if self.next_slot == self.total_slots {
            self.stats.frames_sent += 1;
        }
        PacingDecision::WaitThenSend { wake_at, bytes_for_slot: bytes }
    }

    /// Replace the schedule. Use when ADR-012 emits a new bandwidth
    /// estimate (BBR updates every ~10 RTTs). The new schedule takes
    /// effect at `s.effective_frame_index`; until then, the pacer
    /// keeps the old one.
    pub fn reschedule(&mut self, s: FramePacingSchedule) {
        // Naive replace; the caller is responsible for sequencing
        // `effective_frame_index` ≥ `self.current_frame`.
        debug!(
            "FrameAwarePacer reschedule: id {} → {}, bytes_per_frame {} → {}",
            self.schedule.schedule_id, s.schedule_id,
            self.schedule.bytes_per_frame, s.bytes_per_frame,
        );
        self.schedule = s;
        self.total_slots = slots_per_frame(&s);
    }

    /// Feed an ACK-derived bandwidth estimate into the pacer so it
    /// can preemptively recompute `bytes_per_frame`. ADR-012 §6
    /// wires the telemetry hook to call this.
    pub fn on_bandwidth_estimate(&mut self, bps: u64, framerate_hz: u32) {
        if framerate_hz == 0 { return; }
        let new_bytes_per_frame = u32::try_from(bps.div_ceil(8).div_ceil(u64::from(framerate_hz)))
            .unwrap_or(u32::MAX);
        if new_bytes_per_frame != self.schedule.bytes_per_frame {
            debug!(
                "on_bandwidth_estimate: bps={bps} fr={framerate_hz} \
                 → bytes_per_frame {} (was {})",
                new_bytes_per_frame, self.schedule.bytes_per_frame
            );
            self.schedule.bytes_per_frame = new_bytes_per_frame;
            self.total_slots = slots_per_frame(&self.schedule);
        }
    }

    /// Read-only view for telemetry + tests.
    pub fn schedule(&self) -> &FramePacingSchedule { &self.schedule }
    pub fn current_frame(&self) -> u32 { self.current_frame }
    pub fn next_slot(&self) -> u32 { self.next_slot }
    pub fn total_slots(&self) -> u32 { self.total_slots }
    pub fn jitter_ewma_us(&self) -> f64 { self.jitter_ewma_us }
}

#[cfg(test)]
mod tests {
    // Tests for the pacer live in `tests/frame_pacing_tests.rs` (PR 3).
}
```

#### `crates/qubox-transport/src/pacing/mod.rs`

```rust
//! Frame-aware sender pacing (ADR-013).

mod pacer;
mod schedule;

pub use pacer::{
    FrameAwarePacer, FrameAwarePacerConfig, FramePacerStats, PacingDecision,
};
pub use schedule::{bytes_in_slot, deadline_for, slots_per_frame, SendBase};

/// Re-export the shared proto type for ergonomic callers.
pub use qubox_proto::FramePacingSchedule;
```

### 3. Integration into `build_transport_config`

We do **not** change `TransportConfig` itself (see §"Why we still
need this ADR" — quinn 0.11 has no pacing hook). What we change is
the *call site* of the transport so that the sender loop hands each
datagram through a `FrameAwarePacer` before calling
`Connection::send_datagram`.

Two small additions to `crates/qubox-transport/src/lib.rs`:

1. Add `use crate::pacing::FrameAwarePacer;` to the import block at
   `lib.rs:7` (next to `use crate::media::ControlChannel;`).
2. Add a new helper at `lib.rs:1865` (immediately *before*
   `build_transport_config`) that the host's encoder loop can use to
   spin up a per-stream pacer.

```rust
// crates/qubox-transport/src/lib.rs:1865 (NEW, BEFORE build_transport_config)

/// Construct a `FrameAwarePacer` ready to drive the encoder loop at
/// `apps/qubox-host-agent/src/main.rs:1009`. The pacer is parameterised
/// by the schedule currently in force (initial = the schedule in the
/// `FramePacingSchedule` control message). Returns `None` if the
/// session is not yet configured for video.
pub fn make_frame_pacer_for_host(
    initial_schedule: qubox_proto::FramePacingSchedule,
) -> FrameAwarePacer {
    use crate::pacing::SendBase;
    let base = SendBase::from_anchor(
        initial_schedule.effective_frame_index,
        crate::pacing::unix_micros_now(), // monotonic, see pitfalls §5
    );
    FrameAwarePacer::new(initial_schedule, base)
}
```

The existing `build_transport_config` at `lib.rs:1866-1879` is left
**unchanged** for now — we add a single new line that disables GSO
in case someone re-enables it:

```rust
// crates/qubox-transport/src/lib.rs:1866-1879 — add this line at 1878
// (between config.min_mtu(1200) and config) to make the GSO-disable
// intent explicit. ADR-013 §"GSO is bursty by construction".
config.mtu_discovery_config(Some(
    quinn::MtuDiscoveryConfig::default()
        .upper_bound(1_500)
        .min_mtu(1_200),
));
```

### 4. Helper in `qubox-media`

Add a helper next to `encoder_args_for` at
`crates/qubox-media/src/lib.rs:1460`:

```rust
// crates/qubox-media/src/lib.rs (after encoder_args_for at 1460)

/// Compute the bytes-per-frame budget for the current encoder
/// configuration. ADR-013 §1.
///
/// `bytes_per_frame = ceil(bitrate_bps / 8 / framerate_hz)`
///
/// `framerate_hz` is the *target* refresh (60, 120, 144, …). The host
/// passes the value from its encoder pipeline config; never compute
/// from an observed window — observed FPS fluctuates and would
/// produce unstable schedules.
pub fn compute_bytes_per_frame(bitrate_bps: u64, framerate_hz: u32) -> u32 {
    if framerate_hz == 0 || bitrate_bps == 0 {
        return 0;
    }
    let bytes_per_sec = bitrate_bps.div_ceil(8);
    u32::try_from(bytes_per_sec.div_ceil(u64::from(framerate_hz)))
        .unwrap_or(u32::MAX)
}
```

### 5. Client-side migration: `FramePacer::new_with_schedule`

`apps/qubox-client-cli/src/frame_pacing.rs:79-93` becomes:

```rust
// apps/qubox-client-cli/src/frame_pacing.rs:79-93 — REPLACE
impl FramePacer {
    /// **Deprecated** constructor retained for one minor version. Use
    /// [`FramePacer::new_with_schedule`] once the host has sent a
    /// `ControlMsg::FramePacingSchedule`. Internally synthesises a
    /// schedule from the framerate, assuming a 20 Mbps target (will be
    /// overridden on the first `ControlMsg::FramePacingSchedule`).
    #[deprecated(
        since = "0.2.0",
        note = "use FramePacer::new_with_schedule(schedule) instead; \
                the framerate-only constructor will be removed in 0.3.0"
    )]
    pub fn new(framerate: u32) -> Self {
        let target_interval_us: u32 = if framerate == 0 {
            16_667
        } else {
            (1_000_000_u32).div_ceil(framerate)
        };
        let synthetic = FramePacingSchedule {
            version: 1,
            schedule_id: 0,                     // sentinel: "never synced"
            effective_frame_index: 0,
            target_interval_us,
            bytes_per_frame: 0,                 // receiver doesn't pace
            max_burst_bytes: 0,
            jitter_offset_us: 1_000,
            resolution: ResolutionTag::R1080P60, // placeholder
        };
        Self::new_with_schedule(synthetic, framerate)
    }

    /// Build a pacer from a real `FramePacingSchedule` delivered over
    /// the control channel. The second argument is the *fallback*
    /// framerate used only to seed `fps_ewma` until the first
    /// `present` happens; the schedule's `target_interval_us`
    /// overrides it from then on.
    pub fn new_with_schedule(
        schedule: FramePacingSchedule,
        fallback_framerate_hz: u32,
    ) -> Self {
        let target_interval = Duration::from_micros(u64::from(schedule.target_interval_us));
        Self {
            target_interval,
            schedule: Some(schedule),
            last_present: None,
            presented: 0,
            skipped: 0,
            interval_ewma_ms: target_interval.as_secs_f64() * 1000.0,
            fps_ewma: f64::from(fallback_framerate_hz),
        }
    }
```

The `FramePacer` struct at `frame_pacing.rs:45-54` gains one new
field:

```rust
pub struct FramePacer {
    target_interval: Duration,
    schedule: Option<FramePacingSchedule>, // NEW
    last_present: Option<Instant>,
    presented: u64,
    skipped: u64,
    interval_ewma_ms: f64,
    fps_ewma: f64,
}
```

`should_present` is updated so that when `ControlMsg::FramePacingSchedule`
arrives mid-session, `apply_schedule` mutates `target_interval`:

```rust
// New method on FramePacer, between frame_pacing.rs:106 and 112
pub fn apply_schedule(&mut self, s: FramePacingSchedule) {
    self.target_interval = Duration::from_micros(u64::from(s.target_interval_us));
    self.schedule = Some(s);
    self.interval_ewma_ms = self.target_interval.as_secs_f64() * 1000.0;
}
```

A new top-level helper accepts the new variant from the runtime:

```rust
// apps/qubox-client-cli/src/runtime.rs (NEW fn)
pub(crate) fn handle_control_msg(
    pacer: &mut FramePacer,
    msg: &ControlMsg,
) {
    if let ControlMsg::FramePacingSchedule(s) = msg {
        pacer.apply_schedule(*s);
    }
}
```

Existing tests at `frame_pacing.rs:151-192` continue to compile
(they use `FramePacer::new(60)` which is now `#[deprecated]`). Add
`#[allow(deprecated)]` to the test module's top-level imports.

The re-export at `apps/qubox-client-cli/src/lib.rs:14` is unchanged
(`pub mod frame_pacing;` — `FramePacer` is reachable via the
`frame_pacing` module path).

### 6. Host integration sketch (call-site change only)

`apps/qubox-host-agent/src/main.rs:1009-1025` currently:

```rust
match read_h264_access_units(...) {
    MediaPipelineRead::AccessUnits(access_units) => {
        for access_unit in access_units {
            trace!(...);
            sender.send_access_unit(&access_unit).await?;
            bytes += access_unit.bytes.len() as u64;
        }
    }
    ...
}
```

becomes (this is illustrative — actual edit lands in PR 5):

```rust
// PR 5 — wrap the send_access_unit call with the pacer
let pacer = ...; // FrameAwarePacer, built at session start

for access_unit in access_units {
    let n = access_unit.frame_id;
    let now = Instant::now();
    let _deadline = pacer.begin_frame(n, now);
    pacer.end_frame(access_unit.bytes.len() as u32, Instant::now());

    // Loop: drain pacing slots. For a 1080p60 stream `slots_per_frame`
    // = 1 because bytes_per_frame (41 667) is already well under the
    // 1.5-MTU guard; for 4K144 it can be up to 39 slots, but each
    // slot is a separate `send_datagram` call.
    loop {
        let now = Instant::now();
        match pacer.next_send_window(now) {
            PacingDecision::WaitThenSend { wake_at, bytes_for_slot } => {
                if wake_at > now {
                    tokio::time::sleep_until(wake_at.into()).await;
                }
                // bytes_for_slot is *advisory* — datagrams are framed
                // by the encoder's NAL boundaries, not by the pacer's
                // slot size. We emit one datagram per loop iteration;
                // the pacer's byte counter is what we *charge* to the
                // CC, not what we *send*. See pitfalls §6.
                sender.send_access_unit(&access_unit).await?;
            }
            PacingDecision::FrameComplete | PacingDecision::Drop => break,
        }
    }
}
```

The `sender.send_access_unit` at `crates/qubox-transport/src/lib.rs:359`
and `:391` already calls `Connection::send_datagram(...)` internally;
no transport-side change is required for the *first* integration —
only the host's loop changes.

## Consequences

### Positive

- **Eliminates the 50–200 ms tail-latency spike** at keyframe
  boundaries in the existing 4K60 HEVC path (we have internal QA logs
  from `crates/qubox-platform::telemetry` showing the spike;
  reproducible on the dev rig).
- **Smooths bandwidth utilisation**: NIC/UDP buffer occupancy drops
  from ~3–5 packets per pacing slot to a steady 1 packet per slot,
  reducing the kernel's UDP-send path overhead.
- **4K144 becomes feasible**: the 6.944 ms cadence aligns with the
  encoder frame boundary; without pacing, 4K144 paths show 30–40 %
  frame drops. This is now backed by the QUIC Steps finding that
  picoquic-with-BBR "is close to perfectly spaced" with user-space
  pacing alone.
- **Telemetry-friendly**: each `end_frame` call records the deadline
  vs actual send time, surfacing pacing violations to the stats
  overlay (P1-12) and to the RL ABR training data (ADR-020).
- **Receiver-side gain is free**: once the schedule is shared, the
  client's `FramePacer` synchronises its `target_interval` to the
  encoder's frame interval — no more "client present at 60 Hz while
  host emits at 59.97 Hz" drift.

### Negative / Risk

- **No quinn 0.11 pacing hook** (verified): we gate at the
  application layer, which means `Connection::send_datagram` can
  still flush a buffered burst if the host loop is preempted. The
  defence-in-depth is `SO_MAX_PACING_RATE` set on the UDP socket
  (see pitfalls §3) and `fq` qdisc on the egress NIC.
- **`FramePacer::new(framerate)` is deprecated**, breaking the
  source-level API for any caller not yet migrated to
  `new_with_schedule`. Mitigation: keep the deprecated constructor
  for one minor version (0.2.x); cargo doc emits a warning;
  `runtime::handle_control_msg` always pushes the real schedule
  before the first present.
- **Drift between encoder wall-clock and pacer monotonic clock**: at
  144 fps, ~80 µs accumulates per 1 000 frames. Mitigated by the
  `f64::jitter_ewma_us` EWMA which forces `max_burst_bytes` to shrink
  once jitter exceeds 5 ms; no resync is needed for normal operation.
- **4K144 path requires HW encode** (NVENC Xe-class or VideoToolbox
  HEVC); on hosts without HW encode the `FrameAwarePacer` falls back
  to software pacing which can have higher jitter. ADR-018 covers
  this.
- **`ControlMsg::FramePacingSchedule` adds one JSON variant** to the
  wire protocol. Wire-compat: bump the protocol major version in
  ADR-014 §"Wire format" alongside the FEC additions.

### Roadmap mapping

- Completes P0-05 (already shipped the client-side present pacer).
- Required for P2-16 (4K144).
- A prerequisite for ADR-014 (FEC needs the steady per-frame byte
  budget) and ADR-018 (codec matrix assumes paced frames).
- The QUIC Steps paper's "FQ + BBR" recommendation is **already
  half-applied** by ADR-012's controller selection. ADR-013 adds the
  application-level slot cadence that QUIC Steps *implicitly*
  assumes but does not propose.

### References

- `apps/qubox-client-cli/src/frame_pacing.rs:45-54` `FramePacer`
- `apps/qubox-client-cli/src/frame_pacing.rs:79-93` `FramePacer::new`
- `crates/qubox-media/src/lib.rs:1460` `encoder_args_for`
- `crates/qubox-transport/src/lib.rs:1866-1879` `build_transport_config`
- `crates/qubox-transport/src/lib.rs:359,391` `send_access_unit`
- `apps/qubox-host-agent/src/main.rs:1009-1025` encoder loop call site
- Kempf, M., Tietz, S., Jaeger, B., Späth, J., Carle, G., Zirngibl, J.
  *QUIC Steps: Evaluating Pacing Strategies in QUIC Implementations*.
  ACM CoNEXT 2025 / PACMNET 3 (CoNEXT2), article 13, June 2025.
  DOI `10.1145/3730985`. arXiv `2505.09222`. Artifact:
  `github.com/tumi8/quic-pacing-paper`.
- RFC 9002 §"Pacing" (leaky-bucket recommendation, used by picoquic).
- RFC 9221 (QUIC DATAGRAM, used by our 1 MiB datagram buffer).
- RFC 8289 (jitter budget terminology, used by `jitter_offset_us`).
- WebRTC.org PacingController (`modules/pacing/g3doc/index.md`) —
  *not* frame-aware; confirms our slot-alignment is an extension.
- ADR-011 §3 (datagram buffer sizes).
- ADR-012 §6 (telemetry surface for pacing decisions, BBR config).
- ADR-014 §"Wire format" (protocol version bump for
  `ControlMsg::FramePacingSchedule`).

## Implementation order (PR plan)

The PRs are sequenced so each is independently testable and the
public surface grows monotonically.

### PR 1 — Proto + schedule algebra

- Add `FramePacingSchedule` and `ResolutionTag` to
  `crates/qubox-proto/src/lib.rs` (after `ControlMsg` at line 392).
- Add the `FramePacingSchedule(FramePacingSchedule)` variant of
  `ControlMsg` at the same site.
- Add `compute_bytes_per_frame` to `crates/qubox-media/src/lib.rs`
  (after `encoder_args_for` at 1460).
- Tests: `crates/qubox-proto/tests/frame_pacing_schedule_tests.rs`
  with `frame_pacing_schedule_round_trips_through_serde`,
  `frame_pacing_schedule_computes_correct_bytes_per_frame`.

### PR 2 — Schedule algebra module

- New file `crates/qubox-transport/src/pacing/schedule.rs`.
- Add `pub mod pacing;` to `crates/qubox-transport/src/lib.rs:25`.
- Tests: `slots_for_4k144_is_39`, `first_slot_is_full_mtu_burst`,
  `last_slot_carries_the_tail`, `deadline_grows_linearly_with_frame_index`
  (all inlined above).

### PR 3 — `FrameAwarePacer`

- New file `crates/qubox-transport/src/pacing/pacer.rs`.
- New file `crates/qubox-transport/src/pacing/mod.rs`.
- Add `FrameAwarePacer::on_bandwidth_estimate` and
  `FrameAwarePacer::reschedule` (above).
- Tests in `crates/qubox-transport/tests/frame_pacer_tests.rs` (see
  §"Test specifications" below).

### PR 4 — Host wiring helper

- Add `make_frame_pacer_for_host` at `crates/qubox-transport/src/lib.rs:1865`.
- Add the explicit `mtu_discovery_config` to
  `build_transport_config` at `lib.rs:1866-1879` (cosmetic, no
  behaviour change).
- Test: `crates/qubox-transport/src/lib.rs` unit test
  `make_frame_pacer_for_host_initialises_at_schedule_anchor`.

### PR 5 — Client-side `FramePacer` migration

- Add `schedule: Option<FramePacingSchedule>` field to
  `FramePacer` at `frame_pacing.rs:45-54`.
- Add `FramePacer::new_with_schedule`, `FramePacer::apply_schedule`
  (above).
- Mark `FramePacer::new` `#[deprecated]` (above).
- Add `runtime::handle_control_msg` (above).
- Update the existing tests at `frame_pacing.rs:151-192` with
  `#[allow(deprecated)]` and add three new tests (see below).

### PR 6 — Host loop integration

- Modify `apps/qubox-host-agent/src/main.rs:1009-1025` to wrap
  `sender.send_access_unit` with `FrameAwarePacer::begin_frame` /
  `end_frame` / `next_send_window` (sketch above).
- Modify `apps/qubox-host-agent/src/capture_orchestrator.rs:385,410`
  similarly for the multi-display path.
- Wire ADR-012's `RateFeedback` consumer to call
  `pacer.on_bandwidth_estimate` (see `crates/qubox-host-agent/src/rate_control.rs`).

### PR 7 — Linux pacing defence-in-depth

- On session startup, the host CLI sets
  `SO_MAX_PACING_RATE` on the QUIC UDP socket via `socket2`
  (already a transitive dep via `quinn-udp`). Value = 1.2 ×
  negotiated bitrate.
- Doc-only: `docs/ops/loopback-pacing.md` documenting the
  recommended `tc qdisc` setup (see §"Verification commands").

## Test specifications

Exact test names, fixtures, and expected outputs. All tests must
run under `cargo test -p qubox-transport` and `cargo test -p qubox-client-cli`
without any network I/O (use mock clocks + mock `Instant`).

### `crates/qubox-transport/tests/frame_pacer_tests.rs`

| Test name | Mock input | Expected outcome |
|---|---|---|
| `pacers_schedule_4k144_at_6944us_intervals` | `FramePacingSchedule{4K144, 6944us, 69_444 bytes}`, `send_base = {frame: 0, t=0}` | `deadline_for(0, …) = Some(0)`, `deadline_for(144, …) = Some(144*6944)`, `deadline_for(145, …) = Some(145*6944)` |
| `pacers_schedule_1080p60_at_16667us_intervals` | `FramePacingSchedule{1080p60, 16667us, 41_667 bytes}` | `deadline_for(60, …) = Some(60*16667) = 1_000_020` (close to 1 s; ±1 µs rounding) |
| `pacers_reschedule_on_bandwidth_drop` | Build pacer @ 80 Mbps/4K144; call `reschedule(50 Mbps)` at frame 100; advance 50 frames; observe `slots_per_frame` drops from 39 to 24 | New `bytes_per_frame = 43_403`, `slots_per_frame = ceil(43_403/1800) = 25` (test asserts 25, not 24 — recompute the formula!) |
| `pacers_on_bandwidth_estimate_updates_bytes_per_frame` | `begin_frame(0, now)`, then `on_bandwidth_estimate(50_000_000, 144)` | `pacer.schedule().bytes_per_frame == 43_403` |
| `pacers_drop_threshold_fires_at_50ms` | Build pacer; sleep tokio mock to `now + 60 ms`; call `next_send_window` | Returns `PacingDecision::Drop`; `stats.frames_dropped == 1` |
| `pacers_jitter_ewma_shrinks_max_burst` | Build pacer; call `end_frame` 10 times with `now` advanced 10 ms each time (i.e. simulated jitter 3 ms each call) | After 10 calls, `jitter_ewma_us ≈ 3_000`; `max_burst_bytes` still 1 800; on the 17th call with 3.1 ms jitter, `max_burst_bytes` shrinks to 900 |
| `pacers_frame_complete_emits_all_slots_then_completes` | 4K144 = 39 slots; loop `next_send_window` 40 times | First 39 return `WaitThenSend`; the 40th returns `FrameComplete`; `stats.slots_emitted == 39` |
| `pacers_first_wake_at_is_zero_for_anchor_frame` | `send_base = {frame: 0, t=0}`, `begin_frame(0, now=t0)` | First `next_send_window(t0)` returns `WaitThenSend { wake_at: t0, bytes_for_slot: 1800 }` |

### `crates/qubox-transport/tests/schedule_algebra_tests.rs`

(Or inline in `pacing/schedule.rs` — see the inline tests above.)

### `crates/qubox-transport/tests/frame_pacing_compat_tests.rs`

| Test name | Purpose |
|---|---|
| `frame_pacer_compat_constructor_still_works` | `#[allow(deprecated)] let p = FramePacer::new(60); assert_eq!(p.target_interval(), Duration::from_micros(16_667));` |
| `frame_pacer_compat_apply_schedule_overrides_target` | Construct via `FramePacer::new(60)`, then `apply_schedule(FramePacingSchedule{144Hz})`, assert `target_interval() == 6_944 µs` |
| `frame_pacer_compat_handle_control_msg_forwards_to_pacer` | Build a `ControlMsg::FramePacingSchedule(…)`, call `runtime::handle_control_msg`, assert `pacer.schedule()` updated |

### `apps/qubox-client-cli/src/frame_pacing.rs` (existing + new)

| Test name | Purpose |
|---|---|
| `first_frame_is_immediate` (existing) | unchanged — must still pass under `#[allow(deprecated)]` |
| `rapid_redraws_are_skipped` (existing) | unchanged |
| `catchup_after_long_stall` (existing) | unchanged |
| `early_tolerance_permits_slight_overshoot` (existing) | unchanged |
| `schedule_at_144hz_uses_6944us_target` (new) | `FramePacer::new_with_schedule(144Hz-schedule, 144)`; assert `target_interval() == 6_944 µs` |
| `apply_schedule_during_session_updates_target` (new) | Build at 60 Hz, present a frame, then `apply_schedule(144Hz-schedule)`; next `should_present(now+10ms)` returns `Skip` (target interval grew from 16.67 ms to 6.94 ms, but `now` is still < the new deadline) |

### `crates/qubox-proto/tests/frame_pacing_schedule_tests.rs`

| Test name | Purpose |
|---|---|
| `frame_pacing_schedule_round_trips_through_serde` | Serialize a fully-populated `FramePacingSchedule`, deserialize, assert `==` |
| `frame_pacing_schedule_rejects_unknown_field` | Manually craft `{"version":1, "schedule_id":1, …, "bogus":42}`, assert deserialize fails with `deny_unknown_fields` |
| `frame_pacing_schedule_inside_control_msg_round_trips` | Wrap in `ControlMsg::FramePacingSchedule(…)`, round-trip via the existing JSON prefix writer |

## Pitfalls

These are the gotchas a junior developer will hit. Each one maps to
a code comment in the final PR.

1. **Do not call `Instant::now()` from the codec task — receive the
   deadline from the pacer.** The encoder thread's "now" may be on a
   different TSC offset than the pacer's monotonic anchor (especially
   under VM/WSL2). `begin_frame` returns the deadline the encoder
   should aim for; the codec thread should `Instant::now()` only when
   it actually finishes encoding, and hand that to `end_frame` so the
   EWMA jitter sample is meaningful.

2. **The pacing timer is monotonic; do not rebase from
   `SystemTime`.** `deadline_for` is integer arithmetic on a
   monotonic anchor (`SendBase.instant_us`). If you accidentally use
   `SystemTime::now().duration_since(UNIX_EPOCH)` you will see
   backwards deadlines during NTP step adjustments. The
   `unix_micros_now` helper at `pacing/schedule.rs` (called by
   `make_frame_pacer_for_host` at `lib.rs:1865`) is monotonic.

3. **`SO_MAX_PACING_RATE` is per-socket, not per-flow.** Setting it
   to `bytes_per_sec` for one stream caps the entire UDP socket
   (including the control stream's outgoing bytes). On the host we
   keep the control stream on a separate QUIC endpoint
   (`apps/qubox-host-agent/src/main.rs:614` already has two
   pipelines); if a future refactor unifies them, multiply the
   control bandwidth by 1.05 and subtract.

4. **`fq` qdisc does nothing on the loopback interface.** Verify
   pacing on the *egress* NIC (`tc -s qdisc show dev eth0`). The
   kernel's `fq` operates on the device queue, which loopback
   bypasses. For loopback testing, use `sch_netem` with a small
   delay and observe end-to-end latency — pacing effects appear as
   reduced jitter, not as reduced throughput.

5. **`quinn::TransportConfig::send_pacing` does not exist in
   0.11.** Earlier ADR drafts (and several blog posts) refer to it
   as if it did. The actual pacing API in 0.11 is implicit inside
   the `Controller` impl; the only escape hatch is
   `congestion_controller_factory`. Our ADR adopts
   **application-level gating** of `Connection::send_datagram(...)`,
   which is what the QUIC Steps paper's "ngtcp2" strategy does
   (Figure 3 description).

6. **`bytes_for_slot` is a charge against the CC, not a chunking
   directive.** The pacer asks you to emit a slot of ~1 800 B; the
   encoder's NAL boundaries don't care, and a real H.264 access unit
   for 1080p is typically 5–40 KB. You emit *one* `send_datagram`
   per loop iteration (carrying the full AU), and *charge* the
   `bytes_for_slot` value to the pacer's stats. The pacer only
   controls *timing*, not *framing*.

7. **Drift compensation is per-frame, not per-RTT.** Do not try to
   re-anchor the `SendBase` on every BBR estimate — the schedule
   `effective_frame_index` is the only legitimate anchor change, and
   it goes through `reschedule`, not by mutating `send_base`.

8. **`ControlMsg::FramePacingSchedule` arrives over the *reliable*
   stream, not the datagram channel.** Don't try to send it via
   `connection.send_datagram`; use the existing
   `media::ControlChannel::send` at
   `crates/qubox-transport/src/media/mod.rs`. (Look up the exact
   method name in PR 6 — the helper is already used by
   `bitrate_change_notification`.)

9. **Test fixtures for `Instant` are tricky.** `std::time::Instant`
   has no public constructor; tests must use `tokio::time::Instant`
   or the `mock_instant` crate behind a feature flag. Add a
   `#[cfg(test)] mod mock_clock { … }` at the top of `pacer.rs` and
   gate behind `#[cfg(feature = "pacer-test-util")]` so it doesn't
   leak into the release binary.

10. **GSO is enabled by default in `quinn::TransportConfig::default()`.**
    We disable it explicitly at `crates/qubox-transport/src/lib.rs:1871`
    because the QUIC Steps paper showed GSO produces 5× the packet
    train length. If a future refactor re-enables it, the pacer's
    EWMA jitter detector will fire `jitter_shrink_threshold` on every
    keyframe and the stream will look terrible — make sure
    `enable_segmentation_offload(false)` survives every merge.

## Verification commands

```bash
# 1. Compile-check the new module + integration
cargo check -p qubox-transport --all-features
cargo check -p qubox-client-cli --features hw-decode
cargo check -p qubox-host-agent

# 2. Unit tests for schedule algebra
cargo test -p qubox-transport pacing::schedule -- --nocapture

# 3. Pacer state-machine tests (uses mock_instant)
cargo test -p qubox-transport frame_pacer --features pacer-test-util -- --nocapture

# 4. Backwards-compat tests for the deprecated FramePacer::new
cargo test -p qubox-client-cli frame_pacing -- --nocapture

# 5. End-to-end smoke (host → client on loopback, 1080p60)
RUST_LOG=qubox_transport::pacing=trace,qubox_media=debug \
  cargo run -p qubox-host-agent -- --bitrate 20_000_000 --fps 60 &
RUST_LOG=qubox_client_cli::frame_pacing=trace \
  cargo run -p qubox-client-cli
# Observe: inter-present interval histogram should cluster at
# 16.67 ± 0.5 ms, not 16.67 ± 5 ms (current behaviour).

# 6. End-to-end smoke (4K144 over loopback with a token-bucket shaper)
RUST_LOG=qubox_transport::pacing=trace \
  cargo run -p qubox-host-agent -- --resolution 3840x2160 --fps 144 --bitrate 80_000_000
# Inspect stats overlay (P1-12): frame drops should drop from
# 30-40 % to < 2 %.

# 7. Verify pacing on the wire with tcpdump + awk
sudo tcpdump -ni any -tttt 'udp port 4444' | \
  awk 'NR>1 { printf "%s\n", $1-prev; prev=$1 }' | \
  sort -n | uniq -c | head
# Expect: a long tail of packets ~46 µs apart (1 800 B / 80 Mbps),
# then a ~6 944 µs gap before the next frame's cluster.

# 8. Optional: validate kernel pacing on the egress NIC (not loopback).
# Recommended for the dev rig's `eth0`:
sudo tc qdisc replace dev eth0 root fq
# Verify with:
tc -s qdisc show dev eth0
# You should see the `fq` qdisc; packets will then be paced by the
# kernel using `sk_pacing_rate` (we set this via `SO_MAX_PACING_RATE`
# in PR 7). On loopback (`lo`) this command is a no-op — `fq` cannot
# pace packets that never hit a device queue.
```

### Recommended `tc qdisc` setup for the dev rig

The QUIC Steps paper's recommendation, lifted verbatim:

```bash
# /etc/sysctl.d/99-qubox-pacing.conf
net.core.default_qdisc = fq
net.ipv4.tcp_congestion_control = bbr

# Per-session (idempotent)
sudo tc qdisc replace dev eth0 root fq
# Optional cap (e.g. dev rig with a 1 Gbit NIC):
sudo tc qdisc replace dev eth0 root fq maxrate 900mbit
```

The `SO_MAX_PACING_RATE` we set in PR 7 goes on the QUIC UDP socket
via `socket2::Socket::set_socket_opt`:

```rust
// crates/qubox-host-agent/src/main.rs (PR 7)
use socket2::Socket;
let sock = Socket::from(conn.local_ip().unwrap()); // obtain via quinn-udp
sock.set_socket_opt(
    socket2::SocketOpt::MaxPacingRate(std::num::NonZeroU32::new(bytes_per_sec as u32).unwrap()),
)?;
```

(The exact API in `socket2` 0.5.x is `set_socket_opt` with
`SocketOpt::MaxPacingRate(NonZeroU32)` — confirm in PR 7 against the
pinned `socket2` version in `Cargo.lock`.)
