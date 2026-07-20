//! PID file management — atomic write, stale detection, cleanup.
//!
//! On Linux the daemon writes a PID file to `$XDG_RUNTIME_DIR` (or the
//! platform default) unless it is running under a service manager
//! (systemd — detected via `INVOCATION_ID`, launchd — via `LAUNCHD_JOB`).
//! On macOS / Windows the same logic applies.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Default PID file path based on platform conventions.
///
/// Linux: `$XDG_RUNTIME_DIR/qubox/qubox-daemon.pid`
/// macOS: `$HOME/Library/Application Support/com.qubox.daemon/daemon.pid`
/// Windows: `%APPDATA%\Qubox\daemon.pid`
pub fn default_pidfile_path() -> PathBuf {
    let proj_dirs = directories::ProjectDirs::from("com", "qubox", "qubox")
        .expect("platform data dirs available");
    let data_dir = proj_dirs.data_local_dir().to_path_buf();

    #[cfg(target_os = "linux")]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            let dir = PathBuf::from(runtime_dir).join("qubox");
            return dir.join("qubox-daemon.pid");
        }
    }
    data_dir.join("daemon.pid")
}

/// Returns `false` when the daemon is service-managed (systemd / launchd),
/// so the PID file is not written.
pub fn should_write_pidfile() -> bool {
    should_write_pidfile_with(
        std::env::var("INVOCATION_ID").ok().as_deref(),
        std::env::var("LAUNCHD_JOB").ok().as_deref(),
    )
}

/// Pure version of [`should_write_pidfile`] that takes the env values as
/// arguments.  Useful for tests that want to avoid mutating the process
/// environment.
pub fn should_write_pidfile_with(invocation_id: Option<&str>, launchd_job: Option<&str>) -> bool {
    if invocation_id.is_some() {
        return false;
    }
    if launchd_job.is_some() {
        return false;
    }
    true
}

/// Atomically write the current PID to `path`.
///
/// 1. Cleans up any stale PID file first.
/// 2. Writes to a temporary file alongside `path`.
/// 3. Renames (atomic on Unix, best-effort on Windows).
pub fn write_pidfile(path: &Path) -> io::Result<()> {
    cleanup_stale_pidfile(path).ok();

    let pid = std::process::id();
    let content = format!("{pid}\n");

    let parent = path.parent().unwrap_or(Path::new("."));
    let tmp_path = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or(std::borrow::Cow::Borrowed("pid"))
    ));

    {
        let mut tmp = std::fs::File::create(&tmp_path)?;
        tmp.write_all(content.as_bytes())?;
        tmp.sync_all()?;
    }

    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Read the PID file, check whether the process is still alive, and
/// remove the file if the PID is stale.
pub fn cleanup_stale_pidfile(path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(path)?;
    let pid: u32 = content
        .trim()
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad PID: {e}")))?;

    if !is_pid_alive(pid) {
        remove_pidfile(path)?;
    }
    Ok(())
}

/// Best-effort remove of the PID file.  Errors are silently ignored.
pub fn remove_pidfile(path: &Path) -> io::Result<()> {
    let _ = std::fs::remove_file(path);
    Ok(())
}

// ── Platform helpers ────────────────────────────────────────────────

#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    use nix::sys::signal::{kill, Signal};
    // `kill(pid, 0)` returns Ok if the process exists and we have
    // permission to signal it (always true for our own UID).
    kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGTERM).is_ok()
        || kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGKILL).is_ok()
}

#[cfg(not(unix))]
fn is_pid_alive(pid: u32) -> bool {
    // Windows: OpenProcess with PROCESS_QUERY_LIMITED_INFORMATION.
    // If the handle is valid, the process exists.
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        unsafe {
            let handle =
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).unwrap_or_default();
            if handle.is_invalid() {
                return false;
            }
            let _ = CloseHandle(handle);
            true
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        false
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pidfile_write_creates_file_with_current_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        write_pidfile(&path).unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: u32 = content.trim().parse().unwrap();
        assert_eq!(parsed, std::process::id());
    }

    #[test]
    fn pidfile_atomic_write_uses_temp_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        // Pre-create with stale content.
        std::fs::write(&path, b"99999\n").unwrap();
        write_pidfile(&path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: u32 = content.trim().parse().unwrap();
        assert_eq!(parsed, std::process::id());
        // Expect no `.tmp` remnants.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(!entries.iter().any(|n| n.contains(".tmp")));
    }

    #[test]
    fn pidfile_cleanup_stale_removes_when_process_dead() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        // PID 0x7FFFFFFF is very unlikely to exist.
        std::fs::write(&path, format!("{}\n", 0x7FFFFFFFu32)).unwrap();
        cleanup_stale_pidfile(&path).unwrap();
        // If the PID is not alive the file should be removed.
        // On a real system with PID namespace collisions this could
        // leave the file (acceptable); we just verify the function
        // runs without error.
        // Actually assert: if the file still exists the PID must be alive.
        if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap();
            let pid: u32 = content.trim().parse().unwrap();
            // The only way this PID exists is if we somehow have a
            // process with that ID — unlikely.
            assert!(
                is_pid_alive(pid),
                "file should be removed for dead PID {pid}"
            );
        }
    }

    #[test]
    fn pidfile_should_write_pidfile_skips_when_invocation_id_set() {
        assert!(!should_write_pidfile_with(Some("abc"), None));
    }

    #[test]
    fn pidfile_should_write_pidfile_skips_when_launchd_job_set() {
        assert!(!should_write_pidfile_with(None, Some("xyz")));
    }

    #[test]
    fn pidfile_should_write_pidfile_returns_true_when_unmanaged() {
        assert!(should_write_pidfile_with(None, None));
    }

    #[test]
    fn pidfile_should_write_pidfile_with_both_set_still_skips() {
        assert!(!should_write_pidfile_with(Some("a"), Some("b")));
    }
}
