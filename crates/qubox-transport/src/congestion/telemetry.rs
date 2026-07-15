use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct CongestionTelemetry {
    pub algorithm: &'static str,
    pub target_bitrate_bps: u32,
    pub cwnd_bytes: Option<u64>,
    pub qdelay_ms: Option<f64>,
    pub max_bandwidth_bps: Option<u64>,
    pub min_rtt_ms: Option<f64>,
    pub abw_bps: Option<u64>,
    pub owd_ms_ewma: Option<f64>,
    pub state: Option<String>,
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
            cwnd_bytes: None,
            qdelay_ms: None,
            max_bandwidth_bps: None,
            min_rtt_ms: None,
            abw_bps: None,
            owd_ms_ewma: None,
            state: None,
            l1_rsrp_dbm: None,
            l1_rsrq_db: None,
            l1_rssi_dbm: None,
            l1_sinr_db: None,
            l1_bler: None,
        }
    }
}
