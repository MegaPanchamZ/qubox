//! `qubox-daemon` — the Qubox background daemon.
//!
//! # Architecture
//!
//! The daemon is a long-running per-user background process that owns the
//! signaling WebSocket connection, pairing lifecycle, host/client session
//! lifecycle, TUF auto-update, and state persistence.  GUI and CLI tools
//! communicate with the daemon over a local IPC channel (Unix socket or
//! Named Pipe).
//!
//! ## Module structure
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! | `state` | redb-backed persistent state (pairings, settings, TUF metadata, session history) |
//! | `ipc`   | IPC server: binary-frame protocol over Unix socket / Named Pipe |
//! | `service` | Daemon entry point: tracing init, state open, IPC bind, signal handling |

pub mod ipc;
pub mod notify;
pub mod pidfile;
pub mod service;
pub mod state;
pub mod subprocess;
pub mod tuf;

#[cfg(unix)]
pub mod socket_activation;

#[cfg(not(unix))]
/// Stub for non-Unix platforms — socket activation is Linux-only.
pub mod socket_activation {
    pub struct ActivatedSocket;
    pub fn try_activate() -> Option<ActivatedSocket> {
        None
    }
}

#[cfg(windows)]
pub mod service_scm;

#[cfg(not(windows))]
/// Stub for non-Windows platforms — SCM only exists on Windows.
pub mod service_scm {
    pub fn run_scm() -> anyhow::Result<()> {
        Err(anyhow::anyhow!("SCM mode is only supported on Windows"))
    }
    pub fn is_elevated() -> bool {
        true
    }
    pub fn ensure_elevated() -> anyhow::Result<()> {
        Ok(())
    }
}

use std::path::PathBuf;

/// Resolve the update repository URL from CLI, env vars, or persistent state.
///
/// Priority (highest first):
/// 1. Persistent value in `state` (redb `settings` table, key `"update_repo"`)
/// 2. CLI `--update-repo` flag
/// 3. `QUBOX_UPDATE_REPO` env var
/// 4. `QUBOX_UPDATE_REPO` env var (deprecated, warns)
///
/// When a non-persistent source is resolved, it is written back to state
/// so the daemon remembers it on subsequent restarts.
pub fn load_update_repo(state: &crate::state::StateDb, config: &DaemonConfig) -> Option<String> {
    // 1. Persistent (redb)
    if let Ok(Some(persisted)) = state.get_setting("update_repo") {
        return Some(persisted);
    }
    // 2-4. CLI / env
    let resolved = resolve_update_repo(config.update_repo.as_deref());
    if let Some(url) = &resolved {
        // Persist for future restarts.
        let _ = state.set_setting("update_repo", url);
    }
    resolved
}

/// Resolve the update repo URL from non-persistent sources only.
fn resolve_update_repo(cli_value: Option<&str>) -> Option<String> {
    if let Some(url) = cli_value {
        return Some(url.to_string());
    }
    if let Ok(url) = std::env::var("QUBOX_UPDATE_REPO") {
        return Some(url);
    }
    if let Ok(url) = std::env::var("QUBOX_UPDATE_REPO") {
        tracing::warn!("QUBOX_UPDATE_REPO is deprecated, use QUBOX_UPDATE_REPO instead");
        return Some(url);
    }
    None
}

/// Daemon configuration loaded from CLI flags + platform defaults.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Path to the IPC socket (Unix) or pipe name (Windows).
    pub socket_path: PathBuf,
    /// Path to the redb state database file.
    pub state_db_path: PathBuf,
    /// Log level for tracing.
    pub log_level: tracing::Level,
    /// Whether the daemon was started in service mode (e.g. via systemd,
    /// launchd, or SCM). When true, sd_notify / SCM status reporting are
    /// active.
    pub service_mode: bool,
    /// TUF update repository URL (from --update-repo, env, or persistent state).
    pub update_repo: Option<String>,
    /// Signaling WebSocket URL for share/kick (env QUBOX_SERVER).
    pub signaling_url: Option<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        let proj_dirs = directories::ProjectDirs::from("com", "qubox", "qubox")
            .expect("platform data dirs available");

        let data_dir = proj_dirs.data_local_dir().to_path_buf();
        let config_dir = proj_dirs.config_dir().to_path_buf();

        // Socket path: Linux uses XDG_RUNTIME_DIR, macOS uses ~/Library/…
        #[cfg(target_os = "linux")]
        let socket_path = {
            let dir = std::env::var("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| data_dir.join("run"));
            std::fs::create_dir_all(&dir).ok();
            dir.join("qubox.sock")
        };

        #[cfg(target_os = "macos")]
        let socket_path = data_dir.join("daemon.sock");

        #[cfg(target_os = "windows")]
        let socket_path = PathBuf::from(r"\\.\pipe\Qubox");

        Self {
            socket_path,
            state_db_path: config_dir.join("state.db"),
            log_level: tracing::Level::INFO,
            service_mode: false,
            update_repo: None,
            signaling_url: std::env::var("QUBOX_SERVER").ok().filter(|s| !s.is_empty()),
        }
    }
}

