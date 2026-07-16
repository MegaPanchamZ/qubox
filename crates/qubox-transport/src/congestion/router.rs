use std::time::{Duration, Instant};

use super::bbr::{BbrV3Config, BbrV3RateController};
use super::legacy::{GccConfig, LegacyGccRateController};
use super::occ::{OccConfig, OccRateController};
use super::scream::{ScreamConfig, ScreamRateController};
use super::trait_def::{CongestionAlgorithm, L1Metric, NetworkClass, RateController};

pub struct RouterConfig {
    pub scream: ScreamConfig,
    pub bbr_v3: BbrV3Config,
    pub occ: OccConfig,
    pub gcc_legacy: GccConfig,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            scream: ScreamConfig::default(),
            bbr_v3: BbrV3Config::default(),
            occ: OccConfig::default(),
            gcc_legacy: GccConfig::default(),
        }
    }
}

pub struct CongestionRouter {
    pub algo: CongestionAlgorithm,
    pub class: NetworkClass,
    controller: Box<dyn RateController>,
    last_emitted_telemetry_at: Option<Instant>,
}

impl CongestionRouter {
    pub fn new(
        class: NetworkClass,
        override_algo: Option<CongestionAlgorithm>,
        cfg: RouterConfig,
    ) -> Self {
        let algo = override_algo.unwrap_or_else(|| match class {
            NetworkClass::WiredBroadband => CongestionAlgorithm::BbrV3,
            NetworkClass::Wireless => CongestionAlgorithm::Scream,
            NetworkClass::Cellular {
                has_l1_metrics: true,
            } => CongestionAlgorithm::Occ,
            NetworkClass::Cellular {
                has_l1_metrics: false,
            } => CongestionAlgorithm::Scream,
            NetworkClass::Unknown => CongestionAlgorithm::Scream,
        });
        let controller: Box<dyn RateController> = match algo {
            CongestionAlgorithm::Scream => Box::new(ScreamRateController::new(cfg.scream)),
            CongestionAlgorithm::BbrV3 => Box::new(BbrV3RateController::new(cfg.bbr_v3)),
            CongestionAlgorithm::Occ => Box::new(OccRateController::new(cfg.occ)),
            CongestionAlgorithm::Gcc => Box::new(LegacyGccRateController::new(cfg.gcc_legacy)),
        };
        Self {
            algo,
            class,
            controller,
            last_emitted_telemetry_at: None,
        }
    }

    pub fn on_observation(
        &mut self,
        owd_ms: f64,
        loss_x1000: u16,
        rtt: Duration,
        sent_bytes: u64,
        now: Instant,
    ) -> u32 {
        let bps = self
            .controller
            .on_observation(owd_ms, loss_x1000, rtt, sent_bytes, now);

        if self.last_emitted_telemetry_at.map_or(true, |t| {
            now.saturating_duration_since(t) >= Duration::from_secs(1)
        }) {
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

    pub fn on_l1_metric(&mut self, m: L1Metric) {
        self.controller.on_l1_metric(m);
    }
}

pub fn classify_network(owd_samples: &[f64]) -> NetworkClass {
    if owd_samples.is_empty() {
        return NetworkClass::Unknown;
    }

    let n = owd_samples.len() as f64;
    let mean = owd_samples.iter().sum::<f64>() / n;
    let variance = owd_samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    let stddev = variance.sqrt();

    if stddev <= 1.0 && mean < 30.0 {
        NetworkClass::WiredBroadband
    } else if stddev > 5.0 {
        NetworkClass::Wireless
    } else {
        NetworkClass::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_picks_bbr_v3_for_wired_broadband() {
        let r = CongestionRouter::new(NetworkClass::WiredBroadband, None, RouterConfig::default());
        assert_eq!(r.algo, CongestionAlgorithm::BbrV3);
    }

    #[test]
    fn router_picks_occ_for_cellular_with_l1() {
        let r = CongestionRouter::new(
            NetworkClass::Cellular {
                has_l1_metrics: true,
            },
            None,
            RouterConfig::default(),
        );
        assert_eq!(r.algo, CongestionAlgorithm::Occ);
    }

    #[test]
    fn router_picks_scream_for_cellular_without_l1() {
        let r = CongestionRouter::new(
            NetworkClass::Cellular {
                has_l1_metrics: false,
            },
            None,
            RouterConfig::default(),
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
    fn router_picks_scream_for_wireless() {
        let r = CongestionRouter::new(NetworkClass::Wireless, None, RouterConfig::default());
        assert_eq!(r.algo, CongestionAlgorithm::Scream);
    }

    #[test]
    fn router_picks_scream_for_unknown() {
        let r = CongestionRouter::new(NetworkClass::Unknown, None, RouterConfig::default());
        assert_eq!(r.algo, CongestionAlgorithm::Scream);
    }

    #[test]
    fn network_classifier_wired_when_stddev_below_1ms() {
        let samples: Vec<f64> = (0..100).map(|i| 12.0 + (i % 3) as f64 * 0.2).collect();
        let class = classify_network(&samples);
        assert_eq!(class, NetworkClass::WiredBroadband);
    }

    #[test]
    fn network_classifier_wireless_when_stddev_above_5ms() {
        let samples: Vec<f64> = (0..100).map(|i| 30.0 + (i % 7) as f64 * 3.0).collect();
        let class = classify_network(&samples);
        assert_eq!(class, NetworkClass::Wireless);
    }

    #[test]
    fn network_classifier_unknown_with_moderate_jitter() {
        let samples: Vec<f64> = (0..100).map(|i| 20.0 + (i % 5) as f64 * 0.8).collect();
        let class = classify_network(&samples);
        assert_eq!(class, NetworkClass::Unknown);
    }

    #[test]
    fn router_routes_observation_through_controller() {
        let mut r = CongestionRouter::new(
            NetworkClass::Wireless,
            Some(CongestionAlgorithm::Scream),
            RouterConfig::default(),
        );
        let bps = r.on_observation(20.0, 0, Duration::from_millis(40), 1500, Instant::now());
        assert!(bps >= 500_000);
    }

    #[test]
    fn network_class_enum_round_trip() {
        let classes = [
            NetworkClass::WiredBroadband,
            NetworkClass::Wireless,
            NetworkClass::Cellular {
                has_l1_metrics: true,
            },
            NetworkClass::Cellular {
                has_l1_metrics: false,
            },
            NetworkClass::Unknown,
        ];

        let json = serde_json::to_string(&classes).unwrap();
        let deserialized: Vec<NetworkClass> = serde_json::from_str(&json).unwrap();
        assert_eq!(classes.as_slice(), deserialized.as_slice());
    }

    #[test]
    fn congestion_algorithm_enum_round_trip() {
        let algos = [
            CongestionAlgorithm::Scream,
            CongestionAlgorithm::BbrV3,
            CongestionAlgorithm::Occ,
            CongestionAlgorithm::Gcc,
        ];

        let json = serde_json::to_string(&algos).unwrap();
        let deserialized: Vec<CongestionAlgorithm> = serde_json::from_str(&json).unwrap();
        assert_eq!(algos.as_slice(), deserialized.as_slice());
    }
}
