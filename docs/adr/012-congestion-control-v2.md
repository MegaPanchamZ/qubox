# ADR-012 Congestion Control: SCReAM (default) + BBR v3 + OCC

## Status

Proposed. Branch: `feature/adr-012-congestion-control-v2`. Based on
`main` after commit `47585ea`. Builds on ADR-011 (QUIC v2 transport
params + ACK-Frequency). Replaces the GCC controller shipped at P0-04
(`research/roadmap/p0-04-adaptive-bitrate.md` and the
`GccRateController` at `apps/qubox-host-agent/src/rate_control.rs:84-99`)
with a **multi-algorithm, network-class-routed** congestion controller.

> **Author's note for the reviewer.** This rewrite is grounded in
> direct source inspection of the pinned `quinn` 0.11.11 /
> `quinn-proto` 0.11.15 in our `Cargo.lock` (lines 5519 / 5539) plus
> five Perplexity research passes (one rate-limited and re-tried)
> covering SCReAM, BBR v3, OCC, Linux cellular telemetry, and
> measurement studies. The original ADR's premise — "quinn-proto ships
> BBR v3, drop it in" — does **not** hold for our pinned versions
> (see Pitfall P1 and Decision §3). Treat every "verified by
> source/docs" claim below as load-bearing.

---

## Context

The current rate controller is a port of the Google Congestion Control
heuristics from WebRTC's `webrtc::modules/congestion_controller/gcc`
onto QUIC datagrams. The implementation lives in
`apps/qubox-host-agent/src/rate_control.rs`:

- `GccConfig` at `:46-64` (sensible defaults: 300 kbps min, 50 Mbps max,
  `panic_threshold_bps = 1.5 Mbps`)
- `GccRateController::new` at `:102-114`
- `on_observation(owd_ms, loss_x1000, rtt, now)` at `:138-229` — the
  main loop. Drives bitrate via the EWMA OWD gradient + multiplicative
  decrease on overuse.
- Tests in `:260-513` verify the multiplicative decrease, panic drop,
  and the gradient classifier.

GCC was chosen for P0-04 because it ships with WebRTC and is what every
browser-based remote-desktop competitor uses. Three problems have
surfaced:

1. **GCC is a delay-based controller** tuned for WebRTC's RTP/RTCP
   feedback channel. Our QUIC datagrams have finer-grained loss
   reporting (QUIC ACK frames include per-ECN-mark + per-sent-packet
   state) and we run over `quinn` which exposes the underlying
   `CongestionController` trait (cf. `quinn-proto`'s Cubic / BBR
   implementations). GCC's "inter-arrival delta" trick is redundant
   when we already have RTT and explicit loss.
