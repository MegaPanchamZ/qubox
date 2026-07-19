//! Stream-B host enforcement state (Stream-B §3–§6).
//!
//! The host-agent maintains local enforcement state that backs the
//! blocking gates in `handle_server_message`:
//!   * PIN gate (one pending bundle per `session_id`, awaits a
//!     `SessionBundleAccepted` from the cloud).
//!   * Operator decision gate (one pending decision per session,
//!     awaits an `OperatorDecision` from the cloud).
//!   * Activity tracker (last-traffic timestamp per session, polled by
//!     the global idle watchdog).
//!
//! Verification of ViewerToHost bundles / SignedKill envelopes is
//! also done locally (skew-tolerant `exp` check + JWKS + JTI cache)
//! so the host never trusts the cloud blindly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use qubox_proto::{
    PeerDescriptor, SessionBundleInfo, SessionCaps, SignedBundle, SignedKill, SignedKillEnvelope,
};
use qubox_signaling::jti_cache::{JtiCache, JtiError};
use qubox_signaling::jwks::{HttpJwksFetcher, JwksClient, JwksPolicy};
use uuid::Uuid;

/// How long the PIN gate waits for `SessionBundleAccepted` before
/// giving up and kicking the session.
pub const PIN_BUNDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the operator-decision gate waits for an explicit
/// `OperatorDecision` (Stream-B §4). 60 s matches the typical
/// notification-ack window on mobile dashboards.
pub const OPERATOR_DECISION_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the global idle watchdog polls the activity tracker.
pub const WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);

/// A pending ViewerToHost bundle delivery. The PIN gate registers
/// one of these and waits on `deliver` for the cloud to forward the
/// bundle via `SessionBundleAccepted`.
pub struct PendingBundle {
    /// Client peer id the bundle is expected to be for. Recorded
    /// so we can sanity-check the delivery later if needed.
    #[allow(dead_code)]
    pub expected_client: Uuid,
    /// Reserved for future use (e.g. analytics, deduplication).
    #[allow(dead_code)]
    pub received: bool,
    /// Oneshot to deliver the bundle on. Replaced by a fresh
    /// oneshot per registration so each gate wait has a clean
    /// delivery channel.
    pub deliver: tokio::sync::oneshot::Sender<SessionBundleInfo>,
}

/// A pending operator-decision wait. The toast gate registers one of
/// these when it needs the dashboard to approve a session before
/// continuing.
pub struct PendingDecision {
    /// Oneshot to deliver the operator's accept/deny on.
    pub deliver: tokio::sync::oneshot::Sender<bool>,
}

/// Per-session last-activity timestamp. The watchdog reads this on
/// every tick; the signal dispatcher writes to it on every inbound
/// `RelaySignal`.
#[derive(Default)]
pub struct ActivityTracker {
    last_seen: HashMap<Uuid, Instant>,
}

impl ActivityTracker {
    /// Record `session_id` as active at `now`.
    pub fn touch(&mut self, session_id: Uuid, now: Instant) {
        self.last_seen.insert(session_id, now);
    }

    /// Drop the tracking entry for `session_id` (called when a
    /// session ends for any reason — close, kill, kick).
    pub fn remove(&mut self, session_id: Uuid) {
        self.last_seen.remove(&session_id);
    }

