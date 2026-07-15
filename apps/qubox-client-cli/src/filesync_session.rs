//! ADR-022 FileSync during an active native QUIC session.

use std::path::PathBuf;
use std::time::Duration;

use qubox_daemon::ipc::{IpcClient, IpcRequest, IpcResponse};
use qubox_daemon::DaemonConfig;
use qubox_sync::{job_eligible_for_peer, OutboxStatus};
use qubox_transport::filesync::{push_file_over_connection, run_filesync_accept_loop};
use qubox_transport::QuinnConnection as Connection;
use tracing::{info, warn};

const POLL: Duration = Duration::from_secs(3);

fn incoming_dir() -> PathBuf {
    if let Ok(p) = std::env::var("QUBOX_FILESYNC_DIR") {
        return PathBuf::from(p);
    }
    directories::ProjectDirs::from("com", "qubox", "qubox")
        .map(|d| d.data_local_dir().join("incoming"))
        .unwrap_or_else(|| PathBuf::from("qubox-incoming"))
}

/// Accept incoming FileSync bulk + drain local outbox toward peer.
pub fn spawn_session_filesync(conn: Connection, peer_id: String) {
    let dest = incoming_dir();
    let _ = std::fs::create_dir_all(&dest);
    let c1 = conn.clone();
    let dest_c = dest.clone();
    tokio::spawn(async move {
        info!(path = %dest_c.display(), "FileSync acceptor started");
        run_filesync_accept_loop(c1, dest_c).await;
    });
    let c2 = conn;
    tokio::spawn(async move {
        run_client_outbox_drain(c2, peer_id).await;
    });
}

async fn run_client_outbox_drain(conn: Connection, peer_hint: String) {
    info!(peer = %peer_hint, "client FileSync outbox drain started");
    loop {
        if conn.close_reason().is_some() {
            break;
        }
        let cfg = DaemonConfig::default();
        let mut client = match IpcClient::connect(&cfg).await {
            Ok(c) => c,
            Err(_) => {
                tokio::time::sleep(POLL).await;
                continue;
            }
        };
        let jobs = match client
            .call::<IpcResponse>(&IpcRequest::SyncDrainReady)
            .await
        {
            Ok(IpcResponse::SyncJobs { jobs }) => jobs,
            _ => {
                tokio::time::sleep(POLL).await;
                continue;
            }
        };
        for job in jobs {
            if !job_eligible_for_peer(&job, &peer_hint) {
                continue;
            }
            let files = match client
                .call::<IpcResponse>(&IpcRequest::SyncListTrackedFiles)
                .await
            {
                Ok(IpcResponse::SyncTrackedFiles { files }) => files,
                _ => continue,
            };
            let Some(tf) = files.into_iter().find(|f| f.file_id == job.file_id) else {
                continue;
            };
            let path = std::path::Path::new(&tf.local_path);
            if !path.is_file() {
                continue;
            }
            let _ = client
                .call::<IpcResponse>(&IpcRequest::SyncUpdateJob {
                    job_id: job.job_id.clone(),
                    status: OutboxStatus::InFlight,
                    last_error: None,
                })
                .await;
            let rel = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("file.bin");
            match push_file_over_connection(&conn, path, &tf.file_id, rel).await {
                Ok(()) => {
                    let _ = client
                        .call::<IpcResponse>(&IpcRequest::SyncUpdateJob {
                            job_id: job.job_id.clone(),
                            status: OutboxStatus::Done,
                            last_error: None,
                        })
                        .await;
                    info!(job = %job.job_id, "client FileSync push ok");
                }
                Err(e) => {
                    warn!(error = %e, "client FileSync push failed");
                    let _ = client
                        .call::<IpcResponse>(&IpcRequest::SyncUpdateJob {
                            job_id: job.job_id.clone(),
                            status: OutboxStatus::Failed,
                            last_error: Some(e.to_string()),
                        })
                        .await;
                }
            }
        }
        tokio::time::sleep(POLL).await;
    }
}
