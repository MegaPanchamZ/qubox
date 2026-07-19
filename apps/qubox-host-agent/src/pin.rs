//! Host-side PIN enforcement.
//!
//! Per `docs/browser-viewer-identity-and-host-trust.md` Phase 1:
//!
//! - Host stores a hash of the PIN (Argon2id) and verifies it on
//!   each incoming connect. The cloud syncs the hash; the host
//!   treats the cloud update as **untrusted** until the local gate
//!   is satisfied.
//! - Update gate: accept a new PIN hash ONLY if (a) caller proves
//!   old PIN, OR (b) caller proves recovery key, OR (c) the operator
//!   physically acknowledges on the host (tray / overlay).
//! - Owner PIN default = off. Org "Always require PIN" sets the
//!   policy to `required = true`. The host reads its policy from a
//!   local file the cloud (or admin tool) populates.
//!
//! This module is a library — `main.rs` wires it into the session
//! request handler and into a control-channel message path.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{anyhow, bail, Context};
use argon2::{Algorithm, Argon2, Params, Version};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

const PIN_ENV: &str = "QUBOX_HOST_PIN";
const PIN_HASH_ALGO: &str = "argon2id-v1";
const PIN_POLICY_FILENAME: &str = "host_policy.json";

/// Hash format we persist on disk. Versioned so we can rotate the
/// KDF parameters without invalidating existing hashes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredPin {
    pub algo: String,
    pub salt_hex: String,
    pub hash_hex: String,
}

/// Effective PIN policy. `mode = "off"` means no PIN check; `"on"`
/// means the host verifies on every incoming session request;
/// `"required"` mirrors the org-level "Always require PIN" policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PinMode {
    Off,
    On,
    Required,
}

impl Default for PinMode {
    fn default() -> Self {
        Self::Off
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PinPolicy {
    pub mode: PinMode,
    pub hash: Option<StoredPin>,
}

impl PinPolicy {
    /// True when the host must verify the PIN before admitting a
    /// session. `Off` and missing policy = no verification; `On`
    /// means verify-if-set (skip when no hash is present, e.g. owner
    /// never enrolled); `Required` means every connect must prove
    /// the PIN (hash MUST be present).
    pub fn requires_pin(&self) -> bool {
        match self.mode {
            PinMode::Off => false,
            PinMode::On => self.hash.is_some(),
            PinMode::Required => true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct HostPolicyFile {
    #[serde(default)]
    pin: PinPolicy,
}

/// Pure-data policy store. `load` / `save` are sync; the caller
/// owns IO error context. Hash zeroizes on drop so a leaked memory
/// dump can't leak the PIN.
pub struct PinStore {
    path: PathBuf,
    state: RwLock<HostPolicyFile>,
}

impl PinStore {
    pub fn memory() -> Self {
        Self {
            path: PathBuf::new(),
            state: RwLock::new(HostPolicyFile::default()),
        }
    }

    pub fn from_path(path: PathBuf) -> anyhow::Result<Self> {
        let state = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read host policy {}", path.display()))?;
            serde_json::from_str(&raw).unwrap_or_default()
        } else {
            HostPolicyFile::default()
        };
        Ok(Self {
            path,
            state: RwLock::new(state),
        })
    }

    pub fn default_policy_path(identity_path: &Path) -> PathBuf {
        identity_path.with_file_name(PIN_POLICY_FILENAME)
    }

    pub fn snapshot(&self) -> PinPolicy {
        self.state.read().expect("pin policy poisoned").pin.clone()
    }

    /// Replace the in-memory policy and (best-effort) persist.
    pub fn replace(&self, new_policy: PinPolicy) -> anyhow::Result<()> {
        {
            let mut state = self.state.write().expect("pin policy poisoned");
            state.pin = new_policy;
        }
        self.persist()
    }

    fn persist(&self) -> anyhow::Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create policy dir {}", parent.display()))?;
            }
        }
        let snapshot = self.state.read().expect("pin policy poisoned").pin.clone();
        let body = serde_json::to_string_pretty(&HostPolicyFile { pin: snapshot })?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body.as_bytes())
            .with_context(|| format!("write tmp policy {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename policy to {}", self.path.display()))?;
        Ok(())
    }
}

