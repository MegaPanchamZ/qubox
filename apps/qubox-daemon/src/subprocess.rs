//! Managed subprocess lifecycle — spawn, monitor, restart with backoff,
//! graceful stop with timeout, and stream stderr to tracing.
//!
//! The [`SubprocessManager`] holds a registry of managed subprocesses
//! keyed by user-provided label (e.g. `"host"`, `"client"`).  The daemon's
//! IPC handlers call [`SubprocessManager::start`] / [`SubprocessManager::stop`]
//! to control host-agent and qubox-client-cli subprocesses.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, Mutex, Notify};
use tracing::{error, warn};

use crate::ipc::IpcEvent;

/// Configuration for spawning a managed subprocess.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubprocessConfig {
    /// Path to the binary.
    pub bin_path: PathBuf,
    /// Arguments passed to the binary.
    pub args: Vec<String>,
    /// Environment variables (overrides).  `None` = inherit.
    pub env_override: Option<HashMap<String, String>>,
    /// Maximum restart attempts before giving up.
    #[serde(default = "default_max_restarts")]
    pub max_restarts: usize,
    /// Duration of the restart-backoff window.
    #[serde(default = "default_restart_window")]
    pub restart_window: Duration,
}

fn default_max_restarts() -> usize {
    5
}
fn default_restart_window() -> Duration {
    Duration::from_secs(60)
}

impl Default for SubprocessConfig {
    fn default() -> Self {
        Self {
            bin_path: PathBuf::new(),
            args: Vec::new(),
            env_override: None,
            max_restarts: default_max_restarts(),
            restart_window: default_restart_window(),
        }
    }
}

/// Events emitted by the subprocess lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SubprocessEvent {
    Spawned {
        pid: u32,
    },
    Exited {
        code: Option<i32>,
        reason: String,
    },
    Restarting {
        attempt: usize,
        backoff_ms: u64,
    },
    GiveUp {
        last_code: Option<i32>,
        last_reason: String,
    },
    Stopped,
}

/// A single managed subprocess.
pub struct ManagedSubprocess {
    child: Arc<Mutex<Option<Child>>>,
    stop_flag: Arc<Notify>,
    config: SubprocessConfig,
    restart_history: Arc<Mutex<VecDeque<Instant>>>,
    event_tx: broadcast::Sender<IpcEvent>,
    label: String,
}

impl ManagedSubprocess {
    /// Spawn a new managed subprocess.  The returned handle can be used to
    /// stop the process or wait for it to finish (including auto-restarts).
    pub async fn spawn(
        label: String,
        config: SubprocessConfig,
        event_tx: broadcast::Sender<IpcEvent>,
    ) -> std::io::Result<Arc<Self>> {
        let child = spawn_child(&config).await?;
        let pid = child.id().expect("child should have a PID after spawn");
        let this = Arc::new(Self {
            child: Arc::new(Mutex::new(Some(child))),
            stop_flag: Arc::new(Notify::new()),
            config,
            restart_history: Arc::new(Mutex::new(VecDeque::new())),
            event_tx,
            label,
        });

        // Emit Spawned event
        let _ = this.event_tx.send(IpcEvent::SubprocessEvent {
            label: this.label.clone(),
            event: SubprocessEvent::Spawned { pid },
        });

        Ok(this)
    }

