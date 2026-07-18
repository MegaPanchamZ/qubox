//! JWKS client + cache with fail-closed semantics.
//!
//! Per `docs/browser-viewer-identity-and-host-trust.md` §"Cryptographic
//! root of trust & clock drift" and §"JWKS caching vs instant
//! revocation":
//!
//! - Cache TTL: short (default 5 minutes).
//! - Max stale: hard ceiling (default 1 hour) — past this, treat the
//!   key as untrusted if JWKS is unreachable.
//! - Unknown `kid`: refetch immediately, then consult the cache.
//! - Past max-stale AND unreachable: **fail-closed** for new sessions
//!   on the managed online path.
//!
//! Self-host can opt into a relaxed policy via [`JwksPolicy::strict`]
//! = `false` (used by tests and OSS LAN deployments).
//!
//! The HTTP fetch is pluggable so we don't tie the signaling crate to
//! a specific HTTP stack. The default uses [`reqwest`] (already a
//! workspace dep via `qubox-transport`); callers that need a different
//! transport can construct [`JwksClient::with_fetcher`].

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use qubox_proto::SignedBundle;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// Default cache TTL — most production setups use 5 min.
pub const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);
/// Default hard max-stale — past this + JWKS unreachable = fail-closed.
pub const DEFAULT_MAX_STALE: Duration = Duration::from_secs(60 * 60);
/// HTTP timeout for a single JWKS fetch.
pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Knobs for [`JwksClient`].
#[derive(Debug, Clone)]
pub struct JwksPolicy {
    /// How long a cached JWKS document is considered fresh.
    pub ttl: Duration,
    /// Hard ceiling: even with a cache hit, if it is older than this
    /// AND we cannot reach the JWKS endpoint, fail-closed for new
    /// signatures.
    pub max_stale: Duration,
    /// Whether to fail-closed when JWKS is unreachable. Set `false`
    /// for self-host / LAN / tests that don't need this protection.
    pub strict: bool,
}

impl Default for JwksPolicy {
    fn default() -> Self {
        Self {
            ttl: DEFAULT_TTL,
            max_stale: DEFAULT_MAX_STALE,
            strict: true,
        }
    }
}

/// One signing key as it appears in a JWKS document.
#[derive(Debug, Clone)]
pub struct JwkEntry {
    pub kid: String,
    /// 32-byte Ed25519 public key.
    pub public_key: [u8; 32],
}

/// In-memory cache snapshot — what was last successfully fetched.
#[derive(Debug, Clone)]
struct CacheSnapshot {
    fetched_at: Instant,
    keys: HashMap<String, [u8; 32]>,
}

/// Type alias for our async fetcher. Implementations return the raw
/// HTTP body bytes; the parser handles JSON.
pub type JwksFetchFuture =
    Pin<Box<dyn Future<Output = anyhow::Result<Vec<u8>>> + Send>>;

/// How to actually fetch a JWKS document over the network. The
/// default implementation uses [`reqwest`] (see [`HttpJwksFetcher`]).
/// Tests substitute a fake by constructing an
/// `Arc<FakeFetcher>` directly.
pub trait JwksFetcher: Send + Sync {
    fn fetch(&self, url: &str) -> JwksFetchFuture;
}

/// Default fetcher backed by [`reqwest`].
#[derive(Clone)]
pub struct HttpJwksFetcher {
    client: reqwest::Client,
}

impl HttpJwksFetcher {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_HTTP_TIMEOUT)
            .build()
            .context("build reqwest client for JWKS fetch")?;
        Ok(Self { client })
    }
}

impl JwksFetcher for HttpJwksFetcher {
    fn fetch(&self, url: &str) -> JwksFetchFuture {
        let client = self.client.clone();
        let url = url.to_string();
        Box::pin(async move {
            let resp = client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("JWKS HTTP GET {url}"))?;
            if !resp.status().is_success() {
                bail!("JWKS endpoint returned HTTP {}", resp.status());
            }
            let bytes = resp
                .bytes()
                .await
                .with_context(|| format!("JWKS body read from {url}"))?;
            Ok(bytes.to_vec())
        })
    }
}

