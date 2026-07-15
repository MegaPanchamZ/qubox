use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::telemetry::CongestionTelemetry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkClass {
    WiredBroadband,
    Wireless,
    Cellular { has_l1_metrics: bool },
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CongestionAlgorithm {
    Scream,
    BbrV3,
    Occ,
    Gcc,
}

pub trait RateController: Send {
    fn on_observation(
        &mut self,
        owd_ms: f64,
        loss_x1000: u16,
        rtt: Duration,
        sent_bytes: u64,
        now: Instant,
    ) -> u32;

    fn on_l1_metric(&mut self, _metric: L1Metric) {}

    fn snapshot(&self) -> CongestionTelemetry;

    fn current_bitrate_bps(&self) -> u32;

    fn algorithm_name(&self) -> &'static str;
}

#[derive(Debug, Clone, Copy)]
pub enum L1Metric {
    Rsrp(f32),
    Rsrq(f32),
    Rssi(f32),
    Sinr(f32),
    Bler(f32),
}
