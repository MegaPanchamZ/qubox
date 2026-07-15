//! P0-4 GCC delay-based adaptive bitrate controller.
//!
//! Implements a WebRTC-GCC-style controller that maps one-way-delay
//! (OWD) trends and loss measurements to a target encoder bitrate.
//! The controller runs on the host and is queried once per frame to
//! decide the `-b:v` argument for the next ffmpeg restart (HW encoders
//! that support runtime bitrate change can use `target_bitrate_bps`
//! directly; software encoders need a re-spawn).
//!
//! ## Algorithm
//!
//! Per P0-4 spec research/roadmap/p0-04-adaptive-bitrate.md:
//!
//! - Maintain an EWMA of OWD (`α = 0.9`).
//! - Compute the OWD gradient (delta between consecutive OWD samples).
//! - Map gradient → `OveruseState`:
//!     * gradient > `THRESHOLD_OVERUSE`  → `Overuse`  (back off)
//!     * gradient < `THRESHOLD_UNDERUSE` → `Underuse` (probe up)
//!     * otherwise                      → `Normal`   (hold)
//! - Adjust target bitrate with multiplicative increase / decrease
//!   bounds; clamp to `[min_bitrate_bps, max_bitrate_bps]`.
//! - Enforce a minimum reaction time of 250ms between bitrate
//!   changes (avoid oscillation).
//! - **Panic** mode: if OWD > 200ms or loss > 20%, drop to
//!   `min_bitrate_bps` immediately and freeze for 1s.
//! - **Fast start**: when the first sample arrives, start at
//!   1 Mbps and ramp up by 200 kbps per reaction interval (until
//!   either the first `Overuse` or the ceiling).
//!
//! The controller is stateless across sessions (it owns its state
//! internally); callers pass the current `rtt`, `loss_x1000` (in
//! parts-per-thousand, where 1000 = 100%), and a per-packet
//! `owd_ms` measurement, then read the next target bitrate.

use std::time::{Duration, Instant};

/// Overuse detector state (WebRTC-GCC terminology).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OveruseState {
    Normal,
    Overuse,
    Underuse,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GccConfig {
    pub min_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub start_bitrate_bps: u32,
    /// Multiplicative decrease factor on `Overuse` (e.g. 0.85).
    pub decrease_factor_q12: u32,
    /// Additive increase on `Underuse`, in bps.
    pub increase_step_bps: u32,
    /// Fast-start additive increase, in bps.
    pub fast_start_step_bps: u32,
    /// Minimum interval between bitrate changes.
    pub min_reaction: Duration,
    /// Panic mode: drop to min on OWD > this, ms.
    pub panic_owd_ms: f64,
    /// Panic mode: drop to min on loss > this, fraction (0.0..=1.0).
    pub panic_loss: f64,
    /// Panic mode freeze duration after a panic drop.
    pub panic_freeze: Duration,
}

impl Default for GccConfig {
    fn default() -> Self {
        Self {
            min_bitrate_bps: 500_000,
            max_bitrate_bps: 20_000_000,
            start_bitrate_bps: 1_000_000,
            decrease_factor_q12: (0.85_f64 * 4096.0) as u32,
            increase_step_bps: 200_000,
            fast_start_step_bps: 200_000,
            min_reaction: Duration::from_millis(250),
            panic_owd_ms: 200.0,
            panic_loss: 0.20,
            panic_freeze: Duration::from_secs(1),
        }
    }
}

#[derive(Debug)]
pub struct GccRateController {
    cfg: GccConfig,
    last_owd_ms: Option<f64>,
    last_change: Option<Instant>,
    target_bitrate_bps: u32,
    in_fast_start: bool,
    panic_until: Option<Instant>,
    last_state: OveruseState,
    /// EWMA of the OWD for smoother gradient estimation.
    /// Updated by each call to `on_observation` from the raw `owd_ms`
    /// argument (which is itself expected to be the *trend* OWD,
    /// i.e. relative to a baseline). The previous value is kept in
    /// `prev_owd_ewma_ms` so `classify` can compute the EWMA gradient.
    owd_ewma_ms: f64,
    prev_owd_ewma_ms: f64,
}