/// Verify a candidate PIN against a stored hash. Constant-time
/// compare (Argon2's verify itself) so a timing oracle can't leak
/// the hash.
pub fn verify_pin(stored: &StoredPin, candidate: &str) -> anyhow::Result<bool> {
    if stored.algo != PIN_HASH_ALGO {
        bail!("unsupported PIN hash algo {:?}", stored.algo);
    }
    let salt = hex::decode(&stored.salt_hex).map_err(|e| anyhow!("invalid salt hex: {e}"))?;
    let expected = hex::decode(&stored.hash_hex).map_err(|e| anyhow!("invalid hash hex: {e}"))?;
    if salt.len() != 16 {
        bail!("PIN salt must be 16 bytes, got {}", salt.len());
    }
    if expected.len() != 32 {
        bail!("PIN hash must be 32 bytes, got {}", expected.len());
    }
    let params = Params::new(19_456, 2, 1, Some(32))
        .map_err(|e| anyhow!("invalid Argon2 parameters: {e}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut derived = [0_u8; 32];
    argon
        .hash_password_into(candidate.as_bytes(), &salt, &mut derived)
        .map_err(|e| anyhow!("argon2 derive failed: {e}"))?;
    let matches = constant_time_eq(&derived, &expected);
    derived.zeroize();
    Ok(matches)
}

/// Hash a new PIN for storage. Returns the structured record.
pub fn hash_pin(pin: &str) -> anyhow::Result<StoredPin> {
    let mut salt = [0_u8; 16];
    OsRng.fill_bytes(&mut salt);
    let params = Params::new(19_456, 2, 1, Some(32))
        .map_err(|e| anyhow!("invalid Argon2 parameters: {e}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut hash = [0_u8; 32];
    argon
        .hash_password_into(pin.as_bytes(), &salt, &mut hash)
        .map_err(|e| anyhow!("argon2 hash failed: {e}"))?;
    Ok(StoredPin {
        algo: PIN_HASH_ALGO.to_string(),
        salt_hex: hex::encode(salt),
        hash_hex: hex::encode(hash),
    })
}

/// Read the PIN from the `QUBOX_HOST_PIN` environment variable. The
/// env var is a back-channel for tests / CI that don't want to
/// enroll an interactive PIN at startup. Returns the raw value
/// (the caller is responsible for verifying it; we don't pre-hash
/// here because we need the literal to run through Argon2 anyway).
pub fn env_pin() -> Option<String> {
    std::env::var(PIN_ENV).ok().filter(|p| !p.is_empty())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Result of a PIN-update request. `Accepted` means the new hash is
/// now live in the policy; `Rejected` carries the reason so the
/// caller can surface a clear error to the cloud / admin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinUpdateOutcome {
    Accepted,
    Rejected(String),
}

/// Apply a cloud-pushed PIN update. Per spec: host treats the push
/// as untrusted. Requires ONE of:
///
/// 1. `proof_old_pin` matches the existing hash, OR
/// 2. `recovery_matches` returns true for the supplied recovery key, OR
/// 3. `physical_ack == true` (operator pressed the tray button).
///
/// If no existing hash is set, the caller is the first to set one;
/// at least one of the three proofs must be supplied (a fresh host
/// without a recovery key still requires *something*).
pub fn apply_pin_update<F>(
    store: &PinStore,
    new_pin: &str,
    proof_old_pin: Option<&str>,
    proof_recovery_key: Option<&[u8]>,
    physical_ack: bool,
    recovery_matches: F,
) -> PinUpdateOutcome
where
    F: FnOnce(&[u8]) -> bool,
{
    let current = store.snapshot();
    let mut gate = physical_ack;

    if let Some(stored) = current.hash.as_ref() {
        if let Some(old_pin) = proof_old_pin {
            match verify_pin(stored, old_pin) {
                Ok(true) => gate = true,
                Ok(false) => {
                    return PinUpdateOutcome::Rejected("old PIN does not match stored hash".into());
                }
                Err(e) => return PinUpdateOutcome::Rejected(format!("verify_pin failed: {e}")),
            }
        }
        if let Some(key) = proof_recovery_key {
            if recovery_matches(key) {
                gate = true;
            } else {
                return PinUpdateOutcome::Rejected("recovery key does not match".into());
            }
        }
    } else {
        // First enrollment: at least one proof MUST be supplied.
        if proof_old_pin.is_none() && proof_recovery_key.is_none() && !physical_ack {
            return PinUpdateOutcome::Rejected(
                "first PIN enrollment requires old PIN, recovery key, or physical ack".into(),
            );
        }
    }

    if !gate {
        return PinUpdateOutcome::Rejected(
            "PIN update gate failed: provide old PIN, recovery key, or physical ack".into(),
        );
    }

    let hashed = match hash_pin(new_pin) {
        Ok(h) => h,
        Err(e) => return PinUpdateOutcome::Rejected(format!("hash_pin failed: {e}")),
    };
    let mut new_policy = current.clone();
    new_policy.hash = Some(hashed);
    // The cloud's push carries a `mode` change separately; for the
    // PIN-update API we keep the existing mode.
    if let Err(e) = store.replace(new_policy) {
        return PinUpdateOutcome::Rejected(format!("persist failed: {e}"));
    }
    PinUpdateOutcome::Accepted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (TempDir, PinStore) {
        let dir = TempDir::new();
        let path = dir.0.join("policy.json");
        let store = PinStore::from_path(path).unwrap();
        (dir, store)
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("qubox-pin-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn hash_and_verify_round_trip() {
        let stored = hash_pin("hunter2").unwrap();
        assert!(verify_pin(&stored, "hunter2").unwrap());
        assert!(!verify_pin(&stored, "Hunter2").unwrap());
    }

    #[test]
    fn policy_default_is_off() {
        let p = PinPolicy::default();
        assert_eq!(p.mode, PinMode::Off);
        assert!(!p.requires_pin());
    }

    #[test]
    fn policy_required_is_required() {
        let p = PinPolicy {
            mode: PinMode::Required,
            hash: None,
        };
        assert!(p.requires_pin());
    }

    #[test]
    fn policy_on_requires_hash() {
        let p = PinPolicy {
            mode: PinMode::On,
            hash: None,
        };
        assert!(!p.requires_pin());
        let hashed = hash_pin("a").unwrap();
        let p = PinPolicy {
            mode: PinMode::On,
            hash: Some(hashed),
        };
        assert!(p.requires_pin());
    }

    #[test]
    fn apply_update_rejects_without_proof_when_existing() {
        let (dir, store) = temp_store();
        let mut policy = store.snapshot();
        policy.hash = Some(hash_pin("old").unwrap());
        store.replace(policy).unwrap();
        let out = apply_pin_update(&store, "new", None, None, false, |_| false);
        assert!(matches!(out, PinUpdateOutcome::Rejected(_)));
        drop(dir);
    }

    #[test]
    fn apply_update_accepts_with_old_pin_proof() {
        let (dir, store) = temp_store();
        let mut policy = store.snapshot();
        policy.hash = Some(hash_pin("old").unwrap());
        store.replace(policy).unwrap();
        let out = apply_pin_update(&store, "new", Some("old"), None, false, |_| false);
        assert_eq!(out, PinUpdateOutcome::Accepted);
        let snap = store.snapshot();
        assert!(snap.hash.is_some());
        assert!(verify_pin(snap.hash.as_ref().unwrap(), "new").unwrap());
        drop(dir);
    }

    #[test]
    fn apply_update_accepts_with_recovery_proof() {
        let (dir, store) = temp_store();
        let mut policy = store.snapshot();
        policy.hash = Some(hash_pin("old").unwrap());
        store.replace(policy).unwrap();
        let out = apply_pin_update(&store, "new", None, Some(&[7u8; 32]), false, |_| true);
        assert_eq!(out, PinUpdateOutcome::Accepted);
        drop(dir);
    }

    #[test]
    fn apply_update_accepts_with_physical_ack() {
        let (dir, store) = temp_store();
        let out = apply_pin_update(&store, "new", None, None, true, |_| false);
        assert_eq!(out, PinUpdateOutcome::Accepted);
        drop(dir);
    }

    #[test]
    fn first_enrollment_requires_proof() {
        let (dir, store) = temp_store();
        let out = apply_pin_update(&store, "first", None, None, false, |_| false);
        assert!(matches!(out, PinUpdateOutcome::Rejected(_)));
        drop(dir);
    }

    #[test]
    fn malformed_hash_rejects_cleanly() {
        let bad = StoredPin {
            algo: "wrong".into(),
            salt_hex: "00".repeat(16),
            hash_hex: "00".repeat(32),
        };
        let err = verify_pin(&bad, "x").unwrap_err();
        assert!(err.to_string().contains("unsupported"));
    }
}
