//! ADR-022: drain daemon outbox over live QUIC FileSync streams.
//!
//! Bulk transfers honor [`FileSyncCongestionGate`] when a media bitrate
//! sample is provided (pause near media high-water, resume below low-water).

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use qubox_daemon::ipc::{IpcClient, IpcRequest, IpcResponse};
use qubox_daemon::DaemonConfig;
use qubox_sync::{job_eligible_for_peer, tracked_file_pushable, OutboxStatus};
use qubox_transport::filesync::{
    push_file_over_connection, wait_for_filesync_budget, FileSyncCongestionGate,
};
use qubox_transport::QuinnConnection as Connection;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

const POLL: Duration = Duration::from_secs(3);

/// Live media bitrate sample shared with FileSync drain (bps).
#[derive(Debug, Default)]
pub struct MediaBitrateSample {
    pub current_bps: AtomicU32,
    pub target_bps: AtomicU32,
}

impl MediaBitrateSample {
    pub fn new(target_kbps: u32) -> Arc<Self> {
        let s = Arc::new(Self::default());
        s.target_bps
            .store(target_kbps.saturating_mul(1000), Ordering::Relaxed);
        s
    }

    pub fn set_current_kbps(&self, kbps: u32) {
        self.current_bps
            .store(kbps.saturating_mul(1000), Ordering::Relaxed);
    }

    pub fn sample(&self) -> (u32, u32) {
        (
            self.current_bps.load(Ordering::Relaxed),
            self.target_bps.load(Ordering::Relaxed),
        )
    }
}

/// Poll daemon for pending jobs and push files to the connected peer.
pub async fn run_outbox_drain(conn: Connection, peer_hint: String) {
    run_outbox_drain_with_congestion(conn, peer_hint, None).await;
}

/// Same as [`run_outbox_drain`] but waits on a congestion gate fed by `media`.
pub async fn run_outbox_drain_with_congestion(
    conn: Connection,
    peer_hint: String,
    media: Option<Arc<MediaBitrateSample>>,
) {
    info!(peer = %peer_hint, "FileSync outbox drain started");
    let gate = Arc::new(Mutex::new(FileSyncCongestionGate::default()));
    loop {
        if conn.close_reason().is_some() {
            break;
        }
        if let Some(ref m) = media {
            let sample = m.clone();
            wait_for_filesync_budget(&gate, move || sample.sample()).await;
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
            if let Some(ref m) = media {
                let sample = m.clone();
                wait_for_filesync_budget(&gate, move || sample.sample()).await;
            }
            let file = match client
                .call::<IpcResponse>(&IpcRequest::SyncListTrackedFiles)
                .await
            {
                Ok(IpcResponse::SyncTrackedFiles { files }) => {
                    files.into_iter().find(|f| f.file_id == job.file_id)
                }
                _ => None,
            };
            let Some(tf) = file else {
                let _ = client
                    .call::<IpcResponse>(&IpcRequest::SyncUpdateJob {
                        job_id: job.job_id.clone(),
                        status: OutboxStatus::Failed,
                        last_error: Some("tracked file missing".into()),
                    })
                    .await;
                continue;
            };
            if !tracked_file_pushable(&tf) {
                debug!(file_id = %tf.file_id, "skip locked/conflict file");
                continue;
            }
            let path = Path::new(&tf.local_path);
            if !path.is_file() {
                let _ = client
                    .call::<IpcResponse>(&IpcRequest::SyncUpdateJob {
                        job_id: job.job_id.clone(),
                        status: OutboxStatus::Failed,
                        last_error: Some("path not a file".into()),
                    })
                    .await;
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
                    info!(job = %job.job_id, path = %tf.local_path, "FileSync pushed");
                }
                Err(e) => {
                    warn!(error = %e, job = %job.job_id, "FileSync push failed");
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
    info!("FileSync outbox drain stopped");
}
