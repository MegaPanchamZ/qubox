# P0-4: Adaptive Bitrate (Latency-Based Rate Controller)

Status: **complete** (commits `b1cfb00`, `3aa4eb5`; PR https://github.com/MegaPanchamZ/qubox/pull/1). `GccRateController` (7/7 unit tests) is wired to the host's 4Hz `ControlChannel` → re-spawn ffmpeg with new `-b:v` (coalesced to ≤1Hz to avoid subprocess thrash). NVENC runtime bitrate set is a follow-up when the `--encoder nvenc` path lands.
Owner: `apps/host-agent` (encoder pipeline rate controller), with a new `rate-control` module.
Depends on: P0-2 (datagram media path; needs the control channel), the existing `WireAccessUnit` wire format.
Blockers: none. Pure Rust, no HW-specific code in the rate controller itself.

## Goal

Add an adaptive bitrate controller on the host that adjusts the ffmpeg encoder's `-b:v` and (when possible) `-r` based on **latency** and **loss** feedback from the client, with a Parsec-class "panic mode" for high-loss conditions. Replace the current static-bitrate model. Target: maintain <50 ms one-way queuing delay, avoid bufferbloat, react to loss in <1 second, recover to user's max in <10 seconds.

## Research Summary

### Algorithm choice: GCC (WebRTC delay-based) for game streaming

**Why delay-based, not throughput-based**:
- **Throughput-based** (AIMD / CUBIC / BBR): built for file transfer; tolerates packet loss as "the network is just slow" and builds a large buffer (bufferbloat) to maximize utilization. Wrong for game streaming — the user's perceived latency spikes to seconds during a single dropped packet.
- **Delay-based** (GCC, SCReAM): uses **per-packet one-way delay** as the primary signal. A growing delay means the network buffer is filling up, so the sender is over-running capacity; a stable delay means capacity is well-utilized. Reacts in ~1 round-trip instead of waiting for loss to cross 10%.

**GCC vs SCReAM** for our use case:

| Trait | GCC (WebRTC) | SCReAM (RFC 8298) |
|-------|--------------|---------------------|
| Form | Rate-based (target bitrate + pacer) | Window-based (cwnd + self-clocking) |
| ECN | Not used in current WebRTC | Explicitly supported (draft-ietf-rmcat-scream-cc) |
| Tuning bias | Higher utilization, more queueing delay | Lower utilization, tighter delay |
| Best for | Video telephony, browser parity | Low-latency, ECN/L4S networks |
| Game streaming fit | Good (Parsec-class latency) | Good (tighter latency, lower bitrate) |

**Choice: GCC-style rate controller for the first release.** Rationale: (1) it's the most-deployed algorithm in interactive real-time media and our users are likely on the same networks as WebRTC; (2) we don't yet have ECN feedback wired through QUIC; (3) the bitrate-driven model maps cleanly to `-b:v` changes in ffmpeg HW encoders; (4) the SCReAM window model is harder to integrate with the encoder's rate-control (HW encoders want a bitrate, not a cwnd).

### The GCC algorithm (the two-controller version from libwebrtc)

The original GCC has two coupled controllers:

1. **Delay-based controller**: tracks the per-packet one-way delay (OWD) gradient. The minimum OWD seen over a window is the "base delay" (path propagation, no queuing). The current OWD minus the base is the **queuing delay**. The gradient (change in queuing delay) drives a state machine:
   - **Overuse** (gradient > 12-15 ms): multiplicative decrease 0.85x.
   - **Normal**: additive increase 1.05x per RTT.
   - **Underuse** (gradient < -5 ms): additive increase (more aggressive).
2. **Loss-based controller**: coarse, runs every ~1 s.
   - Loss > 10%: target bitrate *= 0.5.
   - Loss 2-10%: hold.
   - Loss < 2%: target bitrate *= 1.08.

Final send rate = **min(delay-based, loss-based)**.

### WebRTC's actual usage (2024-2026)

- WebRTC's GCC in Chrome/Firefox uses **TWCC (Transport-Wide Congestion Control, RFC 8698)** feedback — per-packet send/recv timestamps on the receiver side, fed back via RTCP.
- The `webrtc::modules::congestion_controller` code in `libwebrtc` is the reference implementation. It's 5,000+ lines of C++ but the core state machine is ~200 lines.
- WebRTC has been considering SCReAM v2 as an alternative for ECN (webrtc issue #447037083).

### Encoder integration (HW encoders, mid-stream `-b:v` changes)

The key constraint is **how cheaply the encoder can change its target bitrate**:

| Encoder          | Per-frame `-b:v` change | Re-init cost | Game-streaming recommendation |
|------------------|--------------------------|----------------|-------------------------------|
| h264_nvenc / hevc_nvenc / av1_nvenc | Yes (in CBR mode, bitrate is the rate controller's setpoint) | None | Change `-b:v` and `-maxrate` together; keep `-bufsize` constant |
| h264_vaapi / hevc_vaapi / av1_vaapi | Yes (some Mesa versions) | None | Same; AV1 VAAPI on Mesa < 23.0 requires re-init |
| h264_qsv / hevc_qsv / av1_qsv | No (must re-open) | 20-50 ms | Coalesce changes to ≤1 Hz to amortize |
| h264_amf / hevc_amf / av1_amf | Yes (AMF SDK) | None | Change `-b:v` and `-rc cbr` together |
| h264_videotoolbox / hevc_videotoolbox | Yes (via `setBitrate` callback) | None | Use the encoder's `setBitrate` API |
| libx264 (software) | Yes (in CBR mode) | None | The current path |

**Plan**: send `-b:v` + `-maxrate` changes via stdin to the ffmpeg subprocess (current architecture). For HW encoders that don't support per-frame bitrate changes, send a "restart encoder" signal and re-open. The new `plan_ffmpeg_args` in P0-1 already takes the target bitrate as a parameter.

**Framerate adaptation** is harder: H.264/HEVC don't support mid-stream `fps` changes without an IDR. AV1 with temporal SVC *does*. Defer framerate adaptation to a follow-up; do bitrate-only for the first release.

### Stability (oscillations, smoothing, rate ping-pong)

- **Smoothing interval**: the delay-based controller smooths the gradient with `alpha = 0.9` (10% weight on new sample). Too much smoothing → reacts too slowly; too little → oscillates.
- **Bound the reaction time**: enforce a minimum 250 ms between bitrate changes. The pacer emits at the new rate, but we hold the bitrate target for 250 ms before accepting the next change.
- **Bursty loss on wifi** (e.g. beacon collision every 100 ms): the loss-based controller would oscillate. Solution: the loss-based controller only triggers on a 1-second rolling loss rate, not a per-100-ms window.
- **Probe ramp**: start at 1 Mbps (lowest playable), add 200 Kbps every 1 second until either the user's max or the first loss / overuse signal. This is Parsec's "ramp up" behavior.

### Recent research (2024-2026)

- **Learned rate controllers** (Pensieve, Comyco, OnRL): use deep RL to pick bitrate ladders. Not directly applicable to live streaming (they need a pre-encoded ladder), but the direction matters for ABR over HTTP — **not** for our real-time CBR case.
- **Media-over-QUIC (MoQ)**: draft-ietf-moq-transport will include rate feedback hooks. Our design maps cleanly to MoQ's `Track` + `Object` model. Migration is local to `media-path`.
- **L4S / ECN**: low-latency queuing is being deployed in residential routers (Comcast, British Telecom). SCReAM-style ECN reaction is the right answer for L4S networks. Our first release doesn't use ECN; add it as a follow-up.

### Rust rate controller (sketch)

```rust
pub struct GccRateController {
    target_bitrate_bps: u32,
    min_bitrate_bps: u32,
    max_bitrate_bps: u32,
    base_delay_ms: f64,
    smoothed_gradient: f64,
    overuse: OveruseState,
    loss_rate: f64,
    last_change: Instant,
    in_probe: bool,
    probe_started: Instant,
    started_at: Instant,
}

#[derive(Copy, Clone, PartialEq)]
pub enum OveruseState { Normal, Overuse, Underuse }

pub struct RateFeedback {
    pub rtt_ms: u16,
    pub loss_x1000: u16,           // loss rate * 1000 (e.g. 12 = 1.2%)
    pub jitter_ms: u16,
    pub ecn: Option<EcnCodepoint>,
    pub one_way_delay_ms: f64,
    pub one_way_delay_min_ms: f64,  // base delay
}

impl GccRateController {
    pub fn on_feedback(&mut self, fb: RateFeedback, now: Instant) -> Option<u32> {
        // 1) Update delay-based state
        let q_delay = (fb.one_way_delay_ms - fb.one_way_delay_min_ms).max(0.0);
        let gradient = q_delay - self.smoothed_gradient;
        self.smoothed_gradient = 0.9 * self.smoothed_gradient + 0.1 * gradient;
        self.overuse = if self.smoothed_gradient > 12.0 { OveruseState::Overuse }
                       else if self.smoothed_gradient < -5.0 { OveruseState::Underuse }
                       else { OveruseState::Normal };

        // 2) Update loss
        self.loss_rate = f64::from(fb.loss_x1000) / 1000.0;

        // 3) Probe / ramp-up
        if self.in_probe {
            if now - self.probe_started > Duration::from_secs(1) {
                self.target_bitrate_bps = (self.target_bitrate_bps + 200_000).min(self.max_bitrate_bps);
                self.probe_started = now;
            }
            if self.overuse == OveruseState::Overuse || self.loss_rate > 0.02 {
                self.in_probe = false;
            }
        }

        // 4) Panic mode
        if fb.one_way_delay_ms > 200.0 || self.loss_rate > 0.20 {
            self.target_bitrate_bps = (self.target_bitrate_bps / 4).max(self.min_bitrate_bps);
            self.in_probe = false;
            self.last_change = now;
            return Some(self.target_bitrate_bps);
        }

        // 5) Bounded reaction time
        if now - self.last_change < Duration::from_millis(250) {
            return None;
        }

        // 6) Loss-based controller
        if self.loss_rate > 0.10 { self.target_bitrate_bps = (self.target_bitrate_bps / 2).max(self.min_bitrate_bps); }
        else if self.loss_rate < 0.02 && self.overuse == OveruseState::Normal {
            self.target_bitrate_bps = (self.target_bitrate_bps * 105 / 100).min(self.max_bitrate_bps);
        }

        // 7) Delay-based controller
        if self.overuse == OveruseState::Overuse {
            self.target_bitrate_bps = (self.target_bitrate_bps * 85 / 100).max(self.min_bitrate_bps);
        }

        self.target_bitrate_bps = self.target_bitrate_bps.clamp(self.min_bitrate_bps, self.max_bitrate_bps);
        self.last_change = now;
        Some(self.target_bitrate_bps)
    }

    pub fn panic(&mut self) {
        self.target_bitrate_bps = self.min_bitrate_bps;
        self.in_probe = false;
    }
}
```

### Game-streaming tuning

- **Target latency**: 30-50 ms over the wire (between sender and receiver). The capture-to-display budget is <60 ms; the network slice is ~30-40 ms; the jitter buffer (P0-2) adds 5-10 ms; the rest is encode + decode + present.
- **Panic mode**: if one-way delay > 200 ms or loss > 20%, immediately drop to 1 Mbps. This is Parsec's behavior; the rationale is that the user's perceptual experience is worse with high-bitrate stutter than with low-bitrate smoothness.
- **Burst mode**: if loss < 1% and delay is stable for 5 seconds, ramp the bitrate up by 1 Mbps per second until either the user's max or a signal.
- **Fast start**: start at 1 Mbps. Rationale: starting at the user's max (e.g. 20 Mbps) and waiting for loss to back off is slow (3-5 seconds of bad quality). Parsec's ramp approach is faster.
- **Min bitrate**: 1 Mbps (H.264 720p30 is playable; below this, the user should switch to a lower resolution). Configurable.

## Implementation Plan

### Step 1: New `rate-control` module in host-agent

`apps/host-agent/src/rate_control/mod.rs`:
- `pub mod gcc;` — `GccRateController`.
- `pub mod feedback;` — `RateFeedback` type and serde derive (matches the wire format).
- `pub mod panic;` — `PanicMode` detector.
- `pub mod probe;` — startup probe state.

`apps/host-agent/src/rate_control/gcc.rs`:
- The `GccRateController` from the sketch above.
- All state machine logic; no I/O.
- `pub fn on_feedback(&mut self, fb: RateFeedback, now: Instant) -> Option<u32>` returns the new target bitrate if it changed.
- Unit tests: simulated feedback sequences that should trigger each state transition.

### Step 2: Wire format for RateFeedback

`crates/qubox-proto/src/lib.rs` — `RateFeedback` already exists as the wire type. Confirm it carries:
- `rtt_ms: u16`
- `loss_x1000: u16` (loss * 1000)
- `jitter_ms: u16`
- `one_way_delay_ms: f32` (current sample, in ms)
- `one_way_delay_min_ms: f32` (base delay, the minimum seen in the session)

Client-side: the receiver tracks the base delay (running minimum over a 10-second window) and the current delay. Both are sent in every `RateFeedback` message.

### Step 3: Client-side one-way delay tracking

`apps/client-cli/src/media/feedback.rs` (new):
- `pub struct OwDelayTracker { base_min_ms: f64, current_ms: f64, ewma_ms: f64 }`.
- `pub fn on_packet_received(&mut self, send_ts_ms: f64, recv_ts_ms: f64)`: computes OWD, updates the running min, smooths current.
- `pub fn snapshot(&self) -> (f64, f64)`: returns `(current_ms, base_min_ms)`.
- The client's wall clock is used for the receive timestamp; the sender's clock for the send timestamp (assumes rough clock sync via the QUIC handshake — `Connection::rtt()` is the QUIC RTT, used to detect clock skew and correct).

### Step 4: Encoder bitrate update (host-side)

`apps/host-agent/src/encoder/pipeline.rs`:
- The encoder subprocess receives `-b:v` and `-maxrate` via its command-line arguments. To change them mid-stream, we send a newline-delimited command on the encoder's stdin: `B<bitrate>\n` and `M<maxrate>\n`. The wrapper script (a small shell or Rust) parses these and sends `SIGUSR1` to the encoder to reload.
- Alternatively: kill and re-spawn the encoder with the new args. This costs ~50 ms but is simpler. ffmpeg accepts a `-re` re-read of params via `av_opt_set` on the live context, but the CLI doesn't expose this. The re-spawn approach is the right first cut.
- The rate controller publishes the new target bitrate via a `tokio::sync::watch::Sender<u32>`. The encoder task subscribes and re-spawns the subprocess when the value changes (and the encoder is HW-restart-incompatible).

### Step 5: Panic mode integration

`apps/host-agent/src/rate_control/panic.rs`:
- `pub fn detect_panic(fb: &RateFeedback, history: &[RateFeedback]) -> bool` — checks one-way delay, loss, ECN, jitter against thresholds.
- When panic is detected, the host immediately drops to the user's `min_bitrate` (default 1 Mbps) and emits a `KEYFRAME_REQUEST` NACK so the client doesn't see frozen frames.

### Step 6: Stats surface

`apps/host-agent/src/rate_control/stats.rs`:
- `pub struct RateControllerStats { target_bitrate_bps: u32, smoothed_gradient: f64, overuse: OveruseState, loss_rate: f64, in_probe: bool, in_panic: bool }`.
- Exposed via the `bp debug rate` CLI subcommand and the stats overlay (P1-12).

### Step 7: Tests

- Unit test: `GccRateController` with a simulated feedback sequence (delay growing 0 → 30 ms over 5 seconds): expect bitrate to drop to ~50% of the start.
- Unit test: panic mode triggers when `one_way_delay_ms > 200.0`.
- Unit test: probe ramps from 1 Mbps to max over 10 seconds in the absence of loss.
- Integration test: e2e on Xephyr, with a 1% loss shaper, verify the bitrate stabilizes around 80% of the channel capacity within 30 seconds.

## Risks and Open Questions

- **Encoder restart on bitrate change**: re-spawning the ffmpeg subprocess costs ~50 ms and resets the rate controller's idea of "current bitrate." Some HW encoders (NVENC, AMF) accept per-frame bitrate changes; we should pick the cheapest path per encoder family. The `plan_ffmpeg_args` module from P0-1 can return a `bitrate_change_strategy` hint (`PerFrame`, `SubprocessRestart`).
- **Clock sync**: we need the client's wall clock and the host's wall clock to be roughly synchronized (within ~50 ms) to compute OWD correctly. QUIC's `Connection::rtt()` gives the RTT; the half-RTT is a lower bound on the clock skew. Use NTP-style correction: each `RateFeedback` carries the receiver's `recv_ts` and the sender's most recent `send_ts` for one packet; the host computes the clock offset and applies it.
- **Bursty loss on wifi**: wifi beacon collisions cause 100-200 ms loss spikes every ~100 ms. Our 1-second rolling window for the loss-based controller handles this, but the delay-based controller can be fooled by a single delay spike. Use a median filter on the per-packet delay, not the mean.
- **Feedback frequency**: 4 Hz is the rate feedback cadence. Too slow (1 Hz) and the controller lags on bursty loss; too fast (10 Hz) and the rate oscillates. 4 Hz matches WebRTC's TWCC report frequency.
- **Encoder doesn't reach the target bitrate**: ffmpeg's rate controller is not perfect; the actual bitrate may be ±15% of `-b:v` for VBR and ±5% for CBR. The host's measured outgoing bitrate (from the encoder's stats) is a more accurate signal than the requested `-b:v`. Expose this in `RateFeedback` as `actual_bitrate_bps`.
- **One stream per client vs. shared media**: when multiple clients connect to one host (multi-client mode), each has its own rate controller. They may compete for the encoder's bitrate; the host's encoder is shared and its total bitrate is the sum of the per-client targets.
- **STUN / clock skew correction**: the QUIC RTT is not a clock-skew measurement; it's the time between sending a packet and receiving the ack. For OWD, we need sender and receiver clocks aligned. The simplest correction: the host periodically (every 5 s) sends its wall clock; the client measures the QUIC RTT and adjusts.
- **SCReAM migration**: if L4S / ECN becomes common in 2026-2027, switch to SCReAM. The wire format is unchanged; only `GccRateController` is replaced.

## References

- RFC 8698: TWCC (Transport-Wide Congestion Control).
- draft-ietf-rmcat-gcc-02: GCC algorithm.
- draft-ietf-rmcat-scream-cc / RFC 8298: SCReAM.
- WebRTC source: webrtc.org/src/modules/congestion_controller.
- WebRTC hacks: GCC probing: https://webrtchacks.com/probing-webrtc-bandwidth-probing-why-and-how-in-gcc/
- WebRTC for the curious, Chapter 6 (media communication): https://webrtcforthecurious.com/docs/06-media-communication/
- WebRTC issue 447037083: adapt to ECN feedback (mentions SCReAM v2).
- Thesis on NADA, GCC, SCReAM comparison: https://lup.lub.lu.se/student-papers/record/9090908/file/9090916.pdf
- WebRTC performance: https://wimnet.ee.columbia.edu/wp-content/uploads/2017/10/WebRTC-Performance.pdf
- Perplexity research, 2026-07-02: GCC vs SCReAM, encoder bitrate change, game-streaming tuning.
