//! `qubox-daemon` — the Qubox background daemon.
//!
//! # Usage
//!
//! ```text
//! qubox-daemon run            [--socket-path <p>] [--state-db-path <p>]
//! qubox-daemon install        [--prefix <dir>]
//! qubox-daemon uninstall      [--prefix <dir>]
//! qubox-daemon service-run
//! qubox-daemon status
//! qubox-daemon update         <check|status|apply <version>>
//! qubox-daemon --help
//! ```
//!
//! Default behaviour: run the daemon as a foreground process.
//!
//! # Service subcommands
//!
//! * `install` — install the OS service (systemd, launchd, or SCM).
//! * `uninstall` — remove the OS service.
//! * `service-run` — entry point used by the service manager (systemd,
//!   launchd, or SCM). On Windows this enters the SCM event loop; on
//!   Unix it is the same as `run` but with sd_notify active.
//! * `status` — check whether the service is installed and running.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use qubox_daemon::ipc::{IpcClient, IpcRequest, IpcResponse, UpdateInfoPublic};
use qubox_daemon::DaemonConfig;

#[derive(Parser, Debug)]
#[command(name = "qubox", version, about = "Qubox background daemon")]
struct Cli {
    /// Log level (trace, debug, info, warn, error).
    #[arg(long, global = true, default_value = "info")]
    log_level: tracing_subscriber::filter::LevelFilter,

    /// Override the IPC socket path.
    #[arg(long, global = true)]
    socket_path: Option<PathBuf>,

    /// Subcommand.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the daemon in the foreground (default).
    Run {
        /// Override the state database path.
        #[arg(long)]
        state_db_path: Option<PathBuf>,

        /// TUF update repository URL.
        /// Overrides QUBOX_UPDATE_REPO env var.
        #[arg(long, env = "QUBOX_UPDATE_REPO")]
        update_repo: Option<String>,
    },
    /// Install the daemon as an OS service (requires root / admin).
    Install {
        /// Installation prefix (default `/`). Useful for testing.
        #[arg(long, default_value = "/")]
        prefix: PathBuf,
    },
    /// Remove the daemon from the OS service manager.
    Uninstall {
        /// Installation prefix (default `/`).
        #[arg(long, default_value = "/")]
        prefix: PathBuf,
    },
    /// Entry point used by the OS service manager.
    ServiceRun {
        /// Override the state database path.
        #[arg(long)]
        state_db_path: Option<PathBuf>,

        /// TUF update repository URL.
        /// Overrides QUBOX_UPDATE_REPO env var.
        #[arg(long, env = "QUBOX_UPDATE_REPO")]
        update_repo: Option<String>,
    },
    /// Show whether the daemon service is installed and running.
    Status,
    /// TUF auto-update operations.
    Update {
        #[command(subcommand)]
        action: UpdateAction,
    },
    /// ADR-022 FileSync control (rules, ignores, jobs, conflicts).
    Sync {
        #[command(subcommand)]
        action: SyncAction,
    },
}

#[derive(Subcommand, Debug)]
enum UpdateAction {
    /// Check for available updates.
    Check,
    /// Show the current update status.
    Status,
    /// Apply an update by version.
    Apply {
        /// Version to apply (e.g. "0.2.0").
        version: String,
    },
}

