//! ADR-022 FileSync sensors: process lock (sysinfo) + FS watcher (notify).
//!
//! Gated by feature `file-sync`. Reports locks and changes to the daemon via IPC.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebouncedEvent, DebouncedEventKind, Debouncer};
use qubox_daemon::ipc::{IpcClient, IpcRequest, IpcResponse};
use qubox_daemon::DaemonConfig;
use qubox_sync::{process_matches, should_ignore_path, SyncRule};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const PROCESS_POLL: Duration = Duration::from_secs(2);
const DEBOUNCE: Duration = Duration::from_secs(2);

pub struct SensorConfig {
    pub socket_path: PathBuf,
    pub node_id: String,
    pub rules: Vec<SyncRule>,
}

/// Spawn process-lock poller + FS watcher.
pub async fn spawn_sensors(cfg: SensorConfig) -> anyhow::Result<()> {
    let rules = Arc::new(cfg.rules);
    let socket = cfg.socket_path.clone();
    let node_id = cfg.node_id.clone();

    let rules_p = Arc::clone(&rules);
    let socket_p = socket.clone();
    tokio::spawn(async move {
        if let Err(e) = process_lock_loop(socket_p, rules_p).await {
            warn!(error = %e, "file-sync process lock loop exited");
        }
    });

    let rules_w = Arc::clone(&rules);
    let socket_w = socket;
    let node_w = node_id;
    tokio::spawn(async move {
        if let Err(e) = watch_loop(socket_w, node_w, rules_w).await {
            warn!(error = %e, "file-sync watch loop exited");
        }
    });

    info!("file-sync sensors started");
    Ok(())
}

fn daemon_cfg(socket: &Path) -> DaemonConfig {
    DaemonConfig {
        socket_path: socket.to_path_buf(),
        ..DaemonConfig::default()
    }
}

async fn process_lock_loop(socket: PathBuf, rules: Arc<Vec<SyncRule>>) -> anyhow::Result<()> {
    let mut sys = sysinfo::System::new();
    let mut previously_locked: HashSet<String> = HashSet::new();
    loop {
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let names: Vec<String> = sys
            .processes()
            .values()
            .map(|p| p.name().to_string_lossy().into_owned())
            .collect();

        let cfg = daemon_cfg(&socket);
        let mut client = match IpcClient::connect(&cfg).await {
            Ok(c) => c,
            Err(e) => {
                debug!(error = %e, "file-sync: daemon not reachable for lock poll");
                tokio::time::sleep(PROCESS_POLL).await;
                continue;
            }
        };

        let tracked = match client
            .call::<IpcResponse>(&IpcRequest::SyncListTrackedFiles)
            .await
        {
            Ok(IpcResponse::SyncTrackedFiles { files }) => files,
            Ok(_) | Err(_) => {
                tokio::time::sleep(PROCESS_POLL).await;
                continue;
            }
        };

        let mut now_locked = HashSet::new();
        for rule in rules.iter().filter(|r| r.enabled) {
            if !process_matches(&names, &rule.process_names) {
                continue;
            }
            for tf in &tracked {
                if let Some(rid) = &tf.rule_id {
                    if rid != &rule.rule_id {
                        continue;
                    }
                } else {
                    let under = rule
                        .paths
                        .iter()
                        .any(|root| Path::new(&tf.local_path).starts_with(Path::new(root)));
                    if !under {
                        continue;
                    }
                }
                now_locked.insert(tf.file_id.clone());
            }
        }

        for id in now_locked.difference(&previously_locked) {
            let _ = client
                .call::<IpcResponse>(&IpcRequest::SyncSetLock {
                    file_id: id.clone(),
                    locked: true,
                })
                .await;
        }
        for id in previously_locked.difference(&now_locked) {
            let _ = client
                .call::<IpcResponse>(&IpcRequest::SyncSetLock {
                    file_id: id.clone(),
                    locked: false,
                })
                .await;
        }
        previously_locked = now_locked;
        tokio::time::sleep(PROCESS_POLL).await;
    }
}

async fn watch_loop(
    socket: PathBuf,
    node_id: String,
    rules: Arc<Vec<SyncRule>>,
) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::channel::<PathBuf>(256);
    let tx_c = tx.clone();

    let mut debouncer: Debouncer<RecommendedWatcher> =
        new_debouncer(DEBOUNCE, move |res: Result<Vec<DebouncedEvent>, _>| {
            if let Ok(events) = res {
                for e in events {
                    if matches!(e.kind, DebouncedEventKind::Any) {
                        let _ = tx_c.blocking_send(e.path);
                    }
                }
            }
        })?;

    for rule in rules.iter().filter(|r| r.enabled) {
        for p in &rule.paths {
            let path = Path::new(p);
            if path.exists() {
                if let Err(e) = debouncer.watcher().watch(path, RecursiveMode::Recursive) {
                    warn!(path = %p, error = %e, "file-sync watch failed");
                } else {
                    info!(path = %p, "file-sync watching");
                }
            }
        }
    }

    // Seed global ignores from daemon (includes .git).
    let mut global_ignores: Vec<String> = Vec::new();
    if let Ok(mut c) = IpcClient::connect(&daemon_cfg(&socket)).await {
        if let Ok(IpcResponse::SyncIgnores { patterns }) =
            c.call::<IpcResponse>(&IpcRequest::SyncListIgnores).await
        {
            global_ignores = patterns;
        }
    }

    while let Some(path) = rx.recv().await {
        let mut extra = global_ignores.clone();
        for r in rules.iter() {
            extra.extend(r.ignore_globs.iter().cloned());
        }
        if should_ignore_path(&path, &extra) {
            continue;
        }
        if !path.is_file() {
            continue;
        }

        let mut matched: Option<(&SyncRule, String)> = None;
        for rule in rules.iter().filter(|r| r.enabled) {
            let under = rule
                .paths
                .iter()
                .any(|root| path.starts_with(Path::new(root)));
            if under {
                let peer = rule.peer_ids.first().cloned().unwrap_or_default();
                matched = Some((rule, peer));
                break;
            }
        }
        let Some((rule, target_peer)) = matched else {
            continue;
        };
        if target_peer.is_empty() {
            continue;
        }

        let cfg = daemon_cfg(&socket);
        let mut client = match IpcClient::connect(&cfg).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        let _ = client
            .call::<IpcResponse>(&IpcRequest::SyncFileChanged {
                local_path: path.to_string_lossy().into_owned(),
                rule_id: Some(rule.rule_id.clone()),
                node_id: node_id.clone(),
                target_peer,
            })
            .await;
    }
    Ok(())
}
