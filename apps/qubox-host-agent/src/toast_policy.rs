//! Host-side toast / pre-connect confirmation policy.
//!
//! Per `docs/browser-viewer-identity-and-host-trust.md` Phase 1:
//!
//! - When a session request arrives, the host MAY show a toast
//!   asking the operator to confirm. The viewer must wait for the
//!   toast to be acknowledged (or auto-ack, gated by org policy).
//! - The org policy lives at `host_policy.json` (see `pin.rs`). This
//!   module is the pure decision layer; the platform-specific
//!   toast renderer is not implemented yet — we model the *decision*
//!   so a Tauri / Win32 / GTK renderer can plug in later.
//! - The decision layer also rate-limits attempts: after N
//!   ignored toasts in a 60s window, the host enters a 24h block
//!   for that client peer. This matches the spec's anti-attack
//!   posture (don't let the cloud "toast-spam" a host).
//!
//! This module is sync. The async message loop calls it once per
//! incoming `SessionRequested`.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use qubox_proto::PeerDescriptor;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToastDecision {
    /// Toast required; block session until operator confirms.
    Show,
    /// Toast optional; session proceeds immediately.
    SkipAutoAck,
    /// Hard block — host policy refuses this peer entirely.
    Block,
}

/// Effective toast policy. Mirrors the org's "Always show toast"
/// toggle plus the host owner's local override.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToastMode {
    /// Never show a toast; sessions proceed automatically. This is
    /// the LAN-only / `auto_approve_pairing` path.
    Off,
    /// Show a toast on every new viewer.
    On,
    /// Show only on previously-unseen viewers (default).
    NewViewersOnly,
}