impl DaemonConfig {
    /// Returns the per-platform data directory used for the rollback sentinel.
    /// Returns `None` if `directories::ProjectDirs` cannot resolve (very rare).
    pub fn rollback_data_dir(&self) -> Option<PathBuf> {
        directories::ProjectDirs::from("com", "qubox", "qubox")
            .map(|d| d.data_local_dir().to_path_buf())
    }
}

/// Returns the default daemon IPC socket path for the current platform.
///
/// Linux: `$XDG_RUNTIME_DIR/qubox.sock` (fallback: `~/.local/share/…/run/qubox.sock`).
/// macOS: `~/Library/Application Support/com.qubox.qubox/daemon.sock`.
/// Windows: `\\.\pipe\Qubox`.
pub fn default_daemon_socket_path() -> PathBuf {
    let proj_dirs = directories::ProjectDirs::from("com", "qubox", "qubox")
        .expect("platform data dirs available");
    let data_dir = proj_dirs.data_local_dir().to_path_buf();

    #[cfg(target_os = "linux")]
    {
        let dir = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir.join("run"));
        dir.join("qubox.sock")
    }

    #[cfg(target_os = "macos")]
    {
        data_dir.join("daemon.sock")
    }

    #[cfg(target_os = "windows")]
    {
        PathBuf::from(r"\\.\pipe\Qubox")
    }
}

/// Top-level daemon error.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("IPC error: {0}")]
    Ipc(String),
    #[error("Database error: {0}")]
    Db(#[from] redb::Error),
    #[error("Serialization error: {0}")]
    Serde(#[from] bincode::Error),
    #[error("Signal: {0}")]
    Signal(String),
}

/// The big-endian wire magic for IPC frames.
pub(crate) const IPC_MAGIC: u32 = 0x71_75_62_78;
/// Current protocol version in the IPC header.
pub(crate) const IPC_VERSION: u16 = 1;
/// Size of the fixed-length IPC header in bytes.
pub(crate) const IPC_HEADER_SIZE: usize = 20;
/// Maximum payload size (1 MiB).
pub(crate) const IPC_MAX_PAYLOAD: u32 = 1 << 20;

#[cfg(test)]
mod install_tests {
    /// Verify that the install script can write unit files to a temp prefix.
    #[test]
    #[cfg(target_os = "linux")]
    fn install_command_writes_unit_file() {
        use std::process::Command;

        let dir = tempfile::tempdir().expect("tempdir");
        let prefix = dir.path().join("testprefix");
        std::fs::create_dir_all(&prefix).unwrap();

        let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dist");
        let script = dist.join("install.sh");

        if !script.exists() {
            eprintln!("install.sh not found at {:?}; skipping test", script);
            return;
        }

        let output = Command::new("bash")
            .arg(script.to_str().unwrap())
            .arg(prefix.to_str().unwrap())
            .output()
            .expect("install.sh should run");

        let systemd_dir = prefix.join("etc/systemd/system");
        assert!(
            systemd_dir.join("qubox.service").exists(),
            "qubox.service should have been copied to {:?}",
            systemd_dir
        );
        assert!(
            systemd_dir.join("qubox.socket").exists(),
            "qubox.socket should have been copied"
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("installed qubox.service"),
            "install output should mention installation, got: {stdout}"
        );
    }
}
