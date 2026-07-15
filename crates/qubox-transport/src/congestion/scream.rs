use std::time::{Duration, Instant};

use super::telemetry::CongestionTelemetry;
use super::trait_def::{L1Metric, RateController};

#[derive(Debug, Clone, Copy)]
pub struct ScreamConfig {
    pub min_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub start_bitrate_bps: u32,
    pub qdelay_target_ms: f64,
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

pub struct ScreamRateController {
    cfg: ScreamConfig,
    cwnd_bytes: u64,
    qdelay_ms_ewma: f64,
    bytes_in_flight_5s: u64,
    last_observation: Option<Instant>,
    target_bitrate_bps: u32,
    rtt_estimate_ms: f64,
}

impl ScreamRateController {
    pub fn new(cfg: ScreamConfig) -> Self {
        Self {
            target_bitrate_bps: cfg.start_bitrate_bps,
            cfg,
            cwnd_bytes: cfg.start_bitrate_bps as u64 / 10,
            qdelay_ms_ewma: 0.0,
            bytes_in_flight_5s: 0,
            last_observation: None,
            rtt_estimate_ms: 40.0,
        }
    }
}

impl RateController for ScreamRateController {
    fn on_observation(
        &mut self,
        owd_ms: f64,
        loss_x1000: u16,
        rtt: Duration,
        sent_bytes: u64,
        now: Instant,
    ) -> u32 {
        let rtt_ms = rtt.as_secs_f64() * 1000.0;
        self.rtt_estimate_ms = self.rtt_estimate_ms * 0.9 + rtt_ms * 0.1;

        self.bytes_in_flight_5s = self.bytes_in_flight_5s.saturating_add(sent_bytes);

        if self.last_observation.map_or(true, |t| {
            now.saturating_duration_since(t).as_secs_f64() > 5.0
        }) {
            self.bytes_in_flight_5s = 0;
        }

        let alpha = self.cfg.qdelay_ewma_alpha;
        self.qdelay_ms_ewma = if self.qdelay_ms_ewma == 0.0 {
            owd_ms
        } else {
            (1.0 - alpha) * self.qdelay_ms_ewma + alpha * owd_ms
        };

        let srtt_ms = self.rtt_estimate_ms.max(10.0);

        if loss_x1000 > 20 {
            let factor = 0.7_f64;
            self.cwnd_bytes = (self.cwnd_bytes as f64 * factor) as u64;
        } else if self.qdelay_ms_ewma > self.cfg.qdelay_target_ms {
            let factor = 0.95_f64;
            self.cwnd_bytes = (self.cwnd_bytes as f64 * factor) as u64;
        } else {
            self.cwnd_bytes = self.cwnd_bytes.saturating_add(1500);
        }

        let min_cwnd = ((self.cfg.min_bitrate_bps as f64 / 8.0) * (srtt_ms / 1000.0)) as u64;
        let max_cwnd = ((self.cfg.max_bitrate_bps as f64 / 8.0) * (srtt_ms / 1000.0)) as u64;
        self.cwnd_bytes = self.cwnd_bytes.clamp(min_cwnd, max_cwnd.max(min_cwnd));

        let bps = ((self.cwnd_bytes as f64) / (srtt_ms / 1000.0)) * 8.0;
        self.target_bitrate_bps =
            (bps as u32).clamp(self.cfg.min_bitrate_bps, self.cfg.max_bitrate_bps);

        self.last_observation = Some(now);
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

    fn current_bitrate_bps(&self) -> u32 {
        self.target_bitrate_bps
    }

    fn algorithm_name(&self) -> &'static str {
        "scream-v2"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk() -> ScreamRateController {
        ScreamRateController::new(ScreamConfig::default())
    }

    #[test]
    fn scream_accepts_observation() {
        let mut c = mk();
        let t0 = Instant::now();
        let mut max_bps = 0u32;
        for i in 0..100 {
            let bps = c.on_observation(
                20.0 + (i as f64) * 0.05,
                0,
                Duration::from_millis(40),
                1500,
                t0 + Duration::from_millis(20 * i),
            );
            assert!(bps >= 500_000 && bps <= 50_000_000);
            max_bps = max_bps.max(bps);
        }
        // Under zero-loss, 40ms RTT, SCReAM probes to max_bitrate.
        // After 100 observations cwnd saturates at max (50 Mbps).
        assert_eq!(
            max_bps, 50_000_000,
            "SCReAM should probe to max_bitrate under ideal conditions, got {max_bps}"
        );
    }

    #[test]
    fn scream_loss_triggers_cwnd_cut() {
        let mut c = mk();
        let t0 = Instant::now();
        let mut pre_loss = 0;
        for i in 0..15 {
            let bps = c.on_observation(
                20.0,
                0,
                Duration::from_millis(40),
                1500,
                t0 + Duration::from_millis(20 * i),
            );
            if i == 10 {
                pre_loss = bps;
            }
        }
        let post_loss = c.on_observation(
            30.0,
            50,
            Duration::from_millis(40),
            1500,
            t0 + Duration::from_millis(300),
        );
        assert!(
            (post_loss as f64) <= (pre_loss as f64) * 0.75,
            "loss > 2% should cut cwnd: pre={pre_loss} post={post_loss}"
        );
    }

    #[test]
    fn scream_qdelay_shrinks_cwnd() {
        let mut c = ScreamRateController::new(ScreamConfig {
            qdelay_target_ms: 60.0,
            ..Default::default()
        });
        let t0 = Instant::now();
        let mut prev = u32::MAX;
        let mut decreased = false;
        for i in 0..30 {
            let bps = c.on_observation(
                100.0,
                0,
                Duration::from_millis(40),
                1500,
                t0 + Duration::from_millis(20 * i),
            );
            if prev != u32::MAX && bps < prev {
                decreased = true;
            }
            prev = bps;
        }
        assert!(decreased, "qdelay above target should shrink cwnd");
    }

    #[test]
    fn scream_trait_object_dispatch() {
        let mut c: Box<dyn RateController> = Box::new(mk());
        let bps = c.on_observation(20.0, 0, Duration::from_millis(20), 1500, Instant::now());
        assert_eq!(c.algorithm_name(), "scream-v2");
        assert!(bps >= 500_000);
        let snap = c.snapshot();
        assert_eq!(snap.algorithm, "scream-v2");
    }
}
