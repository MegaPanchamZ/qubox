//! Host-side recovery key.
//!
//! Per `docs/browser-viewer-identity-and-host-trust.md`:
//!
//! - The host generates a recovery key once at first enrollment and
//!   prints it (or hands it to the GUI / operator console) for the
//!   user to store offline.
//! - The recovery key is wrapped (XChaCha20-Poly1305) under a key
//!   derived from the host's identity seed via Argon2id, so an
//!   attacker that lifts the host policy file cannot immediately
//!   decrypt it.
//! - The recovery key is the gate that lets an operator who has
//!   lost the PIN recover the host without re-pairing.
//!
//! Storage shape (on disk):
//!
//! ```json
//! {
//!   "kdf": "argon2id-v1",
//!   "kdf_salt_hex": "...",
//!   "wrap": "xchacha20poly1305-v1",
//!   "wrapped_key_hex": "...",
//!   "wrapped_nonce_hex": "..."
//! }
//! ```
//!
//! The wrap key is derived from `argon2id(host_identity_seed,
//! kdf_salt)`. The plaintext key is never written to disk.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

const RECOVERY_KEY_LEN: usize = 32;
const WRAP_KDF_ALGO: &str = "argon2id-v1";
const WRAP_CIPHER: &str = "xchacha20poly1305-v1";
const WRAP_NONCE_LEN: usize = 24;
const RECOVERY_FILE_NAME: &str = "recovery_key.json";

/// In-memory recovery key. `Zeroizing` drops the key on scope exit
/// so an attacker that crashes the process and grabs a memory dump
/// has a much smaller window than a plain `Vec<u8>`.
#[derive(Clone)]
pub struct RecoveryKey(Zeroizing<[u8; RECOVERY_KEY_LEN]>);

impl RecoveryKey {
    pub fn generate() -> Self {
        let mut buf = [0u8; RECOVERY_KEY_LEN];
        OsRng.fill_bytes(&mut buf);
        Self(Zeroizing::new(buf))
    }

    pub fn from_raw(raw: [u8; RECOVERY_KEY_LEN]) -> Self {
        Self(Zeroizing::new(raw))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_ref()
    }

