#[cfg(feature = "rl-abr")]
use qubox_rl_policy::action::{idx_to_action, N_ACTIONS};
#[cfg(feature = "rl-abr")]
use qubox_rl_policy::observation::Observation;

pub struct RlAbrController {
    pub enabled: bool,
}

impl RlAbrController {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    #[cfg(feature = "rl-abr")]
    pub fn select_action(&self, obs: &Observation) -> qubox_rl_policy::action::Action {
        if !self.enabled {
            return qubox_rl_policy::action::Action::from_default();
        }
        let idx = self.query_policy_server(obs);
        idx_to_action(idx)
    }

    #[cfg(not(feature = "rl-abr"))]
    pub fn select_action(&self) -> u32 {
        20_000_000
    }

    #[cfg(feature = "rl-abr")]
    fn query_policy_server(&self, _obs: &Observation) -> usize {
        // TODO(adr-020): wire tokio TcpStream to policy server
        N_ACTIONS / 2
    }
}

#[cfg(test)]
#[cfg(feature = "rl-abr")]
mod tests {
    use super::*;

    #[test]
    fn rl_abr_controller_returns_middle_action_when_disconnected() {
        let ctrl = RlAbrController::new(true);
        let obs = Observation::zeroed_for_test();
        let action = ctrl.select_action(&obs);
        assert!(action.bitrate_bps > 0);
    }
}
