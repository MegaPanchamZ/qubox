use std::time::{Duration, Instant};

use super::telemetry::CongestionTelemetry;
use super::trait_def::{L1Metric, RateController};

#[derive(Debug, Clone, Copy)]
pub struct OccConfig {
    pub min_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub start_bitrate_bps: u32,
    pub sinr_floor_db: f32,
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

#[derive(Default, Clone, Copy)]
struct L1Snapshot {
    rsrp: f32,
    rsrq: f32,
    rssi: f32,
    sinr: f32,
    bler: f32,
}

pub struct OccRateController {
    cfg: OccConfig,
    l1: Option<L1Snapshot>,
    abw_bps: u64,
    target_bitrate_bps: u32,
    last_abw_update: Option<Instant>,
}

impl OccRateController {
    pub fn new(cfg: OccConfig) -> Self {
        Self {
            target_bitrate_bps: cfg.start_bitrate_bps,
            cfg,
            l1: None,
            abw_bps: 0,
            last_abw_update: None,
        }
    }

    fn estimate_abw(&self) -> u64 {
        let Some(l1) = &self.l1 else {
            return 0;
        };

        if l1.sinr < self.cfg.sinr_floor_db {
            return self.cfg.min_bitrate_bps as u64;
        }

        let sinr_linear = 10_f32.powf(l1.sinr / 10.0);
        let bw_hz = 20_000_000_f32;
        let capacity_bps = (bw_hz * (1.0 + sinr_linear).log2()) as u64;
        let abw = capacity_bps / 8;

        let mut abw = if l1.bler > self.cfg.bler_cap {
            let cap = self.cfg.bler_cap / l1.bler.max(0.01);
            (abw as f32 * cap) as u64
        } else {
            abw
        };

        abw.clamp(
            self.cfg.min_bitrate_bps as u64,
            self.cfg.max_bitrate_bps as u64,
        )
    }
}

impl RateController for OccRateController {
    fn on_observation(
        &mut self,
        _owd_ms: f64,
        loss_x1000: u16,
        _rtt: Duration,
        sent_bytes: u64,
        now: Instant,
    ) -> u32 {
        if self.l1.is_some() {
            self.abw_bps = self.estimate_abw();
            self.target_bitrate_bps = self.abw_bps as u32;
        }

        if loss_x1000 == 0
            && self.last_abw_update.map_or(true, |t| {
                now.saturating_duration_since(t) >= Duration::from_secs(5)
            })
        {
            let probe_up = (self.target_bitrate_bps as f64 * 1.05) as u32;
            self.target_bitrate_bps = probe_up.min(self.cfg.max_bitrate_bps);
            self.last_abw_update = Some(now);
        }

        self.target_bitrate_bps
    }

    fn on_l1_metric(&mut self, metric: L1Metric) {
        let l1 = self.l1.get_or_insert_with(L1Snapshot::default);
        match metric {
            L1Metric::Rsrp(v) => l1.rsrp = v,
            L1Metric::Rsrq(v) => l1.rsrq = v,
            L1Metric::Rssi(v) => l1.rssi = v,
            L1Metric::Sinr(v) => l1.sinr = v,
            L1Metric::Bler(v) => l1.bler = v,
        }
    }

    fn snapshot(&self) -> CongestionTelemetry {
        CongestionTelemetry {
            algorithm: self.algorithm_name(),
            target_bitrate_bps: self.target_bitrate_bps,
            abw_bps: Some(self.abw_bps),
            l1_rsrp_dbm: self.l1.map(|l| l.rsrp),
            l1_rsrq_db: self.l1.map(|l| l.rsrq),
            l1_rssi_dbm: self.l1.map(|l| l.rssi),
            l1_sinr_db: self.l1.map(|l| l.sinr),
            l1_bler: self.l1.map(|l| l.bler),
            ..Default::default()
        }
    }

    fn current_bitrate_bps(&self) -> u32 {
        self.target_bitrate_bps
    }

    fn algorithm_name(&self) -> &'static str {
        "occ"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::congestion::trait_def::L1Metric as LM;

    fn mk() -> OccRateController {
        OccRateController::new(OccConfig::default())
    }

    #[test]
    fn occ_uses_l1_metrics_when_available() {
        let mut c = mk();
        c.on_l1_metric(LM::Sinr(15.0));
        c.on_l1_metric(LM::Bler(0.05));
        let bps = c.on_observation(0.0, 0, Duration::from_millis(80), 1500, Instant::now());
        assert!(
            (10_000_000..=50_000_000).contains(&bps),
            "OCC should estimate 10-50 Mbps at SINR=15, got {bps}"
        );
    }

    #[test]
    fn occ_falls_back_below_sinr_floor() {
        let mut c = OccRateController::new(OccConfig {
            sinr_floor_db: -3.0,
            ..Default::default()
        });
        c.on_l1_metric(LM::Sinr(-10.0));
        let bps = c.on_observation(0.0, 0, Duration::from_millis(80), 1500, Instant::now());
        assert!(
            bps <= 1_500_000,
            "OCC should drop near min_bitrate when SINR < floor, got {bps}"
        );
    }

    #[test]
    fn occ_high_bler_caps_abw() {
        let mut c = mk();
        c.on_l1_metric(LM::Sinr(15.0));
        c.on_l1_metric(LM::Bler(0.20));
        let bps = c.on_observation(0.0, 0, Duration::from_millis(80), 1500, Instant::now());
        assert!(
            bps <= 30_000_000,
            "OCC should cap ABW for BLER 20 %, got {bps}"
        );
    }

    #[test]
    fn occ_trait_object_dispatch() {
        let mut c: Box<dyn RateController> = Box::new(mk());
        c.on_l1_metric(LM::Sinr(15.0));
        let bps = c.on_observation(0.0, 0, Duration::from_millis(80), 1500, Instant::now());
        assert_eq!(c.algorithm_name(), "occ");
        assert!(bps >= 300_000);
        let snap = c.snapshot();
        assert_eq!(snap.algorithm, "occ");
    }
}