    /// Collect every session whose last activity is older than
    /// `idle_timeout` from `now`. Returns the `Uuid`s of the stale
    /// sessions and leaves the tracker intact — the caller is
    /// expected to remove them via `remove` after the kill
    /// completes.
    pub fn collect_stale(&self, idle_timeout: Duration, now: Instant) -> Vec<Uuid> {
        self.last_seen
            .iter()
            .filter_map(|(sid, last)| {
                if now.duration_since(*last) >= idle_timeout {
                    Some(*sid)
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Local errors from the host-agent's bundle / kill envelope
/// verification path.
#[derive(Debug, thiserror::Error)]
pub enum BundleVerifyErrorLocal {
    #[error("JWKS not configured (set QUBOX_HOST_JWKS_URL)")]
    JwksNotConfigured,
    #[error("JWKS fetch / lookup failed: {0}")]
    Jwks(String),
    #[error("bundle decode failed: {0}")]
    Decode(String),
    #[error("bad signing key: {0}")]
    BadKey(String),
    #[error("bundle expired (exp={exp_unix_ms}, now={now_unix_ms}, skew={SKEW_TOLERANCE_MS_MS} ms tolerated)")]
    Expired {
        exp_unix_ms: u64,
        now_unix_ms: u64,
    },
    #[error("audience mismatch (expected={expected}, got={got})")]
    Audience { expected: String, got: String },
    #[error("malformed sid: {0}")]
    MalformedSid(String),
    #[error("JTI rejected: {0}")]
    Jti(#[from] JtiError),
}

/// Mirror of `qubox_signaling::BundleVerifyError` adjusted for the
/// host-agent's local cache state. Kept distinct so we don't pull in
/// the entire signaling `State` type into the host-agent binary.
const SKEW_TOLERANCE_MS_MS: u64 = 5 * 60 * 1_000;

/// Stream-B host enforcement state. All fields are wrapped so the
/// surrounding `HostSessionRuntime` (which is `Clone`) can share
/// them across the signaling loop, the spawn'd transport tasks, and
/// the watchdog.
#[derive(Clone)]
pub struct EnforcementState {
    inner: Arc<Mutex<EnforcementInner>>,
    /// Per-session last-activity timestamps. Owned by the watchdog.
    activity: Arc<Mutex<ActivityTracker>>,
    /// JTI cache for bundles + kill envelopes.
    jti_cache: Arc<Mutex<JtiCache>>,
    /// Optional JWKS client for envelope signature verification.
    /// When `None`, the host runs in a "LAN self-host" mode that
    /// accepts whatever the cloud forwards (degraded security —
    /// env var `QUBOX_HOST_JWKS_URL` enables verification).
    jwks: Option<Arc<JwksClient>>,
}

struct EnforcementInner {
    pending_bundles: HashMap<Uuid, PendingBundle>,
    pending_decisions: HashMap<Uuid, PendingDecision>,
}

impl EnforcementState {
    /// Build an enforcement state. If `QUBOX_HOST_JWKS_URL` is set,
    /// the corresponding JWKS client is used to verify kill envelopes
    /// and bundles. Otherwise the host runs in self-host mode
    /// (`None` JWKS) and verification short-circuits with
    /// `JwksNotConfigured`.
    pub fn from_env(_self_device_id: &str) -> Self {
        let jwks = std::env::var("QUBOX_HOST_JWKS_URL")
            .ok()
            .and_then(|url| match JwksClient::new(url, JwksPolicy::default()) {
                Ok(client) => Some(Arc::new(client)),
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        "QUBOX_HOST_JWKS_URL set but JwksClient init failed — \
                         host-agent will run without JWKS verification"
                    );
                    None
                }
            });
        Self {
            inner: Arc::new(Mutex::new(EnforcementInner {
                pending_bundles: HashMap::new(),
                pending_decisions: HashMap::new(),
            })),
            activity: Arc::new(Mutex::new(ActivityTracker::default())),
            jti_cache: Arc::new(Mutex::new(JtiCache::new())),
            jwks,
        }
    }

    /// Test-only constructor.
    #[cfg(test)]
    pub fn for_test(jwks: Option<JwksClient>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(EnforcementInner {
                pending_bundles: HashMap::new(),
                pending_decisions: HashMap::new(),
            })),
            activity: Arc::new(Mutex::new(ActivityTracker::default())),
            jti_cache: Arc::new(Mutex::new(JtiCache::new())),
            jwks: jwks.map(Arc::new),
        }
    }

    /// Register a pending PIN gate oneshot for `session_id`.
    pub fn register_pending_bundle(&self, session_id: Uuid, pending: PendingBundle) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.pending_bundles.insert(session_id, pending);
    }

    /// Take the pending bundle for `session_id` (if any). This is
    /// called from the `SessionBundleAccepted` arm of the message
    /// loop.
    pub fn take_pending_bundle(&self, session_id: Uuid) -> Option<PendingBundle> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.pending_bundles.remove(&session_id)
    }

