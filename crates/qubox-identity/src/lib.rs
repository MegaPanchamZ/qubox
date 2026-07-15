use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use ed25519_dalek::{SigningKey, VerifyingKey, SECRET_KEY_LENGTH};
use qubox_proto::PeerRole;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroize;

const IDENTITY_PASSPHRASE_ENV: &str = "QUBOX_IDENTITY_PASSPHRASE";
const IDENTITY_MASTER_KEY_FILENAME: &str = "identity.key";
const AT_REST_AEAD_TAG: &str = "qubox-identity-v3-cha20p1305";

/// Build the AAD used to authenticate the envelope at both encrypt
/// and decrypt time. Must produce byte-identical output for the same
/// `public_key` so the Poly1305 tag verifies.
fn build_aad(public_key: &[u8; SECRET_KEY_LENGTH]) -> String {
    let mut h = Sha256::new();
    h.update(public_key);
    let bytes = h.finalize();
    let mut pk_hash = [0_u8; 32];
    pk_hash.copy_from_slice(&bytes);
    serde_json::json!({
        "format": AT_REST_AEAD_TAG,
        "pk_sha256_hex": hex::encode(pk_hash),
    })
    .to_string()
}

pub const IDENTITY_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceIdentity {
    pub schema_version: u32,
    pub device_id: Uuid,
    pub host_peer_id: Uuid,
    pub client_peer_id: Uuid,
    pub display_name: String,
    pub public_key: [u8; SECRET_KEY_LENGTH],
    pub encrypted_private_key: KeyEnvelope,
}

/// On-disk envelope for the Ed25519 private key. The key-encrypting
/// key is derived from `QUBOX_IDENTITY_PASSPHRASE` (Argon2id)
/// or from a side-car `identity.key` master file (mode 0o600). Without
/// one of those two sources the private key cannot be unsealed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyEnvelope {
    pub tag: String,
    pub salt_hex: String,
    pub nonce_hex: String,
    pub ciphertext_hex: String,
    pub aad_pk_sha256_hex: String,
}

/// Decrypted plaintext form of a [`DeviceIdentity`]; the private key
/// zeroizes on drop. Useful when a caller needs to do many signatures
/// in a hot loop and wants to unseal once.
pub struct UnlockedIdentity {
    pub device_id: Uuid,
    pub host_peer_id: Uuid,
    pub client_peer_id: Uuid,
    pub display_name: String,
    pub public_key: [u8; SECRET_KEY_LENGTH],
    pub private_key: [u8; SECRET_KEY_LENGTH],
}

impl Drop for UnlockedIdentity {
    fn drop(&mut self) {
        self.private_key.zeroize();
    }
}

impl Clone for UnlockedIdentity {
    fn clone(&self) -> Self {
        Self {
            device_id: self.device_id,
            host_peer_id: self.host_peer_id,
            client_peer_id: self.client_peer_id,
            display_name: self.display_name.clone(),
            public_key: self.public_key,
            private_key: self.private_key,
        }
    }
}

impl std::fmt::Debug for UnlockedIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnlockedIdentity")
            .field("device_id", &self.device_id)
            .field("host_peer_id", &self.host_peer_id)
            .field("client_peer_id", &self.client_peer_id)
            .field("display_name", &self.display_name)
            .field("public_key", &hex::encode(self.public_key))
            .field("private_key", &"[redacted]")
            .finish()
    }
}

impl DeviceIdentity {
    /// Construct a fresh identity with a freshly-generated Ed25519
    /// keypair. Returns `(identity, plaintext_private_key)` so the
    /// caller can seal the envelope before persisting. The private
    /// key is wrapped in `Zeroize` and should be dropped promptly.
    pub fn new(display_name: String) -> (Self, [u8; SECRET_KEY_LENGTH]) {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let private_bytes = signing_key.to_bytes();
        let identity = Self {
            schema_version: IDENTITY_SCHEMA_VERSION,
            device_id: Uuid::new_v4(),
            host_peer_id: Uuid::new_v4(),
            client_peer_id: Uuid::new_v4(),
            display_name,
            public_key: verifying_key.to_bytes(),
            encrypted_private_key: KeyEnvelope::empty_placeholder(),
        };
        (identity, private_bytes)
    }