/// JWKS client with TTL + max-stale policy. Cheap to clone (inner
/// state is wrapped in `Arc<Mutex<...>>`).
#[derive(Clone)]
pub struct JwksClient {
    url: String,
    policy: JwksPolicy,
    cache: Arc<Mutex<Option<CacheSnapshot>>>,
    fetcher: Arc<dyn JwksFetcher>,
}

impl JwksClient {
    pub fn new(url: impl Into<String>, policy: JwksPolicy) -> anyhow::Result<Self> {
        Ok(Self {
            url: url.into(),
            policy,
            cache: Arc::new(Mutex::new(None)),
            fetcher: Arc::new(HttpJwksFetcher::new()?),
        })
    }

    /// Substitute the fetcher (used by tests).
    pub fn with_fetcher(mut self, fetcher: Arc<dyn JwksFetcher>) -> Self {
        self.fetcher = fetcher;
        self
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn policy(&self) -> &JwksPolicy {
        &self.policy
    }

    /// Look up a key by `kid`. Triggers a refetch when the kid is
    /// unknown to the cache (per spec: "On signature verify: if key id
    /// unknown → fetch JWKS immediately"). Returns
    /// [`JwksError::UnknownKid`] if even after the refetch the kid is
    /// still absent.
    pub async fn lookup(&self, kid: &str) -> Result<[u8; 32], JwksError> {
        // 1) Fast path: cache hit within TTL.
        {
            let cache = self.cache.lock().await;
            if let Some(snapshot) = cache.as_ref() {
                if snapshot.fetched_at.elapsed() < self.policy.ttl {
                    if let Some(pk) = snapshot.keys.get(kid) {
                        return Ok(*pk);
                    }
                }
            }
        }

        // 2) Force a refetch (unknown kid or stale cache).
        match self.fetch_now().await {
            Ok(()) => {}
            Err(err) => {
                // If the cache has any snapshot within max-stale, fall
                // back to it (still subject to "fail-closed" later).
                let cache = self.cache.lock().await;
                let Some(snapshot) = cache.as_ref() else {
                    return Err(JwksError::Unreachable(err));
                };
                if snapshot.fetched_at.elapsed() <= self.policy.max_stale {
                    if let Some(pk) = snapshot.keys.get(kid) {
                        warn!(
                            ?err,
                            kid,
                            "JWKS refetch failed; serving from cache (within max-stale)"
                        );
                        return Ok(*pk);
                    }
                    return Err(JwksError::UnknownKid {
                        kid: kid.to_string(),
                    });
                }
                if self.policy.strict {
                    return Err(JwksError::UnreachableAndStale(err));
                }
                return Err(JwksError::Unreachable(err));
            }
        }

        let cache = self.cache.lock().await;
        let snapshot = cache
            .as_ref()
            .ok_or_else(|| JwksError::UnknownKid { kid: kid.to_string() })?;
        snapshot
            .keys
            .get(kid)
            .copied()
            .ok_or_else(|| JwksError::UnknownKid {
                kid: kid.to_string(),
            })
    }

    /// Verify the Ed25519 signature inside a [`SignedBundle`].
    /// Convenience wrapper over [`Self::lookup`] + the bundle's own
    /// signature check.
    pub async fn verify_bundle(
        &self,
        envelope: &SignedBundle,
    ) -> Result<ed25519_dalek::VerifyingKey, JwksError> {
        let pk_bytes = self.lookup(&envelope.kid).await?;
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&pk_bytes)
            .map_err(|e| JwksError::BadKey(e.to_string()))?;
        envelope
            .verify_signature(&vk)
            .map_err(|e| JwksError::SignatureMismatch(e.to_string()))?;
        Ok(vk)
    }