impl GccRateController {
    pub fn new(cfg: GccConfig) -> Self {
        Self {
            target_bitrate_bps: cfg.start_bitrate_bps,
            cfg,
            last_owd_ms: None,
            last_change: None,
            in_fast_start: true,
            panic_until: None,
            last_state: OveruseState::Normal,
            owd_ewma_ms: 0.0,
            prev_owd_ewma_ms: 0.0,
        }
    }

    pub fn current_bitrate_bps(&self) -> u32 {
        self.target_bitrate_bps
    }

    pub fn current_state(&self) -> OveruseState {
        self.last_state
    }

    /// Current EWMA of the trend OWD (ms). Exposed for tests and for
    /// callers that want to publish the smoothed value alongside the
    /// raw `owd_ms` they passed in.
    pub fn owd_ewma_ms(&self) -> f64 {
        self.owd_ewma_ms
    }

    /// Feed one observation and return the new target bitrate.
    /// `owd_ms` is the **trend** OWD in milliseconds — i.e. the
    /// growth above the path-propagation baseline (positive when the
    /// link is queueing, negative when it is draining). `loss_x1000`
    /// is the loss fraction in parts-per-thousand (e.g. 50 = 5%).
    /// `rtt` is the QUIC connection RTT (unused today; reserved for
    /// future cross-checking).
    pub fn on_observation(
        &mut self,
        owd_ms: f64,
        loss_x1000: u16,
        _rtt: Duration,
        now: Instant,
    ) -> u32 {
        // Update EWMA first. Keep the *previous* value for the
        // gradient classifier — comparing two EWMA samples is much
        // less noisy than comparing two raw samples (which is what
        // the legacy code did).
        self.prev_owd_ewma_ms = self.owd_ewma_ms;
        self.owd_ewma_ms = 0.9 * self.owd_ewma_ms + 0.1 * owd_ms;

        // Panic mode: drop to min and freeze.
        let loss_fraction = f64::from(loss_x1000) / 1000.0;
        if owd_ms > self.cfg.panic_owd_ms || loss_fraction > self.cfg.panic_loss {
            self.target_bitrate_bps = self.cfg.min_bitrate_bps;
            self.panic_until = Some(now + self.cfg.panic_freeze);
            self.in_fast_start = false;
            self.last_state = OveruseState::Overuse;
            self.last_change = Some(now);
            self.last_owd_ms = Some(owd_ms);
            return self.target_bitrate_bps;
        }

        // Honor panic freeze.
        if let Some(until) = self.panic_until {
            if now < until {
                self.last_owd_ms = Some(owd_ms);
                return self.target_bitrate_bps;
            }
            self.panic_until = None;
        }

        // Reaction gate: don't change bitrate more often than min_reaction.
        let allow_change = match self.last_change {
            Some(last) => now.duration_since(last) >= self.cfg.min_reaction,
            None => true,
        };

        // Compute gradient → state from the EWMA samples. Comparing
        // two EWMA samples is more robust than comparing two raw
        // samples because single-frame jitter is filtered out.
        let new_state = self.classify(owd_ms);
        self.last_state = new_state;

        if !allow_change {
            self.last_owd_ms = Some(owd_ms);
            return self.target_bitrate_bps;
        }

        // Fast start: while we have not seen Overuse, ramp up additively.
        if self.in_fast_start {
            match new_state {
                OveruseState::Overuse => {
                    // End of fast start; apply multiplicative decrease.
                    self.in_fast_start = false;
                    let factor = f64::from(self.cfg.decrease_factor_q12) / 4096.0;
                    self.target_bitrate_bps = ((self.target_bitrate_bps as f64) * factor) as u32;
                }
                _ => {
                    // Probe up.
                    self.target_bitrate_bps = (self.target_bitrate_bps
                        + self.cfg.fast_start_step_bps)
                        .min(self.cfg.max_bitrate_bps);
                }
            }
        } else {
            match new_state {
                OveruseState::Overuse => {
                    let factor = f64::from(self.cfg.decrease_factor_q12) / 4096.0;
                    self.target_bitrate_bps = ((self.target_bitrate_bps as f64) * factor) as u32;
                }
                OveruseState::Underuse => {
                    self.target_bitrate_bps = (self.target_bitrate_bps
                        + self.cfg.increase_step_bps)
                        .min(self.cfg.max_bitrate_bps);
                }
                OveruseState::Normal => {
                    // Hold.
                }
            }
        }

        self.target_bitrate_bps = self
            .target_bitrate_bps
            .clamp(self.cfg.min_bitrate_bps, self.cfg.max_bitrate_bps);
        self.last_change = Some(now);
        self.last_owd_ms = Some(owd_ms);
        self.target_bitrate_bps
    }

