use serde::{Deserialize, Serialize};

pub const BITRATE_LADDER_BPS: &[u32] = &[
    1_000_000, 2_000_000, 4_000_000, 6_000_000,
    8_000_000, 12_000_000, 16_000_000, 20_000_000,
];
pub const RESOLUTION_LADDER: &[(u16, u16)] = &[
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];
pub const REFRESH_LADDER_HZ: &[u8] = &[60, 90, 144];
pub const N_ACTIONS: usize = 96;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Action {
    pub bitrate_bps: u32,
    pub width: u16,
    pub height: u16,
    pub refresh_hz: u8,
    pub codec_idx: u8,
}

impl Action {
    pub fn from_indices(bitrate_idx: usize, res_idx: usize, refresh_idx: usize) -> Self {
        let (w, h) = RESOLUTION_LADDER[res_idx];
        Self {
            bitrate_bps: BITRATE_LADDER_BPS[bitrate_idx],
            width: w,
            height: h,
            refresh_hz: REFRESH_LADDER_HZ[refresh_idx],
            codec_idx: 1,
        }
    }

    pub fn quality_log_ratio(&self, prev: &Action) -> f32 {
        let q_curr = (self.bitrate_bps as f32).ln();
        let q_prev = (prev.bitrate_bps as f32).ln();
        (q_curr - q_prev).abs()
    }

    pub fn from_default() -> Self {
        Self::from_indices(2, 0, 0)
    }
}

pub fn idx_to_action(idx: usize) -> Action {
    let br_idx = idx / (RESOLUTION_LADDER.len() * REFRESH_LADDER_HZ.len());
    let rem = idx % (RESOLUTION_LADDER.len() * REFRESH_LADDER_HZ.len());
    let res_idx = rem / REFRESH_LADDER_HZ.len();
    let refresh_idx = rem % REFRESH_LADDER_HZ.len();
    Action::from_indices(br_idx, res_idx, refresh_idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_space_cardinality() {
        assert_eq!(N_ACTIONS, 8 * 4 * 3);
        assert_eq!(N_ACTIONS, BITRATE_LADDER_BPS.len() * RESOLUTION_LADDER.len() * REFRESH_LADDER_HZ.len());
    }

    #[test]
    fn idx_to_action_round_trip() {
        for i in 0..N_ACTIONS {
            let a = idx_to_action(i);
            assert!(a.bitrate_bps > 0);
            assert!(a.width > 0);
            assert!(a.height > 0);
            assert!(a.refresh_hz > 0);
        }
    }
}