    /// Decrypt the private key using the environmental KEK. The
    /// returned `UnlockedIdentity` zeroizes its private key on drop.
    pub fn unlock(&self, identity_path: Option<&Path>) -> anyhow::Result<UnlockedIdentity> {
        let mut kek = derive_kek(identity_path)?;
        let private = decrypt_envelope(&kek, &self.encrypted_private_key, &self.public_key)?;
        kek.zeroize();
        Ok(UnlockedIdentity {
            device_id: self.device_id,
            host_peer_id: self.host_peer_id,
            client_peer_id: self.client_peer_id,
            display_name: self.display_name.clone(),
            public_key: self.public_key,
            private_key: private,
        })
    }

    /// Seal `private_key` into `self.encrypted_private_key` using
    /// `kek`. Caller picks `kek` from `derive_kek` or
    /// `ensure_kek_for_write`.
    fn seal_with_kek(
        &mut self,
        kek: &[u8; 32],
        salt: &[u8; 16],
        private_key: &[u8; SECRET_KEY_LENGTH],
    ) -> anyhow::Result<()> {
        let sk = SigningKey::from_bytes(private_key);
        if sk.verifying_key().to_bytes() != self.public_key {
            anyhow::bail!("private key does not match identity.public_key; refusing to seal");
        }
        self.encrypted_private_key = encrypt_envelope(kek, salt, private_key, &self.public_key)?;
        Ok(())
    }

    /// Convenience: re-derive the `SigningKey` by unsealing the
    /// private key. Equivalent to `self.unlock(path)?.signing_key()`
    /// but only pays the cost of one AES round trip — useful for
    /// `SignedHello::sign` which is called from a tight loop in
    /// tests/CLI/GUI.
    pub fn signing_key(&self, identity_path: Option<&Path>) -> anyhow::Result<SigningKey> {
        let mut kek = derive_kek(identity_path)?;
        let private = decrypt_envelope(&kek, &self.encrypted_private_key, &self.public_key)?;
        kek.zeroize();
        Ok(SigningKey::from_bytes(&private))
    }

    /// Public key is stored plaintext so this method is free.
    pub fn verifying_key(&self) -> VerifyingKey {
        VerifyingKey::from_bytes(&self.public_key)
            .expect("DeviceIdentity stored an invalid public key")
    }

    pub fn peer_id_for(&self, role: PeerRole) -> Uuid {
        match role {
            PeerRole::Host => self.host_peer_id,
            PeerRole::Client => self.client_peer_id,
        }
    }
}

impl KeyEnvelope {
    fn empty_placeholder() -> Self {
        Self {
            tag: String::new(),
            salt_hex: String::new(),
            nonce_hex: String::new(),
            ciphertext_hex: String::new(),
            aad_pk_sha256_hex: String::new(),
        }
    }
}

fn master_key_path(identity_path: &Path) -> PathBuf {
    identity_path.with_file_name(IDENTITY_MASTER_KEY_FILENAME)
}

fn env_passphrase() -> Option<String> {
    env::var(IDENTITY_PASSPHRASE_ENV)
        .ok()
        .filter(|p| !p.is_empty())
}

/// Derive a 32-byte KEK from the active source. See module docs.
fn derive_kek(identity_path: Option<&Path>) -> anyhow::Result<[u8; 32]> {
    if let Some(passphrase) = env_passphrase() {
        let path = identity_path.ok_or_else(|| {
            anyhow!(
                "identity path required to derive passphrase-mode KEK (env {})",
                IDENTITY_PASSPHRASE_ENV
            )
        })?;
        let salt = read_envelope_salt(path)?;
        let mut out = [0_u8; 32];
        derive_argon2_kek(passphrase.as_bytes(), &salt, &mut out)?;
        return Ok(out);
    }
    let path = identity_path.ok_or_else(|| {
        anyhow!(
            "identity path required in master-key mode (set {} or create {})",
            IDENTITY_PASSPHRASE_ENV,
            IDENTITY_MASTER_KEY_FILENAME
        )
    })?;
    load_master_key(&master_key_path(path))
}

