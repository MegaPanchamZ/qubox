//! Systemd socket activation — receive a pre-opened socket from the service
//! manager via `LISTEN_FDS` / `LISTEN_PID`.
//!
//! This module checks the environment variables set by systemd when socket
//! activation is used and wraps the passed file descriptor as a
//! `tokio::net::UnixListener`.

use std::os::unix::io::FromRawFd;
use tokio::net::UnixListener;

/// A Unix listener that was passed by systemd via socket activation.
pub struct ActivatedSocket {
    pub listener: UnixListener,
    /// The socket path (derived from `/proc/self/fd/<N>` or unknown).
    pub path_buf: Option<std::path::PathBuf>,
}

/// Check `LISTEN_PID` / `LISTEN_FDS` and, if they match our process,
/// wrap the first file descriptor as a `UnixListener`.
///
/// Returns `None` when:
/// - `LISTEN_FDS` or `LISTEN_PID` is not set.
/// - `LISTEN_PID` does not match the current PID.
/// - `LISTEN_FDS` is zero.
pub fn try_activate() -> Option<ActivatedSocket> {
    let listen_pid = std::env::var("LISTEN_PID").ok()?;
    let listen_fds = std::env::var("LISTEN_FDS").ok()?;
    let pid: u32 = listen_pid.parse().ok()?;
    let fds: u32 = listen_fds.parse().ok()?;

    if pid != std::process::id() || fds < 1 {
        return None;
    }

    // The first passed fd is always file descriptor 3 (LISTEN_FDS_START).
    //
    // SAFETY: systemd guarantees fd 3 (and above) is open and refers to
    // the socket described in the .socket unit when LISTEN_PID matches
    // our PID and LISTEN_FDS >= 1. We take ownership of this fd.
    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) };
    let listener = match UnixListener::from_std(std_listener) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("failed to wrap activated socket: {e}");
            return None;
        }
    };

    Some(ActivatedSocket {
        listener,
        path_buf: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_activate_returns_none_when_unset() {
        let prev_pid = std::env::var("LISTEN_PID").ok();
        let prev_fds = std::env::var("LISTEN_FDS").ok();
        std::env::remove_var("LISTEN_PID");
        std::env::remove_var("LISTEN_FDS");

        assert!(try_activate().is_none());

        if let Some(v) = prev_pid {
            std::env::set_var("LISTEN_PID", v);
        }
        if let Some(v) = prev_fds {
            std::env::set_var("LISTEN_FDS", v);
        }
    }

    #[test]
    fn try_activate_returns_none_when_pid_mismatch() {
        let prev_pid = std::env::var("LISTEN_PID").ok();
        let prev_fds = std::env::var("LISTEN_FDS").ok();

        // Wrong PID → None
        std::env::set_var("LISTEN_PID", "99999999");
        std::env::set_var("LISTEN_FDS", "1");
        assert!(try_activate().is_none());

        match prev_pid {
            Some(v) => std::env::set_var("LISTEN_PID", v),
            None => std::env::remove_var("LISTEN_PID"),
        }
        match prev_fds {
            Some(v) => std::env::set_var("LISTEN_FDS", v),
            None => std::env::remove_var("LISTEN_FDS"),
        }
    }
}