    async fn fetch_now(&self) -> anyhow::Result<()> {
        debug!(url = %self.url, "refreshing JWKS");
        let bytes = self.fetcher.fetch(&self.url).await?;
        let parsed = parse_jwks(&bytes)
            .with_context(|| format!("parse JWKS document from {}", self.url))?;
        let mut keys = HashMap::new();
        for entry in parsed {
            keys.insert(entry.kid, entry.public_key);
        }
        let mut cache = self.cache.lock().await;
        *cache = Some(CacheSnapshot {
            fetched_at: Instant::now(),
            keys,
        });
        Ok(())
    }
}

/// Errors returned by [`JwksClient`].
#[derive(Debug)]
pub enum JwksError {
    /// The requested `kid` is not in the JWKS document (even after a
    /// refetch).
    UnknownKid { kid: String },
    /// The JWKS endpoint is unreachable AND no cache is available.
    Unreachable(anyhow::Error),
    /// The JWKS endpoint is unreachable AND the cache is past
    /// `max_stale`. Per the managed-online fail-closed rule, callers
    /// MUST reject new sessions in this state.
    UnreachableAndStale(anyhow::Error),
    /// The JWK entry's public-key bytes are not a valid Ed25519 key.
    BadKey(String),
    /// The bundle's signature did not verify against the resolved key.
    SignatureMismatch(String),
}

impl std::fmt::Display for JwksError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JwksError::UnknownKid { kid } => write!(f, "JWKS does not contain kid {kid}"),
            JwksError::Unreachable(e) => write!(f, "JWKS endpoint unreachable: {e}"),
            JwksError::UnreachableAndStale(e) => write!(
                f,
                "JWKS endpoint unreachable and cache past max-stale (fail-closed): {e}"
            ),
            JwksError::BadKey(e) => write!(f, "JWK public key invalid: {e}"),
            JwksError::SignatureMismatch(e) => write!(f, "signature mismatch: {e}"),
        }
    }
}

impl std::error::Error for JwksError {}