    /// Constant-time compare. Returns true iff `candidate` is the
    /// same length and byte-equal to this key.
    pub fn matches(&self, candidate: &[u8]) -> bool {
        if candidate.len() != self.0.len() {
            return false;
        }
        let mut diff: u8 = 0;
        for (a, b) in self.0.iter().zip(candidate.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

impl Drop for RecoveryKey {
    fn drop(&mut self) {
        self.0.as_mut_slice().zeroize();
    }
}

/// Persisted envelope. Serializes as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryRecord {
    pub kdf: String,
    pub kdf_salt_hex: String,
    pub wrap: String,
    pub wrapped_key_hex: String,
    pub wrapped_nonce_hex: String,
    #[serde(default)]
    pub created_unix_ms: Option<u64>,
}

impl RecoveryRecord {
    pub fn is_well_formed(&self) -> bool {
        self.kdf == WRAP_KDF_ALGO
            && self.wrap == WRAP_CIPHER
            && hex::decode(&self.kdf_salt_hex)
                .map(|s| s.len() == 16)
                .unwrap_or(false)
            && hex::decode(&self.wrapped_key_hex).is_ok()
            && hex::decode(&self.wrapped_nonce_hex)
                .map(|n| n.len() == WRAP_NONCE_LEN)
                .unwrap_or(false)
    }
}

/// Host-side recovery key store. Owns the on-disk `recovery_key.json`
/// and the in-memory plaintext. The plaintext is held only while the
/// agent is running; on restart it is re-derived from the wrap
/// envelope using the host identity seed.
pub struct RecoveryStore {
    path: PathBuf,
    inner: Mutex<RecoveryInner>,
}

struct RecoveryInner {
    key: RecoveryKey,
    record: RecoveryRecord,
}

impl RecoveryStore {
    /// Load (or generate) the recovery key. If the file is missing,
    /// a fresh key is generated, wrapped, and persisted. The host
    /// identity seed is the wrap secret — same seed means same
    /// derived wrap key on every run, so a generated key is
    /// recoverable across restarts.
    pub fn load_or_generate(host_identity_seed: &[u8], path: PathBuf) -> anyhow::Result<Self> {
        let inner = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read recovery {}", path.display()))?;
            let record: RecoveryRecord = serde_json::from_str(&raw)
                .with_context(|| format!("parse recovery {}", path.display()))?;
            if !record.is_well_formed() {
                bail!("recovery record malformed");
            }
            let key = unwrap_key(&record, host_identity_seed)?;
            RecoveryInner { key, record }
        } else {
            let key = RecoveryKey::generate();
            let record = wrap_key(&key, host_identity_seed)?;
            let body = serde_json::to_string_pretty(&record)?;
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("create recovery dir {}", parent.display()))?;
                }
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, body.as_bytes())
                .with_context(|| format!("write tmp recovery {}", tmp.display()))?;
            std::fs::rename(&tmp, &path)
                .with_context(|| format!("rename recovery to {}", path.display()))?;
            tracing::warn!(
                path = %path.display(),
                "first-boot: generated recovery key. Print `qubox-host-agent recovery show` and store it offline."
            );
            RecoveryInner { key, record }
        };
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    pub fn default_path(identity_path: &Path) -> PathBuf {
        identity_path.with_file_name(RECOVERY_FILE_NAME)
    }

    /// Return a copy of the plaintext key. The caller MUST treat it
    /// as secret: wrap in `Zeroizing`, do not log, etc.
    pub fn reveal_key(&self) -> RecoveryKey {
        self.inner.lock().expect("recovery poisoned").key.clone()
    }

    /// Convenience: match a candidate against the stored key.
    pub fn matches(&self, candidate: &[u8]) -> bool {
        self.inner
            .lock()
            .expect("recovery poisoned")
            .key
            .matches(candidate)
    }

    /// Best-effort print of the recovery key as a hex string. Used
    /// by the `recovery show` CLI subcommand.
    pub fn display_key(&self) -> String {
        hex::encode(self.inner.lock().expect("recovery poisoned").key.as_bytes())
    }

    /// Replace the stored key (operator wants to rotate). Re-wraps
    /// and persists; returns the new key in plaintext so the
    /// operator can read it back via `recovery show`.
    pub fn rotate(&self, host_identity_seed: &[u8]) -> anyhow::Result<RecoveryKey> {
        let new_key = RecoveryKey::generate();
        let record = wrap_key(&new_key, host_identity_seed)?;
        {
            let mut inner = self.inner.lock().expect("recovery poisoned");
            inner.key = new_key.clone();
            inner.record = record.clone();
        }
        persist_record(&self.path, &record)?;
        Ok(new_key)
    }
}

fn derive_wrap_key(seed: &[u8], salt: &[u8]) -> anyhow::Result<[u8; 32]> {
    if salt.len() != 16 {
        bail!("KDF salt must be 16 bytes");
    }
    let params = Params::new(19_456, 2, 1, Some(32))
        .map_err(|e| anyhow!("invalid Argon2 parameters: {e}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    argon
        .hash_password_into(seed, salt, &mut out)
        .map_err(|e| anyhow!("argon2 derive failed: {e}"))?;
    Ok(out)
}

fn wrap_key(key: &RecoveryKey, host_identity_seed: &[u8]) -> anyhow::Result<RecoveryRecord> {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let wrap = derive_wrap_key(host_identity_seed, &salt)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&wrap));
    let mut nonce = [0u8; WRAP_NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let payload = Payload {
        msg: key.as_bytes(),
        aad: b"qubox-recovery-v1",
    };
    let mut ct = cipher
        .encrypt(XNonce::from_slice(&nonce), payload)
        .map_err(|e| anyhow!("wrap_key encrypt failed: {e}"))?;
    // bound to record_version
    let record = RecoveryRecord {
        kdf: WRAP_KDF_ALGO.into(),
        kdf_salt_hex: hex::encode(salt),
        wrap: WRAP_CIPHER.into(),
        wrapped_key_hex: hex::encode(ct.as_slice()),
        wrapped_nonce_hex: hex::encode(nonce),
        created_unix_ms: Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        ),
    };
    ct.zeroize();
    Ok(record)
}

