use serde::{Deserialize, Serialize};

pub mod norm {
    pub const THROUGHPUT_DIV_BPS: f32 = 50_000_000.0;
    pub const DECODE_LATENCY_DIV_MS: f32 = 33.3;
    pub const RATIO_CLIP: f32 = 1.0;
    pub const FEC_LOSS_DIV: f32 = 1.0;
    pub const SCREEN_CONTENT_DIV: f32 = 1.0;
    pub const DEADLINE_SLACK_DIV_MS: f32 = 16.67;
    pub const BITRATE_DIV_BPS: f32 = 20_000_000.0;
    pub const EWMA_ALPHA_THROUGHPUT: f32 = 0.30;
    pub const EWMA_ALPHA_DECODE_LATENCY: f32 = 0.20;
    pub const EWMA_ALPHA_RATIO: f32 = 0.20;
    pub const EWMA_ALPHA_FEC_LOSS: f32 = 0.15;
    pub const EWMA_ALPHA_DEADLINE_SLACK: f32 = 0.40;
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Observation {
    pub throughput_bps: u32,
    pub decode_latency_ms: f32,
    pub encode_delivered_ratio: f32,
    pub fec_loss_rate: f32,
    pub screen_content_score: f32,
    pub deadline_slack_ms: f32,
    pub past_actions: [u32; 5],
}

impl Observation {
    pub fn to_normalised_vec(&self) -> [f32; 11] {
        let mut v = [0.0_f32; 11];
        v[0] = (self.throughput_bps as f32 / norm::THROUGHPUT_DIV_BPS).clamp(0.0, 2.0);
        v[1] = (self.decode_latency_ms / norm::DECODE_LATENCY_DIV_MS).clamp(0.0, 2.0);
        v[2] = (self.encode_delivered_ratio).clamp(-norm::RATIO_CLIP, norm::RATIO_CLIP);
        v[3] = (self.fec_loss_rate / norm::FEC_LOSS_DIV).clamp(0.0, 1.0);
        v[4] = (self.screen_content_score / norm::SCREEN_CONTENT_DIV).clamp(0.0, 1.0);
        v[5] = (self.deadline_slack_ms / norm::DEADLINE_SLACK_DIV_MS).clamp(-1.0, 1.0);
        for (i, bps) in self.past_actions.iter().enumerate() {
            v[6 + i] = (*bps as f32 / norm::BITRATE_DIV_BPS).clamp(0.0, 1.0);
        }
        v
    }

    pub fn zeroed_for_test() -> Self {
        Self {
            throughput_bps: 0,
            decode_latency_ms: 0.0,
            encode_delivered_ratio: 0.0,
            fec_loss_rate: 0.0,
            screen_content_score: 0.0,
            deadline_slack_ms: 0.0,
            past_actions: [0; 5],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_normalisation_round_trip() {
        let obs = Observation {
            throughput_bps: 10_000_000,
            decode_latency_ms: 16.67,
            encode_delivered_ratio: 1.0,
            fec_loss_rate: 0.05,
            screen_content_score: 0.3,
            deadline_slack_ms: 8.33,
            past_actions: [6_000_000; 5],
        };
        let v = obs.to_normalised_vec();
        assert!((v[0] - 0.2).abs() < 1e-5);
        assert!((v[1] - 0.5).abs() < 1e-3);
        assert!((v[2] - 1.0).abs() < 1e-5);
        assert!((v[3] - 0.05).abs() < 1e-5);
        assert!((v[4] - 0.3).abs() < 1e-5);
        assert!((v[5] - 0.5).abs() < 1e-3);
        assert!((v[6] - 0.3).abs() < 1e-5);
        assert!((v[10] - 0.3).abs() < 1e-5);
    }

    #[test]
    fn observation_serde_round_trip() {
        let obs = Observation {
            throughput_bps: 12_000_000,
            decode_latency_ms: 18.5,
            encode_delivered_ratio: 1.05,
            fec_loss_rate: 0.01,
            screen_content_score: 0.7,
            deadline_slack_ms: -2.0,
            past_actions: [4_000_000; 5],
        };
        let json = serde_json::to_string(&obs).unwrap();
        let decoded: Observation = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.throughput_bps, 12_000_000);
        assert_eq!(decoded.past_actions, [4_000_000; 5]);
    }
}