/// Minimal JWKS parser. Accepts an RFC 7517-style object
/// (`{"keys": [...]}`). Filters for `kty == "OKP"` + `crv == "Ed25519"`
/// entries with a 32-byte `x` field. Accepts either URL-safe or
/// standard base64 for `x` (some issuers ignore RFC 8037).
pub fn parse_jwks(bytes: &[u8]) -> anyhow::Result<Vec<JwkEntry>> {
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use base64::Engine as _;

    #[derive(serde::Deserialize)]
    struct Doc {
        #[serde(default)]
        keys: Vec<Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        #[serde(default)]
        kid: Option<String>,
        #[serde(default)]
        kty: Option<String>,
        #[serde(default)]
        crv: Option<String>,
        #[serde(default)]
        x: Option<String>,
    }

    let doc: Doc = serde_json::from_slice(bytes).context("JWKS JSON parse")?;

    let mut out = Vec::new();
    for (idx, entry) in doc.keys.iter().enumerate() {
        if entry.kty.as_deref() != Some("OKP") {
            continue;
        }
        if entry.crv.as_deref() != Some("Ed25519") {
            continue;
        }
        let Some(x_b64) = &entry.x else {
            continue;
        };
        let pk_bytes = URL_SAFE_NO_PAD
            .decode(x_b64.as_bytes())
            .or_else(|_| STANDARD.decode(x_b64.as_bytes()))
            .with_context(|| format!("decode JWK `x` for entry {idx}"))?;
        let pk_array: [u8; 32] = pk_bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("JWK `x` is not 32 bytes"))?;
        let kid = entry.kid.clone().unwrap_or_else(|| format!("entry-{idx}"));
        out.push(JwkEntry {
            kid,
            public_key: pk_array,
        });
    }

    if out.is_empty() {
        if doc.keys.is_empty() {
            bail!("JWKS document contains no OKP/Ed25519 entries");
        } else {
            bail!("JWKS document contained keys but none matched kty=OKP/crv=Ed25519");
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use qubox_proto::{generate_signing_key, SignedBundle, SessionCaps, ViewerToHost};

    fn sample_key_b64() -> String {
        let sk = generate_signing_key();
        URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes())
    }

    fn jwks_with(kid: &str, pk_b64: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "keys": [
                {
                    "kid": kid,
                    "kty": "OKP",
                    "crv": "Ed25519",
                    "x": pk_b64,
                }
            ]
        }))
        .unwrap()
    }

    /// Test fetcher that hands back canned bytes and (optionally)
    /// fails the next call to `fetch`.
    struct FakeFetcher {
        bytes: std::sync::Mutex<Option<Vec<u8>>>,
        fail_next: std::sync::atomic::AtomicBool,
    }

    impl FakeFetcher {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                bytes: std::sync::Mutex::new(Some(bytes)),
                fail_next: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    impl JwksFetcher for FakeFetcher {
        fn fetch(&self, _url: &str) -> JwksFetchFuture {
            let fail = self.fail_next.swap(false, std::sync::atomic::Ordering::SeqCst);
            let bytes = self
                .bytes
                .lock()
                .ok()
                .and_then(|guard| guard.clone());
            Box::pin(async move {
                if fail {
                    bail!("scripted failure");
                }
                bytes.ok_or_else(|| anyhow!("no canned bytes"))
            })
        }
    }

    fn fast_policy() -> JwksPolicy {
        JwksPolicy {
            ttl: Duration::from_millis(100),
            max_stale: Duration::from_millis(500),
            strict: true,
        }
    }

    #[test]
    fn parse_jwks_extracts_okp_ed25519() {
        let pk = sample_key_b64();
        let doc = jwks_with("kid-1", &pk);
        let entries = parse_jwks(&doc).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kid, "kid-1");
    }

    #[test]
    fn parse_jwks_accepts_stream_a_wire_format() {
        let pk = sample_key_b64();
        let doc = serde_json::to_vec(&serde_json::json!({
            "keys": [{
                "kty": "OKP",
                "crv": "Ed25519",
                "kid": "qb1_12345678",
                "x": pk,
                "use": "sig",
                "alg": "EdDSA"
            }]
        }))
        .unwrap();
        let entries = parse_jwks(&doc).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kid, "qb1_12345678");
    }

    #[test]
    fn parse_jwks_ignores_rsa_entries() {
        let pk = sample_key_b64();
        let doc = serde_json::to_vec(&serde_json::json!({
            "keys": [
                {"kid": "rsa-1", "kty": "RSA", "n": "abc"},
                {"kid": "okp-1", "kty": "OKP", "crv": "Ed25519", "x": pk},
            ]
        }))
        .unwrap();
        let entries = parse_jwks(&doc).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kid, "okp-1");
    }

    #[test]
    fn parse_jwks_rejects_bad_base64() {
        let doc = serde_json::to_vec(&serde_json::json!({
            "keys": [
                {"kid": "okp-1", "kty": "OKP", "crv": "Ed25519", "x": "!!!"},
            ]
        }))
        .unwrap();
        assert!(parse_jwks(&doc).is_err());
    }

    #[test]
    fn parse_jwks_rejects_wrong_length_key() {
        let doc = serde_json::to_vec(&serde_json::json!({
            "keys": [
                {"kid": "okp-1", "kty": "OKP", "crv": "Ed25519", "x": "AAA"},
            ]
        }))
        .unwrap();
        assert!(parse_jwks(&doc).is_err());
    }

    #[tokio::test]
    async fn lookup_returns_key_from_initial_fetch() {
        let pk_b64 = sample_key_b64();
        let doc = jwks_with("kid-1", &pk_b64);
        let fetcher: Arc<dyn JwksFetcher> = Arc::new(FakeFetcher::new(doc));
        let client = JwksClient::new("https://test/jwks", fast_policy())
            .unwrap()
            .with_fetcher(fetcher);
        let pk = client.lookup("kid-1").await.unwrap();
        assert_eq!(pk.len(), 32);
    }

    #[tokio::test]
    async fn unknown_kid_after_fetch_is_error() {
        let pk_b64 = sample_key_b64();
        let doc = jwks_with("kid-1", &pk_b64);
        let fetcher: Arc<dyn JwksFetcher> = Arc::new(FakeFetcher::new(doc));
        let client = JwksClient::new("https://test/jwks", fast_policy())
            .unwrap()
            .with_fetcher(fetcher);
        let err = client.lookup("kid-missing").await.unwrap_err();
        assert!(matches!(err, JwksError::UnknownKid { .. }));
    }

    #[tokio::test]
    async fn fail_closed_when_unreachable_and_stale_in_strict_mode() {
        let pk_b64 = sample_key_b64();
        let doc = jwks_with("kid-1", &pk_b64);
        let fake = Arc::new(FakeFetcher::new(doc));
        let fetcher: Arc<dyn JwksFetcher> = fake.clone();
        let client = JwksClient::new("https://test/jwks", fast_policy())
            .unwrap()
            .with_fetcher(fetcher);
        client.lookup("kid-1").await.unwrap();
        fake.fail_next.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let err = client.lookup("kid-1").await.unwrap_err();
        assert!(matches!(err, JwksError::UnreachableAndStale(_)));
    }

    #[tokio::test]
    async fn relaxed_policy_returns_unreachable_when_stale() {
        let pk_b64 = sample_key_b64();
        let doc = jwks_with("kid-1", &pk_b64);
        let fake = Arc::new(FakeFetcher::new(doc));
        let fetcher: Arc<dyn JwksFetcher> = fake.clone();
        let mut policy = fast_policy();
        policy.strict = false;
        let client = JwksClient::new("https://test/jwks", policy)
            .unwrap()
            .with_fetcher(fetcher);
        client.lookup("kid-1").await.unwrap();
        fake.fail_next.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let err = client.lookup("kid-1").await.unwrap_err();
        assert!(matches!(err, JwksError::Unreachable(_)));
    }

    #[tokio::test]
    async fn within_max_stale_serves_from_cache_when_refetch_fails() {
        let pk_b64 = sample_key_b64();
        let doc = jwks_with("kid-1", &pk_b64);
        let fake = Arc::new(FakeFetcher::new(doc));
        let fetcher: Arc<dyn JwksFetcher> = fake.clone();
        let mut policy = fast_policy();
        policy.ttl = Duration::from_millis(50);
        policy.max_stale = Duration::from_secs(60);
        let client = JwksClient::new("https://test/jwks", policy)
            .unwrap()
            .with_fetcher(fetcher);
        client.lookup("kid-1").await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        fake.fail_next.store(true, std::sync::atomic::Ordering::SeqCst);
        let pk = client.lookup("kid-1").await.unwrap();
        assert_eq!(pk.len(), 32);
    }

    #[tokio::test]
    async fn verify_bundle_succeeds_with_correct_kid_and_sig() {
        let sk = generate_signing_key();
        let pk_bytes = sk.verifying_key().to_bytes();
        let pk_b64 = URL_SAFE_NO_PAD.encode(pk_bytes);
        let doc = jwks_with("kid-1", &pk_b64);
        let fetcher: Arc<dyn JwksFetcher> = Arc::new(FakeFetcher::new(doc));
        let client = JwksClient::new("https://test/jwks", fast_policy())
            .unwrap()
            .with_fetcher(fetcher);
        let payload = ViewerToHost {
            v: 1,
            jti: "abc".into(),
            sid: "abc".into(),
            sub: "sub".into(),
            aud: "aud".into(),
            iat: 1_000,
            exp: 2_000,
            caps: SessionCaps::default(),
            viewer_dtls_fp: "AA".into(),
        };
        let env = SignedBundle::new(&payload, "kid-1", &sk).unwrap();
        client.verify_bundle(&env).await.unwrap();
    }
}