impl Default for ToastMode {
    fn default() -> Self {
        Self::NewViewersOnly
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToastPolicy {
    #[serde(default)]
    pub mode: ToastMode,
}

impl ToastPolicy {
    pub fn requires_toast(&self, viewer_is_known: bool) -> bool {
        match self.mode {
            ToastMode::Off => false,
            ToastMode::On => true,
            ToastMode::NewViewersOnly => !viewer_is_known,
        }
    }
}

/// Sliding-window rate limit. Records (Instant, peer_id) tuples and
/// emits a `Block` decision once the threshold is crossed inside
/// the configured window.
pub struct ToastRateLimiter {
    window: Duration,
    threshold: usize,
    block_duration: Duration,
    seen: Mutex<HashMap<String, VecDeque<Instant>>>,
    blocked_until: Mutex<HashMap<String, Instant>>,
}

impl ToastRateLimiter {
    pub fn new(window: Duration, threshold: usize, block_duration: Duration) -> Self {
        Self {
            window,
            threshold,
            block_duration,
            seen: Mutex::new(HashMap::new()),
            blocked_until: Mutex::new(HashMap::new()),
        }
    }

    /// Returns `true` if `peer_id` is currently blocked.
    pub fn is_blocked(&self, peer_id: &str) -> bool {
        let blocked = self.blocked_until.lock().expect("toast rl poisoned");
        blocked
            .get(peer_id)
            .map(|until| *until > Instant::now())
            .unwrap_or(false)
    }

    /// Record a toast attempt and return the decision.
    pub fn record_attempt(&self, peer_id: &str, now: Instant) -> ToastDecision {
        if self.is_blocked_at(peer_id, now) {
            return ToastDecision::Block;
        }
        let mut seen = self.seen.lock().expect("toast rl poisoned");
        let bucket = seen.entry(peer_id.to_string()).or_default();
        bucket.push_back(now);
        // prune outside the window
        while let Some(front) = bucket.front() {
            if now.duration_since(*front) > self.window {
                bucket.pop_front();
            } else {
                break;
            }
        }
        if bucket.len() > self.threshold {
            let until = now + self.block_duration;
            drop(seen);
            self.blocked_until
                .lock()
                .expect("toast rl poisoned")
                .insert(peer_id.to_string(), until);
            return ToastDecision::Block;
        }
        ToastDecision::Show
    }

    fn is_blocked_at(&self, peer_id: &str, now: Instant) -> bool {
        let blocked = self.blocked_until.lock().expect("toast rl poisoned");
        blocked
            .get(peer_id)
            .map(|until| *until > now)
            .unwrap_or(false)
    }

    /// Exposed for tests / admin tools.
    pub fn unblock(&self, peer_id: &str) {
        self.blocked_until
            .lock()
            .expect("toast rl poisoned")
            .remove(peer_id);
        self.seen.lock().expect("toast rl poisoned").remove(peer_id);
    }
}

/// Full decision context. Combines policy + rate limit + a small
/// in-memory "known viewer" cache so `NewViewersOnly` is meaningful.
pub struct ToastGate {
    pub policy: ToastPolicy,
    pub rate_limiter: ToastRateLimiter,
    known_viewers: Mutex<HashMap<String, Instant>>,
}

impl ToastGate {
    pub fn new(policy: ToastPolicy, rate_limiter: ToastRateLimiter) -> Self {
        Self {
            policy,
            rate_limiter,
            known_viewers: Mutex::new(HashMap::new()),
        }
    }

    pub fn mark_known(&self, viewer: &PeerDescriptor) {
        self.known_viewers
            .lock()
            .expect("toast gate poisoned")
            .insert(viewer.peer_id.to_string(), Instant::now());
    }

    pub fn is_known(&self, viewer: &PeerDescriptor) -> bool {
        self.known_viewers
            .lock()
            .expect("toast gate poisoned")
            .contains_key(&viewer.peer_id.to_string())
    }

    /// Combine policy + rate limit into a single decision.
    pub fn evaluate(&self, viewer: &PeerDescriptor, now: Instant) -> ToastDecision {
        let peer_key = viewer.peer_id.to_string();
        if self.rate_limiter.is_blocked(&peer_key) {
            return ToastDecision::Block;
        }
        // Always record the attempt — even if policy says "Off" we
        // want to be able to retroactively detect spam. The limiter
        // handles block-on-threshold.
        let rl = self.rate_limiter.record_attempt(&peer_key, now);
        if rl == ToastDecision::Block {
            return ToastDecision::Block;
        }
        if !self.policy.requires_toast(self.is_known(viewer)) {
            return ToastDecision::SkipAutoAck;
        }
        ToastDecision::Show
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn viewer() -> PeerDescriptor {
        PeerDescriptor {
            role: qubox_proto::PeerRole::Client,
            device_name: "test-client".into(),
            device_id: Uuid::new_v4(),
            peer_id: Uuid::new_v4(),
            os: qubox_proto::PlatformOs::Linux,
            capabilities: Default::default(),
        }
    }

    #[test]
    fn off_mode_skips_toast() {
        let p = ToastPolicy {
            mode: ToastMode::Off,
        };
        assert!(!p.requires_toast(false));
        assert!(!p.requires_toast(true));
    }

    #[test]
    fn on_mode_always_requires() {
        let p = ToastPolicy {
            mode: ToastMode::On,
        };
        assert!(p.requires_toast(true));
        assert!(p.requires_toast(false));
    }

    #[test]
    fn new_viewers_only_gates_only_unknowns() {
        let p = ToastPolicy {
            mode: ToastMode::NewViewersOnly,
        };
        assert!(p.requires_toast(false));
        assert!(!p.requires_toast(true));
    }

    #[test]
    fn rate_limiter_blocks_after_threshold() {
        let rl = ToastRateLimiter::new(Duration::from_secs(60), 3, Duration::from_secs(86_400));
        let now = Instant::now();
        let peer = "p";
        assert!(!rl.is_blocked(peer));
        assert_eq!(rl.record_attempt(peer, now), ToastDecision::Show);
        assert_eq!(rl.record_attempt(peer, now), ToastDecision::Show);
        assert_eq!(rl.record_attempt(peer, now), ToastDecision::Show);
        assert_eq!(rl.record_attempt(peer, now), ToastDecision::Block);
        assert!(rl.is_blocked(peer));
    }

    #[test]
    fn rate_limiter_window_expires() {
        let rl = ToastRateLimiter::new(Duration::from_millis(100), 2, Duration::from_secs(10));
        let peer = "p";
        let t0 = Instant::now();
        assert_eq!(rl.record_attempt(peer, t0), ToastDecision::Show);
        assert_eq!(rl.record_attempt(peer, t0), ToastDecision::Show);
        // No third attempt; window expires.
        let t1 = t0 + Duration::from_millis(150);
        assert_eq!(rl.record_attempt(peer, t1), ToastDecision::Show);
    }

    #[test]
    fn gate_combines_policy_and_limiter() {
        let rl = ToastRateLimiter::new(Duration::from_secs(60), 100, Duration::from_secs(10));
        let gate = ToastGate::new(
            ToastPolicy {
                mode: ToastMode::NewViewersOnly,
            },
            rl,
        );
        let v = viewer();
        // First time = unknown -> Show
        assert_eq!(gate.evaluate(&v, Instant::now()), ToastDecision::Show);
        // After mark_known -> SkipAutoAck
        gate.mark_known(&v);
        assert_eq!(
            gate.evaluate(&v, Instant::now()),
            ToastDecision::SkipAutoAck
        );
    }
}
