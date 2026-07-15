//! Daemon entry point — init, signal handling, graceful shutdown.

use std::sync::Arc;

use tracing::{error, info, warn};

use crate::ipc::IpcServer;
use crate::load_update_repo;
use crate::notify::{self, spawn_watchdog};
use crate::pidfile;
use crate::socket_activation::try_activate;
use crate::state::StateDb;
use crate::tuf::UpdateChecker;
use crate::DaemonConfig;

/// The top-level daemon service.
pub struct Daemon;

impl Daemon {
    /// Run the daemon.
    ///
    /// 1. Initialise `tracing` at the configured log level.
    /// 2. Check for rollback (`.prev` backup + restart sentinel) and restore if needed.
    /// 3. Open (or create) the state database.
    /// 4. Construct the TUF `UpdateChecker` from `QUBOX_UPDATE_REPO` env var (None if unset).
    /// 5. Bind the IPC listener and attach the checker.
    /// 6. Wait for either the IPC accept loop or Ctrl+C / SIGTERM.
    /// 7. On shutdown, gracefully stop the IPC listener and close the DB.
    pub async fn run(config: DaemonConfig) -> anyhow::Result<()> {
        let filter = tracing_subscriber::EnvFilter::builder()
            .with_default_directive(config.log_level.into())
            .from_env_lossy();
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .init();

        info!(
            socket_path = %config.socket_path.display(),
            state_db_path = %config.state_db_path.display(),
            "qubox-daemon starting"
        );

        // Check for rollback before doing anything else.
        if let Some(data_dir) = config.rollback_data_dir() {
            let current_binary =
                std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("qubox"));
            match UpdateChecker::check_rollback(&current_binary, &data_dir) {
                Ok(true) => info!("rolled back to previous binary"),
                Ok(false) => {}
                Err(e) => warn!("rollback check failed: {e}"),
            }
        }

        let state = StateDb::open(&config.state_db_path)?;
        let state = Arc::new(state);

        // Construct the TUF update checker from CLI flag / env / persisted state.
        let repo_url = load_update_repo(&state, &config);
        let update_checker = repo_url
            .map(|url| {
                let current_version = env!("CARGO_PKG_VERSION").to_string();
                UpdateChecker::new(url, state.clone(), current_version)
                    .map(Arc::new)
                    .map_err(|e| anyhow::anyhow!("init update checker: {e}"))
            })
            .transpose()?;

        // Try systemd socket activation first.
        let activated = {
            #[cfg(unix)]
            {
                try_activate()
            }
            #[cfg(not(unix))]
            {
                None::<crate::socket_activation::ActivatedSocket>
            }
        };
        let mut ipc_server = IpcServer::bind(
            &config,
            state.clone(),
            #[cfg(unix)]
            activated.map(|a| a.listener),
        )
        .await?;

        if let Some(checker) = update_checker {
            ipc_server = ipc_server.with_checker(checker);
        }

        // Write PID file unless service-managed.
        let pidfile_path = pidfile::default_pidfile_path();
        if pidfile::should_write_pidfile() {
            if let Err(e) = pidfile::write_pidfile(&pidfile_path) {
                warn!("failed to write PID file: {e}");
            }
        }

        // Notify systemd that we are ready (service mode only).
        if config.service_mode {
            if let Err(e) = notify::notify_ready() {
                warn!("sd_notify READY=1 failed: {e}");
            }
            spawn_watchdog();
        }

        let shutdown_handle = ipc_server.shutdown_handle();

        // Signal channels
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

        // Ctrl+C handler (uses a clone of the sender)
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to install Ctrl+C handler");
            info!("Ctrl+C received, shutting down");
            let _ = tx.send(true);
        });

        // SIGTERM on Unix (uses a clone of the sender)
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm =
                signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
            let tx = shutdown_tx.clone();
            tokio::spawn(async move {
                sigterm.recv().await;
                info!("SIGTERM received, shutting down");
                let _ = tx.send(true);
            });
        }

        let run_fut = ipc_server.run();

        tokio::select! {
            result = run_fut => {
                if let Err(e) = result {
                    error!("IPC server exited with error: {e}");
                }
            }
            _ = shutdown_rx.changed() => {
                info!("shutdown signal received");
                shutdown_handle.signal_shutdown();
            }
        }

        drop(shutdown_tx);

        // Notify systemd that we are stopping (service mode only).
        if config.service_mode {
            if let Err(e) = notify::notify_stopping() {
                warn!("sd_notify STOPPING=1 failed: {e}");
            }
        }

        // Clean up PID file.
        if pidfile::should_write_pidfile() {
            pidfile::remove_pidfile(&pidfile_path).ok();
        }

        info!("qubox-daemon stopped");
        Ok(())
    }
}