#[derive(Subcommand, Debug)]
enum SyncAction {
    /// List global never-track patterns (defaults include `.git`).
    ListIgnores,
    /// Replace the full ignore list (JSON array or newline-separated via multiple --pattern).
    SetIgnores {
        #[arg(long = "pattern", required = true)]
        patterns: Vec<String>,
    },
    /// Add one never-track pattern (glob or path segment, e.g. `.git`, `*.rom`).
    AddIgnore { pattern: String },
    /// Remove one never-track pattern.
    RemoveIgnore { pattern: String },
    /// Merge a named preset: default|git|emulator-saves|dev.
    ApplyPreset { name: String },
    /// List sync rules.
    ListRules,
    /// Add a watch rule.
    AddRule {
        #[arg(long)]
        path: Vec<String>,
        #[arg(long)]
        process: Vec<String>,
        #[arg(long)]
        peer: Vec<String>,
        #[arg(long)]
        ignore: Vec<String>,
        #[arg(long, default_value_t = 268435456)]
        max_bytes: u64,
    },
    /// Remove a rule by id.
    RemoveRule { rule_id: String },
    /// Enable/disable a rule.
    SetEnabled {
        rule_id: String,
        #[arg(long)]
        enabled: bool,
    },
    /// List outbox jobs.
    ListJobs,
    /// List conflicts.
    ListConflicts,
    /// Resolve a conflict: keep-local|keep-remote|keep-both.
    ResolveConflict {
        conflict_id: String,
        #[arg(long, value_parser = ["keep-local", "keep-remote", "keep-both"])]
        resolution: String,
    },
    /// Manual push a path to a peer (queues outbox).
    Push {
        path: String,
        #[arg(long)]
        peer: String,
        #[arg(long, default_value = "local")]
        node_id: String,
    },
}