fn unwrap_key(record: &RecoveryRecord, host_identity_seed: &[u8]) -> anyhow::Result<RecoveryKey> {
    if !record.is_well_formed() {
        bail!("recovery record not well-formed");
    }
    let salt = hex::decode(&record.kdf_salt_hex).map_err(|e| anyhow!("salt hex: {e}"))?;
    let ct = hex::decode(&record.wrapped_key_hex).map_err(|e| anyhow!("ct hex: {e}"))?;
    let nonce_bytes =
        hex::decode(&record.wrapped_nonce_hex).map_err(|e| anyhow!("nonce hex: {e}"))?;
    let wrap = derive_wrap_key(host_identity_seed, &salt)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&wrap));
    let payload = Payload {
        msg: &ct,
        aad: b"qubox-recovery-v1",
    };
    let mut pt = cipher
        .decrypt(XNonce::from_slice(&nonce_bytes), payload)
        .map_err(|e| anyhow!("unwrap_key decrypt failed: {e}"))?;
    if pt.len() != RECOVERY_KEY_LEN {
        pt.zeroize();
        bail!("decrypted recovery key has wrong length: {}", pt.len());
    }
    let mut buf = [0u8; RECOVERY_KEY_LEN];
    buf.copy_from_slice(&pt);
    pt.zeroize();
    Ok(RecoveryKey::from_raw(buf))
}

fn persist_record(path: &Path, record: &RecoveryRecord) -> anyhow::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let body = serde_json::to_string_pretty(record)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("qubox-recov-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    const SEED: [u8; 32] = [
        0x9b, 0x2c, 0x88, 0xd1, 0x52, 0x1f, 0xc6, 0x6e, 0x71, 0xa4, 0xab, 0x77, 0x21, 0x3a, 0x4d,
        0x10, 0x55, 0x90, 0xee, 0x83, 0x4f, 0x2b, 0xb1, 0x6c, 0x18, 0x73, 0xd4, 0x25, 0xfa, 0x06,
        0x91, 0xc7,
    ];

    #[test]
    fn round_trip_persists_and_reloads() {
        let dir = tempdir();
        let path = dir.join("recovery_key.json");
        let store = RecoveryStore::load_or_generate(&SEED, path.clone()).unwrap();
        let original = store.display_key();
        let store2 = RecoveryStore::load_or_generate(&SEED, path.clone()).unwrap();
        let restored = store2.display_key();
        assert_eq!(original, restored);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotate_changes_key() {
        let dir = tempdir();
        let path = dir.join("recovery_key.json");
        let store = RecoveryStore::load_or_generate(&SEED, path.clone()).unwrap();
        let original = store.display_key();
        let new_key = store.rotate(&SEED).unwrap();
        assert_eq!(hex::encode(new_key.as_bytes()), store.display_key());
        assert_ne!(original, store.display_key());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wrong_seed_fails_unwrap() {
        let dir = tempdir();
        let path = dir.join("recovery_key.json");
        let _store = RecoveryStore::load_or_generate(&SEED, path.clone()).unwrap();
        let err = RecoveryStore::load_or_generate(&[0u8; 32], path.clone())
            .err()
            .expect("expected failure with wrong seed");
        assert!(err.to_string().contains("unwrap_key"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn matches_constant_time() {
        let k = RecoveryKey::from_raw([7u8; 32]);
        assert!(k.matches(&[7u8; 32]));
        assert!(!k.matches(&[8u8; 32]));
        assert!(!k.matches(&[7u8; 31]));
    }
}