    fn classify(&self, owd_ms: f64) -> OveruseState {
        // Use the EWMA gradient instead of the raw delta. The legacy
        // code computed `(owd_ms - prev_owd_ms)` where both values
        // could spike on a single frame; the EWMA already smooths
        // single-frame noise, so the gradient between two adjacent
        // EWMA samples is the right signal to classify.
        if self.owd_ewma_ms.abs() < 1e-9 && self.prev_owd_ewma_ms.abs() < 1e-9 {
            // EWMA is still at its initial 0 → no history yet.
            return OveruseState::Normal;
        }
        let gradient = self.owd_ewma_ms - self.prev_owd_ewma_ms;
        // Thresholds derived from WebRTC-GCC defaults: 12.5 ms/s for
        // overuse, -12.5 ms/s for underuse. We scale by 250 ms reaction
        // interval so the per-sample threshold is ~3.0 ms. Using the
        // EWMA gradient means single-frame spikes are already absorbed.
        if gradient > 3.0 {
            OveruseState::Overuse
        } else if gradient < -3.0 {
            OveruseState::Underuse
        } else {
            OveruseState::Normal
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk() -> GccRateController {
        GccRateController::new(GccConfig {
            min_bitrate_bps: 500_000,
            max_bitrate_bps: 10_000_000,
            start_bitrate_bps: 1_000_000,
            decrease_factor_q12: (0.85 * 4096.0) as u32,
            increase_step_bps: 200_000,
            fast_start_step_bps: 200_000,
            min_reaction: Duration::from_millis(250),
            panic_owd_ms: 200.0,
            panic_loss: 0.20,
            panic_freeze: Duration::from_secs(1),
        })
    }

    #[test]
    fn fast_start_ramps_up_under_normal() {
        let mut c = mk();
        let now = Instant::now();
        let mut last = c.current_bitrate_bps();
        // 5 observations, all Normal, reaction interval respected.
        for i in 0..5 {
            let now = now + Duration::from_millis(250 * (i + 1));
            let b = c.on_observation(30.0 + (i as f64) * 0.1, 0, Duration::from_millis(20), now);
            assert!(
                b >= last,
                "fast-start should monotonically increase: last={last} b={b}"
            );
            last = b;
        }
        assert!(c.current_bitrate_bps() >= 1_000_000);
    }

    #[test]
    fn overuse_applies_multiplicative_decrease() {
        let mut c = mk();
        let now = Instant::now();
        // First call (fast start, Normal, no prior OWD): probe up.
        let _ = c.on_observation(30.0, 0, Duration::from_millis(20), now);
        // Second call (fast start, gradient=0, Normal): probe up.
        let before = c.on_observation(
            30.0,
            0,
            Duration::from_millis(20),
            now + Duration::from_millis(250),
        );
        assert!(
            before > 1_000_000,
            "fast-start should have ramped up: {before}"
        );
        // Third call: gradient +20ms → Overuse, exit fast start, apply
        // 0.85x multiplicative decrease.
        let after = c.on_observation(
            50.0,
            0,
            Duration::from_millis(20),
            now + Duration::from_millis(500),
        );
        assert!(
            after < before,
            "overuse should reduce bitrate: before={before} after={after}"
        );
    }

    #[test]
    fn panic_drops_to_min_immediately() {
        let mut c = mk();
        let now = Instant::now();
        let _ = c.on_observation(30.0, 0, Duration::from_millis(20), now);
        let b = c.on_observation(
            300.0,
            0,
            Duration::from_millis(20),
            now + Duration::from_millis(250),
        );
        assert_eq!(b, 500_000);
        assert_eq!(c.current_state(), OveruseState::Overuse);
    }

    #[test]
    fn panic_on_high_loss() {
        let mut c = mk();
        let now = Instant::now();
        // 30% loss, OWD fine.
        let b = c.on_observation(30.0, 300, Duration::from_millis(20), now);
        assert_eq!(b, 500_000);
    }

    #[test]
    fn reaction_gate_swallows_rapid_changes() {
        let mut c = mk();
        let now = Instant::now();
        // First call sets baseline at min_reaction.
        let b1 = c.on_observation(30.0, 0, Duration::from_millis(20), now);
        // Immediately after: OWD spikes, but reaction gate blocks.
        let b2 = c.on_observation(
            80.0,
            0,
            Duration::from_millis(20),
            now + Duration::from_millis(10),
        );
        assert_eq!(b1, b2, "reaction gate should swallow rapid change");
    }

    /// Simulates the 1 Hz external coalescing: the controller allows a
    /// change after its internal 250 ms reaction gate, but the
    /// rate-feedback loop only emits when 1 s has elapsed since the last
    /// emit.  This test verifies the controller is *ready* to produce a
    /// new value after the reaction gate, which is the prerequisite for
    /// the coalescing layer to work.
    #[test]
    fn controller_is_ready_after_reaction_gate() {
        let mut c = mk();
        let now = Instant::now();
        // Three rapid calls that the controller *would* process if the
        // external coalescing allowed (the controller's own gate is
        // 250 ms, so every 250 ms call is accepted).
        let b1 = c.on_observation(30.0, 0, Duration::from_millis(20), now);
        let b2 = c.on_observation(
            50.0,
            0,
            Duration::from_millis(20),
            now + Duration::from_millis(250),
        );
        let b3 = c.on_observation(
            60.0,
            0,
            Duration::from_millis(20),
            now + Duration::from_millis(500),
        );
        // b2 is a spike (gradient +20 → Overuse) so it should be lower
        // than b1. b3 is a further increase (gradient +10 → Overuse) so
        // it should be lower than b2. If the controller's reaction gate
        // were blocking, b2 or b3 would equal b1.
        assert!(
            b2 < b1,
            "overuse at 250ms should decrease rate: {b2} >= {b1}"
        );
        assert!(
            b3 < b2,
            "continued overuse at 500ms should decrease further: {b3} >= {b2}"
        );
    }

    /// Under stable conditions (constant OWD, no loss) after fast-start
    /// exits and an Overuse is processed, the bitrate should stabilise.
    /// The coalescing layer relies on this stability to avoid needless
    /// ffmpeg restarts during steady-state.
    #[test]
    fn stable_owd_keeps_bitrate_constant() {
        let mut c = mk();
        let now = Instant::now();
        // Two fast-start probes.
        let _ = c.on_observation(30.0, 0, Duration::from_millis(20), now);
        let b1 = c.on_observation(
            30.0,
            0,
            Duration::from_millis(20),
            now + Duration::from_millis(250),
        );
        // Overuse spike: exits fast-start and applies 0.85x decrease.
        let b2 = c.on_observation(
            80.0,
            0,
            Duration::from_millis(20),
            now + Duration::from_millis(500),
        );
        assert!(b2 < b1, "overuse should reduce bitrate: {b2} >= {b1}");
        // Stable OWD at 80 ms. The new EWMA-based classifier still
        // sees a positive gradient for a few samples while the EWMA
        // catches up to the new level, so we feed many samples
        // (≈50 with α=0.9 to reach ≥99% of the new steady state)
        // before checking that the bitrate has stabilised.
        let mut last_bps = b2;
        let mut stable_count = 0_u32;
        for i in 0..60 {
            let b = c.on_observation(
                80.0,
                0,
                Duration::from_millis(20),
                now + Duration::from_millis(750 + i * 250),
            );
            if b == last_bps {
                stable_count += 1;
                if stable_count >= 3 {
                    // Converged: EWMA matches the new steady state,
                    // gradient ≈ 0, no more bitrate changes.
                    return;
                }
            } else {
                stable_count = 0;
                last_bps = b;
            }
        }
        panic!("EWMA did not converge after 60 stable OWD samples; last_bps={last_bps}");
    }

    /// EWMA-based gradient must produce a stable Overuse state when
    /// the OWD has a *step* (sudden jump from 30 → 70 ms and held).
    /// The raw delta would also classify as Overuse immediately; the
    /// EWMA ramp absorbs the step but should still reach Overuse
    /// after a few samples because the step is large.
    #[test]
    fn gcc_classify_uses_ewma_gradient() {
        let mut c = mk();
        let now = Instant::now();
        // Warm-up with steady low OWD to seed the EWMA. With α=0.9
        // we need ~50 samples for the EWMA to converge within 1 % of
        // the steady state.
        for i in 0..60 {
            c.on_observation(
                30.0,
                0,
                Duration::from_millis(20),
                now + Duration::from_millis(i * 50),
            );
        }
        // The EWMA should now be ~30.
        let ewma_pre = c.owd_ewma_ms();
        assert!(
            (ewma_pre - 30.0).abs() < 0.5,
            "EWMA should converge to 30, got {ewma_pre}"
        );
        // Step OWD up to 70 ms and feed enough samples for the EWMA
        // to ramp past the Overuse threshold (gradient > 3 ms between
        // two EWMA samples). With α=0.9 each step adds 0.1 * Δ to
        // the EWMA, so a 40 ms step gives a +4 ms gradient — which
        // is above the 3 ms Overuse threshold.
        let mut overuse_seen = false;
        for i in 0..30 {
            let _ = c.on_observation(
                70.0,
                0,
                Duration::from_millis(20),
                now + Duration::from_millis(1000 + i * 50),
            );
            if c.current_state() == OveruseState::Overuse {
                overuse_seen = true;
                break;
            }
        }
        assert!(
            overuse_seen,
            "EWMA gradient classifier should detect the +40 ms step as Overuse"
        );
    }

    /// Flat OWD (no growth, no shrinkage) must keep the controller
    /// in `Normal` even after many samples. The previous
    /// (raw-delta) classifier could erroneously trigger Overuse on a
    /// single jittery frame; the EWMA gradient should be near zero
    /// when the trend is flat.
    #[test]
    fn gcc_classify_flat_owd_stays_normal() {
        let mut c = mk();
        let now = Instant::now();
        // Feed a flat 40 ms OWD for many samples (long enough to exit
        // fast-start on the first call and converge the EWMA).
        let mut state = OveruseState::Normal;
        for i in 0..50 {
            let _ = c.on_observation(
                40.0,
                0,
                Duration::from_millis(20),
                now + Duration::from_millis(250 * (i + 1)),
            );
            state = c.current_state();
        }
        assert_eq!(
            state,
            OveruseState::Normal,
            "flat OWD must keep state = Normal, got {state:?}"
        );
    }
}
