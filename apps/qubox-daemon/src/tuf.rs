//! # TUF (The Update Framework) client wrapper
//!
//! Provides [`UpdateChecker`] — a focused API for checking, downloading, and
//! applying binary updates.  Internally uses the `tough` crate for TUF metadata
//! verification (root → timestamp → snapshot → targets chain).
//!
//! ## Throttling
//!
//! [`check_for_update`](UpdateChecker::check_for_update) uses a 60-second
//! throttle: repeated calls within the window return the cached result without
//! hitting the network.
//!
//! ## Rollback
//!
//! Before applying a new binary we back up the current one (`*.prev`).  If the
//! daemon crashes within 60 seconds of the update a restart handler restores
//! the backup.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use semver::Version;
use sha2::{Digest, Sha256};
use url::Url;

use crate::state::{StateDb, UpdateRecord};
use crate::DaemonError;

// ── Public types ─────────────────────────────────────────────────────────

/// Information about an available update.
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub available: bool,
    pub size_bytes: u64,
    pub manifest_url: String,
    /// SHA-256 hex digest from the TUF metadata (internal).
    pub sha256: String,
}

/// Current update status, returned by [`get_status`](UpdateChecker::get_status).
#[derive(Debug, Clone)]
pub struct UpdateStatus {
    pub last_check: Option<Instant>,
    pub current_version: String,
    pub available_update: Option<UpdateInfo>,
}

/// Errors that can occur during the TUF update flow.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("HTTP fetch failed for {url}: {source}")]
    Fetch {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("TUF verification failed: {reason}")]
    Verify { reason: String },
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("SHA-256 hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("no update available")]
    NoUpdate,
    #[error("already on the latest version")]
    AlreadyOnLatest,
    #[error("expired metadata for role {role}: expired at {expired_at}")]
    ExpiredMetadata { role: String, expired_at: String },
    #[error("internal: {0}")]
    Internal(String),
}

impl From<DaemonError> for UpdateError {
    fn from(e: DaemonError) -> Self {
        UpdateError::Internal(e.to_string())
    }
}

// ── Constants ────────────────────────────────────────────────────────────

const THROTTLE_SECS: u64 = 60;
const RESTART_SENTINEL: &str = "restart-requested";
const ROLLBACK_GRACE_SECS: u64 = 60;

// ── UpdateChecker ───────────────────────────────────────────────────────

/// High-level TUF update client.
pub struct UpdateChecker {
    repo_url: String,
    state: Arc<StateDb>,
    http: reqwest::Client,
    last_check: tokio::sync::Mutex<Option<Instant>>,
    cached_update: tokio::sync::Mutex<Option<UpdateInfo>>,
    current_version: String,
    /// Set to `true` when an update has been staged and applied.
    pub update_pending: AtomicBool,
    /// Target name prefix: `qubox-daemon-{arch}-{os}-`.
    target_prefix: String,
}