/// Same as `derive_kek` but uses a *newly-generated* salt instead of
/// reading one off disk. Only used for the very first run of a brand
/// new identity in passphrase mode.
fn derive_passphrase_kek_new_salt(passphrase: &str, salt: &[u8; 16]) -> anyhow::Result<[u8; 32]> {
    let mut out = [0_u8; 32];
    derive_argon2_kek(passphrase.as_bytes(), salt, &mut out)?;
    Ok(out)
}

fn derive_argon2_kek(passphrase: &[u8], salt: &[u8], out: &mut [u8; 32]) -> anyhow::Result<()> {
    let params = Params::new(19_456, 2, 1, Some(32))
        .map_err(|e| anyhow!("invalid Argon2 parameters: {e}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    argon
        .hash_password_into(passphrase, salt, out)
        .map_err(|e| anyhow!("argon2id KDF failed: {e}"))?;
    Ok(())
}

fn read_envelope_salt(path: &Path) -> anyhow::Result<Vec<u8>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read identity file {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("identity file {} is not valid JSON", path.display()))?;
    let salt_hex = value
        .get("encrypted_private_key")
        .and_then(|v| v.get("salt_hex"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow!(
                "identity file {} has no encrypted_private_key.salt_hex (cannot derive passphrase KEK)",
                path.display()
            )
        })?;
    hex::decode(salt_hex).with_context(|| format!("invalid salt hex in {}", path.display()))
}

#[cfg(unix)]
fn load_master_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    use std::os::unix::fs::PermissionsExt;

    if !path.exists() {
        anyhow::bail!(
            "missing identity master key at {}: set {} or run with the file in place",
            path.display(),
            IDENTITY_PASSPHRASE_ENV
        );
    }
    let mode = fs::metadata(path)?.permissions().mode() & 0o777;
    if mode != 0o600 {
        tracing::warn!(
            path = %path.display(),
            mode = %format_args!("0o{:o}", mode),
            "tightening identity master key permissions from 0o{:o} to 0o600",
            mode
        );
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms)?;
    }
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read identity master key {}", path.display()))?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "identity master key at {} has wrong length: {} bytes (expected 32)",
            path.display(),
            bytes.len()
        );
    }
    let mut out = [0_u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

#[cfg(not(unix))]
fn load_master_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    static ACL_WARN: std::sync::Once = std::sync::Once::new();
    if !path.exists() {
        anyhow::bail!(
            "missing identity master key at {}: set {} or create the file with 32 random bytes",
            path.display(),
            IDENTITY_PASSPHRASE_ENV
        );
    }
    ACL_WARN.call_once(|| {
        tracing::warn!(
            path = %path.display(),
            "POSIX 0o600 not enforced on this platform — restrict the master key ACL through your OS-specific mechanism"
        );
    });
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read identity master key {}", path.display()))?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "identity master key at {} has wrong length: {} bytes (expected 32)",
            path.display(),
            bytes.len()
        );
    }
    let mut out = [0_u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

#[cfg(unix)]
fn ensure_master_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if path.exists() {
        return load_master_key(path);
    }
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true).mode(0o600);
    let mut file = opts
        .open(path)
        .with_context(|| format!("failed to create master key file {}", path.display()))?;
    file.write_all(&bytes)?;
    let _ = file.sync_all();
    Ok(bytes)
}

#[cfg(not(unix))]
fn ensure_master_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    use std::io::Write;
    static ACL_WARN: std::sync::Once = std::sync::Once::new();
    if path.exists() {
        return load_master_key(path);
    }
    ACL_WARN.call_once(|| {
        tracing::warn!(
            path = %path.display(),
            "POSIX 0o600 not enforced on this platform — restrict the master key ACL through your OS-specific mechanism"
        );
    });
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create master key file {}", path.display()))?;
    file.write_all(&bytes)?;
    Ok(bytes)
}