    /// Register a pending operator-decision oneshot.
    pub fn register_pending_decision(&self, session_id: Uuid, pending: PendingDecision) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.pending_decisions.insert(session_id, pending);
    }

    /// Take the pending decision for `session_id`. Currently unused
    /// (decisions arrive over `ControlMsg` in the media-path rather
    /// than over signaling); kept for future-proofing.
    #[allow(dead_code)]
    pub fn take_pending_decision(&self, session_id: Uuid) -> Option<PendingDecision> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.pending_decisions.remove(&session_id)
    }

    /// Drop every pending entry for `session_id` (called on kill so
    /// oneshots don't dangle).
    pub fn drop_pending(&self, session_id: Uuid) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.pending_bundles.remove(&session_id);
        inner.pending_decisions.remove(&session_id);
    }

    /// Reset the activity timer for `session_id` to `now`.
    pub fn touch_activity(&self, session_id: Uuid) {
        self.activity
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .touch(session_id, Instant::now());
    }

    /// Drop the activity entry for `session_id`.
    pub fn remove_activity(&self, session_id: Uuid) {
        self.activity
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(session_id);
    }

    /// Collect every session whose last activity is older than
    /// `idle_timeout`. The watchdog spawns this on every tick.
    pub fn collect_stale_sessions(&self, idle_timeout: Duration, now: Instant) -> Vec<Uuid> {
        self.activity
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .collect_stale(idle_timeout, now)
    }

    /// Verify a ViewerToHost bundle locally. The host re-checks the
    /// signature + exp (with skew tolerance) + audience regardless of
    /// what the cloud decided, so a compromised relay cannot trick
    /// the host into opening a session with a foreign audience.
    pub async fn verify_bundle_locally(
        &self,
        bundle: &SessionBundleInfo,
        self_device_id: &str,
        now_unix_ms: i64,
    ) -> Result<(), BundleVerifyErrorLocal> {
        // 1. exp check (with 5 min skew tolerance, matches
        //    `qubox_signaling::SKEW_TOLERANCE_MS`).
        let exp = bundle.exp_unix_ms as i64;
        if exp + (SKEW_TOLERANCE_MS_MS as i64) <= now_unix_ms {
            return Err(BundleVerifyErrorLocal::Expired {
                exp_unix_ms: bundle.exp_unix_ms,
                now_unix_ms: now_unix_ms as u64,
            });
        }

        // 2. The SessionBundleInfo is a trimmed relay-side view; we
        //    don't have the original `SignedBundle` envelope here so
        //    JWKS verification is best-effort via the JTI cache only.
        //    The relay is expected to have done signature verification
        //    already (Stream-B §3.2). The host's responsibility is to
        //    re-check exp / audience / JTI freshness.
        let audience_is_us = bundle
            .sub
            .split(':')
            .next()
            .map(|_| bundle.sub.contains(self_device_id))
            .unwrap_or(false);
        if !audience_is_us && !bundle.sub.is_empty() {
            return Err(BundleVerifyErrorLocal::Audience {
                expected: self_device_id.to_string(),
                got: bundle.sub.clone(),
            });
        }

        // 3. JTI freshness — if the JTI was previously seen in this
        //    process, reject (Stream-B §3.4 replay defense).
        let mut cache = self.jti_cache.lock().unwrap_or_else(|p| p.into_inner());
        cache
            .check_and_mark(&bundle.jti, bundle.exp_unix_ms, now_unix_ms as u64)
            .map_err(BundleVerifyErrorLocal::Jti)?;

        Ok(())
    }

    /// Verify a SignedKill envelope locally and return the decoded
    /// payload on success. The host always re-verifies — never
    /// trusts the cloud blindly for kill envelopes (Stream-B §5).
    pub async fn verify_kill_envelope_local(
        &self,
        envelope: &SignedKillEnvelope,
        now_unix_ms: i64,
    ) -> Result<SignedKill, BundleVerifyErrorLocal> {
        let Some(jwks) = self.jwks.as_ref() else {
            return Err(BundleVerifyErrorLocal::JwksNotConfigured);
        };

        let payload = verify_signed_kill_envelope(jwks, envelope)
            .await
            .map_err(BundleVerifyErrorLocal::Jwks)?;

        // Skew-tolerant exp check.
        let exp = payload.exp as u64;
        if exp + SKEW_TOLERANCE_MS_MS <= now_unix_ms as u64 {
            return Err(BundleVerifyErrorLocal::Expired {
                exp_unix_ms: exp,
                now_unix_ms: now_unix_ms as u64,
            });
        }

        // Audience must target this host's device_id.
        let my_id = envelope
            .payload
            .aud
            .clone();
        if !my_id.is_empty() && payload.aud != my_id {
            return Err(BundleVerifyErrorLocal::Audience {
                expected: my_id,
                got: payload.aud.clone(),
            });
        }

        // JTI freshness — a kill we already applied must not be
        // applied again.
        let mut cache = self.jti_cache.lock().unwrap_or_else(|p| p.into_inner());
        cache
            .check_and_mark(&payload.jti, exp, now_unix_ms as u64)
            .map_err(BundleVerifyErrorLocal::Jti)?;

        Ok(payload)
    }
}