impl UpdateChecker {
    /// Create a new `UpdateChecker`.
    ///
    /// If `state` contains a stored `root.json` it is used as the trust
    /// anchor.  Otherwise the root is fetched on the first call to
    /// [`check_for_update`](UpdateChecker::check_for_update).
    pub fn new(
        repo_url: String,
        state: Arc<StateDb>,
        current_version: String,
    ) -> Result<Self, UpdateError> {
        let target_prefix = format!(
            "qubox-daemon-{}-{}-",
            std::env::consts::ARCH,
            std::env::consts::OS
        );
        Ok(Self {
            repo_url,
            state,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(120))
                .build()
                .map_err(|e| UpdateError::Internal(format!("build reqwest client: {e}")))?,
            last_check: tokio::sync::Mutex::new(None),
            cached_update: tokio::sync::Mutex::new(None),
            current_version,
            update_pending: AtomicBool::new(false),
            target_prefix,
        })
    }

    // ── Public API ──────────────────────────────────────────────────────

    /// Check for an available update.
    ///
    /// Throttled to once per 60 seconds.  Returns the cached result if the
    /// window is still active.
    pub async fn check_for_update(&self) -> Result<UpdateInfo, UpdateError> {
        // Throttle check
        {
            let mut last = self.last_check.lock().await;
            if let Some(ts) = *last {
                if ts.elapsed() < Duration::from_secs(THROTTLE_SECS) {
                    let cached = self.cached_update.lock().await;
                    if let Some(info) = cached.clone() {
                        return Ok(info);
                    }
                }
            }
            *last = Some(Instant::now());
        }

        // Ensure root metadata is available
        self.ensure_root().await?;

        let root_bytes = self
            .state
            .get_tuf_root()
            .map_err(|e| UpdateError::Internal(e.to_string()))?
            .ok_or_else(|| UpdateError::Internal("root metadata missing after ensure".into()))?;

        let metadata_base_url = Url::parse(&format!("{}/metadata/", self.repo_url))
            .map_err(|e| UpdateError::Internal(format!("invalid metadata URL: {e}")))?;
        let targets_base_url = Url::parse(&format!("{}/targets/", self.repo_url))
            .map_err(|e| UpdateError::Internal(format!("invalid targets URL: {e}")))?;

        // Load and verify via tough
        let repo = self
            .load_repo(&root_bytes, &metadata_base_url, &targets_base_url)
            .await?;

        // Persist fetched metadata to redb
        self.cache_repo_metadata(&repo).await?;

        // Walk targets and find latest version
        let result = self.find_update(&repo, &self.current_version)?;

        // Cache the result
        {
            let mut cached = self.cached_update.lock().await;
            *cached = Some(result.clone());
        }

        Ok(result)
    }

    /// Download a verified update binary to the staging directory.
    ///
    /// Returns the path to the staged binary.
    pub async fn download_update(&self, info: &UpdateInfo) -> Result<PathBuf, UpdateError> {
        let staged_dir = self.staging_dir(&info.version);
        let target_path = staged_dir.join("qubox");
        if target_path.exists() {
            return Ok(target_path);
        }
        std::fs::create_dir_all(&staged_dir).map_err(|e| UpdateError::Io {
            path: staged_dir.clone(),
            source: e,
        })?;

        let url = &info.manifest_url;
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| UpdateError::Fetch {
                url: url.clone(),
                source: e,
            })?;
        resp.error_for_status_ref()
            .map_err(|e| UpdateError::Fetch {
                url: url.clone(),
                source: e,
            })?;

        let bytes = resp.bytes().await.map_err(|e| UpdateError::Fetch {
            url: url.clone(),
            source: e,
        })?;

        // Verify SHA-256 hash
        let actual_hash = hex::encode(Sha256::digest(&bytes));
        if actual_hash != info.sha256 {
            return Err(UpdateError::HashMismatch {
                expected: info.sha256.clone(),
                actual: actual_hash,
            });
        }

        std::fs::write(&target_path, &bytes).map_err(|e| UpdateError::Io {
            path: target_path.clone(),
            source: e,
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target_path, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| UpdateError::Io {
                    path: target_path.clone(),
                    source: e,
                })?;
        }

        // Cleanup old staged versions
        self.cleanup_old_staged(&info.version).ok();

        Ok(target_path)
    }

    /// Apply a staged update.
    ///
    /// Backs up the current binary (`*.prev`), atomically renames the staged
    /// binary into place, writes a restart sentinel, and records the update
    /// in the state database.
    pub async fn apply_update(
        &self,
        staged: &Path,
        current_binary: &Path,
    ) -> Result<(), UpdateError> {
        if !staged.exists() {
            return Err(UpdateError::Io {
                path: staged.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "staged binary missing"),
            });
        }

        // Backup current binary → *.prev
        let backup = current_binary.with_extension(
            current_binary
                .extension()
                .map(|e| format!("{}.prev", e.to_string_lossy()))
                .unwrap_or_else(|| "prev".into()),
        );
        if current_binary.exists() {
            std::fs::rename(current_binary, &backup).map_err(|e| UpdateError::Io {
                path: backup.clone(),
                source: e,
            })?;
        }

        // ── Linux / macOS: two-step atomic rename ─────────────────────
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let new_path = current_binary.with_extension(
                current_binary
                    .extension()
                    .map(|e| format!("{}.new", e.to_string_lossy()))
                    .unwrap_or_else(|| "new".into()),
            );
            std::fs::rename(staged, &new_path).map_err(|e| UpdateError::Io {
                path: new_path.clone(),
                source: e,
            })?;
            std::fs::rename(&new_path, current_binary).map_err(|e| UpdateError::Io {
                path: current_binary.to_path_buf(),
                source: e,
            })?;
        }

        // ── Windows: stage as current.new (task 4 handles swap) ───────
        #[cfg(target_os = "windows")]
        {
            let new_path = current_binary.with_extension(
                current_binary
                    .extension()
                    .map(|e| format!("{}.new", e.to_string_lossy()))
                    .unwrap_or_else(|| "new".into()),
            );
            std::fs::rename(staged, &new_path).map_err(|e| UpdateError::Io {
                path: new_path.clone(),
                source: e,
            })?;
        }

        // Write restart sentinel
        if let Some(data_dir) = self.data_dir() {
            std::fs::create_dir_all(&data_dir).map_err(|e| UpdateError::Io {
                path: data_dir.clone(),
                source: e,
            })?;
            let sentinel = data_dir.join(RESTART_SENTINEL);
            std::fs::write(&sentinel, self.current_version.as_bytes()).map_err(|e| {
                UpdateError::Io {
                    path: sentinel,
                    source: e,
                }
            })?;
        }

        // Record the update
        let record = UpdateRecord {
            applied_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            binary_path: current_binary.to_string_lossy().to_string(),
            prev_version: Some(self.current_version.clone()),
        };
        self.state
            .record_update(&record)
            .map_err(|e| UpdateError::Internal(e.to_string()))?;

        self.update_pending.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Return the current update status.
    pub async fn get_status(&self) -> UpdateStatus {
        let last_check = *self.last_check.lock().await;
        let available = self.cached_update.lock().await.clone();
        UpdateStatus {
            last_check,
            current_version: self.current_version.clone(),
            available_update: available,
        }
    }

    // ── Rollback (static, called at startup) ──────────────────────────

    /// Check if a rollback is needed and perform it.
    ///
    /// Called during daemon startup.  If a `*.prev` backup exists and the
    /// restart sentinel is present, and the sentinel's mtime is within
    /// [`ROLLBACK_GRACE_SECS`] of now, the backup is restored.
    pub fn check_rollback(current_binary: &Path, data_dir: &Path) -> Result<bool, UpdateError> {
        let sentinel = data_dir.join(RESTART_SENTINEL);
        if !sentinel.exists() {
            return Ok(false);
        }

        let backup = current_binary.with_extension(
            current_binary
                .extension()
                .map(|e| format!("{}.prev", e.to_string_lossy()))
                .unwrap_or_else(|| "prev".into()),
        );
        if !backup.exists() {
            std::fs::remove_file(&sentinel).ok();
            return Ok(false);
        }

        let sentinel_mtime = std::fs::metadata(&sentinel)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let within_grace = match sentinel_mtime {
            Some(mtime) => now.saturating_sub(mtime) < ROLLBACK_GRACE_SECS,
            None => false,
        };

        if within_grace {
            std::fs::rename(&backup, current_binary).map_err(|e| UpdateError::Io {
                path: current_binary.to_path_buf(),
                source: e,
            })?;
            std::fs::remove_file(&sentinel).ok();
            return Ok(true);
        }

        std::fs::remove_file(&sentinel).ok();
        std::fs::remove_file(&backup).ok();
        Ok(false)
    }

    // ── Internal helpers ────────────────────────────────────────────────

    /// Fetch and store root.json if not already in redb.
    async fn ensure_root(&self) -> Result<(), UpdateError> {
        if self
            .state
            .get_tuf_root()
            .map_err(|e| UpdateError::Internal(e.to_string()))?
            .is_some()
        {
            return Ok(());
        }

        let root_url = format!("{}/metadata/root.json", self.repo_url);
        let resp = self
            .http
            .get(&root_url)
            .send()
            .await
            .map_err(|e| UpdateError::Fetch {
                url: root_url.clone(),
                source: e,
            })?;
        resp.error_for_status_ref()
            .map_err(|e| UpdateError::Fetch {
                url: root_url.clone(),
                source: e,
            })?;

        let bytes = resp.bytes().await.map_err(|e| UpdateError::Fetch {
            url: root_url,
            source: e,
        })?;

        self.state
            .put_tuf_root(&bytes)
            .map_err(|e| UpdateError::Internal(e.to_string()))?;
        Ok(())
    }

    /// Load a verified TUF repository via `tough::RepositoryLoader`.
    async fn load_repo(
        &self,
        root_bytes: &Vec<u8>,
        metadata_base_url: &Url,
        targets_base_url: &Url,
    ) -> Result<tough::Repository, UpdateError> {
        tough::RepositoryLoader::new(
            root_bytes,
            metadata_base_url.clone(),
            targets_base_url.clone(),
        )
        .load()
        .await
        .map_err(|e| UpdateError::Verify {
            reason: format!("tough load failed: {e}"),
        })
    }

    /// Persist fetched metadata from a `tough::Repository` to redb.
    async fn cache_repo_metadata(&self, repo: &tough::Repository) -> Result<(), UpdateError> {
        let root_json =
            serde_json::to_vec(repo.root()).map_err(|e| UpdateError::Internal(e.to_string()))?;
        let targets_json =
            serde_json::to_vec(repo.targets()).map_err(|e| UpdateError::Internal(e.to_string()))?;
        let snapshot_json = serde_json::to_vec(repo.snapshot())
            .map_err(|e| UpdateError::Internal(e.to_string()))?;
        let timestamp_json = serde_json::to_vec(repo.timestamp())
            .map_err(|e| UpdateError::Internal(e.to_string()))?;

        self.state
            .put_tuf_root(&root_json)
            .map_err(|e| UpdateError::Internal(e.to_string()))?;
        self.state
            .put_tuf_targets(&targets_json)
            .map_err(|e| UpdateError::Internal(e.to_string()))?;
        self.state
            .put_tuf_snapshot(&snapshot_json)
            .map_err(|e| UpdateError::Internal(e.to_string()))?;
        self.state
            .put_tuf_timestamp(&timestamp_json)
            .map_err(|e| UpdateError::Internal(e.to_string()))?;
        Ok(())
    }

    /// Walk targets and find the newest version newer than `current_version`.
    fn find_update(
        &self,
        repo: &tough::Repository,
        current_version: &str,
    ) -> Result<UpdateInfo, UpdateError> {
        let current = Version::parse(current_version)
            .map_err(|e| UpdateError::Internal(format!("invalid current version: {e}")))?;

        let targets = &repo.targets().signed.targets;
        let candidates: Vec<_> = targets
            .iter()
            .filter(|(name, _)| name.raw().starts_with(&self.target_prefix))
            .collect();

        if candidates.is_empty() {
            return Err(UpdateError::NoUpdate);
        }

        let mut best: Option<(Version, String, &tough::schema::Target)> = None;

        for (name, target) in &candidates {
            let version_str = name
                .raw()
                .strip_prefix(&self.target_prefix)
                .unwrap_or(name.raw());
            if let Ok(ver) = Version::parse(version_str) {
                if ver > current {
                    let is_newer = match &best {
                        None => true,
                        Some((best_ver, _, _)) => ver > *best_ver,
                    };
                    if is_newer {
                        best = Some((ver, version_str.to_string(), target));
                    }
                }
            }
        }

        match best {
            Some((_ver, version, target)) => {
                let manifest_url = format!(
                    "{}/targets/{}",
                    self.repo_url.trim_end_matches('/'),
                    self.target_prefix.clone() + &version
                );

                // Extract SHA-256 from custom metadata.  Fall back to the
                // `hashes.sha256` field which is a `Decoded<Hex>`.
                let sha256 = target
                    .custom
                    .get("sha256")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| hex::encode(target.hashes.sha256.as_ref()));

                Ok(UpdateInfo {
                    version,
                    available: true,
                    size_bytes: target.length,
                    manifest_url,
                    sha256,
                })
            }
            None => Err(UpdateError::AlreadyOnLatest),
        }
    }

    // ── Path helpers ────────────────────────────────────────────────────

    fn staging_dir(&self, version: &str) -> PathBuf {
        let proj_dirs = directories::ProjectDirs::from("com", "qubox", "qubox")
            .expect("platform data dirs available");
        proj_dirs.cache_dir().join("staged").join(version)
    }

    fn data_dir(&self) -> Option<PathBuf> {
        directories::ProjectDirs::from("com", "qubox", "qubox")
            .map(|d| d.data_local_dir().to_path_buf())
    }

    fn cleanup_old_staged(&self, keep_version: &str) -> Result<(), UpdateError> {
        let proj_dirs = directories::ProjectDirs::from("com", "qubox", "qubox")
            .expect("platform data dirs available");
        let staged_root = proj_dirs.cache_dir().join("staged");
        if !staged_root.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(&staged_root).map_err(|e| UpdateError::Io {
            path: staged_root.clone(),
            source: e,
        })? {
            let entry = entry.map_err(|e| UpdateError::Io {
                path: staged_root.clone(),
                source: e,
            })?;
            let dir_name = entry.file_name().to_string_lossy().to_string();
            if dir_name != keep_version {
                std::fs::remove_dir_all(entry.path()).ok();
            }
        }
        Ok(())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a self-signed TUF root.json via `tough`'s high-level API
    /// (which handles all the ed25519 signing, canonicalization, and
    /// `Decoded<Hex>` plumbing internally). Reserved for the integration test
    /// suite — the `tough` 0.17 `RepositoryEditor::sign` API is async and
    /// requires a real root.json on disk, which is too heavy for a unit test
    /// here.  See the TUF integration tests under `tests/` for the working
    /// version.
    #[allow(dead_code)]
    fn _make_test_repo_unused() {}

    #[tokio::test]
    async fn tuf_hash_mismatch_returns_error() {
        let content = b"original content";
        let mut corrupted = content.to_vec();
        if !corrupted.is_empty() {
            corrupted[0] ^= 0xFF;
        }
        let expected = hex::encode(Sha256::digest(content));
        let actual = hex::encode(Sha256::digest(&corrupted));
        assert_ne!(expected, actual);

        let err = UpdateError::HashMismatch { expected, actual };
        assert!(err.to_string().contains("hash mismatch"));
    }

    #[tokio::test]
    async fn tuf_throttles_repeat_checks() {
        let state = {
            let dir = tempfile::tempdir().unwrap();
            Arc::new(StateDb::open(&dir.path().join("state.db")).unwrap())
        };
        let checker =
            UpdateChecker::new("http://localhost:9999".into(), state, "0.1.0".into()).unwrap();

        // Seed cache
        {
            let mut cached = checker.cached_update.lock().await;
            *cached = Some(UpdateInfo {
                version: "0.2.0".into(),
                available: true,
                size_bytes: 100,
                manifest_url: "http://example.com/b".into(),
                sha256: "abc".into(),
            });
        }
        {
            let mut last = checker.last_check.lock().await;
            *last = Some(Instant::now());
        }

        let result = checker.check_for_update().await.unwrap();
        assert!(result.available);
        assert_eq!(result.version, "0.2.0");
    }

    #[tokio::test]
    async fn tuf_apply_update_linux() {
        if cfg!(not(target_os = "linux")) {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let state = {
            let sdir = tempfile::tempdir().unwrap();
            Arc::new(StateDb::open(&sdir.path().join("state.db")).unwrap())
        };

        let current_binary = dir.path().join("qubox");
        std::fs::write(&current_binary, b"old content").unwrap();

        let staged = dir.path().join("staged").join("qubox");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"new content").unwrap();

        let checker = UpdateChecker::new(
            "http://localhost:9999".into(),
            state.clone(),
            "0.1.0".into(),
        )
        .unwrap();

        checker
            .apply_update(&staged, &current_binary)
            .await
            .unwrap();

        assert_eq!(std::fs::read(&current_binary).unwrap(), b"new content");

        let backup = current_binary.with_extension(
            current_binary
                .extension()
                .map(|e| format!("{}.prev", e.to_string_lossy()))
                .unwrap_or_else(|| "prev".into()),
        );
        assert!(backup.exists());

        let proj_dirs = directories::ProjectDirs::from("com", "qubox", "qubox").unwrap();
        let sentinel = proj_dirs.data_local_dir().join(RESTART_SENTINEL);
        assert!(sentinel.exists());
        std::fs::remove_file(&sentinel).ok();

        let records = state.list_updates().unwrap();
        assert!(!records.is_empty());
        assert_eq!(records.last().unwrap().prev_version, Some("0.1.0".into()));
    }

    #[tokio::test]
    async fn tuf_rollback_restores_previous_binary() {
        if cfg!(not(target_os = "linux")) {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let current_binary = dir.path().join("qubox");
        std::fs::write(&current_binary, b"original").unwrap();

        let backup = current_binary.with_extension("prev");
        std::fs::write(&backup, b"original").unwrap();

        std::fs::write(&current_binary, b"corrupted").unwrap();

        let proj_dirs = directories::ProjectDirs::from("com", "qubox", "qubox").unwrap();
        let data_dir = proj_dirs.data_local_dir().to_path_buf();
        std::fs::create_dir_all(&data_dir).unwrap();
        let sentinel = data_dir.join(RESTART_SENTINEL);
        std::fs::write(&sentinel, b"0.2.0").unwrap();

        let rolled_back = UpdateChecker::check_rollback(&current_binary, &data_dir).unwrap();
        assert!(rolled_back, "rollback should have occurred");
        assert_eq!(std::fs::read(&current_binary).unwrap(), b"original");
        assert!(!sentinel.exists(), "sentinel removed");
    }

    #[test]
    fn tuf_update_error_display() {
        let err = UpdateError::HashMismatch {
            expected: "abc".into(),
            actual: "def".into(),
        };
        assert!(err.to_string().contains("abc"));
    }

    #[test]
    fn tuf_version_parsing() {
        assert!(Version::parse("0.2.0").unwrap() > Version::parse("0.1.0").unwrap());
    }
}