#[cfg(unix)]
fn tighten_existing_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path)?;
    let current_mode = metadata.permissions().mode() & 0o777;
    if current_mode != 0o600 {
        tracing::warn!(
            path = %path.display(),
            mode = %format_args!("0o{:o}", current_mode),
            "tightening identity file permissions from 0o{:o} to 0o600",
            current_mode,
        );
        let mut perms = metadata.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn tighten_existing_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

pub fn default_identity_path() -> PathBuf {
    if let Ok(path) = env::var("QUBOX_IDENTITY_PATH") {
        return PathBuf::from(path);
    }
    if cfg!(target_os = "windows") {
        if let Ok(app_data) = env::var("APPDATA") {
            return PathBuf::from(app_data).join("Qubox").join("identity.json");
        }
    }
    if cfg!(target_os = "macos") {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Qubox")
                .join("identity.json");
        }
    }
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home)
            .join("qubox")
            .join("identity.json");
    }
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("qubox")
            .join("identity.json");
    }
    PathBuf::from(".qubox").join("identity.json")
}

fn default_device_name() -> String {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unnamed-device".to_string())
}

pub fn load_or_create_identity(
    path_override: Option<PathBuf>,
    display_name_override: Option<String>,
) -> anyhow::Result<(DeviceIdentity, PathBuf)> {
    let path = path_override.unwrap_or_else(default_identity_path);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create identity directory {}", parent.display()))?;
    }

    let display_name = display_name_override.unwrap_or_else(default_device_name);

    if !path.exists() {
        // First-run path.
        let (mut identity, private_bytes) = DeviceIdentity::new(display_name);
        let KekAndSalt { kek, salt } = pick_kek_and_salt_for_first_run(&path)?;
        identity
            .seal_with_kek(&kek, &salt, &private_bytes)
            .map_err(|e| anyhow!("first-run seal failed: {e}"))?;
        write_identity_with_mode(&path, &identity)?;
        let mut kb = kek;
        kb.zeroize();
        let mut pb = private_bytes;
        pb.zeroize();
        tighten_files(&path)?;
        return Ok((identity, path));
    }

    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read identity file {}", path.display()))?;

    let mut identity: DeviceIdentity = match serde_json::from_str(&text) {
        Ok(identity) => identity,
        Err(parse_err) => {
            // Either v1 (no keypair) or v2 (plaintext private_key). The
            // v3 schema is incompatible on the field level so the
            // initial parse fails for both. Disambiguate by looking at
            // whether the JSON has a `private_key` field.
            let raw: serde_json::Value = serde_json::from_str(&text).with_context(|| {
                format!(
                    "failed to parse identity file {} (neither v1, v2 nor v3): {parse_err}",
                    path.display()
                )
            })?;
            if raw.get("private_key").is_some() {
                // v2: reparse with the V2 shape.
                #[derive(Deserialize)]
                struct V2Identity {
                    #[allow(dead_code)]
                    schema_version: u32,
                    device_id: Uuid,
                    host_peer_id: Uuid,
                    client_peer_id: Uuid,
                    display_name: String,
                    private_key: [u8; SECRET_KEY_LENGTH],
                    public_key: [u8; SECRET_KEY_LENGTH],
                }
                let v2: V2Identity = serde_json::from_value(raw).with_context(|| {
                    format!("failed to parse v2 identity file {}", path.display())
                })?;
                let mut migrated = DeviceIdentity {
                    schema_version: IDENTITY_SCHEMA_VERSION,
                    device_id: v2.device_id,
                    host_peer_id: v2.host_peer_id,
                    client_peer_id: v2.client_peer_id,
                    display_name: v2.display_name.clone(),
                    public_key: v2.public_key,
                    encrypted_private_key: KeyEnvelope::empty_placeholder(),
                };
                let KekAndSalt { kek, salt } = pick_kek_and_salt_for_first_run(&path)?;
                migrated.seal_with_kek(&kek, &salt, &v2.private_key)?;
                write_identity_with_mode(&path, &migrated)?;
                let mut kb = kek;
                kb.zeroize();
                tighten_files(&path)?;
                return Ok((migrated, path));
            } else {
                // v1: no keypair at all.
                #[derive(Deserialize)]
                struct V1Identity {
                    #[allow(dead_code)]
                    schema_version: u32,
                    device_id: Uuid,
                    host_peer_id: Uuid,
                    client_peer_id: Uuid,
                    display_name: String,
                }
                let v1: V1Identity = serde_json::from_value(raw).with_context(|| {
                    format!("failed to parse v1 identity file {}", path.display())
                })?;
                let (mut fresh, private_bytes) = DeviceIdentity::new(v1.display_name.clone());
                fresh.device_id = v1.device_id;
                fresh.host_peer_id = v1.host_peer_id;
                fresh.client_peer_id = v1.client_peer_id;
                let KekAndSalt { kek, salt } = pick_kek_and_salt_for_first_run(&path)?;
                fresh.seal_with_kek(&kek, &salt, &private_bytes)?;
                write_identity_with_mode(&path, &fresh)?;
                let mut kb = kek;
                kb.zeroize();
                let mut pb = private_bytes;
                pb.zeroize();
                tighten_files(&path)?;
                return Ok((fresh, path));
            }
        }
    };

    if identity.schema_version != IDENTITY_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported identity schema version {} in {}",
            identity.schema_version,
            path.display()
        );
    }

    // Sanity-check: re-derive the public key from the unsealed
    // private key and assert equality with `identity.public_key`.
    // This catches wrong-passphrase / wrong-master-key / corrupted
    // envelope on every load.
    let unlocked = identity.unlock(Some(&path))?;
    {
        let sk = SigningKey::from_bytes(&unlocked.private_key);
        let derived = sk.verifying_key().to_bytes();
        if derived != identity.public_key {
            anyhow::bail!(
                "identity file {} has mismatched private/public keys (passphrase/master key wrong?)",
                path.display()
            );
        }
    }

    let rename = identity.display_name != display_name;
    if rename {
        identity.display_name = display_name;
        // Re-seal with the same KEK so the on-disk file reflects
        // the new display name. We re-use the existing envelope's
        // salt so a reload still derives the same KEK.
        let mut kek = derive_kek(Some(&path))?;
        let existing_salt = hex::decode(&identity.encrypted_private_key.salt_hex)
            .map_err(|e| anyhow!("invalid salt hex in existing envelope: {e}"))?;
        let mut salt = [0_u8; 16];
        if existing_salt.len() != 16 {
            anyhow::bail!("existing envelope salt is not 16 bytes");
        }
        salt.copy_from_slice(&existing_salt);
        let mut sealed_private = unlocked.private_key;
        identity.seal_with_kek(&kek, &salt, &sealed_private)?;
        sealed_private.zeroize();
        kek.zeroize();
        write_identity_with_mode(&path, &identity)?;
    }
    tighten_files(&path)?;

    Ok((identity, path))
}