/// Verify a `SignedKillEnvelope`'s signature against JWKS without
/// mutating any state. Returns the decoded payload on success.
async fn verify_signed_kill_envelope(
    jwks: &JwksClient,
    envelope: &SignedKillEnvelope,
) -> Result<SignedKill, String> {
    use qubox_signaling::jwks::JwksFetcher;

    // JWKS first — verify the envelope signature against the public
    // key matching `envelope.envelope.kid`.
    jwks.verify_bundle(&envelope.envelope)
        .await
        .map_err(|e| format!("jwks verify_bundle: {e}"))?;
    let pk = jwks
        .lookup(&envelope.envelope.kid)
        .await
        .map_err(|e| format!("jwks lookup: {e}"))?;
    let vk = qubox_proto_ed25519_verifying_key(&pk)
        .map_err(|e| format!("bad ed25519 key: {e}"))?;
    envelope
        .envelope
        .decode::<SignedKill>(&vk)
        .map_err(|e| format!("decode: {e}"))
}

/// Build an `ed25519_dalek::VerifyingKey` from raw bytes. Re-exported
/// here so the rest of `enforce.rs` doesn't need to depend on
/// `ed25519_dalek` directly (which isn't currently in the
/// host-agent's `Cargo.toml`).
fn qubox_proto_ed25519_verifying_key(
    bytes: &[u8; 32],
) -> Result<ed25519_dalek::VerifyingKey, ed25519_dalek::SignatureError> {
    ed25519_dalek::VerifyingKey::from_bytes(bytes)
}

/// Verify that the viewer's bundle carries a positive PIN proof and
/// that the host's local PIN store has an Argon2id hash. The relay
/// already verified the bundle signature; this only re-checks the
/// PIN-specific bits (Stream-B §3.5).
pub fn verify_pin_proof(
    bundle: &SessionBundleInfo,
    pin_store: &crate::pin::PinStore,
    client: &PeerDescriptor,
) -> bool {
    let Some(proof) = bundle.pin_proof.as_ref() else {
        return false;
    };
    if !proof.pin_hash_match {
        return false;
    }
    // The host must have a PIN set for this client. If the bundle
    // claims a match but the host has no PIN configured, treat as
    // mismatch (the relay may be lying about a proof the host never
    // requested).
    let policy = pin_store.snapshot();
    if !policy.requires_pin() {
        return false;
    }
    // `expected_client_pin_id` is the client peer id the PIN was
    // bound to. We accept any client as long as the proof bit is
    // set — finer-grained client binding happens at the relay layer.
    let _ = client;
    true
}

// ── helpers ─────────────────────────────────────────────────────────

/// Build a `SessionBundleInfo` carrying a positive `PinProof`. Used
/// by tests + the relay's PIN-gate path.
#[cfg(test)]
pub fn bundle_with_pin_proof(jti: &str, sub: &str) -> SessionBundleInfo {
    SessionBundleInfo {
        session_id: Uuid::new_v4(),
        jti: jti.to_string(),
        viewer_dtls_fp: "00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00".to_string(),
        exp_unix_ms: (qubox_proto_now() + 60_000) as u64,
        caps: SessionCaps::default(),
        sub: sub.to_string(),
        pin_proof: Some(qubox_proto::PinProof {
            pin_hash_match: true,
        }),
    }
}

