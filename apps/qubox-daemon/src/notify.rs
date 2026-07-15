//! Safe wrappers around `sd_notify` for systemd service notification.
//!
//! On non-systemd systems (or when `$NOTIFY_SOCKET` is unset) all functions
//! are **no-ops** — they return `Ok(())` without touching any socket.

use std::io;
use std::time::Duration;
use tokio::time;

/// Notify systemd that the daemon has finished starting up.
///
/// No-op when `NOTIFY_SOCKET` is not set in the environment.
pub fn notify_ready() -> io::Result<()> {
    if !notify_socket_is_set() {
        return Ok(());
    }
    #[cfg(all(unix, feature = "systemd"))]
    {
        let entries = [("READY", "1")];
        let _ = systemd::daemon::notify(false, entries.iter()).map_err(|e| io::Error::other(e))?;
    }
    Ok(())
}

/// Notify systemd that the daemon is shutting down.
pub fn notify_stopping() -> io::Result<()> {
    if !notify_socket_is_set() {
        return Ok(());
    }
    #[cfg(all(unix, feature = "systemd"))]
    {
        let entries = [("STOPPING", "1")];
        let _ = systemd::daemon::notify(false, entries.iter()).map_err(|e| io::Error::other(e))?;
    }
    Ok(())
}

/// Send a watchdog ping (heartbeat) to systemd.
pub fn watchdog_ping() -> io::Result<()> {
    if !notify_socket_is_set() {
        return Ok(());
    }
    #[cfg(all(unix, feature = "systemd"))]
    {
        let entries = [("WATCHDOG", "1")];
        let _ = systemd::daemon::notify(false, entries.iter()).map_err(|e| io::Error::other(e))?;
    }
    Ok(())
}

/// If `WATCHDOG_USEC` is set in the environment, spawn a background tokio
/// task that pings the watchdog every `WATCHDOG_USEC / 2` microseconds.
pub fn spawn_watchdog() {
    let usec = match std::env::var("WATCHDOG_USEC") {
        Ok(val) => match val.parse::<u64>() {
            Ok(v) if v > 0 => v,
            _ => return,
        },
        _ => return,
    };
    let interval = Duration::from_micros(usec / 2);
    tokio::spawn(async move {
        let mut timer = time::interval(interval);
        timer.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            if let Err(e) = watchdog_ping() {
                tracing::warn!("watchdog ping failed: {e}");
            } else {
                tracing::debug!("watchdog ping");
            }
        }
    });
}

fn notify_socket_is_set() -> bool {
    std::env::var("NOTIFY_SOCKET").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_is_noop_when_notify_socket_unset() {
        let prev = std::env::var("NOTIFY_SOCKET").ok();
        std::env::remove_var("NOTIFY_SOCKET");
        assert!(notify_ready().is_ok());
        assert!(notify_stopping().is_ok());
        assert!(watchdog_ping().is_ok());
        if let Some(val) = prev {
            std::env::set_var("NOTIFY_SOCKET", val);
        }
    }

    #[cfg(all(unix, feature = "systemd"))]
    #[test]
    fn notify_writes_to_notify_socket() {
        use std::os::unix::net::UnixDatagram;
        let dir = std::env::temp_dir().join(format!("notify-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("notify.sock");
        let listener = UnixDatagram::bind(&socket_path).unwrap();
        let prev = std::env::var("NOTIFY_SOCKET").ok();
        std::env::set_var("NOTIFY_SOCKET", &socket_path);

        let result = notify_ready();
        assert!(result.is_ok());

        let mut buf = [0u8; 1024];
        let (n, _) = listener.recv_from(&mut buf).unwrap();
        let msg = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(msg.contains("READY=1"));

        drop(listener);
        std::fs::remove_dir_all(&dir).unwrap();
        match prev {
            Some(v) => std::env::set_var("NOTIFY_SOCKET", v),
            None => std::env::remove_var("NOTIFY_SOCKET"),
        }
    }
}
