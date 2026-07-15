use std::time::{Duration, Instant};

use super::telemetry::CongestionTelemetry;
use super::trait_def::RateController;

#[derive(Debug, Clone, Copy)]
pub struct BbrV3Config {
    pub min_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub start_bitrate_bps: u32,
    pub probe_rtt_interval: Duration,
    pub loss_rate_target: f64,
    pub cwnd_gain_probe_up: f64,
}

impl Default for BbrV3Config {
    fn default() -> Self {
        Self {
            min_bitrate_bps: 1_000_000,
            max_bitrate_bps: 1_000_000_000,
            start_bitrate_bps: 5_000_000,
            probe_rtt_interval: Duration::from_secs(5),
            loss_rate_target: 0.02,
            cwnd_gain_probe_up: 2.25,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum BbrMode {
    Startup,
    Drain,
    ProbeBwUp,
    ProbeBwDown,
    ProbeBwCruise,
    ProbeRtt,
}

pub struct BbrV3RateController {
    cfg: BbrV3Config,
    max_bw_bps: u64,
    min_rtt_ms: f64,
    last_probe_rtt: Option<Instant>,
    mode: BbrMode,
    target_bitrate_bps: u32,
    rtt_estimate_ms: f64,
    startup_rounds: u8,
    prev_inflight_growth: bool,
}

impl BbrV3RateController {
    pub fn new(cfg: BbrV3Config) -> Self {
        Self {
            target_bitrate_bps: cfg.start_bitrate_bps,
            max_bw_bps: 0,
            min_rtt_ms: f64::MAX,
            last_probe_rtt: None,
            mode: BbrMode::Startup,
            cfg,
            rtt_estimate_ms: 50.0,
            startup_rounds: 0,
            prev_inflight_growth: false,
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
        let rtt_ms = rtt.as_secs_f64() * 1000.0;
        self.rtt_estimate_ms = self.rtt_estimate_ms * 0.9 + rtt_ms * 0.1;

        if rtt_ms > 0.0 && rtt_ms < self.min_rtt_ms {
            self.min_rtt_ms = rtt_ms;
        }

        if sent_bytes > 0 {
            let bw_bps = ((sent_bytes as f64) / (self.rtt_estimate_ms / 1000.0)) * 8.0;
            if (bw_bps as u64) > self.max_bw_bps {
                self.max_bw_bps = bw_bps as u64;
            }
        }

        let pacing_gain = match self.mode {
            BbrMode::Startup => 2.885_f64,
            BbrMode::Drain => 1.0 / self.cfg.cwnd_gain_probe_up,
            BbrMode::ProbeBwUp => 1.25,
            BbrMode::ProbeBwDown => 0.75,
            BbrMode::ProbeBwCruise => 1.0,
            BbrMode::ProbeRtt => 1.0,
        };

        let loss_rate = loss_x1000 as f64 / 1000.0;
        if loss_rate > self.cfg.loss_rate_target && self.max_bw_bps > 0 {
            let reduction = 1.0 - (loss_rate - self.cfg.loss_rate_target).min(0.5);
            self.max_bw_bps = (self.max_bw_bps as f64 * reduction) as u64;
        }

        if let BbrMode::Startup = self.mode {
            self.startup_rounds += 1;
            let growth = sent_bytes > 0 && bw_bps_growth_ok(self.startup_rounds);
            if self.prev_inflight_growth && !growth && self.startup_rounds >= 3 {
                self.mode = BbrMode::Drain;
            }
            self.prev_inflight_growth = growth;
        }

        if let BbrMode::Drain = self.mode {
            self.mode = BbrMode::ProbeBwUp;
        }

        if self.last_probe_rtt.map_or(true, |t| {
            now.saturating_duration_since(t) >= self.cfg.probe_rtt_interval
        }) {
            self.mode = BbrMode::ProbeRtt;
            self.last_probe_rtt = Some(now);
        }

        if let BbrMode::ProbeRtt = self.mode {
            if now.saturating_duration_since(self.last_probe_rtt.unwrap())
                >= Duration::from_millis(200)
            {
                self.mode = BbrMode::ProbeBwUp;
            }
        }

        let pace_bps = (self.max_bw_bps as f64 * pacing_gain) as u64;
        self.target_bitrate_bps =
            (pace_bps as u32).clamp(self.cfg.min_bitrate_bps, self.cfg.max_bitrate_bps);

        self.target_bitrate_bps
    }

    fn snapshot(&self) -> CongestionTelemetry {
        CongestionTelemetry {
            algorithm: self.algorithm_name(),
            target_bitrate_bps: self.target_bitrate_bps,
            max_bandwidth_bps: Some(self.max_bw_bps),
            min_rtt_ms: if self.min_rtt_ms.is_finite() {
                Some(self.min_rtt_ms)
            } else {
                None
            },
            state: Some(format!("{:?}", self.mode)),
            ..Default::default()
        }
    }

    fn current_bitrate_bps(&self) -> u32 {
        self.target_bitrate_bps
    }

    fn algorithm_name(&self) -> &'static str {
        "bbr-v3"
    }
}

fn bw_bps_growth_ok(_round: u8) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk() -> BbrV3RateController {
        BbrV3RateController::new(BbrV3Config::default())
    }

    #[test]
    fn bbr_v3_accepts_observations() {
        let mut c = mk();
        let t0 = Instant::now();
        let mut last = 0;
        let mut stable = 0;
        for i in 0..200 {
            let bps = c.on_observation(
                0.0,
                0,
                Duration::from_millis(50),
                1500,
                t0 + Duration::from_millis(250 * i),
            );
            if i > 0 && ((bps as i64 - last as i64).abs() < (last as i64) / 20) {
                stable += 1;
            }
            last = bps;
        }
        assert!(
            stable >= 100,
            "BBR v3 should have stable periods; got {stable}"
        );
    }

    #[test]
    fn bbr_v3_trait_object_dispatch() {
        let mut c: Box<dyn RateController> = Box::new(mk());
        let bps = c.on_observation(0.0, 0, Duration::from_millis(50), 1500, Instant::now());
        assert_eq!(c.algorithm_name(), "bbr-v3");
        assert!(bps >= 1_000_000);
        let snap = c.snapshot();
        assert_eq!(snap.algorithm, "bbr-v3");
    }

    #[test]
    fn bbr_v3_sensible_defaults() {
        let cfg = BbrV3Config::default();
        assert_eq!(cfg.probe_rtt_interval, Duration::from_secs(5));
        assert!((cfg.cwnd_gain_probe_up - 2.25).abs() < 1e-9);
    }
}