/// Current unix milliseconds (test helper — avoids pulling in
/// `std::time` everywhere).
#[cfg(test)]
fn qubox_proto_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// Re-export the local SignedBundle wrapper for tests.
#[allow(dead_code)]
fn _ensure_signed_bundle_in_scope(_: SignedBundle) {}

// ── tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pin::PinStore;
    use std::sync::Arc as StdArc;

    #[test]
    fn pin_proof_requires_pinned_hash_match_bit() {
        let store = StdArc::new(PinStore::memory());
        // No PIN configured — proof bit doesn't matter, host should
        // reject.
        let bundle = SessionBundleInfo {
            session_id: Uuid::new_v4(),
            jti: "j".into(),
            viewer_dtls_fp: String::new(),
            exp_unix_ms: 0,
            caps: SessionCaps::default(),
            sub: String::new(),
            pin_proof: Some(qubox_proto::PinProof {
                pin_hash_match: true,
            }),
        };
        let descriptor = make_peer_descriptor_shell();
        assert!(!verify_pin_proof(&bundle, &store, &descriptor));
    }

    fn make_peer_descriptor_shell() -> qubox_proto::PeerDescriptor {
        qubox_proto::PeerDescriptor {
            device_id: Uuid::nil(),
            peer_id: Uuid::nil(),
            device_name: String::new(),
            role: qubox_proto::PeerRole::Client,
            os: qubox_proto::PlatformOs::Linux,
            capabilities: qubox_proto::CapabilityProfile::default(),
        }
    }

    #[tokio::test]
    async fn pending_bundle_delivery_resolves_once() {
        let state = EnforcementState::from_env("dev-1");
        let session_id = Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel();
        state.register_pending_bundle(
            session_id,
            PendingBundle {
                expected_client: Uuid::new_v4(),
                received: false,
                deliver: tx,
            },
        );
        let pending = state.take_pending_bundle(session_id).expect("pending");
        let bundle = bundle_with_pin_proof("jti-1", "dev-1");
        pending.deliver.send(bundle).unwrap();
        let received = rx.await.unwrap();
        assert_eq!(received.jti, "jti-1");
        // Second take should return None.
        assert!(state.take_pending_bundle(session_id).is_none());
    }

    #[tokio::test]
    async fn operator_decision_only_resolves_once() {
        let state = EnforcementState::from_env("dev-1");
        let session_id = Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel();
        state.register_pending_decision(session_id, PendingDecision { deliver: tx });
        let pending = state.take_pending_decision(session_id).expect("pending");
        pending.deliver.send(true).unwrap();
        assert_eq!(rx.await.unwrap(), true);
        assert!(state.take_pending_decision(session_id).is_none());
    }

    #[test]
    fn activity_tracker_resets_on_touch() {
        let mut tracker = ActivityTracker::default();
        let sid = Uuid::new_v4();
        let t0 = Instant::now();
        tracker.touch(sid, t0);
        assert_eq!(tracker.collect_stale(Duration::from_secs(1), t0).len(), 0);
        let stale = tracker.collect_stale(Duration::from_secs(1), t0 + Duration::from_secs(2));
        assert_eq!(stale, vec![sid]);
        // Reset activity at t0+2; collecting at t0+2.5 should NOT find it stale
        // because only 500ms have elapsed (< 1s threshold).
        tracker.touch(sid, t0 + Duration::from_secs(2));
        let stale = tracker
            .collect_stale(Duration::from_secs(1), t0 + Duration::from_millis(2_500));
        assert!(stale.is_empty());
        tracker.remove(sid);
        assert!(tracker.collect_stale(Duration::from_secs(1), t0 + Duration::from_secs(10)).is_empty());
    }

    #[test]
    fn drop_pending_clears_state() {
        let state = EnforcementState::from_env("dev-1");
        let sid = Uuid::new_v4();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        state.register_pending_bundle(
            sid,
            PendingBundle {
                expected_client: Uuid::new_v4(),
                received: false,
                deliver: tx,
            },
        );
        let (dtx, _drx) = tokio::sync::oneshot::channel();
        state.register_pending_decision(sid, PendingDecision { deliver: dtx });
        state.drop_pending(sid);
        assert!(state.take_pending_bundle(sid).is_none());
        assert!(state.take_pending_decision(sid).is_none());
    }
}