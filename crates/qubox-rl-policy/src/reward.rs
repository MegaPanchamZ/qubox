use crate::action::Action;
use crate::observation::Observation;

pub const ALPHA_REBUF: f32 = 4.3;
pub const BETA_SMOOTH: f32 = 1.0;
pub const LATENCY_PENALTY_PER_MS: f32 = 0.10;
pub const DEADLINE_MISS_PENALTY: f32 = 50.0;

pub fn reward(prev_action: &Action, action: &Action, next_obs: &Observation) -> f32 {
    let quality = (action.bitrate_bps as f32).ln();
    let rebuf_term = ALPHA_REBUF * (next_obs.fec_loss_rate * 1.0);
    let smooth_term = BETA_SMOOTH * action.quality_log_ratio(prev_action);
    let latency_penalty = if next_obs.decode_latency_ms > 16.67 {
        LATENCY_PENALTY_PER_MS * (next_obs.decode_latency_ms - 16.67)
    } else {
        0.0
    };
    let deadline_penalty = if next_obs.deadline_slack_ms < 0.0 {
        DEADLINE_MISS_PENALTY
    } else {
        0.0
    };
    quality - rebuf_term - smooth_term - latency_penalty - deadline_penalty
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;

    #[test]
    fn reward_penalises_deadline_violation() {
        let prev = Action::from_indices(2, 1, 0);
        let action = Action::from_indices(4, 1, 0);
        let mut next = Observation::zeroed_for_test();
        next.deadline_slack_ms = -1.0;
        let r = reward(&prev, &action, &next);
        assert!(r > -40.0 && r < -30.0, "reward = {r}");
    }

    #[test]
    fn reward_rewards_high_bitrate_when_no_problems() {
        let prev = Action::from_indices(2, 1, 0);
        let action = Action::from_indices(6, 1, 0);
        let next = Observation::zeroed_for_test();
        let r = reward(&prev, &action, &next);
        assert!(r > 14.0 && r < 16.0, "reward = {r}");
    }

    #[test]
    fn reward_formula_known_fixture() {
        let prev = Action::from_indices(0, 0, 0);
        let action = Action::from_indices(2, 1, 0);
        let mut next = Observation::zeroed_for_test();
        next.fec_loss_rate = 0.02;
        next.decode_latency_ms = 20.0;
        next.deadline_slack_ms = 0.5;
        let r = reward(&prev, &action, &next);
        let expected_quality = (4_000_000_f32).ln();
        let expected_rebuf = 4.3 * 0.02;
        let expected_smooth = 1.0 * ((4_000_000_f32).ln() - (1_000_000_f32).ln()).abs();
        let expected_latency = 0.10 * (20.0 - 16.67);
        let expected = expected_quality - expected_rebuf - expected_smooth - expected_latency;
        assert!(
            (r - expected).abs() < 1e-3,
            "reward = {r}, expected = {expected}"
        );
    }
}