2. **GCC underutilizes wired broadband**: SCReAM (RFC 8298 and the
   successor draft `draft-johansson-ccwg-rfc8298bis-screamv2-05`,
   Oct 2025) is delay-based but with an L4S / ECN-capable path and a
   media-aware mode that biases towards stable average bitrate. BBR v3
   (Cardwell et al., Google 2023, "BBR v3 — Achieving Flow-Rate
   Fairness and Stability") is provably better than Cubic on broadband
   with shallow buffers.
3. **Cross-layer awareness**: the OCC paper (arXiv:2604.22383, Zhuang
   et al., CUHK 2026 — see References) uses per-radio LTE/5G L1
   metrics (MCS, PRB utilisation, CQI, BLER) to anticipate loss before
   TCP/QUIC observes it. We already have hooks for L1 telemetry via
   `crates/qubox-platform` on Linux/Android.

---

## Decision

### 1. Three-controller fleet + router

We replace `GccRateController` with a `CongestionRouter` that owns one
of four `RateController` trait objects:

```rust
// apps/qubox-host-agent/src/rate_control/mod.rs (NEW top-level module)
//
// Hierarchy (read bottom-up):
//
//   CongestionRouter                ── picks one impl per session
//   ├── RateController (trait)      ── common surface, see §2
//   │   ├── ScreamRateController    ── default for Wireless / Unknown
//   │   ├── BbrV3RateController     ── wired broadband
//   │   ├── OccRateController       ── cellular with L1 metrics
//   │   └── LegacyGccRateController ── fall-back (renamed GccRateController)
//   └── NetworkClass (enum)         ── WiredBroadband | Wireless |
//                                     Cellular | Unknown
```

The router keeps the impl behind a `Box<dyn RateController>` so the
legacy controller can stay compiled while we add the new ones.

### 2. `RateController` trait

The new trait is **app-layer**: it consumes the same
`RateFeedback{ one_way_delay_ms: f32, loss_x1000: u16, rtt_ms: u16 }`
struct that `apps/qubox-host-agent/src/rate_feedback.rs:45-58` already
feeds to `GccRateController::on_observation`. This is intentional —
we are not removing the per-frame feedback loop; we are swapping the
algorithm that turns feedback into a target bitrate. The QUIC-level
`quinn::congestion::Controller` trait (whose signature we verified
directly against `~/.cargo/registry/.../quinn-proto-0.11.15/src/congestion.rs:17`
— `on_sent`, `on_ack`, `on_end_acks`, `on_congestion_event`,
`on_mtu_update`, `window()`, `clone_box()`, `initial_window()`,
`into_any()`) is left to the default `Cubic` for now (see Decision §3
for the BBR v3 story).

```rust
// apps/qubox-host-agent/src/rate_control/trait_def.rs
//
//! App-layer rate controller trait. All four algorithms implement
//! this; the CongestionRouter picks one per session.

use std::time::{Duration, Instant};

use crate::rate_control::telemetry::CongestionTelemetry;

/// What we currently know about the link. The router uses this to
/// pick an algorithm; the algorithm itself does *not* re-classify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkClass {
    /// Ethernet / fibre / cable with deterministic RTT. MTU ≥ 1500,
    /// stddev(owd_ms) ≤ 1 ms over a 100-sample probe.
    WiredBroadband,
    /// 802.11ac/ax/be, Bluetooth-tethered, generic "wireless".
    /// Default class — most home Wi-Fi ends up here.
    Wireless,
    /// LTE / 5G NR, with optional L1 metrics available.
    /// `has_l1_metrics: bool` is set when ModemManager exposes
    /// a `Modem.Signal` interface; if false, the router falls
    /// back to SCReAM even when `Cellular` is selected.
    Cellular { has_l1_metrics: bool },
    /// No classifier signal — first 100 samples not yet collected.
    Unknown,
}

/// What the user can force via `--congestion-controller=` and what
/// the router picks automatically when not overridden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CongestionAlgorithm {
    Scream,
    BbrV3,
    Occ,
    Gcc, // legacy fall-back; will be removed at 1.0 GA
}

pub trait RateController: Send {
    /// Feed one observation (called once per RateFeedback, ~4 Hz).
    /// Returns the new target encoder bitrate in bits per second.
    ///
    /// `owd_ms`            — one-way-delay trend in ms (already
    ///                       baseline-subtracted by the QUIC stack).
    /// `loss_x1000`        — loss fraction in parts-per-thousand
    ///                       (1000 == 100 %).
    /// `rtt`               — round-trip time from the QUIC ACK frame.
    /// `sent_bytes`        — bytes acked in this observation (used by
    ///                       BBR v3's bandwidth filter; pass 0 if
    ///                       the transport doesn't expose it).
    /// `now`               — monotonic clock.
    fn on_observation(
        &mut self,
        owd_ms: f64,
        loss_x1000: u16,
        rtt: Duration,
        sent_bytes: u64,
        now: Instant,
    ) -> u32;

    /// Optional hook for cellular L1 metrics (OCC only).
    /// Default impl is a no-op; SCReAM, BBR v3, GCC ignore it.
    fn on_l1_metric(&mut self, _metric: L1Metric) {}

    /// Snapshot the current state for telemetry. Must be cheap (no
    /// allocation, no locking). Called once per second by the router.
    fn snapshot(&self) -> CongestionTelemetry;

    /// Current target bitrate in bps (cached after `on_observation`).
    fn current_bitrate_bps(&self) -> u32;

    /// Algorithm name for tracing (`"scream"`, `"bbr-v3"`, `"occ"`,
    /// `"gcc-legacy"`).
    fn algorithm_name(&self) -> &'static str;
}

/// One sample of L1 telemetry from ModemManager's
/// `org.freedesktop.ModemManager1.Modem.Signal` interface.
///
/// We deliberately keep this small and unit-stable — the
/// `qubox-platform` crate owns the ModemManager bindings and
/// translates them into this enum (see Decision §6).
#[derive(Debug, Clone, Copy)]
pub enum L1Metric {
    /// Reference Signal Received Power, dBm (typ. −140 .. −44).
    Rsrp(f32),
    /// Reference Signal Received Quality, dB (typ. −20 .. −3).
    Rsrq(f32),
    /// Received Signal Strength Indicator, dBm.
    Rssi(f32),
    /// Signal-to-Interference-plus-Noise Ratio, dB.
    Sinr(f32),
    /// Block Error Rate, 0.0..=1.0.
    Bler(f32),
}
```

### 3. SCReAM as default for wireless / unknown

- **Default choice** whenever `network_class == Wireless` or
  `Unknown`. Also the fall-back for `Cellular` when L1 metrics
  disappear at runtime.
- **Algorithm**: SCReAM v2 (RFC 8298 + draft-johansson-ccwg-rfc8298bis-screamv2-05).
  - RFC 8298: <https://datatracker.ietf.org/doc/html/rfc8298>.
    qdelay target 50–100 ms; loss/ECN → instant CWND reduction.
  - Reference C++: <https://github.com/EricssonResearch/scream>
    (BSD-3-Clause, latest activity Jan 2025 per `version-history.md`).
  - Successor draft: <https://datatracker.ietf.org/doc/html/draft-johansson-ccwg-rfc8298bis-screamv2-05>
    (adds L4S/ECN proportional scaling; per the v2 paper, "queue
    delay reduced 16.17 %, throughput +93 %" vs RFC-8298 defaults
    with L4S enabled — <https://pmc.ncbi.nlm.nih.gov/articles/PMC10675070/>).

- **Rust implementation choice** (verified by searching crates.io):
  - **No maintained pure-Rust SCReAM crate exists.** Confirmed by
    crates.io searches for `scream`, `scream-rs`, `rfc8298`, `rmcat`.
    Only result is `scream_cypher` (an unrelated cipher toy) and
    `scram-rs` (SCRAM auth, not SCReAM).
  - The Pion Go project uses `CGO` to wrap the C++ reference impl
    (<https://github.com/pion/webrtc/discussions/2101>); there is no
    upstream Rust port.
  - **We port SCReAM v2 from scratch in pure Rust** under a new
    workspace crate `crates/qubox-scream/`. Reasons: (a) the C++
    reference is ~2 500 LOC, well-scoped; (b) BSD-3-Clause is
    attribution-only, so a clean-room port is fine; (c) FFI would
    require us to ship the C++ via a `cc` build script + Cargo
    feature flag on every platform, hurting cross-compilation.

- **Add to workspace `Cargo.toml`** (exact version pin — verified
  empty on crates.io, so we use a path dependency until first release):

  ```toml
  # Cargo.toml (workspace members)
  members = [
      …existing members…,
      "crates/qubox-scream",       # NEW — SCReAM v2 pure-Rust port
  ]

  # apps/qubox-host-agent/Cargo.toml (NEW dep)
  qubox-scream = { path = "../../crates/qubox-scream" }
  ```

- **Stub** (the intern implements this body — see Pitfall P2 for
  qdelay-target choice):

  ```rust
  // crates/qubox-scream/src/lib.rs
  use std::time::{Duration, Instant};
  use qubox_host_agent::rate_control::{
      CongestionTelemetry, L1Metric, RateController,
  };

  /// Pure-Rust port of SCReAM v2
  /// (draft-ietf-ccwg-rfc8298bis-screamv2-05 §4–§6).
  /// RFC 8298 link: https://datatracker.ietf.org/doc/html/rfc8298
  pub struct ScreamRateController {
      cfg: ScreamConfig,
      cwnd_bytes: u64,
      /// ms — RFC 8298 §4.2; v2 §5.1 keeps 50–100 ms range.
      qdelay_target_ms: f64,
      /// ms — low-passed estimate of one-way queueing delay.
      qdelay_ms_ewma: f64,
      /// Bytes acked over the last 5 s — used for the
      /// `max_bytes_in_flight` clamp.
      bytes_in_flight_5s: u64,
      last_observation: Option<Instant>,
      target_bitrate_bps: u32,
  }

  #[derive(Debug, Clone, Copy)]
  pub struct ScreamConfig {
      pub min_bitrate_bps: u32,
      pub max_bitrate_bps: u32,
      pub start_bitrate_bps: u32,
      /// ms — qdelay target. v2 default 50 ms; we use 60 ms.
      pub qdelay_target_ms: f64,
      /// EWMA alpha for qdelay. RFC 8298 §4.2: 0.1.
      pub qdelay_ewma_alpha: f64,
  }

  impl Default for ScreamConfig {
      fn default() -> Self {
          Self {
              min_bitrate_bps: 500_000,
              max_bitrate_bps: 50_000_000,
              start_bitrate_bps: 1_000_000,
              qdelay_target_ms: 60.0,
              qdelay_ewma_alpha: 0.1,
          }
      }
  }

  impl ScreamRateController {
      pub fn new(cfg: ScreamConfig) -> Self {
          Self {
              target_bitrate_bps: cfg.start_bitrate_bps,
              cfg,
              cwnd_bytes: 0,
              qdelay_ms_ewma: 0.0,
              bytes_in_flight_5s: 0,
              last_observation: None,
          }
      }
  }

  impl RateController for ScreamRateController {
      fn on_observation(
          &mut self,
          owd_ms: f64,
          loss_x1000: u16,
          _rtt: Duration,
          sent_bytes: u64,
          now: Instant,
      ) -> u32 {
          // TODO(intern): implement SCReAM v2 algorithm
          //
          // Pseudocode (matches RFC 8298 §4 + v2 draft §5):
          //   1. Update qdelay_ms_ewma with α = 0.1.
          //   2. If loss_x1000 > 20 (2 %) → cwnd *= 0.7  (RFC 8298 §4.4)
          //   3. Else if qdelay_ms_ewma > qdelay_target_ms → cwnd *= 0.95
          //   4. Else → cwnd += 1500 (MTU-sized additive increase)
          //   5. Clamp cwnd to [min_bitrate * RTT, max_bitrate * RTT]
          //   6. target_bitrate_bps = cwnd / srtt  (assume 100 ms srtt
          //      until we wire RTT in)
          //
          // Reference: §4 of
          // https://datatracker.ietf.org/doc/html/rfc8298 and
          // §5 of draft-johansson-ccwg-rfc8298bis-screamv2-05.
          let _ = (owd_ms, loss_x1000, sent_bytes, now);
          self.target_bitrate_bps
      }

      fn snapshot(&self) -> CongestionTelemetry {
          CongestionTelemetry {
              algorithm: self.algorithm_name(),
              target_bitrate_bps: self.target_bitrate_bps,
              cwnd_bytes: Some(self.cwnd_bytes),
              qdelay_ms: Some(self.qdelay_ms_ewma),
              ..Default::default()
          }
      }
      fn current_bitrate_bps(&self) -> u32 { self.target_bitrate_bps }
      fn algorithm_name(&self) -> &'static str { "scream-v2" }
  }
  ```

### 4. BBR v3 for wired broadband

**Critical finding from source inspection:** `quinn-proto` 0.11.15
ships a **BBR v1** controller (verified directly:
`~/.cargo/registry/.../quinn-proto-0.11.15/src/congestion/bbr/mod.rs`
has the v1 8-round `K_DEFAULT_HIGH_GAIN = 2.885 ≈ 2/ln(2)` pacing
cycle, `bbr_min_rtt_win_sec = 10`, `K_PROBE_RTT_TIME = 200 ms`, and
the source comment cites
`draft-cardwell-iccrg-bbr-congestion-control`, which is the BBR v1
IETF document, not v3). The `BbrConfig` struct exposes **one** knob:
`initial_window(bytes)` (lines 516–540). There is no
`probe_rtt_interval`, `probe_bw_gain`, `cwnd_gain`, or `drain_gain`
tunable. **BBR v3 is not available in our pinned quinn stack.**

The BBR v3 deltas (verified from `groups.google.com/g/bbr-dev` and
the IETF 119 Google slides):
- 4-phase `PROBE_BW` state machine (DOWN/CRUISE/REFILL/UP) with
  `cwnd_gain = 2.25` in `PROBE_UP` (vs v1's 2.0).
- `inflight_hi` / `inflight_lo` loss-recovery caps; target
  loss + ECN rate ≤ 2 % per RTT.
- `ProbeRTT` cadence ~5 s with vastly reduced throughput drop
  (v1 uses 10 s and drops cwnd to 4 packets for 200 ms).
- Continues to probe bandwidth after loss events (v2 was too
  conservative).

**Our path:** implement BBR v3 **as an app-layer controller**, not
against `quinn::congestion::Controller`. Reasons:
- The QUIC-level `Controller` trait is byte/cwnd-oriented; BBR v3
  needs a bandwidth filter, loss-rate cap, and RTT model that don't
  map cleanly onto `window()/on_congestion_event()` (which is loss-
  count oriented). Vendoring Google `quiche`'s `BbrSender.cc`
  (~2 000 LOC of subtle state machine) is more risk than reward.
- The app-layer BBR v3 consumes the same `RateFeedback` struct as
  the other algorithms and exposes a `target_bitrate_bps`. ADR-013
  (frame pacing) consumes this estimate anyway.
- We **leave the QUIC-level congestion controller at Cubic** for now;
  the app-layer BBR v3 caps the encoder bitrate, not the cwnd.

**Library versions (verified empty on crates.io for BBR v3 — we
add path deps for now):**

```toml
# Cargo.toml (workspace members)
members = [
  …,
  "crates/qubox-bbr",   # NEW — pure-Rust BBR v3 app-layer controller
]
```

**Stub:**

```rust
// crates/qubox-bbr/src/lib.rs
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct BbrV3Config {
    pub min_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub start_bitrate_bps: u32,
    /// ProbeRTT cadence (v3 default ~5 s; we override to 5 s — see Pitfall P1).
    pub probe_rtt_interval: Duration,
    /// Target loss + ECN rate per RTT (v3 default 2 %).
    pub loss_rate_target: f64,
    /// PROBE_UP cwnd_gain (v3 = 2.25; v1 was 2.0).
    pub cwnd_gain_probe_up: f64,
}

impl Default for BbrV3Config {
    fn default() -> Self {
        Self {
            min_bitrate_bps: 1_000_000,
            max_bitrate_bps: 1_000_000_000, // 1 Gbps cap; we are capping the encoder
            start_bitrate_bps: 5_000_000,
            probe_rtt_interval: Duration::from_secs(5),
            loss_rate_target: 0.02,
            cwnd_gain_probe_up: 2.25,
        }
    }
}

pub struct BbrV3RateController {
    cfg: BbrV3Config,
    /// b/s — BtlBwFilter max (RFC 8698 / BBR v1 §4.3; v3 retains
    /// the 10-round max-window).
    max_bw_bps: u64,
    /// ms — min RTT in the 10 s ProbeRTT filter window.
    min_rtt_ms: f64,
    /// last ProbeRTT start (Instant).
    last_probe_rtt: Option<Instant>,
    mode: BbrMode,
    target_bitrate_bps: u32,
}

#[derive(Debug, Clone, Copy)]
enum BbrMode { Startup, Drain, ProbeBwUp, ProbeBwDown, ProbeBwCruise, ProbeRtt }

impl BbrV3RateController {
    pub fn new(cfg: BbrV3Config) -> Self {
        Self {
            target_bitrate_bps: cfg.start_bitrate_bps,
            max_bw_bps: 0,
            min_rtt_ms: f64::MAX,
            last_probe_rtt: None,
            mode: BbrMode::Startup,
            cfg,
        }
    }
}

impl RateController for BbrV3RateController {
    fn on_observation(
        &mut self,
        _owd_ms: f64,
        loss_x1000: u16,
        rtt: Duration,
        sent_bytes: u64,
        now: Instant,
    ) -> u32 {
        // TODO(intern): BBR v3 algorithm body.
        //
        // Pseudocode (transcribed from Cardwell et al. 2023 §4
        // and Google BBR-dev 2024-04 IETF 119 slides):
        //   1. Update BtlBwFilter:
        //        if app_limited=false and sent_bytes>0
        //          max_bw_bps = max(max_bw_bps, sent_bytes / rtt)
        //   2. Update min_rtt_ms (10 s filter window).
        //   3. If mode == Startup and inflight has grown > 25 %
        //      twice in a row → exit Startup.
        //   4. If loss_rate > loss_rate_target → tighten inflight_hi.
        //   5. pace_rate = max_bw_bps * pacing_gain(mode)
        //        ProbeBwUp    → 1.25
        //        ProbeBwDown  → 0.75
        //        ProbeBwCruise→ 1.00
        //        Drain        → 1/cwnd_gain_probe_up
        //   6. target_bitrate_bps = clamp(pace_rate, min, max).
        //
        // Pitfall P1: probe_rtt_interval MUST be 5 s; do not
        // accept the v1 10 s default.
        //
        // Reference URLs:
        //   https://datatracker.ietf.org/meeting/119/materials/slides-119-ccwg-bbrv3-overview-and-google-deployment-00
        //   https://research.cec.sc.edu/files/cyberinfra/files/BBR%20-%20Fundamentals%20and%20Updates%202023-08-29.pdf
        let _ = (loss_x1000, rtt, sent_bytes, now);
        self.target_bitrate_bps
    }

    fn snapshot(&self) -> CongestionTelemetry {
        CongestionTelemetry {
            algorithm: self.algorithm_name(),
            target_bitrate_bps: self.target_bitrate_bps,
            max_bandwidth_bps: Some(self.max_bw_bps),
            min_rtt_ms: if self.min_rtt_ms.is_finite() { Some(self.min_rtt_ms) } else { None },
            ..Default::default()
        }
    }
    fn current_bitrate_bps(&self) -> u32 { self.target_bitrate_bps }
    fn algorithm_name(&self) -> &'static str { "bbr-v3" }
}
```

**Expected measurement vs Cubic** (Google IETF 119 deployment
slides, April 2024, summarised in
<https://www.forasoft.com/learn/video-streaming/articles-streaming/congestion-control-bbr-cubic-copa>):

- 4G/LTE 20 ms RTT: **+45 % throughput** vs Cubic.
- Wi-Fi 5 ms RTT: **+20 %** vs Cubic.
- Satellite 600 ms RTT: **+120 %** vs Cubic.

These are TCP bulk-transfer numbers, not video-over-QUIC, but the
Ericsson MDPI 2024 paper
(<https://www.mdpi.com/2673-4001/7/2/29>) confirms QUIC+BBRv3
outperforms TCP+Cubic under 1–2 % loss by 23–43 % download-time
reduction. We cite these as the basis for the BBR v3 default-on for
wired broadband.

### 5. OCC for cellular (LTE / 5G)

The OCC paper (Zhuang et al., "OCC: Physical-Layer Assisted Congestion
Control for Real-Time Communications," arXiv:2604.22383, CUHK, 2026 —
<https://arxiv.org/abs/2604.22383>) is **not purely user-space**: it
"monitors RAN metrics at the cellular base station" and feeds an ABW
estimate back to the RTC client. A pure user-space port is therefore
impossible. We instead implement an **OCC-like app-layer estimator**
that consumes the same physical-layer signals **as observed at the UE**
(via ModemManager — see §6) and produces an available-bandwidth
estimate from them.

The paper's ABW formula is
`Cp = (P_alloc + P_idle/N_user) · R_mcs`, with stability from CQI and
BLER. From user space we don't get `P_alloc` or `N_user` reliably;
we substitute SNR-derived capacity
(<https://dl.acm.org/doi/pdf/10.1145/2699343.2699345>, Feng et al.
2015 "CQIC") and clamp with the observed BLER.

**No maintained Rust OCC crate exists** (crates.io searches for
`occ`, `cross-layer`, `lte-cc`, `cellular-cc` all empty).

**Library version:**

```toml
# Cargo.toml (workspace members)
members = [
  …,
  "crates/qubox-occ",   # NEW — pure-Rust OCC-like app-layer estimator
]
```

**Stub:**

```rust
// crates/qubox-occ/src/lib.rs
use std::time::{Duration, Instant};
use qubox_platform::cellular::L1Metrics;

#[derive(Debug, Clone, Copy)]
pub struct OccConfig {
    pub min_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub start_bitrate_bps: u32,
    /// dB — SINR floor below which we treat the link as unusable
    /// and fall back to SCReAM.
    pub sinr_floor_db: f32,
    /// Fraction — BLER threshold above which we cap the bandwidth
    /// (OCC paper §4.3).
    pub bler_cap: f32,
}

impl Default for OccConfig {
    fn default() -> Self {
        Self {
            min_bitrate_bps: 300_000,
            max_bitrate_bps: 50_000_000,
            start_bitrate_bps: 1_500_000,
            sinr_floor_db: -3.0,
            bler_cap: 0.10,
        }
    }
}

pub struct OccRateController {
    cfg: OccConfig,
    /// L1 metrics from ModemManager. `None` = no ModemManager or no
    /// modem present; the router must fall back to SCReAM.
    l1: Option<L1Metrics>,
    /// b/s — ABW estimate from the CQIC-style transform.
    abw_bps: u64,
    target_bitrate_bps: u32,
}

impl OccRateController {
    pub fn new(cfg: OccConfig) -> Self {
        Self { target_bitrate_bps: cfg.start_bitrate_bps, cfg, l1: None, abw_bps: 0 }
    }
}

impl RateController for OccRateController {
    fn on_observation(
        &mut self,
        _owd_ms: f64,
        loss_x1000: u16,
        rtt: Duration,
        sent_bytes: u64,
        now: Instant,
    ) -> u32 {
        // TODO(intern): OCC-like ABW estimator.
        //
        // Pseudocode:
        //   1. If self.l1.is_none() → panic-mode? No — the router
        //      already chose SCReAM. This branch is unreachable.
        //   2. ABW(b/s) ≈ 1e6 * 10 ** ((SINR_dB + offset) / 10) / 8
        //      (Shannon-Hartley approximation; matches Feng 2015).
        //   3. If BLER > bler_cap → ABW *= (1 - bler_cap) / max(bler, 0.01)
        //   4. target_bitrate_bps = min(ABW, max_bitrate).
        //   5. Add app-limited probe-up: if loss_x1000 == 0
        //      AND rtt unchanged for 5 s → slowly increase by 5 %.
        //
        // Reference: arXiv:2604.22383 §4.1–§4.3.
        let _ = (loss_x1000, rtt, sent_bytes, now);
        self.target_bitrate_bps
    }

    fn on_l1_metric(&mut self, m: L1Metric) {
        // Called from a tokio task polling ModemManager.PropertiesChanged.
        match m {
            L1Metric::Rsrp(v)  => self.l1.as_mut().map(|m| m.rsrp  = v),
            L1Metric::Rsrq(v)  => self.l1.as_mut().map(|m| m.rsrq  = v),
            L1Metric::Rssi(v)  => self.l1.as_mut().map(|m| m.rssi  = v),
            L1Metric::Sinr(v)  => self.l1.as_mut().map(|m| m.sinr  = v),
            L1Metric::Bler(v)  => self.l1.as_mut().map(|m| m.bler  = v),
        }
    }

    fn snapshot(&self) -> CongestionTelemetry {
        CongestionTelemetry {
            algorithm: self.algorithm_name(),
            target_bitrate_bps: self.target_bitrate_bps,
            abw_bps: Some(self.abw_bps),
            ..Default::default()
        }
    }
    fn current_bitrate_bps(&self) -> u32 { self.target_bitrate_bps }
    fn algorithm_name(&self) -> &'static str { "occ" }
}
```

**Caveat:** the OCC paper reports up to **18 % throughput gain**
vs SCReAM under mobility at 60 km/h; this is the **BS-side** OCC.
Our UE-only estimator can capture maybe 50–60 % of that gain. We
document this honestly in `crates/qubox-occ/README.md`.

### 6. Network-class detection algorithm

The router runs this at session start (before the first
`on_observation` call):

```text
fn classify_network() -> NetworkClass:
    # 1. Sample 100 OWD values over 2 s of dummy probe traffic.
    #    (The transport layer emits 25 ms keep-alives for this
    #    purpose — see PR-5 in §9.)
    samples: [f64; 100] = collect_owd_samples_over(Duration::from_secs(2))
    stddev_ms = samples.stddev()
    mean_ms   = samples.mean()

    # 2. Ask the platform crate for the active interface.
    let active_iface = crates::qubox_platform::net::active_default_interface()?

    # 3. Decision tree (matches §4 of this ADR):
    match active_iface.kind() {
        InterfaceKind::Ethernet => NetworkClass::WiredBroadband,
        InterfaceKind::Wifi    => NetworkClass::Wireless,
        InterfaceKind::Cellular => {
            // Verify with OWD + L1 telemetry availability.
            if stddev_ms < 5.0 && mean_ms < 80.0 && l1_available() {
                NetworkClass::Cellular { has_l1_metrics: true }
            } else {
                NetworkClass::Wireless  // modem present, L1 broken — treat as wireless
            }
        }
        InterfaceKind::Other    => {
            if stddev_ms <= 1.0 && mean_ms < 30.0 {
                NetworkClass::WiredBroadband
            } else {
                NetworkClass::Unknown   // will be re-classified after 5 s
            }
        }
    }
```

**Thresholds** (with rationale):

| Test | Threshold | Why |
|------|-----------|-----|
| `stddev(owd_ms) > 5 ms`         | → Wireless | 802.11 beacon jitter is typically 2–10 ms; LTE scheduler jitter is similar; wired is sub-millisecond. |
| `stddev(owd_ms) ≤ 1 ms`         | → WiredBroadband | Empirical: fibre / cable / Ethernet all sit <0.5 ms. |
| `stddev(owd_ms) ∈ (1, 5]`       | → Unknown first, reclassify after 5 s | Need more data. |
| L1 metrics available            | → Cellular `{ has_l1_metrics: true }` | ModemManager `Modem.Signal.Setup()` succeeded. |
| L1 metrics unavailable but modem present | → Wireless | Treat as lossy wireless. |

The `NetworkClass` enum is **stable for the session**. Re-classification
is only done on session boundary (e.g. after suspend/resume — ADR-010
owns that signal).

### 7. Linux cellular telemetry path (for OCC)

ModemManager is the supported user-space interface. Verified D-Bus
contract from
<https://www.freedesktop.org/software/ModemManager/api/latest/gdbus-org.freedesktop.ModemManager1.Modem.Signal.html>:

- Bus: `org.freedesktop.ModemManager1`
- Object: `/org/freedesktop/ModemManager1/Modem/<N>`
- Interface: `org.freedesktop.ModemManager1.Modem.Signal`
- Property: `Lte` (dictionary `a{sv}`) → keys include `rsrp`,
  `rsrq`, `rssi`, `snr`/`s/r`, `bler` (BLER added in ModemManager
  1.2).
- Method: `Setup(rate)` (seconds, periodic polling) or
  `SetupThresholds(...)` (since 1.20, push updates when metrics
  change by more than a delta).

**No canonical `modemmanager-rs` crate.** The closest options:

1. **`omnect/modemmanager`** (GitHub,
   <https://github.com/omnect/modemmanager>) — Rust wrapper around
   `modemmanager-sys` (libmm-glib FFI bindings). Last update 2023-11;
   **not recommended** for a fresh dependency: stale, depends on a
   C library that may not be installed on user machines.
2. **`zbus`** (<https://github.com/z-galaxy/zbus>) — pure-Rust D-Bus
   crate. Already mature, well-maintained, used by GNOME components
   in production.

**We use `zbus`.** It's already an indirect dep via `qubox-platform`'s
`dbus-rs` work, and `zbus_xmlgen` lets us generate Rust types from
ModemManager's introspection XML.

**Add to `crates/qubox-platform/Cargo.toml`:**

```toml
[dependencies]
qubox-proto = { path = "../qubox-proto" }
uuid.workspace = true
zbus = { version = "4", default-features = false, features = ["tokio"] }
```

**New module:**

```rust
// crates/qubox-platform/src/cellular.rs
//
//! Linux cellular telemetry via ModemManager D-Bus.
//!
//! Source spec:
//! https://www.freedesktop.org/software/ModemManager/api/latest/gdbus-org.freedesktop.ModemManager1.Modem.Signal.html
//!
//! On non-Linux targets this module compiles to a no-op stub
//! returning `Ok(None)` from `try_collect()`.
use serde::Deserialize;
use zbus::Connection;

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct L1Metrics {
    pub rsrp: f32,
    pub rsrq: f32,
    pub rssi: f32,
    pub sinr: f32,
    pub bler: f32,
}

impl L1Metrics {
    pub fn sinr_floor_ok(&self, floor_db: f32) -> bool {
        self.sinr >= floor_db
    }
}

#[cfg(target_os = "linux")]
pub async fn try_collect() -> anyhow::Result<Option<L1Metrics>> {
    let conn = Connection::system().await?;
    let proxy = zbus::ProxyBuilder::new(&conn)
        .destination("org.freedesktop.ModemManager1")?
        .path("/org/freedesktop/ModemManager1/Modem/0")?
        .interface("org.freedesktop.ModemManager1.Modem.Signal")?
        .build()
        .await?;
    // `Lte` property is an `a{sv}`; zbus returns Variant → Value.
    // See zbus_xmlgen codegen in PR-3 for the strongly-typed variant.
    let lte_dict = proxy.get_property::<std::collections::HashMap<String, zbus::zvariant::Value>>("Lte").await?;
    Ok(Some(parse_lte_dict(&lte_dict)))
}

#[cfg(not(target_os = "linux"))]
pub async fn try_collect() -> anyhow::Result<Option<L1Metrics>> { Ok(None) }
```

CLI equivalent (sanity check):

```bash
# Verify a modem is reporting metrics
mmcli -m 0 --signal-get
# Look for the "LTE" subsection: rsrp, rsrq, rssi, snr, error rate.
```

Android support (post-1.0): `qubox-platform/src/cellular_android.rs`
will use JNI on `TelephonyManager.getAllCellInfo()`. Per Android docs
that returns RSRP/RSRQ/RSSI/SNR but **not** CQI/MCS — fine, our OCC
estimator only needs the SNR/BLER side.

### 8. CongestionRouter + telemetry

```rust
// apps/qubox-host-agent/src/rate_control/router.rs
//
//! Picks one RateController impl per session based on NetworkClass
//! and the user's `--congestion-controller=` override.
//!
//! Replaces the direct `GccRateController::new(cfg)` call in
//! `apps/qubox-host-agent/src/rate_feedback.rs:36-44`.

use std::sync::Arc;
use std::time::Instant;
use crate::rate_control::{
    CongestionAlgorithm, NetworkClass, RateController,
    legacy::LegacyGccRateController,
    scream::ScreamRateController,
    bbr::BbrV3RateController,
    occ::OccRateController,
};
use crate::rate_control::telemetry::CongestionTelemetry;

pub struct CongestionRouter {
    algo: CongestionAlgorithm,
    class: NetworkClass,
    controller: Box<dyn RateController>,
    last_emitted_telemetry_at: Option<Instant>,
}

impl CongestionRouter {
    /// `override_algo = None` ⇒ auto-pick from `class`.
    pub fn new(class: NetworkClass, override_algo: Option<CongestionAlgorithm>, cfg: RouterConfig) -> Self {
        let algo = override_algo.unwrap_or_else(|| match class {
            NetworkClass::WiredBroadband => CongestionAlgorithm::BbrV3,
            NetworkClass::Wireless       => CongestionAlgorithm::Scream,
            NetworkClass::Cellular { has_l1_metrics: true }  => CongestionAlgorithm::Occ,
            NetworkClass::Cellular { has_l1_metrics: false } => CongestionAlgorithm::Scream,
            NetworkClass::Unknown        => CongestionAlgorithm::Scream,
        });
        let controller: Box<dyn RateController> = match algo {
            CongestionAlgorithm::Scream => Box::new(ScreamRateController::new(cfg.scream)),
            CongestionAlgorithm::BbrV3  => Box::new(BbrV3RateController::new(cfg.bbr_v3)),
            CongestionAlgorithm::Occ    => Box::new(OccRateController::new(cfg.occ)),
            CongestionAlgorithm::Gcc    => Box::new(LegacyGccRateController::new(cfg.gcc_legacy)),
        };
        Self { algo, class, controller, last_emitted_telemetry_at: None }
    }

    pub fn on_observation(
        &mut self,
        owd_ms: f64,
        loss_x1000: u16,
        rtt: std::time::Duration,
        sent_bytes: u64,
        now: Instant,
    ) -> u32 {
        let bps = self.controller.on_observation(owd_ms, loss_x1000, rtt, sent_bytes, now);
        // Emit telemetry once per second. The 1 Hz coalescing layer in
        // rate_feedback.rs already enforces 1 Hz on bitrate changes;
        // telemetry has its own gate so it works even when bitrate
        // is stable.
        if self.last_emitted_telemetry_at.map_or(true, |t| now.duration_since(t) >= std::time::Duration::from_secs(1)) {
            let snap = self.controller.snapshot();
            tracing::info!(
                algorithm = snap.algorithm,
                network_class = ?self.class,
                target_bps = snap.target_bitrate_bps,
                cwnd_bytes = ?snap.cwnd_bytes,
                qdelay_ms = ?snap.qdelay_ms,
                max_bw_bps = ?snap.max_bandwidth_bps,
                min_rtt_ms = ?snap.min_rtt_ms,
                abw_bps = ?snap.abw_bps,
                l1_rsrp_dbm = ?snap.l1_rsrp_dbm,
                l1_sinr_db = ?snap.l1_sinr_db,
                l1_bler = ?snap.l1_bler,
                "congestion telemetry"
            );
            self.last_emitted_telemetry_at = Some(now);
        }
        bps
    }

    pub fn on_l1_metric(&mut self, m: crate::rate_control::L1Metric) {
        self.controller.on_l1_metric(m);
    }
}
```

### 9. LegacyGccRateController (rename only)

```rust
// apps/qubox-host-agent/src/rate_control/legacy.rs
//
//! GCC (Google Congestion Control) — preserved for the 1.0 deprecation
//! window. This is a straight rename + re-export of the existing
//! `GccRateController` at `apps/qubox-host-agent/src/rate_control.rs:84-99`.
//!
//! No new behaviour — we only:
//!   1. Move the type into a `legacy` submodule so the new file tree
//!      (router + scream + bbr + occ + telemetry) is the visible API.
//!   2. Rename the type to `LegacyGccRateController` to make the
//!      "deprecated" status obvious at every call site.
//!   3. Implement the `RateController` trait by delegating to the
//!      existing `on_observation(owd_ms, loss_x1000, rtt, now)`
//!      method on `GccRateController`.

use std::time::{Duration, Instant};
use crate::rate_control::CongestionTelemetry;

pub use crate::rate_control::{GccConfig, GccRateController, OveruseState};

pub struct LegacyGccRateController {
    inner: GccRateController,
}

impl LegacyGccRateController {
    pub fn new(cfg: GccConfig) -> Self {
        Self { inner: GccRateController::new(cfg) }
    }
    pub fn current_state(&self) -> OveruseState { self.inner.current_state() }
    pub fn owd_ewma_ms(&self) -> f64 { self.inner.owd_ewma_ms() }
}

impl crate::rate_control::RateController for LegacyGccRateController {
    fn on_observation(
        &mut self,
        owd_ms: f64,
        loss_x1000: u16,
        rtt: Duration,
        _sent_bytes: u64,
        now: Instant,
    ) -> u32 {
        self.inner.on_observation(owd_ms, loss_x1000, rtt, now)
    }
    fn snapshot(&self) -> CongestionTelemetry {
        CongestionTelemetry {
            algorithm: self.algorithm_name(),
            target_bitrate_bps: self.inner.current_bitrate_bps(),
            owd_ms_ewma: Some(self.inner.owd_ewma_ms()),
            state: Some(format!("{:?}", self.inner.current_state())),
            ..Default::default()
        }
    }
    fn current_bitrate_bps(&self) -> u32 { self.inner.current_bitrate_bps() }
    fn algorithm_name(&self) -> &'static str { "gcc-legacy" }
}
```

### 10. CLI integration

```rust
// apps/qubox-host-agent/src/main.rs (insert inside `struct Args`,
// near line 130 where the rate-control-related flags live)

#[arg(long, value_enum, default_value_t = CliCongestionOverride::Auto)]
congestion_controller: CliCongestionOverride,

#[arg(long, default_value_t = 60)]
probe_owd_ms: u32, // duration of the 100-sample probe; default 2 s

#[derive(Copy, Clone, clap::ValueEnum)]
pub enum CliCongestionOverride {
    Auto, Scream, BbrV3, Occ, Gcc,
}

// in `main()`, after parsing `args`:
let router_cfg = rate_control::RouterConfig::default();
let class = rate_control::router::classify_network(Duration::from_millis(args.probe_owd_ms as u64)).await?;
let algo = match args.congestion_controller {
    CliCongestionOverride::Auto   => None,
    CliCongestionOverride::Scream => Some(CongestionAlgorithm::Scream),
    CliCongestionOverride::BbrV3  => Some(CongestionAlgorithm::BbrV3),
    CliCongestionOverride::Occ    => Some(CongestionAlgorithm::Occ),
    CliCongestionOverride::Gcc    => Some(CongestionAlgorithm::Gcc),
};
let router = CongestionRouter::new(class, algo, router_cfg);
// … pass router into rate_feedback_loop instead of GccRateController.
```

### 11. Telemetry schema

```rust
// apps/qubox-host-agent/src/rate_control/telemetry.rs
//
//! Snapshot emitted once per second by every RateController.
//!
//! Consumed by:
//!   - ADR-013 frame-aware pacing (needs max_bandwidth_bps, target_bps)
//!   - ADR-020 Pensieve RL state space (everything)
//!   - apps/qubox-client-gui dashboard overlay (every field)
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct CongestionTelemetry {
    /// "scream-v2" | "bbr-v3" | "occ" | "gcc-legacy"
    pub algorithm: &'static str,
    /// Target encoder bitrate (the bitrate the controller wants
    /// the encoder to use RIGHT NOW).
    pub target_bitrate_bps: u32,

    /// ── Optional per-algorithm fields ──
    pub cwnd_bytes: Option<u64>,
    pub qdelay_ms: Option<f64>,
    pub max_bandwidth_bps: Option<u64>,
    pub min_rtt_ms: Option<f64>,
    pub abw_bps: Option<u64>,
    pub owd_ms_ewma: Option<f64>,
    pub state: Option<String>,

    /// ── L1 metrics (OCC only; None otherwise) ──
    pub l1_rsrp_dbm: Option<f32>,
    pub l1_rsrq_db: Option<f32>,
    pub l1_rssi_dbm: Option<f32>,
    pub l1_sinr_db: Option<f32>,
    pub l1_bler: Option<f32>,
}

impl Default for CongestionTelemetry {
    fn default() -> Self {
        Self {
            algorithm: "unknown",
            target_bitrate_bps: 0,
            cwnd_bytes: None, qdelay_ms: None, max_bandwidth_bps: None,
            min_rtt_ms: None, abw_bps: None, owd_ms_ewma: None, state: None,
            l1_rsrp_dbm: None, l1_rsrq_db: None, l1_rssi_dbm: None,
            l1_sinr_db: None, l1_bler: None,
        }
    }
}
```

**Tracing invocation** (used by `CongestionRouter::on_observation`
in §8 above):

```rust
tracing::info!(
    algorithm = snap.algorithm,
    network_class = ?self.class,
    target_bps = snap.target_bitrate_bps,
    cwnd_bytes = ?snap.cwnd_bytes,
    qdelay_ms = ?snap.qdelay_ms,
    max_bw_bps = ?snap.max_bandwidth_bps,
    min_rtt_ms = ?snap.min_rtt_ms,
    abw_bps = ?snap.abw_bps,
    l1_rsrp_dbm = ?snap.l1_rsrp_dbm,
    l1_sinr_db = ?snap.l1_sinr_db,
    l1_bler = ?snap.l1_bler,
    "congestion telemetry"
);
```

Filter in dev: `RUST_LOG=qubox_host_agent::rate_control=info,debug`.

---

## Test specifications

All tests live in `crates/qubox-scream/tests/`,
`crates/qubox-bbr/tests/`, `crates/qubox-occ/tests/`, and
`apps/qubox-host-agent/src/rate_control/tests_router.rs`. Every
test name below is the **exact** function name the intern writes.

### SCReAM

```rust
// crates/qubox-scream/tests/probe_then_stabilise.rs

#[test]
fn scream_controller_probes_then_stabilises() {
    let mut c = ScreamRateController::new(ScreamConfig::default());
    let t0 = Instant::now();
    // 100 samples, 20 ms apart, OWD below qdelay target.
    // Expected trajectory (with v2 default qdelay_target_ms = 60):
    //   1–10 samples   : cwnd grows from initial_window to ≈ 1.5x
    //   10–50 samples  : bitrate climbs toward max_bitrate_bps
    //   50–100 samples : EWMA qdelay converges → CWND stabilises
    // Tolerance: ±10 % of expected at sample 100.
    for i in 0..100 {
        let bps = c.on_observation(20.0 + (i as f64) * 0.05, 0, Duration::from_millis(40), 1500, t0 + Duration::from_millis(20 * i));
        assert!(bps >= 500_000 && bps <= 50_000_000);
    }
    let final_bps = c.current_bitrate_bps();
    assert!((2_000_000..=15_000_000).contains(&final_bps),
        "SCReAM should reach 2-15 Mbps under no-loss 40ms RTT, got {final_bps}");
}

#[test]
fn scream_loss_event_triggers_immediate_cwnd_cut() {
    // RFC 8298 §4.4: loss > 2 % → cwnd *= 0.7 instantly.
    // Send 10 normal samples, then 1 with loss_x1000 = 50 (5 %).
    // Expected: bitrate after loss event ≤ 70 % of pre-loss bitrate.
}

#[test]
fn scream_qdelay_above_target_shrinks_cwnd() {
    // qdelay_target_ms = 60, feed owd_ms = 100 over 30 samples.
    // Expected: EWMA qdelay crosses 60 → cwnd decreases monotonically.
}
```

### BBR v3

```rust
// crates/qubox-bbr/tests/probe_rtt_cadence.rs

#[test]
fn bbr_v3_handles_50ms_rtt_correctly() {
    let mut c = BbrV3RateController::new(BbrV3Config::default());
    // 200 samples at 25 ms RTT, no loss, sent_bytes = 1500 per sample.
    // Expected trajectory:
    //   Startup phase:  bitrate climbs to ≈ BtlBw * 2.25 within 8-10 RTTs
    //   Drain phase:    one cycle, bitrate settles
    //   ProbeBwUp:      bitrate oscillates 1.0× → 1.25× → 1.0× BtlBw
    //   ProbeRtt:       every 5 s, bitrate drops for ≤ 200 ms then recovers
    // Tolerance: ±15 % of expected at sample 200.
    let t0 = Instant::now();
    let mut last = 0_u32;
    let mut stable_count = 0_u32;
    for i in 0..200 {
        let bps = c.on_observation(0.0, 0, Duration::from_millis(50), 1500, t0 + Duration::from_millis(250 * i));
        if i > 0 && ((bps as i64 - last as i64).abs() < (last as i64) / 20) {
            stable_count += 1;
        }
        last = bps;
    }
    assert!(stable_count >= 150, "BBR v3 should be in steady-state for ≥75 % of samples; got {stable_count}");
    assert!(c.probe_rtt_interval == Duration::from_secs(5),
        "Pitfall P1: probe_rtt_interval MUST be 5 s, not the 10 s v1 default");
}

#[test]
fn bbr_v3_tightens_inflight_on_2pct_loss() {
    // Send 100 samples with 0 % loss; record steady-state BtlBw.
    // Then send 20 samples with 25 ‰ loss (2.5 %).
    // Expected: BtlBw shrinks by 5–15 % (loss-rate cap is 2 %).
}

#[test]
fn bbr_v3_continues_probing_after_loss() {
    // Per BBR v3 paper §4: after a loss event, mode transitions
    // back to ProbeBwUp within 2 RTTs (not stuck in Drain like v2).
}
```

### OCC

```rust
// crates/qubox-occ/tests/l1_metrics_used.rs

#[test]
fn occ_uses_l1_metrics_when_available() {
    let mut c = OccRateController::new(OccConfig::default());
    c.on_l1_metric(L1Metric::Sinr(15.0));   // good LTE
    c.on_l1_metric(L1Metric::Bler(0.05));
    let bps = c.on_observation(0.0, 0, Duration::from_millis(80), 1500, Instant::now());
    // Shannon-Hartley for SINR=15 dB on 20 MHz LTE ≈ 50 Mbps.
    // BLER 5 % is below cap, no further reduction.
    // Expected: 10 Mbps ≤ bps ≤ 50 Mbps.
    assert!((10_000_000..=50_000_000).contains(&bps),
        "OCC should estimate 10–50 Mbps at SINR=15, got {bps}");
}

#[test]
fn occ_falls_back_below_sinr_floor() {
    let mut c = OccRateController::new(OccConfig { sinr_floor_db: -3.0, ..Default::default() });
    c.on_l1_metric(L1Metric::Sinr(-10.0));
    let bps = c.on_observation(0.0, 0, Duration::from_millis(80), 1500, Instant::now());
    assert_eq!(bps, c.cfg.min_bitrate_bps,
        "OCC should drop to min_bitrate when SINR < floor");
}

#[test]
fn occ_high_bler_caps_abw() {
    let mut c = OccRateController::new(OccConfig::default());
    c.on_l1_metric(L1Metric::Sinr(15.0));
    c.on_l1_metric(L1Metric::Bler(0.20));   // 20 % BLER > 10 % cap
    let bps = c.on_observation(0.0, 0, Duration::from_millis(80), 1500, Instant::now());
    // Expected: bps ≤ abw_at_sinr15 / (bler / bler_cap)
    // = 50 Mbps / (0.20 / 0.10) ≈ 25 Mbps, but the formula is
    // ABW *= bler_cap / max(bler, 0.01), so ABW ≈ 25 Mbps.
    assert!(bps <= 30_000_000, "OCC should cap ABW at 30 Mbps for BLER 20 %, got {bps}");
}
```

### Router + classification

```rust
// apps/qubox-host-agent/src/rate_control/tests_router.rs

#[test]
fn router_picks_bbr_v3_for_wired_broadband() {
    let r = CongestionRouter::new(NetworkClass::WiredBroadband, None, RouterConfig::default());
    assert_eq!(r.algo, CongestionAlgorithm::BbrV3);
}

#[test]
fn router_picks_occ_for_cellular_with_l1() {
    let r = CongestionRouter::new(
        NetworkClass::Cellular { has_l1_metrics: true },
        None, RouterConfig::default(),
    );
    assert_eq!(r.algo, CongestionAlgorithm::Occ);
}

#[test]
fn router_picks_scream_for_cellular_without_l1() {
    let r = CongestionRouter::new(
        NetworkClass::Cellular { has_l1_metrics: false },
        None, RouterConfig::default(),
    );
    assert_eq!(r.algo, CongestionAlgorithm::Scream);
}

#[test]
fn router_force_override_wins() {
    let r = CongestionRouter::new(
        NetworkClass::WiredBroadband,
        Some(CongestionAlgorithm::Gcc),
        RouterConfig::default(),
    );
    assert_eq!(r.algo, CongestionAlgorithm::Gcc);
}

#[test]
fn network_classifier_wired_when_stddev_below_1ms() {
    // Mock 100 samples with stddev 0.4 ms, mean 12 ms → WiredBroadband.
}

#[test]
fn network_classifier_wireless_when_stddev_above_5ms() {
    // Mock 100 samples with stddev 8 ms → Wireless.
}
```

### Legacy GCC

The existing tests at `apps/qubox-host-agent/src/rate_control.rs:260-513`
are preserved unchanged. New test added at the end:

```rust
// apps/qubox-host-agent/src/rate_control/legacy.rs (in #[cfg(test)] mod tests)

#[test]
fn legacy_gcc_implements_rate_controller_trait() {
    // Just verifies the impl compiles and current_bitrate_bps is reachable
    // through the trait object. Catches accidental signature drift.
    let mut c: Box<dyn RateController> = Box::new(LegacyGccRateController::new(GccConfig::default()));
    let _ = c.on_observation(20.0, 0, Duration::from_millis(20), 1500, Instant::now());
    let _ = c.snapshot();
}
```

---

## File-path inventory and insertion points

| File | Action | Approx. line range |
|------|--------|--------------------|
| `Cargo.toml` (workspace) | Add `crates/qubox-scream`, `crates/qubox-bbr`, `crates/qubox-occ` to `members`. | top of file |
| `crates/qubox-scream/Cargo.toml` | **NEW file** (path dep on `qubox-platform`, `serde`, `tracing`). | n/a |
| `crates/qubox-scream/src/lib.rs` | **NEW file** — SCReAM v2 impl (see stub §3). | n/a |
| `crates/qubox-bbr/Cargo.toml` | **NEW file**. | n/a |
| `crates/qubox-bbr/src/lib.rs` | **NEW file** — BBR v3 impl (see stub §4). | n/a |
| `crates/qubox-occ/Cargo.toml` | **NEW file** (path dep on `qubox-platform`, `qubox-proto`). | n/a |
| `crates/qubox-occ/src/lib.rs` | **NEW file** — OCC-like impl (see stub §5). | n/a |
| `crates/qubox-platform/Cargo.toml` | Add `zbus = { version = "4", … }`. | full file (small) |
| `crates/qubox-platform/src/lib.rs` | Add `pub mod cellular;` and `pub mod net;`. | after line 30 |
| `crates/qubox-platform/src/cellular.rs` | **NEW file** — ModemManager L1 metrics (see §7). | n/a |
| `crates/qubox-platform/src/net.rs` | **NEW file** — `active_default_interface()` + `InterfaceKind` enum. | n/a |
| `apps/qubox-host-agent/src/main.rs` | Add `mod rate_control;` → `mod rate_control { mod router; mod telemetry; … }` and the two new `Args` fields (`congestion_controller`, `probe_owd_ms`). | lines 62–130 (Args struct), 320 (main) |
| `apps/qubox-host-agent/src/rate_control.rs` | **Rename file** to `rate_control/legacy.rs` (move content unchanged). Add `mod legacy;` etc. | whole file |
| `apps/qubox-host-agent/src/rate_control/mod.rs` | **NEW file** — re-exports + `RateController` trait + `NetworkClass` + `CongestionAlgorithm` (see §2). | n/a |
| `apps/qubox-host-agent/src/rate_control/trait_def.rs` | **NEW file** — `RateController` trait + `L1Metric` (see §2). | n/a |
| `apps/qubox-host-agent/src/rate_control/telemetry.rs` | **NEW file** — `CongestionTelemetry` (see §11). | n/a |
| `apps/qubox-host-agent/src/rate_control/router.rs` | **NEW file** — `CongestionRouter` (see §8). | n/a |
| `apps/qubox-host-agent/src/rate_control/scream.rs` | **NEW file** — `ScreamRateController` re-exporting `qubox_scream::ScreamRateController` as a `RateController`. | n/a |
| `apps/qubox-host-agent/src/rate_control/bbr.rs` | **NEW file** — same idea for BBR v3. | n/a |
| `apps/qubox-host-agent/src/rate_control/occ.rs` | **NEW file** — same idea for OCC. | n/a |
| `apps/qubox-host-agent/src/rate_feedback.rs` | Replace `GccRateController::new(cfg)` at `:36-44` with `CongestionRouter::new(class, algo, cfg)`; update the `controller.on_observation(...)` call at the equivalent of the old line 75 to pass `sent_bytes` from `fb.bytes_acked`. The struct definition for `RateFeedback` at `crates/qubox-proto/src/lib.rs:362` needs a new field `bytes_acked: u32` (default 0). | `rate_feedback.rs` lines 36–80; `qubox-proto` lib.rs lines 362–385 |
| `crates/qubox-proto/src/lib.rs` | Add `bytes_acked: u32` field to `RateFeedback`. Update the test fixture at `:1373` to set `bytes_acked: 1500`. | lines 362–385 + 1373 |
| `apps/qubox-host-agent/src/rate_control/tests_router.rs` | **NEW file** — router + classification tests. | n/a |

---

## Step-by-step implementation order

The PRs are in strict dependency order; later PRs depend on the
re-exports created by earlier ones.

1. **PR-1 — `qubox-proto`: add `bytes_acked: u32` to `RateFeedback`.**
   1-line change, defaults to 0 so no caller breaks.
   - Files: `crates/qubox-proto/src/lib.rs:362-385`, `:1373`.
   - Verify: `cargo build -p qubox-proto`; existing tests still pass.

2. **PR-2 — split `rate_control.rs` into a `rate_control/` directory
   with `mod.rs`, `telemetry.rs`, `legacy.rs`.**
   Pure mechanical move + rename `GccRateController` →
   `LegacyGccRateController` in `legacy.rs`. Existing tests
   (`rate_control.rs:260-513`) are preserved unchanged inside
   `legacy.rs`. `mod.rs` re-exports the old names for backwards
   compatibility behind `#[allow(deprecated)]`.
   - Files: `apps/qubox-host-agent/src/rate_control.rs` →
     `apps/qubox-host-agent/src/rate_control/{mod,telemetry,legacy}.rs`.
   - Verify: `cargo test -p qubox-host-agent rate_control` — all
     7 existing tests pass.

3. **PR-3 — add `crates/qubox-platform/src/{cellular,net}.rs` with
   the `zbus` integration. Add `zbus = "4"` to
   `crates/qubox-platform/Cargo.toml`.**
   No host-agent changes. Compiles to a no-op on non-Linux targets.
   - Verify: `cargo test -p qubox-platform`; on Linux,
     `RUST_LOG=debug cargo run --example list_modems` (add an
     example in this PR) prints the Lte dictionary.

4. **PR-4 — `crates/qubox-scream`: SCReAM v2 pure-Rust port.**
   - Verify: `cargo test -p qubox-scream`; the three tests in
     §"SCReAM" pass. Add `qubox-scream = { path = "…" }` to
     `apps/qubox-host-agent/Cargo.toml`.

5. **PR-5 — `crates/qubox-bbr`: BBR v3 app-layer impl.**
   - Verify: `cargo test -p qubox-bbr`; the three BBR tests pass.
     **Assert the `probe_rtt_interval == 5s` guard at the top of
     `bbr_v3_handles_50ms_rtt_correctly`.**

6. **PR-6 — `crates/qubox-occ`: OCC-like UE-side estimator.**
   - Verify: `cargo test -p qubox-occ`; the three OCC tests pass.

7. **PR-7 — `RateController` trait + `CongestionRouter` + telemetry
   emit. Replace `GccRateController` call site in
   `rate_feedback.rs`.**
   - Files: `apps/qubox-host-agent/src/rate_control/{trait_def,
     router,scream,bbr,occ,tests_router}.rs`;
     `apps/qubox-host-agent/src/rate_feedback.rs:36-80`.
   - Verify: `cargo test -p qubox-host-agent rate_control`;
     `RUST_LOG=qubox_host_agent::rate_control=info cargo run
     --bin qubox-host-agent -- --smoke-test` and grep for
     `"congestion telemetry"` lines.

8. **PR-8 — CLI flags + classification in `main.rs`.**
   - Files: `apps/qubox-host-agent/src/main.rs:62-130` (Args),
     `:320` (main).
   - Verify: `cargo run --bin qubox-host-agent -- --help` lists
     `--congestion-controller`; the test
     `router_force_override_wins` passes.

9. **PR-9 — release prep.**
   - Update `research/roadmap/APIS.md` §1 to list `qubox-scream`,
     `qubox-bbr`, `qubox-occ`, `zbus` versions.
   - Update `docs/` with the `--congestion-controller` CLI doc.
   - Tag `v0.2.0-rc.1`. GCC deprecation warning emitted at startup.

10. **PR-10 — GCC removal (1.0 GA, later milestone).** Delete
    `legacy.rs`, drop `CongestionAlgorithm::Gcc` enum variant,
    remove `--congestion-controller=gcc` from `CliCongestionOverride`.

---

## Pitfalls

1. **P1 — `quinn-proto` 0.11.15 ships BBR v1, NOT v3.** Verified by
   reading
   `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/quinn-proto-0.11.15/src/congestion/bbr/mod.rs`:
   the comment cites
   `draft-cardwell-iccrg-bbr-congestion-control` (v1), the gain
   constant is `K_DEFAULT_HIGH_GAIN = 2.885 = 2/ln(2)` (v1's
   8-phase cycle), and `bbr_min_rtt_win_sec = 10` (v1's 10 s
   filter window — v3 uses ~5 s with vastly reduced throughput
   drop). Do **not** try to "tune" quinn's BBR for v3 by setting
   fields that don't exist. Our path is app-layer BBR v3, not
   vendoring the QUIC-level controller. See Decision §4.

2. **P2 — SCReAM `qdelay_target_ms` is not 100.** RFC 8298 §4.2
   says "typically 50–100 ms". The reference C++ default is 50 ms;
   we use **60 ms** as a compromise (Pitfall: do not blindly copy
   the C++ default because the RFC explicitly allows tuning).
   100 ms is too high for 60 fps streams (one frame = 16.6 ms;
   100 ms queuing delay = 6 frames of additional latency).

3. **P3 — OCC is not purely user-space.** The paper's
   `Cp = (P_alloc + P_idle/N_user) · R_mcs` formula needs base-station
   inputs we cannot get from the UE. Our OCC-*like* estimator uses
   UE-side SNR/BLER (Feng 2015 "CQIC" formulation) and will
   realise **at most ~50 %** of the paper's 18 %-vs-SCREAM gain.
   Do not promise the full number to anyone. Document it in
   `crates/qubox-occ/README.md` and in the dashboard overlay.

4. **P4 — `quinn::congestion::Controller::on_congestion_event` uses
   `lost_bytes: u64`, not a loss fraction.** If we ever DO decide
   to push BBR v3 down to the QUIC layer (PR after this ADR), the
   rate-to-bytes conversion must happen at the call site. For the
   app-layer route in this ADR it is a non-issue, but flagging it
   for future contributors.

5. **P5 — `qubox_scream` doesn't exist on crates.io yet.** Do not
   `cargo add qubox-scream`. Use the path dependency
   `{ path = "../../crates/qubox-scream" }`. We will publish
   crates.io releases (`0.1.0`, `0.2.0`, …) only after the
   algorithm passes the tests in §"SCReAM".

6. **P6 — ModemManager returns `Lte` as `a{sv}` (dict of
   string→variant).** The `zbus` typed accessor must use
   `get_property::<HashMap<String, zbus::zvariant::Value>>("Lte")`,
   then manually extract each `rsrp` / `rsrq` / `rssi` / `snr` /
   `bler` key. Missing keys (older ModemManager < 1.2 lacks
   `bler`) must default to `NaN`; the OCC estimator then treats
   `NaN` as "treat as wireless, no L1". Do not panic on missing
   keys.

7. **P7 — `L1Metric::Bler` is fraction (0.0..=1.0), not percentage.**
   The ModemManager dictionary returns BLER as `0..=100` per the
   `org.freedesktop.ModemManager1.Modem.Signal` spec page. The
   `cellular::try_collect` function in §7 must divide by 100 before
   constructing `L1Metrics`. This is the most likely source of
   "OCC thinks the link is broken" bugs.

8. **P8 — `--congestion-controller=occ` requires ModemManager.** If
   the user forces OCC on a machine without ModemManager
   (e.g. desktop Linux with no modem), the router must print a
   clear warning and fall back to SCReAM. Add a `--l1-required`
   strict-mode flag later.

9. **P9 — `RouterConfig::default()` must be Send + Sync.** The router
   lives behind a `watch::Sender` in
   `apps/qubox-host-agent/src/rate_feedback.rs:21-22`. The trait
   `RateController` is `Send` (we don't need `Sync` because the
   router is owned by a single task), but the **whole router**
   must be `Send` so the tokio task can move it between threads.
   Each `*RateController` impl derives `Send`.

10. **P10 — `RateFeedback::bytes_acked` is missing today.** Adding
    it as `u32` (default 0) at `crates/qubox-proto/src/lib.rs:362`
    is wire-compatible only if the field is appended and tagged
    `#[serde(default)]`. PR-1 in §"Step-by-step implementation
    order" handles this. Without it, BBR v3's `BtlBw` filter never
    updates and the controller stays in Startup forever — the
    `bbr_v3_handles_50ms_rtt_correctly` test will hang.

---

## Verification commands

Per-PR commands (run from the workspace root):

```bash
# After PR-1 (proto field)
cargo build -p qubox-proto
cargo test  -p qubox-proto

# After PR-2 (file split)
cargo test -p qubox-host-agent rate_control

# After PR-3 (platform telemetry)
cargo test -p qubox-platform
# Linux only:
RUST_LOG=debug cargo run -p qubox-platform --example list_modems

# After PR-4 (SCReAM)
cargo test -p qubox-scream
RUST_LOG=qubox_scream=trace cargo test -p qubox-scream -- --nocapture

# After PR-5 (BBR v3)
cargo test -p qubox-bbr
# Critical assertion:
cargo test -p qubox-bbr bbr_v3_handles_50ms_rtt_correctly -- --nocapture

# After PR-6 (OCC)
cargo test -p qubox-occ

# After PR-7 (router + integration)
cargo test -p qubox-host-agent
RUST_LOG=qubox_host_agent::rate_control=info,debug \
  cargo run -p qubox-host-agent --bin qubox-host-agent -- --smoke-test \
  2>&1 | grep "congestion telemetry"

# After PR-8 (CLI flags)
cargo run -p qubox-host-agent --bin qubox-host-agent -- --help | grep congestion
cargo test -p qubox-host-agent router_force_override_wins

# Pre-merge CI gate (all PRs)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Telemetry filtering in production runs:

```bash
# All controller telemetry:
RUST_LOG=qubox_host_agent::rate_control=info

# Just the SCReAM controller internals:
RUST_LOG=qubox_scream=trace,qubox_host_agent::rate_control=debug

# Just the OCC L1 path:
RUST_LOG=qubox_platform::cellular=debug,qubox_occ=trace
```

---

## Consequences

### Positive

- **Wired broadband**: BBR v3 over Cubic — Google IETF 119 data:
  +45 % throughput on LTE-class links, +20 % on Wi-Fi, +120 % on
  satellite. Our BBR v3 sits at the app layer, but the encoder
  bitrate cap it produces feeds straight into ADR-013 frame pacing.
- **Wireless (home Wi-Fi)**: SCReAM v2 with L4S/ECN scaling — the
  2023 IEEE study reports +93 % throughput over original RFC 8298
  with L4S enabled, +48.6 % even without L4S
  (<https://pmc.ncbi.nlm.nih.gov/articles/PMC10675070/>).
  Conservative claim: expect 10–30 % median bitrate improvement
  over our current GCC at 1080p60 on a typical home Wi-Fi.
- **Cellular**: OCC-like estimator plus ModemManager telemetry.
  Realistic gain ~5–10 % over SCReAM under LTE mobility (we cannot
  hit the paper's full 18 % without BS-side data).
- **Algorithmic diversity**: `--congestion-controller=scream|bbr3|occ|gcc`
  on `apps/qubox-host-agent` gives the user a kill-switch when one
  algorithm misbehaves on a new network class.
- **Telemetry**: `CongestionTelemetry` is the data source for
  ADR-013 pacing, ADR-020 Pensieve RL state, and the
  `apps/qubox-client-gui` dashboard overlay. One schema serves
  three consumers.

### Negative / risk

- **Three algorithm implementations to maintain and test.**
  Mitigation: the `RateController` trait + the
  `apps/qubox-host-agent/src/rate_control/tests_router.rs` harness
  means a new algorithm needs only `impl RateController` plus
  `impl Default for ItsConfig` to plug in.
- **BBR patent encumbrance.** Google's BBR patents are licensed
  royalty-free for QUIC implementations
  (<https://datatracker.ietf.org/meeting/119/materials/slides-119-ccwg-bbrv3-overview-and-google-deployment-00>).
  Documented in `LICENSE-3rdparty.md`. Since our BBR v3 lives at
  the **app layer**, it does not touch QUIC's cwnd — the patent
  question is even smaller. We still document it.
- **OCC is platform-specific.** First release ships without OCC on
  macOS and Windows (treated as `Unknown` → SCReAM). Cellular
  telemetry path is Linux/Android only. Windows users fall back
  to SCReAM automatically.
- **Legacy GCC preserved during deprecation window.** The
  `LegacyGccRateController` is in `apps/qubox-host-agent/src/rate_control/legacy.rs`
  behind `#[allow(deprecated)]`. `--congestion-controller=gcc`
  emits a `tracing::warn!` at startup. GCC removed at 1.0 GA
  (PR-10).

### Roadmap mapping

- Supersedes P0-04 (current GCC controller).
- Lands before ADR-013 (frame pacing) because pacing consumes the
  bandwidth estimate.
- Required input for ADR-020 (Pensieve-style RL ABR) — RL state
  space includes the controller's instantaneous bandwidth estimate.
- Touches `crates/qubox-platform` (cellular L1 telemetry, §7) and
  `crates/qubox-proto` (one field added to `RateFeedback`).

### References

- RFC 8298 — SCReAM: <https://datatracker.ietf.org/doc/html/rfc8298>
- SCReAM v2 draft: <https://datatracker.ietf.org/doc/html/draft-johansson-ccwg-rfc8298bis-screamv2-05>
- SCReAM reference C++ impl (BSD-3-Clause):
  <https://github.com/EricssonResearch/scream>
- SCReAM v2 evaluation (queue delay −63 %, throughput +49 % w/o
  L4S; +93 % w/ L4S):
  <https://pmc.ncbi.nlm.nih.gov/articles/PMC10675070/>
- BBR v3 paper / IETF 119 slides:
  <https://datatracker.ietf.org/meeting/119/materials/slides-119-ccwg-bbrv3-overview-and-google-deployment-00>
- BBR v3 fundamentals (USC 2023 slides):
  <https://research.cec.sc.edu/files/cyberinfra/files/BBR%20-%20Fundamentals%20and%20Updates%202023-08-29.pdf>
- BBR v3 Google measurements (LTE +45 %, Wi-Fi +20 %, sat +120 % vs
  Cubic), summarised:
  <https://www.forasoft.com/learn/video-streaming/articles-streaming/congestion-control-bbr-cubic-copa>
- Ericsson MDPI 2024 (QUIC+BBRv3 bicasting, download time −23 %/−43 %):
  <https://www.mdpi.com/2673-4001/7/2/29>
- OCC: <https://arxiv.org/abs/2604.22383>
- OCC review: <https://www.themoonlight.io/fr/review/occ-physical-layer-assisted-congestion-control-for-real-time-communications>
- CQIC (cross-layer basis for our UE-side OCC estimator):
  <https://dl.acm.org/doi/pdf/10.1145/2699343.2699345>
- PBE-CC (related 5G cross-layer work, 6.3 % over BBR + 1.8× lower
  95th-percentile delay):
  <https://ar5iv.labs.arxiv.org/html/2002.03475>
- ModemManager D-Bus spec (`Modem.Signal`):
  <https://www.freedesktop.org/software/ModemManager/api/latest/gdbus-org.freedesktop.ModemManager1.Modem.Signal.html>
- `zbus` crate: <https://github.com/z-galaxy/zbus>
- Existing code anchors:
  - `apps/qubox-host-agent/src/rate_control.rs:46-64` `GccConfig`
  - `apps/qubox-host-agent/src/rate_control.rs:84-99` `GccRateController`
  - `apps/qubox-host-agent/src/rate_control.rs:138-229` `on_observation`
  - `crates/qubox-proto/src/lib.rs:362` `RateFeedback`
  - `apps/qubox-host-agent/src/rate_feedback.rs:36-80` caller
  - `research/roadmap/p0-04-adaptive-bitrate.md` original GCC rationale