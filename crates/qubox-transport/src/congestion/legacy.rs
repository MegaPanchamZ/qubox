use std::time::{Duration, Instant};

use super::telemetry::CongestionTelemetry;
use super::trait_def::RateController;

#[derive(Debug, Clone, Copy)]
pub struct GccConfig {
    pub min_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub start_bitrate_bps: u32,
}

impl Default for GccConfig {
    fn default() -> Self {
        Self {
            min_bitrate_bps: 300_000,
            max_bitrate_bps: 50_000_000,
            start_bitrate_bps: 1_500_000,
        }
    }
}

pub struct LegacyGccRateController {
    cfg: GccConfig,
    target_bitrate_bps: u32,
    owd_ewma_ms: f64,
    prev_owd_ewma_ms: f64,
}

impl LegacyGccRateController {
    pub fn new(cfg: GccConfig) -> Self {
        Self {
            target_bitrate_bps: cfg.start_bitrate_bps,
            owd_ewma_ms: 0.0,
            prev_owd_ewma_ms: 0.0,
            cfg,
        }
    }
}

impl RateController for LegacyGccRateController {
    fn on_observation(
        &mut self,
        owd_ms: f64,
        loss_x1000: u16,
        _rtt: Duration,
        _sent_bytes: u64,
        _now: Instant,
    ) -> u32 {
        self.prev_owd_ewma_ms = self.owd_ewma_ms;
        let alpha = 0.1;
        if self.owd_ewma_ms == 0.0 {
            self.owd_ewma_ms = owd_ms;
        } else {
            self.owd_ewma_ms = (1.0 - alpha) * self.owd_ewma_ms + alpha * owd_ms;
        }

        let gradient = self.owd_ewma_ms - self.prev_owd_ewma_ms;

        if loss_x1000 > 20 {
            self.target_bitrate_bps =
                ((self.target_bitrate_bps as f64) * 0.85) as u32;
        } else if gradient > 5.0 {
            let decrease = ((self.target_bitrate_bps as f64) * 0.90) as u32;
            self.target_bitrate_bps = decrease.max(self.cfg.min_bitrate_bps);
        } else if gradient < -1.0 {
            let increase = ((self.target_bitrate_bps as f64) * 1.05) as u32;
            self.target_bitrate_bps = increase.min(self.cfg.max_bitrate_bps);
        }

        self.target_bitrate_bps
    }

    fn snapshot(&self) -> CongestionTelemetry {
        CongestionTelemetry {
            algorithm: self.algorithm_name(),
            target_bitrate_bps: self.target_bitrate_bps,
            owd_ms_ewma: Some(self.owd_ewma_ms),
            state: Some("legacy-gcc".into()),
            ..Default::default()
        }
    }

    fn current_bitrate_bps(&self) -> u32 {
        self.target_bitrate_bps
    }

    fn algorithm_name(&self) -> &'static str {
        "gcc-legacy"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk() -> LegacyGccRateController {
        LegacyGccRateController::new(GccConfig::default())
    }

    #[test]
    fn legacy_gcc_implements_rate_controller_trait() {
        let mut c: Box<dyn RateController> = Box::new(mk());
        let bps = c.on_observation(20.0, 0, Duration::from_millis(20), 1500, Instant::now());
        assert_eq!(c.algorithm_name(), "gcc-legacy");
        assert!(bps >= 300_000);
        let snap = c.snapshot();
        assert_eq!(snap.algorithm, "gcc-legacy");
    }

    #[test]
    fn legacy_gcc_reduces_on_loss() {
        let mut c = mk();
        let baseline = c.on_observation(10.0, 0, Duration::from_millis(20), 1500, Instant::now());
        let after_loss = c.on_observation(15.0, 50, Duration::from_millis(25), 1500, Instant::now());
        assert!(after_loss < baseline, "GCC should reduce bitrate on loss");
    }
}