    /// Drive the subprocess lifecycle: wait for exit, restart with backoff,
    /// or propagate a stop signal.
    pub async fn run_to_completion(self: Arc<Self>) {
        loop {
            // Wait for the current child or a stop signal.
            let exit_result = {
                let mut guard = self.child.lock().await;
                let child = guard.as_mut();
                match child {
                    Some(c) => {
                        tokio::select! {
                            biased;
                            _ = self.stop_flag.notified() => {
                                let _ = self.kill_child(c).await;
                                let _ = guard.take();
                                let _ = self.event_tx.send(IpcEvent::SubprocessEvent {
                                    label: self.label.clone(),
                                    event: SubprocessEvent::Stopped,
                                });
                                return;
                            }
                            status = c.wait() => {
                                guard.take(); // child consumed
                                status
                            }
                        }
                    }
                    None => {
                        // No child (should not happen in normal flow).
                        return;
                    }
                }
            };

            let code = exit_result.ok().and_then(|s| s.code());
            let reason = match code {
                Some(0) => "exited successfully".into(),
                Some(c) => format!("exited with code {c}"),
                None => "terminated by signal".into(),
            };

            let _ = self.event_tx.send(IpcEvent::SubprocessEvent {
                label: self.label.clone(),
                event: SubprocessEvent::Exited {
                    code,
                    reason: reason.clone(),
                },
            });

            // Record restart attempt.
            let attempt = {
                let mut hist = self.restart_history.lock().await;
                let now = Instant::now();
                // Prune entries outside the window.
                while let Some(&t) = hist.front() {
                    if now.duration_since(t) > self.config.restart_window {
                        hist.pop_front();
                    } else {
                        break;
                    }
                }
                hist.push_back(now);
                hist.len()
            };

            if attempt > self.config.max_restarts {
                let _ = self.event_tx.send(IpcEvent::SubprocessEvent {
                    label: self.label.clone(),
                    event: SubprocessEvent::GiveUp {
                        last_code: code,
                        last_reason: reason.clone(),
                    },
                });
                warn!(
                    label = %self.label,
                    max_restarts = %self.config.max_restarts,
                    last_code = ?code,
                    last_reason = %reason,
                    "subprocess gave up after max restarts"
                );
                return;
            }

            // Backoff: linear 1s, 2s, 4s, 8s, etc.
            let backoff_ms = (1000u64 << (attempt.saturating_sub(1))).min(30_000);
            let _ = self.event_tx.send(IpcEvent::SubprocessEvent {
                label: self.label.clone(),
                event: SubprocessEvent::Restarting {
                    attempt,
                    backoff_ms,
                },
            });
            warn!(
                label = %self.label,
                attempt,
                backoff_ms,
                last_code = ?code,
                "subprocess restarting"
            );
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;

            // Respawn
            match spawn_child(&self.config).await {
                Ok(new_child) => {
                    let pid = new_child.id().expect("child should have a PID after spawn");
                    *self.child.lock().await = Some(new_child);
                    let _ = self.event_tx.send(IpcEvent::SubprocessEvent {
                        label: self.label.clone(),
                        event: SubprocessEvent::Spawned { pid },
                    });
                }
                Err(e) => {
                    error!(label = %self.label, error = %e, "failed to respawn subprocess");
                    // Continue loop to retry.
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    /// Signal the subprocess to stop.  Sends SIGTERM (Unix) /
    /// TerminateProcess (Windows), waits up to 5 s, then SIGKILL.
    pub async fn stop(&self) {
        self.stop_flag.notify_one();
        // Wait a little for the run_to_completion loop to process the
        // stop signal — actual killing happens there.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// Get the current child PID, if running.
    pub async fn current_pid(&self) -> Option<u32> {
        let guard = self.child.lock().await;
        guard.as_ref().and_then(|c| c.id())
    }

    #[cfg(unix)]
    async fn kill_child(&self, child: &mut Child) {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        if let Some(pid) = child.id() {
            let nix_pid = Pid::from_raw(pid as i32);
            // SIGTERM
            let _ = kill(nix_pid, Signal::SIGTERM);
            // Wait up to 5 s
            tokio::select! {
                biased;
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    // SIGKILL
                    let _ = kill(nix_pid, Signal::SIGKILL);
                    let _ = child.wait().await;
                }
                _ = child.wait() => {}
            }
        }
    }

    #[cfg(not(unix))]
    async fn kill_child(&self, child: &mut Child) {
        #[cfg(windows)]
        {
            if let Some(handle) = child.raw_handle() {
                unsafe {
                    let _ = windows::Win32::System::Threading::TerminateProcess(
                        windows::Win32::Foundation::HANDLE(handle as isize),
                        1,
                    );
                }
            }
        }
        // Give it time to exit.
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            _ = child.wait() => {}
        }
    }
}

/// Registry of managed subprocesses, keyed by label.
#[derive(Clone)]
pub struct SubprocessManager {
    registry: Arc<Mutex<HashMap<String, Arc<ManagedSubprocess>>>>,
    event_tx: broadcast::Sender<IpcEvent>,
}

impl SubprocessManager {
    pub fn new(event_tx: broadcast::Sender<IpcEvent>) -> Self {
        Self {
            registry: Arc::new(Mutex::new(HashMap::new())),
            event_tx,
        }
    }

    /// Start a subprocess with the given label.
    ///
    /// If a process with the same label is already running, returns an error.
    pub async fn start(&self, label: String, config: SubprocessConfig) -> Result<(), String> {
        let mut reg = self.registry.lock().await;
        if reg.contains_key(&label) {
            return Err(format!("subprocess '{label}' is already running"));
        }
        let proc = ManagedSubprocess::spawn(label.clone(), config, self.event_tx.clone())
            .await
            .map_err(|e| format!("spawn failed: {e}"))?;
        let proc_arc = proc.clone();
        reg.insert(label.clone(), proc);
        // Spawn the lifecycle task.
        tokio::spawn(async move {
            proc_arc.run_to_completion().await;
            // Remove from registry when done.
            // (The registry entry is cleaned up via stop() explicitly.)
        });
        Ok(())
    }

    /// Stop a subprocess by label.  Returns an error if not found.
    pub async fn stop(&self, label: &str) -> Result<(), String> {
        let mut reg = self.registry.lock().await;
        let proc = reg
            .remove(label)
            .ok_or_else(|| format!("subprocess '{label}' is not running"))?;
        proc.stop().await;
        Ok(())
    }

    /// Check if a subprocess with the given label is registered.
    pub async fn is_running(&self, label: &str) -> bool {
        let reg = self.registry.lock().await;
        reg.contains_key(label)
    }

    /// Get the current PID for a subprocess, if it is running.
    pub async fn current_pid(&self, label: &str) -> Option<u32> {
        let reg = self.registry.lock().await;
        reg.get(label)?.current_pid().await
    }
}

async fn spawn_child(config: &SubprocessConfig) -> std::io::Result<Child> {
    let mut cmd = Command::new(&config.bin_path);
    cmd.args(&config.args);
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    if let Some(env_overrides) = &config.env_override {
        cmd.envs(env_overrides);
    }

    let mut child = cmd.spawn()?;

    // Stream stderr to tracing in the background.
    if let Some(stderr) = child.stderr.take() {
        let label = config.bin_path.to_string_lossy().to_string();
        tokio::spawn(async move {
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                warn!(subprocess = %label, stderr = %line);
            }
        });
    }

    Ok(child)
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_event_tx() -> broadcast::Sender<IpcEvent> {
        let (tx, _) = broadcast::channel(256);
        tx
    }

    #[tokio::test]
    async fn subprocess_spawns_and_exits_cleanly() {
        let event_tx = test_event_tx();
        let config = SubprocessConfig {
            bin_path: if cfg!(windows) {
                "cmd".into()
            } else {
                "/bin/true".into()
            },
            args: if cfg!(windows) {
                vec!["/c".into(), "exit".into(), "0".into()]
            } else {
                vec![]
            },
            max_restarts: 0,
            restart_window: Duration::from_secs(1),
            ..Default::default()
        };
        let proc = ManagedSubprocess::spawn("test".into(), config, event_tx.clone())
            .await
            .unwrap();
        let pid = proc.current_pid().await;
        assert!(pid.is_some(), "child should have a PID");

        let arc = Arc::clone(&proc);
        tokio::spawn(async move { arc.run_to_completion().await });

        // Wait for exit.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let pid_after = proc.current_pid().await;
        assert!(pid_after.is_none(), "child should have exited");
    }

    #[tokio::test]
    async fn subprocess_restarts_on_crash() {
        let event_tx = test_event_tx();
        let mut rx = event_tx.subscribe();
        let config = SubprocessConfig {
            bin_path: if cfg!(windows) {
                "cmd".into()
            } else {
                "/bin/false".into()
            },
            args: if cfg!(windows) {
                vec!["/c".into(), "exit".into(), "1".into()]
            } else {
                vec![]
            },
            max_restarts: 2,
            restart_window: Duration::from_secs(60),
            ..Default::default()
        };
        let proc = ManagedSubprocess::spawn("crash".into(), config, event_tx.clone())
            .await
            .unwrap();

        let arc = Arc::clone(&proc);
        tokio::spawn(async move { arc.run_to_completion().await });

        // We should see at least one Spawned and one Restarting event.
        let mut saw_spawned = false;
        let mut saw_restarting = false;
        let mut saw_giveup = false;
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            while let Ok(ev) = rx.try_recv() {
                if let IpcEvent::SubprocessEvent { event, .. } = ev {
                    match event {
                        SubprocessEvent::Spawned { .. } => saw_spawned = true,
                        SubprocessEvent::Exited { .. } => {}
                        SubprocessEvent::Restarting { .. } => saw_restarting = true,
                        SubprocessEvent::GiveUp { .. } => saw_giveup = true,
                        SubprocessEvent::Stopped => {}
                    }
                }
            }
            if saw_giveup {
                break;
            }
        }
        assert!(saw_spawned, "should have seen Spawned event");
        assert!(saw_restarting, "should have seen Restarting event");
        assert!(saw_giveup, "should have seen GiveUp event");
    }

    #[tokio::test]
    async fn subprocess_gives_up_after_max_restarts() {
        let event_tx = test_event_tx();
        let mut rx = event_tx.subscribe();
        let config = SubprocessConfig {
            bin_path: if cfg!(windows) {
                "cmd".into()
            } else {
                "/bin/false".into()
            },
            args: if cfg!(windows) {
                vec!["/c".into(), "exit".into(), "1".into()]
            } else {
                vec![]
            },
            max_restarts: 2,
            restart_window: Duration::from_secs(60),
            ..Default::default()
        };
        let proc = ManagedSubprocess::spawn("giveup".into(), config, event_tx.clone())
            .await
            .unwrap();

        let arc = Arc::clone(&proc);
        tokio::spawn(async move { arc.run_to_completion().await });

        let mut giveup_count = 0;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            while let Ok(ev) = rx.try_recv() {
                if let IpcEvent::SubprocessEvent {
                    event: SubprocessEvent::GiveUp { .. },
                    ..
                } = ev
                {
                    giveup_count += 1;
                }
            }
            if giveup_count > 0 {
                break;
            }
        }
        assert!(giveup_count > 0, "should have seen GiveUp");
    }