fn load_qubox_env() {
    if let Some(proj_dirs) = directories::ProjectDirs::from("com", "qubox", "qubox") {
        let env_path = proj_dirs.config_dir().join("env");
        if env_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&env_path) {
                for line in content.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                        continue;
                    }
                    let line = if line.starts_with("export ") {
                        &line[7..]
                    } else {
                        line
                    };
                    if let Some((key, val)) = line.split_once('=') {
                        let key = key.trim();
                        let val = val.trim();
                        let val = val.trim_matches(|c| c == '"' || c == '\'');
                        if !key.is_empty() {
                            std::env::set_var(key, val);
                        }
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    load_qubox_env();
    let cli = Cli::parse();

    let log_level: tracing::Level = match cli.log_level {
        tracing_subscriber::filter::LevelFilter::OFF => tracing::Level::ERROR,
        tracing_subscriber::filter::LevelFilter::ERROR => tracing::Level::ERROR,
        tracing_subscriber::filter::LevelFilter::WARN => tracing::Level::WARN,
        tracing_subscriber::filter::LevelFilter::INFO => tracing::Level::INFO,
        tracing_subscriber::filter::LevelFilter::DEBUG => tracing::Level::DEBUG,
        tracing_subscriber::filter::LevelFilter::TRACE => tracing::Level::TRACE,
    };

    let mut config = DaemonConfig::default();
    config.log_level = log_level;
    if let Some(p) = cli.socket_path {
        config.socket_path = p;
    }

    match cli.command.unwrap_or(Command::Run {
        state_db_path: None,
        update_repo: None,
    }) {
        // ── Run (foreground) ──────────────────────────────────────────
        Command::Run {
            state_db_path,
            update_repo,
        } => {
            if let Some(p) = state_db_path {
                config.state_db_path = p;
            }
            config.update_repo = update_repo;
            qubox_daemon::service::Daemon::run(config).await
        }

        // ── Install ───────────────────────────────────────────────────
        Command::Install { prefix } => {
            #[cfg(windows)]
            qubox_daemon::service_scm::ensure_elevated()?;
            install_service(&prefix)?;
            Ok(())
        }

        // ── Uninstall ─────────────────────────────────────────────────
        Command::Uninstall { prefix } => {
            #[cfg(windows)]
            qubox_daemon::service_scm::ensure_elevated()?;
            uninstall_service(&prefix)?;
            Ok(())
        }

        // ── Service run ───────────────────────────────────────────────
        Command::ServiceRun {
            state_db_path,
            update_repo,
        } => {
            if let Some(p) = state_db_path {
                config.state_db_path = p;
            }
            config.update_repo = update_repo;
            config.service_mode = true;

            #[cfg(target_os = "windows")]
            {
                return qubox_daemon::service_scm::run_scm()
                    .map_err(|e| anyhow::anyhow!("SCM error: {e}"));
            }

            #[cfg(not(target_os = "windows"))]
            {
                qubox_daemon::service::Daemon::run(config).await
            }
        }

        // ── Status ────────────────────────────────────────────────────
        Command::Status => {
            print_status();
            Ok(())
        }

        // ── FileSync (ADR-022) ────────────────────────────────────────
        Command::Sync { action } => {
            let mut client = IpcClient::connect(&config).await?;
            run_sync_action(&mut client, action).await
        }

        // ── Update ────────────────────────────────────────────────────
        Command::Update { action } => {
            let mut client = IpcClient::connect(&config).await?;
            match action {
                UpdateAction::Check => {
                    let resp: IpcResponse = client.call(&IpcRequest::CheckUpdate).await?;
                    match resp {
                        IpcResponse::UpdateAvailable {
                            version,
                            manifest_url,
                        } => {
                            println!("update available: {version} ({manifest_url})");
                        }
                        IpcResponse::UpdateStatusResponse {
                            current_version,
                            available: None,
                            ..
                        } => {
                            println!("no update available (current: {current_version})");
                        }
                        IpcResponse::Error { code, message } => {
                            eprintln!("error {code}: {message}");
                            std::process::exit(1);
                        }
                        other => {
                            eprintln!("unexpected response: {other:?}");
                            std::process::exit(1);
                        }
                    }
                }
                UpdateAction::Status => {
                    let resp: IpcResponse = client.call(&IpcRequest::GetUpdateStatus).await?;
                    match resp {
                        IpcResponse::UpdateStatusResponse {
                            current_version,
                            available,
                            last_check_unix,
                        } => {
                            println!("current version: {current_version}");
                            if let Some(UpdateInfoPublic {
                                version,
                                size_bytes,
                                manifest_url,
                            }) = available
                            {
                                println!(
                                    "available: {version} ({size_bytes} bytes, {manifest_url})"
                                );
                            } else {
                                println!("no update pending");
                            }
                            if let Some(ts) = last_check_unix {
                                println!("last check: {ts}");
                            }
                        }
                        IpcResponse::Error { code, message } => {
                            eprintln!("error {code}: {message}");
                            std::process::exit(1);
                        }
                        other => {
                            eprintln!("unexpected response: {other:?}");
                            std::process::exit(1);
                        }
                    }
                }
                UpdateAction::Apply { version } => {
                    let resp: IpcResponse = client
                        .call(&IpcRequest::ApplyUpdate {
                            staged_version: version,
                        })
                        .await?;
                    match resp {
                        IpcResponse::Unit => println!("update applied (daemon will restart)"),
                        IpcResponse::Error { code, message } => {
                            eprintln!("error {code}: {message}");
                            std::process::exit(1);
                        }
                        other => {
                            eprintln!("unexpected response: {other:?}");
                            std::process::exit(1);
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

async fn run_sync_action(client: &mut IpcClient, action: SyncAction) -> anyhow::Result<()> {
    use qubox_sync::{ConflictResolution, SyncRule};

    let print_resp = |resp: IpcResponse| match resp {
        IpcResponse::SyncIgnores { patterns } => {
            if patterns.is_empty() {
                println!("(no ignore patterns)");
            } else {
                for p in patterns {
                    println!("{p}");
                }
            }
        }
        IpcResponse::SyncRules { rules } => {
            for r in rules {
                println!(
                    "{} enabled={} paths={:?} processes={:?} peers={:?} ignore={:?}",
                    r.rule_id, r.enabled, r.paths, r.process_names, r.peer_ids, r.ignore_globs
                );
            }
        }
        IpcResponse::SyncJobs { jobs } => {
            for j in jobs {
                println!(
                    "{} file={} peer={} status={:?} retries={}",
                    j.job_id, j.file_id, j.target_peer, j.status, j.retry_count
                );
            }
        }
        IpcResponse::SyncConflicts { conflicts } => {
            for c in conflicts {
                println!(
                    "{} file={} local={} remote={} peer={}",
                    c.conflict_id, c.file_id, c.local_path, c.remote_path, c.peer_id
                );
            }
        }
        IpcResponse::SyncJob { job } => {
            println!("queued job {} for file {}", job.job_id, job.file_id);
        }
        IpcResponse::Unit => println!("ok"),
        IpcResponse::Error { code, message } => {
            eprintln!("error {code}: {message}");
            std::process::exit(1);
        }
        other => {
            eprintln!("unexpected: {other:?}");
            std::process::exit(1);
        }
    };

    let resp: IpcResponse = match action {
        SyncAction::ListIgnores => client.call(&IpcRequest::SyncListIgnores).await?,
        SyncAction::SetIgnores { patterns } => {
            client
                .call(&IpcRequest::SyncSetIgnores { patterns })
                .await?
        }
        SyncAction::AddIgnore { pattern } => {
            client.call(&IpcRequest::SyncAddIgnore { pattern }).await?
        }
        SyncAction::RemoveIgnore { pattern } => {
            client
                .call(&IpcRequest::SyncRemoveIgnore { pattern })
                .await?
        }
        SyncAction::ApplyPreset { name } => {
            client
                .call(&IpcRequest::SyncApplyIgnorePreset { name })
                .await?
        }
        SyncAction::ListRules => client.call(&IpcRequest::SyncListRules).await?,
        SyncAction::AddRule {
            path,
            process,
            peer,
            ignore,
            max_bytes,
        } => {
            let rule = SyncRule {
                rule_id: String::new(),
                paths: path,
                process_names: process,
                peer_ids: peer,
                enabled: true,
                max_file_bytes: max_bytes,
                ignore_globs: ignore,
            };
            client.call(&IpcRequest::SyncAddRule { rule }).await?
        }
        SyncAction::RemoveRule { rule_id } => {
            client.call(&IpcRequest::SyncRemoveRule { rule_id }).await?
        }
        SyncAction::SetEnabled { rule_id, enabled } => {
            client
                .call(&IpcRequest::SyncSetEnabled { rule_id, enabled })
                .await?
        }
        SyncAction::ListJobs => client.call(&IpcRequest::SyncListJobs).await?,
        SyncAction::ListConflicts => client.call(&IpcRequest::SyncListConflicts).await?,
        SyncAction::ResolveConflict {
            conflict_id,
            resolution,
        } => {
            let resolution = match resolution.as_str() {
                "keep-local" => ConflictResolution::KeepLocal,
                "keep-remote" => ConflictResolution::KeepRemote,
                "keep-both" => ConflictResolution::KeepBoth,
                _ => anyhow::bail!("invalid resolution"),
            };
            client
                .call(&IpcRequest::SyncResolveConflict {
                    conflict_id,
                    resolution,
                })
                .await?
        }
        SyncAction::Push {
            path,
            peer,
            node_id,
        } => {
            client
                .call(&IpcRequest::SyncPushNow {
                    local_path: path,
                    target_peer: peer,
                    node_id,
                })
                .await?
        }
    };
    print_resp(resp);
    Ok(())
}

// ── Platform-specific install / uninstall / status helpers ───────────

/// Install the daemon as an OS service.
#[cfg(target_os = "linux")]
fn install_service(prefix: &std::path::Path) -> anyhow::Result<()> {
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dist");
    let script = dist.join("install.sh");
    if !script.exists() {
        anyhow::bail!("install.sh not found at {:?}", script);
    }
    let output = std::process::Command::new("bash")
        .arg(script.to_str().unwrap())
        .arg(prefix.to_str().unwrap_or("/"))
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run install.sh: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!("install.sh failed:\n{stdout}\n{stderr}");
    }
    print!("{stdout}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_service(prefix: &std::path::Path) -> anyhow::Result<()> {
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dist");
    let script = dist.join("install-macos.sh");
    if !script.exists() {
        anyhow::bail!("install-macos.sh not found at {:?}", script);
    }
    let output = std::process::Command::new("bash")
        .arg(script.to_str().unwrap())
        .arg(prefix.to_str().unwrap_or("/"))
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run install-macos.sh: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!("install-macos.sh failed:\n{stdout}\n{stderr}");
    }
    print!("{stdout}");
    Ok(())
}

#[cfg(target_os = "windows")]
fn install_service(prefix: &std::path::Path) -> anyhow::Result<()> {
    let bin_path = prefix
        .join("Program Files/Qubox/qubox-daemon.exe")
        .to_string_lossy()
        .to_string();
    qubox_daemon::service_scm::install_service("Qubox Daemon", &bin_path)?;
    println!("service registered: QuboxDaemon");
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn install_service(_prefix: &std::path::Path) -> anyhow::Result<()> {
    anyhow::bail!("service install not supported on this platform");
}

/// Uninstall the daemon from the OS service manager.
#[cfg(target_os = "linux")]
fn uninstall_service(prefix: &std::path::Path) -> anyhow::Result<()> {
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dist");
    let script = dist.join("uninstall.sh");
    if !script.exists() {
        anyhow::bail!("uninstall.sh not found at {:?}", script);
    }
    let output = std::process::Command::new("bash")
        .arg(script.to_str().unwrap())
        .arg(prefix.to_str().unwrap_or("/"))
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run uninstall.sh: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!("uninstall.sh failed:\n{stdout}\n{stderr}");
    }
    print!("{stdout}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_service(prefix: &std::path::Path) -> anyhow::Result<()> {
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dist");
    let script = dist.join("uninstall-macos.sh");
    if !script.exists() {
        anyhow::bail!("uninstall-macos.sh not found at {:?}", script);
    }
    let output = std::process::Command::new("bash")
        .arg(script.to_str().unwrap())
        .arg(prefix.to_str().unwrap_or("/"))
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run uninstall-macos.sh: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!("uninstall-macos.sh failed:\n{stdout}\n{stderr}");
    }
    print!("{stdout}");
    Ok(())
}

#[cfg(target_os = "windows")]
fn uninstall_service(_prefix: &std::path::Path) -> anyhow::Result<()> {
    qubox_daemon::service_scm::uninstall_service()?;
    println!("service removed: QuboxDaemon");
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn uninstall_service(_prefix: &std::path::Path) -> anyhow::Result<()> {
    anyhow::bail!("service uninstall not supported on this platform");
}

/// Print the service status.
#[cfg(target_os = "linux")]
fn print_status() {
    let active = run_systemctl(&["is-active", "qubox.service"]).ok();
    let enabled = run_systemctl(&["is-enabled", "qubox.service"]).ok();
    println!("service: qubox");
    println!("  active:  {}", active.unwrap_or_else(|| "unknown".into()));
    println!("  enabled: {}", enabled.unwrap_or_else(|| "unknown".into()));
}

#[cfg(target_os = "macos")]
fn print_status() {
    let result = run_launchctl(&["list", "com.qubox.daemon"]);
    match result {
        Ok(out) => println!("com.qubox.daemon:\n{out}"),
        Err(e) => println!("com.qubox.daemon: not loaded ({e})"),
    }
}

#[cfg(target_os = "windows")]
fn print_status() {
    use windows_service::service::ServiceState;
    match qubox_daemon::service_scm::service_status() {
        Some(ServiceState::Running) => println!("QuboxDaemon: Running"),
        Some(ServiceState::Stopped) => println!("QuboxDaemon: Stopped"),
        Some(state) => println!("QuboxDaemon: {state:?}"),
        None => println!("QuboxDaemon: Not installed"),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn print_status() {
    println!("status not supported on this platform");
}

// ── Shell helpers ────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn run_systemctl(args: &[&str]) -> anyhow::Result<String> {
    let output = std::process::Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run systemctl: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("systemctl {} failed: {stderr}", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(target_os = "macos")]
fn run_launchctl(args: &[&str]) -> anyhow::Result<String> {
    let output = std::process::Command::new("launchctl")
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run launchctl: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("launchctl {} failed: {stderr}", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