/// Compute a KEK and the salt that goes with it (passphrase mode) or
/// the master-key file that backs it (master-key mode). For passphrase
/// mode the caller MUST pass the returned salt to `encrypt_envelope`
/// so the on-disk envelope matches the KEK derivation. For
/// master-key mode the returned salt is unused (the master key is
/// opaque).
struct KekAndSalt {
    kek: [u8; 32],
    salt: [u8; 16],
}

fn pick_kek_and_salt_for_first_run(path: &Path) -> anyhow::Result<KekAndSalt> {
    if let Some(passphrase) = env_passphrase() {
        let mut salt = [0_u8; 16];
        OsRng.fill_bytes(&mut salt);
        let kek = derive_passphrase_kek_new_salt(&passphrase, &salt)?;
        return Ok(KekAndSalt { kek, salt });
    }
    let kek = ensure_master_key(&master_key_path(path))?;
    Ok(KekAndSalt {
        kek,
        salt: [0_u8; 16],
    })
}

fn tighten_files(path: &Path) -> anyhow::Result<()> {
    tighten_existing_permissions(path)?;
    let mk = master_key_path(path);
    if mk.exists() {
        let _ = tighten_existing_permissions(&mk);
    }
    Ok(())
}

#[cfg(unix)]
fn write_identity_with_mode(path: &Path, identity: &DeviceIdentity) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let bytes = serde_json::to_string_pretty(identity)?;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true).mode(0o600);
    let mut file = opts
        .open(path)
        .with_context(|| format!("failed to write identity file {}", path.display()))?;
    std::io::Write::write_all(&mut file, bytes.as_bytes())
        .with_context(|| format!("failed to write identity file {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_identity_with_mode(path: &Path, identity: &DeviceIdentity) -> anyhow::Result<()> {
    static ACL_WARNING: std::sync::Once = std::sync::Once::new();
    ACL_WARNING.call_once(|| {
        tracing::warn!(
            path = %path.display(),
            "storing encrypted identity on a non-unix platform; POSIX 0o600 is not enforced — restrict via your OS-specific mechanism"
        );
    });
    fs::write(path, serde_json::to_string_pretty(identity)?)
        .with_context(|| format!("failed to write identity file {}", path.display()))
}

fn encrypt_envelope(
    kek: &[u8; 32],
    salt: &[u8; 16],
    private_key: &[u8; SECRET_KEY_LENGTH],
    public_key: &[u8; SECRET_KEY_LENGTH],
) -> anyhow::Result<KeyEnvelope> {
    let mut nonce_bytes = [0_u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);

    let cipher = ChaCha20Poly1305::new(Key::from_slice(kek));
    let nonce = Nonce::from_slice(&nonce_bytes);

    let pk_hash: [u8; 32] = {
        let mut h = Sha256::new();
        h.update(public_key);
        let bytes = h.finalize();
        let mut out = [0_u8; 32];
        out.copy_from_slice(&bytes);
        out
    };
    let aad = build_aad(public_key);

    let payload = Payload {
        msg: private_key,
        aad: aad.as_bytes(),
    };
    let ciphertext = cipher
        .encrypt(nonce, payload)
        .map_err(|e| anyhow!("chacha20poly1305 encryption failed: {e}"))?;

    Ok(KeyEnvelope {
        tag: AT_REST_AEAD_TAG.to_string(),
        salt_hex: hex::encode(salt),
        nonce_hex: hex::encode(nonce_bytes),
        ciphertext_hex: hex::encode(ciphertext),
        aad_pk_sha256_hex: hex::encode(pk_hash),
    })
}

fn decrypt_envelope(
    kek: &[u8; 32],
    envelope: &KeyEnvelope,
    public_key: &[u8; SECRET_KEY_LENGTH],
) -> anyhow::Result<[u8; SECRET_KEY_LENGTH]> {
    if envelope.tag != AT_REST_AEAD_TAG {
        anyhow::bail!(
            "unknown identity envelope tag {:?} (this build expects {})",
            envelope.tag,
            AT_REST_AEAD_TAG
        );
    }
    let nonce_bytes =
        hex::decode(&envelope.nonce_hex).map_err(|e| anyhow!("invalid nonce hex: {e}"))?;
    if nonce_bytes.len() != 12 {
        anyhow::bail!("envelope nonce must be 12 bytes, got {}", nonce_bytes.len());
    }
    let ciphertext = hex::decode(&envelope.ciphertext_hex)
        .map_err(|e| anyhow!("invalid ciphertext hex: {e}"))?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(kek));
    // Rebuild the AAD from the public key so the Poly1305 tag
    // authenticates "this ciphertext is bound to this public key".
    let aad = build_aad(public_key);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &ciphertext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|e| anyhow!("chacha20poly1305 decryption failed (wrong KEK?): {e}"))?;
    if plaintext.len() != SECRET_KEY_LENGTH {
        anyhow::bail!(
            "decrypted private key has wrong length: {} (expected {})",
            plaintext.len(),
            SECRET_KEY_LENGTH
        );
    }
    let mut out = [0_u8; SECRET_KEY_LENGTH];
    out.copy_from_slice(&plaintext);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, Verifier};

    fn unique_dir(suffix: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("qubox-identity-{}-{}", suffix, Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_master_key_env(f: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = env::var(IDENTITY_PASSPHRASE_ENV).ok();
        unsafe {
            env::remove_var(IDENTITY_PASSPHRASE_ENV);
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f();
        }));
        match prev {
            Some(v) => unsafe {
                env::set_var(IDENTITY_PASSPHRASE_ENV, v);
            },
            None => unsafe {
                env::remove_var(IDENTITY_PASSPHRASE_ENV);
            },
        }
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    fn identity_survives_reload_with_master_key() {
        with_master_key_env(|| {
            let dir = unique_dir("reload");
            let path = dir.join("identity.json");

            let (first, _) =
                load_or_create_identity(Some(path.clone()), Some("test-device".to_string()))
                    .unwrap();
            let (second, _) =
                load_or_create_identity(Some(path.clone()), Some("test-device".to_string()))
                    .unwrap();

            assert_eq!(first.device_id, second.device_id);
            assert_eq!(first.host_peer_id, second.host_peer_id);
            assert_eq!(first.client_peer_id, second.client_peer_id);
            assert_eq!(first.public_key, second.public_key);

            assert_eq!(first.encrypted_private_key, second.encrypted_private_key);

            let unlocked = second.unlock(Some(&path)).unwrap();
            let derived = SigningKey::from_bytes(&unlocked.private_key)
                .verifying_key()
                .to_bytes();
            assert_eq!(derived, second.public_key);

            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn new_identity_has_v3_schema() {
        let (identity, _priv) = DeviceIdentity::new("dev".into());
        assert_eq!(identity.schema_version, IDENTITY_SCHEMA_VERSION);
        // Newly-constructed identity has an empty placeholder until
        // `seal_with_kek` populates it (first-run path).
        assert!(identity.encrypted_private_key.ciphertext_hex.is_empty());
    }

    #[test]
    fn identity_signing_round_trips() {
        with_master_key_env(|| {
            let dir = unique_dir("sign");
            let path = dir.join("identity.json");

            let (identity, _) =
                load_or_create_identity(Some(path.clone()), Some("dev".to_string())).unwrap();
            let signing = identity.signing_key(Some(&path)).unwrap();
            let verifying = identity.verifying_key();
            let message = b"hello, world";
            let signature = signing.sign(message);
            assert!(verifying.verify(message, &signature).is_ok());

            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn v1_identity_is_migrated_with_fresh_keypair() {
        with_master_key_env(|| {
            let dir = unique_dir("v1");
            let path = dir.join("identity.json");

            let device_id = Uuid::new_v4();
            let host_peer_id = Uuid::new_v4();
            let client_peer_id = Uuid::new_v4();
            let v1_json = serde_json::json!({
                "schema_version": 1_u32,
                "device_id": device_id,
                "host_peer_id": host_peer_id,
                "client_peer_id": client_peer_id,
                "display_name": "migrated",
            });
            fs::write(&path, serde_json::to_string_pretty(&v1_json).unwrap()).unwrap();

            let (identity, _) =
                load_or_create_identity(Some(path.clone()), Some("migrated".into())).unwrap();
            assert_eq!(identity.schema_version, IDENTITY_SCHEMA_VERSION);
            assert_eq!(identity.display_name, "migrated");
            assert_eq!(identity.device_id, device_id);
            assert_eq!(identity.host_peer_id, host_peer_id);
            assert_eq!(identity.client_peer_id, client_peer_id);

            let (reloaded, _) =
                load_or_create_identity(Some(path.clone()), Some("migrated".into())).unwrap();
            assert_eq!(reloaded.public_key, identity.public_key);
            assert_eq!(reloaded.device_id, device_id);

            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn v2_identity_is_migrated_to_encrypted_envelope() {
        with_master_key_env(|| {
            let dir = unique_dir("v2");
            let path = dir.join("identity.json");

            let signing_key = SigningKey::generate(&mut OsRng);
            let public_key = signing_key.verifying_key().to_bytes();
            let private_key = signing_key.to_bytes();
            let device_id = Uuid::new_v4();
            let host_peer_id = Uuid::new_v4();
            let client_peer_id = Uuid::new_v4();
            let v2_json = serde_json::json!({
                "schema_version": 2_u32,
                "device_id": device_id,
                "host_peer_id": host_peer_id,
                "client_peer_id": client_peer_id,
                "display_name": "v2",
                "private_key": private_key.to_vec(),
                "public_key": public_key.to_vec(),
            });
            fs::write(&path, serde_json::to_string_pretty(&v2_json).unwrap()).unwrap();

            let (identity, _) =
                load_or_create_identity(Some(path.clone()), Some("v2".into())).unwrap();
            assert_eq!(identity.schema_version, IDENTITY_SCHEMA_VERSION);
            assert_eq!(identity.public_key, public_key);
            assert!(
                identity.encrypted_private_key.tag == AT_REST_AEAD_TAG,
                "v2→v3 migration must produce a sealed envelope"
            );

            let (reloaded, _) =
                load_or_create_identity(Some(path.clone()), Some("v2".into())).unwrap();
            let unlocked = reloaded.unlock(Some(&path)).unwrap();
            assert_eq!(unlocked.private_key, private_key);

            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn wrong_master_key_fails_to_unseal() {
        with_master_key_env(|| {
            let dir = unique_dir("wrong");
            let path = dir.join("identity.json");

            let (identity, _) =
                load_or_create_identity(Some(path.clone()), Some("dev".to_string())).unwrap();

            let mk_path = master_key_path(&path);
            let garbage = vec![0xABu8; 32];
            fs::write(&mk_path, &garbage).unwrap();

            let result = identity.unlock(Some(&path));
            assert!(result.is_err(), "unsealing with wrong master key must fail");

            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn env_passphrase_seals_and_unseals() {
        // `std::env::set_var` is process-global and racy with other
        // tests running in parallel; serialize via the shared
        // `ENV_LOCK` so a master-key test cannot observe the
        // passphrase env var this test sets, and vice versa.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = env::var(IDENTITY_PASSPHRASE_ENV).ok();

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let dir = unique_dir("pass");
            let path = dir.join("identity.json");

            unsafe {
                env::set_var(IDENTITY_PASSPHRASE_ENV, "correct horse battery staple");
            }
            let (first, _) = load_or_create_identity(Some(path.clone()), Some("p".into())).unwrap();
            let first_pk = first.public_key;

            let (second, _) =
                load_or_create_identity(Some(path.clone()), Some("p".into())).unwrap();
            assert_eq!(second.public_key, first_pk);

            // Wrong passphrase must fail.
            unsafe {
                env::set_var(IDENTITY_PASSPHRASE_ENV, "wrong");
            }
            let result = second.unlock(Some(&path));
            assert!(result.is_err(), "wrong passphrase must fail to unseal");

            let _ = fs::remove_dir_all(&dir);
        }));

        // Always restore the env var to whatever it was on entry.
        match prev {
            Some(v) => unsafe {
                env::set_var(IDENTITY_PASSPHRASE_ENV, v);
            },
            None => unsafe {
                env::remove_var(IDENTITY_PASSPHRASE_ENV);
            },
        }

        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    #[cfg(unix)]
    #[test]
    fn identity_file_has_0o600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        with_master_key_env(|| {
            let dir = unique_dir("perms");
            let path = dir.join("identity.json");
            load_or_create_identity(Some(path.clone()), Some("device".into())).unwrap();

            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);

            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[cfg(unix)]
    #[test]
    fn identity_master_key_has_0o600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        with_master_key_env(|| {
            let dir = unique_dir("mk-perms");
            let path = dir.join("identity.json");
            load_or_create_identity(Some(path.clone()), Some("device".into())).unwrap();

            let mk_path = master_key_path(&path);
            let mode = fs::metadata(&mk_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);

            let _ = fs::remove_dir_all(&dir);
        });
    }
}