    #[tokio::test]
    async fn subprocess_stop_kills_child() {
        let event_tx = test_event_tx();
        let mut rx = event_tx.subscribe();
        let config = SubprocessConfig {
            bin_path: if cfg!(windows) {
                "cmd".into()
            } else {
                "/bin/sleep".into()
            },
            args: if cfg!(windows) {
                vec![
                    "/c".into(),
                    "ping".into(),
                    "-n".into(),
                    "60".into(),
                    "127.0.0.1".into(),
                ]
            } else {
                vec!["60".into()]
            },
            max_restarts: 0,
            restart_window: Duration::from_secs(1),
            ..Default::default()
        };
        let proc = ManagedSubprocess::spawn("sleep".into(), config, event_tx.clone())
            .await
            .unwrap();

        let arc = Arc::clone(&proc);
        tokio::spawn(async move { arc.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(200)).await;
        proc.stop().await;

        let mut saw_stopped = false;
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            while let Ok(ev) = rx.try_recv() {
                if let IpcEvent::SubprocessEvent {
                    event: SubprocessEvent::Stopped,
                    ..
                } = ev
                {
                    saw_stopped = true;
                }
            }
            if saw_stopped {
                break;
            }
        }
        assert!(saw_stopped, "should have seen Stopped event after kill");
    }

    #[tokio::test]
    async fn subprocess_pipes_stderr_to_tracing() {
        let event_tx = test_event_tx();
        let config = SubprocessConfig {
            bin_path: if cfg!(windows) {
                "cmd".into()
            } else {
                "/bin/sh".into()
            },
            args: if cfg!(windows) {
                vec!["/c".into(), "echo".into(), "error".into(), ">&2".into()]
            } else {
                vec!["-c".into(), "echo error >&2".into()]
            },
            max_restarts: 0,
            restart_window: Duration::from_secs(1),
            ..Default::default()
        };
        let proc = ManagedSubprocess::spawn("stderr-test".into(), config, event_tx.clone())
            .await
            .unwrap();
        let arc = Arc::clone(&proc);
        tokio::spawn(async move { arc.run_to_completion().await });
        // Allow time for stderr to be read; panic if the pipe task crashes.
        tokio::time::sleep(Duration::from_millis(500)).await;
        drop(proc);
    }
}
