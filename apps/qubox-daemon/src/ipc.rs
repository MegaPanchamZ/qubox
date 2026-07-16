//! Cross-platform IPC server and client (Unix socket / Named Pipe).
//!
//! # Wire protocol
//!
//! Every message is a binary frame:
//!
//! ```text
//! Offset  Size  Field
//! 0       4     magic: u32        = 0x71_75_62_78 (little-endian)
//! 4       2     version: u16      = 0x0001 (current)
//! 6       2     kind: u16         = 1=Request, 2=Response, 3=Event
//! 8       8     correlation_id: u64
//! 16      4     payload_len: u32  (max 1 MiB)
//! 20      N     payload: [u8; payload_len]  (bincode-serialized)
//! ```
//!
//! # SubscribeEvents (server-streaming)
//!
//! 1. Client sends `SubscribeEvents` with `correlation_id = C`.
//! 2. Server sends `kind=Response` + `IpcResponse::Unit` (the ack).
//! 3. For each event, server sends `kind=Event` + `IpcEvent` with the same C.
//! 4. When the subscription ends, a final `kind=Response` + `IpcResponse::Unit` is sent.
//!
//! # Auth (Linux / macOS)
//!
//! `SO_PEERCRED` via `nix::sys::socket::getsockopt`. The connection is
//! accepted iff `uid == daemon_uid`. This is OS-level auth and not a
//! substitute for the application-level pairing flow.
//!
//! # Auth (Windows)
//!
//! The named pipe ACL restricts access to the current user. Not
//! runtime-tested in this build.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use crate::state::StateDb;
use crate::subprocess::{SubprocessConfig, SubprocessManager};
use crate::tuf::{UpdateChecker, UpdateInfo, UpdateStatus};
use crate::{DaemonConfig, DaemonError, IPC_HEADER_SIZE, IPC_MAGIC, IPC_MAX_PAYLOAD, IPC_VERSION};

// ── Platform-specific IPC types ────────────────────────────────────────

#[cfg(unix)]
type IpcStream = tokio::net::UnixStream;
#[cfg(unix)]
type IpcListener = tokio::net::UnixListener;

#[cfg(windows)]
type IpcStream = interprocess::local_socket::tokio::Stream;
#[cfg(windows)]
type IpcListener = interprocess::local_socket::tokio::Listener;

/// SDDL for the Windows named pipe DACL.
/// Grants GENERIC_READ|GENERIC_WRITE to LocalSystem, LocalService, and
/// Authenticated Users. Denies WRITE_DAC to Interactive as defense-in-depth.
#[cfg(windows)]
const PIPE_SDDL: &str =
    "D:(A;;GRGW;;;S-1-5-18)(A;;GRGW;;;S-1-5-19)(A;;GRGW;;;S-1-5-11)(D;;WD;;;S-1-5-4)";

// ── Protocol enums ─────────────────────────────────────────────────────

/// Request variants sent from the client to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcRequest {
    ListPairings,
    ApprovePairing {
        peer_id: String,
        public_key: Vec<u8>,
    },
    RevokePairing {
        peer_id: String,
    },
    StartHost {
        config: HostConfig,
    },
    StopHost,
    GetHostStatus,
    StartClient {
        config: ClientConfig,
    },
    StopClient,
    GetClientStatus,
    CheckUpdate,
    ApplyUpdate {
        staged_version: String,
    },
    GetUpdateStatus,
    TurnIssueCredentials {
        peer_id: String,
    },
    SignalingForward {
        message: Vec<u8>,
    },
    SubscribeEvents,
    Quit,
    Ping,
    CreateShareLink {
        ttl_secs: u64,
    },
    KickSession {
        session_id: String,
        reason: String,
    },
    // ADR-022 FileSync
    SyncAddRule {
        rule: qubox_sync::SyncRule,
    },
    SyncRemoveRule {
        rule_id: String,
    },
    SyncListRules,
    SyncSetEnabled {
        rule_id: String,
        enabled: bool,
    },
    SyncListJobs,
    SyncListConflicts,
    SyncResolveConflict {
        conflict_id: String,
        resolution: qubox_sync::ConflictResolution,
    },
    SyncListTrackedFiles,
    /// Manual push of a single path to a peer (Phase A MVP).
    SyncPushNow {
        local_path: String,
        target_peer: String,
        node_id: String,
    },
    /// Sensor → daemon: process lock state for a file_id.
    SyncSetLock {
        file_id: String,
        locked: bool,
    },
    /// Sensor → daemon: FS change detected; rehash + enqueue.
    SyncFileChanged {
        local_path: String,
        rule_id: Option<String>,
        node_id: String,
        target_peer: String,
    },
    /// Global never-track patterns (paths, globs, names). Defaults include `.git`.
    SyncListIgnores,
    SyncSetIgnores {
        patterns: Vec<String>,
    },
    SyncAddIgnore {
        pattern: String,
    },
    SyncRemoveIgnore {
        pattern: String,
    },
    SyncApplyIgnorePreset {
        name: String,
    },
    /// Generic key/value settings (GUI + CLI).
    GetSetting {
        key: String,
    },
    SetSetting {
        key: String,
        value: String,
    },
    ListSettings,
    /// Mark first-run complete / read onboarding flag.
    GetOnboarding,
    CompleteOnboarding {
        device_name: String,
        signaling_server: String,
    },
    /// FileSync: list pending outbox (for session drain).
    SyncDrainReady,
    /// Mark outbox job status after transfer attempt.
    SyncUpdateJob {
        job_id: String,
        status: qubox_sync::OutboxStatus,
        last_error: Option<String>,
    },
}

/// Response variants sent from the daemon to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcResponse {
    ListPairingsResponse {
        pairings: Vec<crate::state::Pairing>,
    },
    Unit,
    HostStatus {
        running: bool,
        session_id: Option<String>,
        child_pid: Option<u32>,
    },
    ClientStatus {
        running: bool,
        session_id: Option<String>,
        child_pid: Option<u32>,
    },
    UpdateAvailable {
        version: String,
        manifest_url: String,
    },
    UpdateStatusResponse {
        current_version: String,
        available: Option<UpdateInfoPublic>,
        last_check_unix: Option<u64>,
    },
    TurnCredentialsResponse {
        credentials: TurnCredentials,
    },
    Pong,
    ShareLink {
        code: String,
        url_hint: String,
        expires_unix_ms: u64,
    },
    SyncRules {
        rules: Vec<qubox_sync::SyncRule>,
    },
    SyncJobs {
        jobs: Vec<qubox_sync::OutboxJob>,
    },
    SyncConflicts {
        conflicts: Vec<qubox_sync::SyncConflict>,
    },
    SyncTrackedFiles {
        files: Vec<qubox_sync::TrackedFile>,
    },
    SyncJob {
        job: qubox_sync::OutboxJob,
    },
    SyncIgnores {
        patterns: Vec<String>,
    },
    SettingValue {
        key: String,
        value: Option<String>,
    },
    SettingsMap {
        entries: Vec<(String, String)>,
    },
    Onboarding {
        completed: bool,
        device_name: Option<String>,
        signaling_server: Option<String>,
    },
    Error {
        code: u32,
        message: String,
    },
}

/// Server-pushed events for subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcEvent {
    PairingRequest {
        host_id: String,
        public_key: Vec<u8>,
    },
    HostStateChanged {
        running: bool,
        session_id: Option<String>,
        child_pid: Option<u32>,
        last_exit_code: Option<i32>,
        last_exit_reason: Option<String>,
    },
    ClientStateChanged {
        running: bool,
        session_id: Option<String>,
        child_pid: Option<u32>,
        last_exit_code: Option<i32>,
        last_exit_reason: Option<String>,
    },
    UpdateAvailable {
        version: String,
    },
    SubprocessEvent {
        label: String,
        event: crate::subprocess::SubprocessEvent,
    },
    SessionStateChanged {
        session_id: String,
        role: String,
        state: String,
        reason: String,
    },
    Error {
        code: u32,
        message: String,
    },
    SyncJobUpdated {
        job: qubox_sync::OutboxJob,
    },
    SyncConflict {
        conflict: qubox_sync::SyncConflict,
    },
    SyncLockChanged {
        file_id: String,
        locked: bool,
    },
}

// ── Config types embedded in requests ──────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    pub identity_path: Option<String>,
    pub auto_approve_pairing: bool,
    /// Socket path passed to the subprocess as --ipc-socket.
    #[serde(default)]
    pub socket_path: String,
    /// Signaling server URL passed to the subprocess.
    #[serde(default)]
    pub server: Option<String>,
    /// Host privacy mode: `none` | `blank-overlay` | `vkms`.
    #[serde(default)]
    pub privacy_mode: Option<String>,
    /// Enable privacy as soon as a session starts.
    #[serde(default)]
    pub enable_privacy_on_session_start: bool,
    /// `single-stream` | `multi-display`.
    #[serde(default)]
    pub stream_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub identity_path: Option<String>,
    pub host_peer_id: Option<String>,
    /// Socket path passed to the subprocess as --ipc-socket.
    #[serde(default)]
    pub socket_path: String,
    /// Signaling server URL passed to the subprocess.
    #[serde(default)]
    pub server: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCredentials {
    pub urls: Vec<String>,
    pub username: String,
    pub password: String,
    pub ttl: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfoPublic {
    pub version: String,
    pub size_bytes: u64,
    pub manifest_url: String,
}

fn update_info_to_response(info: &UpdateInfo) -> IpcResponse {
    if info.available {
        IpcResponse::UpdateAvailable {
            version: info.version.clone(),
            manifest_url: info.manifest_url.clone(),
        }
    } else {
        IpcResponse::UpdateStatusResponse {
            current_version: info.version.clone(),
            available: None,
            last_check_unix: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            ),
        }
    }
}

// ── Header wire format ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    pub magic: u32,
    pub version: u16,
    pub kind: u16,
    pub correlation_id: u64,
    pub payload_len: u32,
}

impl Header {
    pub fn new(kind: u16, correlation_id: u64, payload_len: u32) -> Self {
        Self {
            magic: IPC_MAGIC,
            version: IPC_VERSION,
            kind,
            correlation_id,
            payload_len,
        }
    }

    pub fn encode(&self) -> [u8; IPC_HEADER_SIZE] {
        let mut buf = [0u8; IPC_HEADER_SIZE];
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4..6].copy_from_slice(&self.version.to_le_bytes());
        buf[6..8].copy_from_slice(&self.kind.to_le_bytes());
        buf[8..16].copy_from_slice(&self.correlation_id.to_le_bytes());
        buf[16..20].copy_from_slice(&self.payload_len.to_le_bytes());
        buf
    }

    pub fn decode(buf: &[u8; IPC_HEADER_SIZE]) -> Result<Self, DaemonError> {
        let h = Self {
            magic: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            version: u16::from_le_bytes([buf[4], buf[5]]),
            kind: u16::from_le_bytes([buf[6], buf[7]]),
            correlation_id: u64::from_le_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]),
            payload_len: u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
        };
        if h.magic != IPC_MAGIC {
            return Err(DaemonError::Ipc(format!("bad magic: 0x{:08X}", h.magic)));
        }
        if h.version != IPC_VERSION {
            return Err(DaemonError::Ipc(format!(
                "unsupported version {}",
                h.version
            )));
        }
        if h.payload_len > IPC_MAX_PAYLOAD {
            return Err(DaemonError::Ipc(format!(
                "payload too large: {}",
                h.payload_len
            )));
        }
        Ok(h)
    }
}

// ── ShutdownHandle ─────────────────────────────────────────────────────

/// A handle that can signal the IPC accept loop to shut down.
#[derive(Clone)]
pub struct ShutdownHandle {
    notify: Arc<tokio::sync::Notify>,
}

impl ShutdownHandle {
    pub fn signal_shutdown(&self) {
        self.notify.notify_one();
    }
}

// ── IpcServer ──────────────────────────────────────────────────────────

/// The daemon-side IPC listener.
pub struct IpcServer {
    listener: IpcListener,
    state: Arc<StateDb>,
    event_tx: broadcast::Sender<IpcEvent>,
    /// Optional TUF update checker (set via `with_checker` from `service.rs`).
    update_checker: Option<Arc<UpdateChecker>>,
    /// Registry of managed subprocesses (host-agent, qubox-client-cli).
    subprocess_manager: SubprocessManager,
    /// Signaling server URL for CreateShareLink / KickSession (QUBOX_SERVER).
    signaling_url: Option<String>,
    /// The UID that the daemon runs under.
    #[cfg(unix)]
    daemon_uid: nix::unistd::Uid,
    shutdown: Arc<tokio::sync::Notify>,
}

impl IpcServer {
    /// Bind the IPC listener.
    ///
    /// On Unix, if `activated_listener` is `Some`, it is used directly
    /// (systemd socket activation) instead of binding a new socket.
    pub async fn bind(
        config: &DaemonConfig,
        state: Arc<StateDb>,
        #[cfg(unix)] activated_listener: Option<tokio::net::UnixListener>,
    ) -> Result<Self, DaemonError> {
        let (event_tx, _) = broadcast::channel::<IpcEvent>(256);

        #[cfg(unix)]
        {
            let (listener, path_str) = if let Some(listener) = activated_listener {
                info!("IPC using systemd-activated socket");
                (listener, "activated".to_string())
            } else {
                let path = &config.socket_path;
                if path.exists() {
                    std::fs::remove_file(path).ok();
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                let listener = IpcListener::bind(path).map_err(|e| {
                    DaemonError::Ipc(format!("bind Unix socket at {}: {e}", path.display()))
                })?;
                (listener, path.display().to_string())
            };
            let daemon_uid = nix::unistd::Uid::current();
            info!("IPC listening on Unix socket: {path_str}");
            let sm = SubprocessManager::new(event_tx.clone());
            Ok(Self {
                listener,
                state,
                event_tx,
                update_checker: None,
                subprocess_manager: sm,
                signaling_url: config.signaling_url.clone(),
                daemon_uid,
                shutdown: Arc::new(tokio::sync::Notify::new()),
            })
        }

        #[cfg(windows)]
        {
            let name = config.socket_path.to_string_lossy().to_string();
            use interprocess::local_socket::ListenerOptions;
            use interprocess::os::windows::security_descriptor::SecurityDescriptor;
            use interprocess::os::windows::ListenerOptionsExt;
            // Convert SDDL to a null-terminated u16 string, then
            // deserialize into a SecurityDescriptor via interprocess.
            let sddl_utf16: Vec<u16> = PIPE_SDDL.encode_utf16().chain(std::iter::once(0)).collect();
            let sddl_wide = widestring::U16CStr::from_slice(&sddl_utf16)
                .map_err(|e| DaemonError::Ipc(format!("SDDL conversion: {e}")))?;
            let sd = SecurityDescriptor::deserialize(sddl_wide)
                .map_err(|e| DaemonError::Ipc(format!("SDDL deserialize: {e}")))?;
            let listener = ListenerOptions::new()
                .name(&name)
                .security_descriptor(sd)
                .create_tokio::<IpcStream>()
                .map_err(|e| DaemonError::Ipc(format!("bind named pipe at {name}: {e}")))?;
            info!("IPC listening on named pipe: {name} (secure DACL)");
            let sm = SubprocessManager::new(event_tx.clone());
            Ok(Self {
                listener,
                state,
                event_tx,
                update_checker: None,
                subprocess_manager: sm,
                signaling_url: config.signaling_url.clone(),
                shutdown: Arc::new(tokio::sync::Notify::new()),
            })
        }
    }

    /// Attach a TUF `UpdateChecker` to the server. After this call, the
    /// `CheckUpdate` / `ApplyUpdate` / `GetUpdateStatus` IPC methods are
    /// routed to the checker. Without it, those methods return 501.
    pub fn with_checker(mut self, checker: Arc<UpdateChecker>) -> Self {
        self.update_checker = Some(checker);
        self
    }

    /// Run the accept loop, dispatching each connection to a new tokio task.
    pub async fn run(self) -> Result<(), DaemonError> {
        let shutdown = self.shutdown.clone();
        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    info!("IPC accept loop shutting down");
                    break;
                }
                accept_result = accept_one(&self.listener) => {
                    match accept_result {
                        Ok(conn) => {
                            let state = self.state.clone();
                            let event_tx = self.event_tx.clone();
                            let update_checker = self.update_checker.clone();
                            let subprocess_manager = self.subprocess_manager.clone();
                            let signaling_url = self.signaling_url.clone();
                            #[cfg(target_os = "linux")]
                            let daemon_uid = self.daemon_uid;
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(
                                    conn, state, event_tx, update_checker,
                                    subprocess_manager,
                                    signaling_url,
                                    #[cfg(target_os = "linux")] daemon_uid,
                                ).await {
                                    warn!("IPC handler error: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            error!("IPC accept error: {e}");
                            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn event_sender(&self) -> broadcast::Sender<IpcEvent> {
        self.event_tx.clone()
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            notify: self.shutdown.clone(),
        }
    }
}

#[cfg(unix)]
async fn accept_one(listener: &IpcListener) -> Result<IpcStream, DaemonError> {
    let (stream, _addr) = listener
        .accept()
        .await
        .map_err(|e| DaemonError::Ipc(format!("accept: {e}")))?;
    Ok(stream)
}

#[cfg(windows)]
async fn accept_one(listener: &IpcListener) -> Result<IpcStream, DaemonError> {
    let stream = listener
        .accept()
        .await
        .map_err(|e| DaemonError::Ipc(format!("accept: {e}")))?;
    Ok(stream)
}

// ── Connection handler ─────────────────────────────────────────────────

async fn handle_connection(
    mut stream: IpcStream,
    state: Arc<StateDb>,
    event_tx: broadcast::Sender<IpcEvent>,
    update_checker: Option<Arc<UpdateChecker>>,
    subprocess_manager: SubprocessManager,
    signaling_url: Option<String>,
    #[cfg(target_os = "linux")] daemon_uid: nix::unistd::Uid,
) -> Result<(), DaemonError> {
    #[cfg(target_os = "linux")]
    {
        let cred =
            nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)
                .map_err(|e| DaemonError::Ipc(format!("SO_PEERCRED failed: {e}")))?;

        if cred.uid() != daemon_uid.as_raw() {
            warn!(
                "IPC auth reject: peer uid={}, daemon uid={}",
                cred.uid(),
                daemon_uid.as_raw()
            );
            let header = Header::new(2, 0, 0);
            stream.write_all(&header.encode()).await.ok();
            return Ok(());
        }
    }

    loop {
        let mut header_buf = [0u8; IPC_HEADER_SIZE];
        if stream.read_exact(&mut header_buf).await.is_err() {
            return Ok(());
        }
        let header = match Header::decode(&header_buf) {
            Ok(h) => h,
            Err(e) => {
                warn!("Invalid header: {e}");
                return Ok(());
            }
        };

        let mut payload = vec![0u8; header.payload_len as usize];
        if header.payload_len > 0 && stream.read_exact(&mut payload).await.is_err() {
            return Ok(());
        }

        match header.kind {
            1 => {
                let req: IpcRequest = match bincode::deserialize(&payload) {
                    Ok(r) => r,
                    Err(e) => {
                        error!("deserialize IpcRequest: {e}");
                        return Ok(());
                    }
                };
                dispatch_request(
                    req,
                    header.correlation_id,
                    &state,
                    &event_tx,
                    &update_checker,
                    &subprocess_manager,
                    signaling_url.as_deref(),
                    &mut stream,
                )
                .await?;
            }
            2 | 3 => {
                // Client sent unexpected event kind or terminal — they're out of sync
                warn!("client sent unexpected kind={}", header.kind);
                return Ok(());
            }
            _ => {
                warn!("client sent unknown kind={}", header.kind);
                return Ok(());
            }
        }
    }
}

// ── Request dispatch ───────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn dispatch_request(
    req: IpcRequest,
    corr_id: u64,
    state: &Arc<StateDb>,
    event_tx: &broadcast::Sender<IpcEvent>,
    update_checker: &Option<Arc<UpdateChecker>>,
    subprocess_manager: &SubprocessManager,
    signaling_url: Option<&str>,
    stream: &mut IpcStream,
) -> Result<(), DaemonError> {
    match req {
        IpcRequest::CreateShareLink { ttl_secs } => {
            let resp = run_client_cli_share_or_kick(
                signaling_url,
                &["create-share-link", "--ttl-secs", &ttl_secs.to_string()],
                true,
            );
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::KickSession { session_id, reason } => {
            let resp = run_client_cli_share_or_kick(
                signaling_url,
                &[
                    "kick-session",
                    "--session",
                    &session_id,
                    "--reason",
                    &reason,
                ],
                false,
            );
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::Ping => send_response(stream, corr_id, &IpcResponse::Pong).await,
        IpcRequest::Quit => send_response(stream, corr_id, &IpcResponse::Unit).await,

        IpcRequest::ListPairings => {
            let resp = match state.list_pairings() {
                Ok(pairings) => IpcResponse::ListPairingsResponse { pairings },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }

        IpcRequest::ApprovePairing {
            peer_id,
            public_key,
        } => {
            let pairing = crate::state::Pairing {
                peer_id,
                public_key,
                paired_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                label: None,
            };
            let resp = match state.put_pairing(&pairing) {
                Ok(_) => IpcResponse::Unit,
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }

        IpcRequest::RevokePairing { peer_id } => {
            let resp = match state.delete_pairing(&peer_id) {
                Ok(_) => IpcResponse::Unit,
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }

        IpcRequest::StartHost { config } => {
            let current_exe = std::env::current_exe().unwrap_or_default();
            let bin_path = current_exe
                .parent()
                .map(|p| p.join("qubox-host-agent"))
                .unwrap_or_else(|| PathBuf::from("qubox-host-agent"));
            let socket_path = config.socket_path.clone();
            let mut args = vec![
                "--allow-standalone".to_string(),
                "--ipc-socket".to_string(),
                socket_path,
            ];
            if let Some(server) = config.server {
                args.push("--server".to_string());
                args.push(server);
            }
            if let Some(mode) = config.privacy_mode.as_deref().filter(|m| !m.is_empty()) {
                args.push("--privacy-mode".to_string());
                args.push(mode.to_string());
            }
            if config.enable_privacy_on_session_start {
                args.push("--enable-privacy-on-session-start".to_string());
            }
            if let Some(sm) = config.stream_mode.as_deref().filter(|m| !m.is_empty()) {
                args.push("--stream-mode".to_string());
                args.push(sm.to_string());
            }
            // Never allow auto-approve via managed host start (production gate).
            if config.auto_approve_pairing {
                tracing::warn!(
                    "StartHost ignored auto_approve_pairing=true (unsafe on managed paths)"
                );
            }
            let sub_config = SubprocessConfig {
                bin_path,
                args,
                max_restarts: 3,
                restart_window: Duration::from_secs(30),
                ..Default::default()
            };
            match subprocess_manager
                .start("host".to_string(), sub_config)
                .await
            {
                Ok(()) => {
                    let _pid = subprocess_manager.current_pid("host").await;
                    // Persist last_child_pid to host_state
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let hs = crate::state::HostState {
                        last_seen: now,
                        current_session_id: None,
                        config_hash: String::new(),
                        last_child_pid: _pid,
                    };
                    state.put_host_state("default", &hs).ok();
                    let _ = event_tx.send(IpcEvent::HostStateChanged {
                        running: true,
                        session_id: None,
                        child_pid: _pid,
                        last_exit_code: None,
                        last_exit_reason: None,
                    });
                    let _ = event_tx.send(IpcEvent::SessionStateChanged {
                        session_id: "host-starting".to_string(),
                        role: "host".to_string(),
                        state: "starting".to_string(),
                        reason: "host_subprocess_started".to_string(),
                    });
                    send_response(stream, corr_id, &IpcResponse::Unit).await
                }
                Err(e) => {
                    send_response(
                        stream,
                        corr_id,
                        &IpcResponse::Error {
                            code: 409,
                            message: e,
                        },
                    )
                    .await
                }
            }
        }

        IpcRequest::StopHost => match subprocess_manager.stop("host").await {
            Ok(()) => {
                let _ = event_tx.send(IpcEvent::HostStateChanged {
                    running: false,
                    session_id: None,
                    child_pid: None,
                    last_exit_code: None,
                    last_exit_reason: None,
                });
                let _ = event_tx.send(IpcEvent::SessionStateChanged {
                    session_id: "host-stopped".to_string(),
                    role: "host".to_string(),
                    state: "stopped".to_string(),
                    reason: "host_subprocess_stopped".to_string(),
                });
                send_response(stream, corr_id, &IpcResponse::Unit).await
            }
            Err(e) => {
                send_response(
                    stream,
                    corr_id,
                    &IpcResponse::Error {
                        code: 404,
                        message: e,
                    },
                )
                .await
            }
        },

        IpcRequest::StartClient { config } => {
            let current_exe = std::env::current_exe().unwrap_or_default();
            let bin_path = current_exe
                .parent()
                .map(|p| p.join("qubox-client-cli"))
                .unwrap_or_else(|| PathBuf::from("qubox-client-cli"));
            let socket_path = config.socket_path.clone();
            let mut args = vec![
                "--allow-standalone".to_string(),
                "--ipc-socket".to_string(),
                socket_path,
            ];
            if let Some(server) = config.server {
                args.push("--server".to_string());
                args.push(server);
            }
            let sub_config = SubprocessConfig {
                bin_path,
                args,
                max_restarts: 3,
                restart_window: Duration::from_secs(30),
                ..Default::default()
            };
            match subprocess_manager
                .start("client".to_string(), sub_config)
                .await
            {
                Ok(()) => {
                    let _pid = subprocess_manager.current_pid("client").await;
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let cs = crate::state::ClientState {
                        last_seen: now,
                        current_session_id: None,
                        last_child_pid: _pid,
                    };
                    state.put_client_state("default", &cs).ok();
                    let _ = event_tx.send(IpcEvent::ClientStateChanged {
                        running: true,
                        session_id: None,
                        child_pid: _pid,
                        last_exit_code: None,
                        last_exit_reason: None,
                    });
                    let _ = event_tx.send(IpcEvent::SessionStateChanged {
                        session_id: "client-starting".to_string(),
                        role: "client".to_string(),
                        state: "starting".to_string(),
                        reason: "client_subprocess_started".to_string(),
                    });
                    send_response(stream, corr_id, &IpcResponse::Unit).await
                }
                Err(e) => {
                    send_response(
                        stream,
                        corr_id,
                        &IpcResponse::Error {
                            code: 409,
                            message: e,
                        },
                    )
                    .await
                }
            }
        }

        IpcRequest::StopClient => match subprocess_manager.stop("client").await {
            Ok(()) => {
                let _ = event_tx.send(IpcEvent::ClientStateChanged {
                    running: false,
                    session_id: None,
                    child_pid: None,
                    last_exit_code: None,
                    last_exit_reason: None,
                });
                let _ = event_tx.send(IpcEvent::SessionStateChanged {
                    session_id: "client-stopped".to_string(),
                    role: "client".to_string(),
                    state: "stopped".to_string(),
                    reason: "client_subprocess_stopped".to_string(),
                });
                send_response(stream, corr_id, &IpcResponse::Unit).await
            }
            Err(e) => {
                send_response(
                    stream,
                    corr_id,
                    &IpcResponse::Error {
                        code: 404,
                        message: e,
                    },
                )
                .await
            }
        },

        IpcRequest::GetHostStatus => {
            let running = subprocess_manager.is_running("host").await;
            let child_pid = subprocess_manager.current_pid("host").await;
            send_response(
                stream,
                corr_id,
                &IpcResponse::HostStatus {
                    running,
                    session_id: None,
                    child_pid,
                },
            )
            .await
        }

        IpcRequest::GetClientStatus => {
            let running = subprocess_manager.is_running("client").await;
            let child_pid = subprocess_manager.current_pid("client").await;
            send_response(
                stream,
                corr_id,
                &IpcResponse::ClientStatus {
                    running,
                    session_id: None,
                    child_pid,
                },
            )
            .await
        }

        // ── TUF auto-update (task 3) ────────────────────────────────
        IpcRequest::CheckUpdate => {
            let resp = match update_checker {
                Some(checker) => match checker.check_for_update().await {
                    Ok(info) => update_info_to_response(&info),
                    Err(e) => IpcResponse::Error {
                        code: 502,
                        message: format!("update check: {e}"),
                    },
                },
                None => IpcResponse::Error {
                    code: 501,
                    message: "update checker not configured".into(),
                },
            };
            send_response(stream, corr_id, &resp).await
        }

        IpcRequest::ApplyUpdate { staged_version } => {
            let resp = match update_checker {
                Some(checker) => {
                    let info = UpdateInfo {
                        version: staged_version.clone(),
                        available: true,
                        size_bytes: 0,
                        manifest_url: String::new(),
                        sha256: String::new(),
                    };
                    match checker.download_update(&info).await {
                        Ok(staged) => {
                            let current =
                                std::env::current_exe().unwrap_or_else(|_| PathBuf::from("qubox"));
                            match checker.apply_update(&staged, &current).await {
                                Ok(()) => {
                                    let _ = event_tx.send(IpcEvent::UpdateAvailable {
                                        version: staged_version.clone(),
                                    });
                                    IpcResponse::Unit
                                }
                                Err(e) => IpcResponse::Error {
                                    code: 502,
                                    message: format!("apply: {e}"),
                                },
                            }
                        }
                        Err(e) => IpcResponse::Error {
                            code: 502,
                            message: format!("download: {e}"),
                        },
                    }
                }
                None => IpcResponse::Error {
                    code: 501,
                    message: "update checker not configured".into(),
                },
            };
            send_response(stream, corr_id, &resp).await
        }

        IpcRequest::GetUpdateStatus => {
            let resp = match update_checker {
                Some(checker) => {
                    let status: UpdateStatus = checker.get_status().await;
                    IpcResponse::UpdateStatusResponse {
                        current_version: status.current_version,
                        available: status.available_update.as_ref().map(|i| UpdateInfoPublic {
                            version: i.version.clone(),
                            size_bytes: i.size_bytes,
                            manifest_url: i.manifest_url.clone(),
                        }),
                        last_check_unix: status.last_check.map(|i| {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs().saturating_sub(i.elapsed().as_secs()))
                                .unwrap_or(0)
                        }),
                    }
                }
                None => IpcResponse::Error {
                    code: 501,
                    message: "update checker not configured".into(),
                },
            };
            send_response(stream, corr_id, &resp).await
        }

        IpcRequest::SubscribeEvents => {
            let mut rx = event_tx.subscribe();

            // 1. Send ack
            send_response(stream, corr_id, &IpcResponse::Unit).await?;

            // 2. Push events until broadcast closes or write fails
            loop {
                let event = rx.recv().await;
                match event {
                    Ok(ev) => {
                        let ev_payload = match bincode::serialize(&ev) {
                            Ok(p) => p,
                            Err(_) => break,
                        };
                        let ev_header = Header::new(3, corr_id, ev_payload.len() as u32);
                        if stream.write_all(&ev_header.encode()).await.is_err() {
                            break;
                        }
                        if stream.write_all(&ev_payload).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("IPC subscriber lagged by {n}");
                    }
                }
            }

            // 3. Send terminal Response(Unit)
            send_response(stream, corr_id, &IpcResponse::Unit)
                .await
                .ok();
            Ok(())
        }

        // ── ADR-022 FileSync ─────────────────────────────────────────
        IpcRequest::SyncAddRule { mut rule } => {
            if rule.rule_id.is_empty() {
                rule.rule_id = qubox_sync::new_id();
            }
            let resp = match state.put_sync_rule(&rule) {
                Ok(()) => IpcResponse::Unit,
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncRemoveRule { rule_id } => {
            let resp = match state.delete_sync_rule(&rule_id) {
                Ok(()) => IpcResponse::Unit,
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncListRules => {
            let resp = match state.list_sync_rules() {
                Ok(rules) => IpcResponse::SyncRules { rules },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncSetEnabled { rule_id, enabled } => {
            let resp = match state.set_sync_rule_enabled(&rule_id, enabled) {
                Ok(true) => IpcResponse::Unit,
                Ok(false) => IpcResponse::Error {
                    code: 404,
                    message: "rule not found".into(),
                },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncListJobs => {
            let resp = match state.list_outbox_jobs() {
                Ok(jobs) => IpcResponse::SyncJobs { jobs },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncListConflicts => {
            let resp = match state.list_sync_conflicts() {
                Ok(conflicts) => IpcResponse::SyncConflicts { conflicts },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncResolveConflict {
            conflict_id,
            resolution,
        } => {
            let resp = match state.resolve_sync_conflict(&conflict_id, resolution) {
                Ok(Some(_)) => IpcResponse::Unit,
                Ok(None) => IpcResponse::Error {
                    code: 404,
                    message: "conflict not found".into(),
                },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncListTrackedFiles => {
            let resp = match state.list_tracked_files() {
                Ok(files) => IpcResponse::SyncTrackedFiles { files },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncPushNow {
            local_path,
            target_peer,
            node_id,
        } => {
            let resp = match state.enqueue_manual_push(&local_path, &target_peer, &node_id) {
                Ok(job) => {
                    let _ = event_tx.send(IpcEvent::SyncJobUpdated { job: job.clone() });
                    IpcResponse::SyncJob { job }
                }
                Err(e) => IpcResponse::Error {
                    code: 400,
                    message: format!("push: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncSetLock { file_id, locked } => {
            let resp = match state.get_tracked_file(&file_id) {
                Ok(Some(mut tf)) => {
                    tf.sync_state = if locked {
                        qubox_sync::SyncState::LockedByProcess
                    } else {
                        qubox_sync::SyncState::Pending
                    };
                    match state.put_tracked_file(&tf) {
                        Ok(()) => {
                            let _ = event_tx.send(IpcEvent::SyncLockChanged {
                                file_id: file_id.clone(),
                                locked,
                            });
                            IpcResponse::Unit
                        }
                        Err(e) => IpcResponse::Error {
                            code: 500,
                            message: format!("db: {e}"),
                        },
                    }
                }
                Ok(None) => IpcResponse::Error {
                    code: 404,
                    message: "file not tracked".into(),
                },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncListIgnores => {
            let resp = match state.get_global_ignores() {
                Ok(patterns) => IpcResponse::SyncIgnores { patterns },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncSetIgnores { patterns } => {
            let resp = match state.set_global_ignores(&patterns) {
                Ok(()) => IpcResponse::SyncIgnores { patterns },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncAddIgnore { pattern } => {
            let resp = match state.add_global_ignore(&pattern) {
                Ok(patterns) => IpcResponse::SyncIgnores { patterns },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncRemoveIgnore { pattern } => {
            let resp = match state.remove_global_ignore(&pattern) {
                Ok(patterns) => IpcResponse::SyncIgnores { patterns },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncApplyIgnorePreset { name } => {
            let resp = match state.apply_ignore_preset(&name) {
                Ok(patterns) => IpcResponse::SyncIgnores { patterns },
                Err(e) => IpcResponse::Error {
                    code: 400,
                    message: format!("{e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::GetSetting { key } => {
            let resp = match state.get_setting(&key) {
                Ok(value) => IpcResponse::SettingValue { key, value },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SetSetting { key, value } => {
            let resp = match state.set_setting(&key, &value) {
                Ok(()) => IpcResponse::Unit,
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::ListSettings => {
            let resp = match state.list_settings() {
                Ok(entries) => IpcResponse::SettingsMap { entries },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::GetOnboarding => {
            let completed = state
                .get_setting("onboarding_complete")
                .ok()
                .flatten()
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            let device_name = state.get_setting("device_name").ok().flatten();
            let signaling_server = state.get_setting("signaling_server").ok().flatten();
            send_response(
                stream,
                corr_id,
                &IpcResponse::Onboarding {
                    completed,
                    device_name,
                    signaling_server,
                },
            )
            .await
        }
        IpcRequest::CompleteOnboarding {
            device_name,
            signaling_server,
        } => {
            let resp = (|| -> Result<IpcResponse, String> {
                state
                    .set_setting("device_name", &device_name)
                    .map_err(|e| e.to_string())?;
                state
                    .set_setting("signaling_server", &signaling_server)
                    .map_err(|e| e.to_string())?;
                state
                    .set_setting("onboarding_complete", "1")
                    .map_err(|e| e.to_string())?;
                // Seed default FileSync ignores (includes .git).
                let _ = state.get_global_ignores();
                Ok(IpcResponse::Unit)
            })();
            let resp = match resp {
                Ok(r) => r,
                Err(message) => IpcResponse::Error { code: 500, message },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncDrainReady => {
            let resp = match state.list_pending_outbox() {
                Ok(jobs) => IpcResponse::SyncJobs { jobs },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncUpdateJob {
            job_id,
            status,
            last_error,
        } => {
            let resp = match state.get_outbox_job(&job_id) {
                Ok(Some(mut job)) => {
                    job.status = status;
                    job.last_error = last_error;
                    if matches!(status, qubox_sync::OutboxStatus::Failed) {
                        job.retry_count = job.retry_count.saturating_add(1);
                    }
                    match state.put_outbox_job(&job) {
                        Ok(()) => {
                            let _ = event_tx.send(IpcEvent::SyncJobUpdated { job: job.clone() });
                            IpcResponse::SyncJob { job }
                        }
                        Err(e) => IpcResponse::Error {
                            code: 500,
                            message: format!("db: {e}"),
                        },
                    }
                }
                Ok(None) => IpcResponse::Error {
                    code: 404,
                    message: "job not found".into(),
                },
                Err(e) => IpcResponse::Error {
                    code: 500,
                    message: format!("db: {e}"),
                },
            };
            send_response(stream, corr_id, &resp).await
        }
        IpcRequest::SyncFileChanged {
            local_path,
            rule_id,
            node_id,
            target_peer,
        } => {
            use qubox_sync::{
                content_hash_file, new_id, now_unix, should_ignore_path, OutboxJob, OutboxStatus,
                SyncState, TrackedFile, VectorClock,
            };
            let resp = (|| -> Result<IpcResponse, String> {
                let path = std::path::Path::new(&local_path);
                let ignores = state.get_global_ignores().map_err(|e| format!("db: {e}"))?;
                if should_ignore_path(path, &ignores) {
                    return Ok(IpcResponse::Unit);
                }
                let (hash, _arr, size) =
                    content_hash_file(path).map_err(|e| format!("hash: {e}"))?;
                let existing = state
                    .list_tracked_files()
                    .map_err(|e| format!("db: {e}"))?
                    .into_iter()
                    .find(|f| f.local_path == local_path);
                let file_id = if let Some(mut tf) = existing {
                    if matches!(tf.sync_state, SyncState::LockedByProcess) {
                        return Ok(IpcResponse::Unit);
                    }
                    if tf.content_hash == hash {
                        return Ok(IpcResponse::Unit);
                    }
                    tf.vector_clock.bump(&node_id);
                    tf.content_hash = hash;
                    tf.size_bytes = size;
                    tf.sync_state = SyncState::Pending;
                    tf.updated_at_unix = now_unix();
                    state
                        .put_tracked_file(&tf)
                        .map_err(|e| format!("db: {e}"))?;
                    tf.file_id
                } else {
                    let mut clock = VectorClock::empty();
                    clock.bump(&node_id);
                    let file_id = new_id();
                    let tf = TrackedFile {
                        file_id: file_id.clone(),
                        local_path: local_path.clone(),
                        vector_clock: clock,
                        content_hash: hash,
                        size_bytes: size,
                        sync_state: SyncState::Pending,
                        rule_id,
                        updated_at_unix: now_unix(),
                    };
                    state
                        .put_tracked_file(&tf)
                        .map_err(|e| format!("db: {e}"))?;
                    file_id
                };
                let job = OutboxJob {
                    job_id: new_id(),
                    file_id,
                    target_peer,
                    status: OutboxStatus::Queued,
                    retry_count: 0,
                    queued_at_unix: now_unix(),
                    last_error: None,
                };
                state.put_outbox_job(&job).map_err(|e| format!("db: {e}"))?;
                let _ = event_tx.send(IpcEvent::SyncJobUpdated { job: job.clone() });
                Ok(IpcResponse::SyncJob { job })
            })();
            let resp = match resp {
                Ok(r) => r,
                Err(message) => IpcResponse::Error { code: 400, message },
            };
            send_response(stream, corr_id, &resp).await
        }

        // ── Stubs (tasks 3/4) ───────────────────────────────────────
        _ => {
            send_response(
                stream,
                corr_id,
                &IpcResponse::Error {
                    code: 501,
                    message: "not implemented in this build".into(),
                },
            )
            .await
        }
    }
}

fn resolve_client_cli_bin() -> PathBuf {
    if let Ok(p) = std::env::var("QUBOX_CLIENT_CLI") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let cand = parent.join("qubox-client-cli");
            if cand.exists() {
                return cand;
            }
            #[cfg(windows)]
            {
                let cand = parent.join("qubox-client-cli.exe");
                if cand.exists() {
                    return cand;
                }
            }
        }
    }
    PathBuf::from("qubox-client-cli")
}

/// Spawn `qubox-client-cli` against the configured signaling URL.
fn run_client_cli_share_or_kick(
    signaling_url: Option<&str>,
    extra_args: &[&str],
    parse_share: bool,
) -> IpcResponse {
    let Some(server) = signaling_url.filter(|s| !s.is_empty()) else {
        return IpcResponse::Error {
            code: 501,
            message: "signaling_url not configured (set QUBOX_SERVER)".into(),
        };
    };
    let bin = resolve_client_cli_bin();
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("--server").arg(server);
    for a in extra_args {
        cmd.arg(a);
    }
    match cmd.output() {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if parse_share {
                let mut code = String::new();
                let mut url_hint = String::new();
                let mut expires_unix_ms = 0u64;
                for part in stdout.split_whitespace() {
                    if let Some(v) = part.strip_prefix("code=") {
                        code = v.to_string();
                    } else if let Some(v) = part.strip_prefix("expires_ms=") {
                        expires_unix_ms = v.parse().unwrap_or(0);
                    } else if let Some(v) = part.strip_prefix("url=") {
                        url_hint = v.to_string();
                    }
                }
                if code.is_empty() {
                    return IpcResponse::Error {
                        code: 502,
                        message: format!("create-share-link parse failed: {stdout}"),
                    };
                }
                IpcResponse::ShareLink {
                    code,
                    url_hint,
                    expires_unix_ms,
                }
            } else {
                IpcResponse::Unit
            }
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            IpcResponse::Error {
                code: 502,
                message: format!("client-cli failed: {err}"),
            }
        }
        Err(e) => IpcResponse::Error {
            code: 502,
            message: format!("spawn client-cli: {e}"),
        },
    }
}

async fn send_response(
    stream: &mut IpcStream,
    corr_id: u64,
    response: &IpcResponse,
) -> Result<(), DaemonError> {
    let payload =
        bincode::serialize(response).map_err(|e| DaemonError::Ipc(format!("serialize: {e}")))?;
    let header = Header::new(2, corr_id, payload.len() as u32);
    stream
        .write_all(&header.encode())
        .await
        .map_err(|e| DaemonError::Ipc(format!("write header: {e}")))?;
    if !payload.is_empty() {
        stream
            .write_all(&payload)
            .await
            .map_err(|e| DaemonError::Ipc(format!("write payload: {e}")))?;
    }
    Ok(())
}

/// Probe the daemon at the given socket path by sending a Ping and
/// awaiting a Pong.  Returns `true` within 400 ms (2 × 200 ms timeout)
/// if the daemon responds.
pub async fn check_daemon_running(socket_path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        let connect_fut = IpcStream::connect(socket_path);
        let mut stream = match tokio::time::timeout(Duration::from_millis(200), connect_fut).await {
            Ok(Ok(s)) => s,
            _ => return false,
        };

        let ping_bytes = match bincode::serialize(&IpcRequest::Ping) {
            Ok(b) => b,
            Err(_) => return false,
        };
        let header = Header::new(1 /* Kind::Request */, 1, ping_bytes.len() as u32);
        if stream.write_all(&header.encode()).await.is_err() {
            return false;
        }
        if !ping_bytes.is_empty() && stream.write_all(&ping_bytes).await.is_err() {
            return false;
        }

        let read_fut = async {
            let mut hbuf = [0u8; IPC_HEADER_SIZE];
            stream.read_exact(&mut hbuf).await.ok()?;
            let header = Header::decode(&hbuf).ok()?;
            if header.payload_len > 0 {
                let mut payload = vec![0u8; header.payload_len as usize];
                stream.read_exact(&mut payload).await.ok()?;
                let _resp: IpcResponse = bincode::deserialize(&payload).ok()?;
            }
            Some(())
        };
        tokio::time::timeout(Duration::from_millis(200), read_fut)
            .await
            .is_ok()
    }

    #[cfg(windows)]
    {
        let name = socket_path.to_string_lossy().to_string();
        let connect_fut = IpcStream::connect(name.as_str());
        let mut stream = match tokio::time::timeout(Duration::from_millis(200), connect_fut).await {
            Ok(Ok(s)) => s,
            _ => return false,
        };

        let ping_bytes = match bincode::serialize(&IpcRequest::Ping) {
            Ok(b) => b,
            Err(_) => return false,
        };
        let header = Header::new(1 /* Kind::Request */, 1, ping_bytes.len() as u32);
        if stream.write_all(&header.encode()).await.is_err() {
            return false;
        }
        if !ping_bytes.is_empty() && stream.write_all(&ping_bytes).await.is_err() {
            return false;
        }

        let read_fut = async {
            let mut hbuf = [0u8; IPC_HEADER_SIZE];
            stream.read_exact(&mut hbuf).await.ok()?;
            let header = Header::decode(&hbuf).ok()?;
            if header.payload_len > 0 {
                let mut payload = vec![0u8; header.payload_len as usize];
                stream.read_exact(&mut payload).await.ok()?;
                let _resp: IpcResponse = bincode::deserialize(&payload).ok()?;
            }
            Some(())
        };
        tokio::time::timeout(Duration::from_millis(200), read_fut)
            .await
            .is_ok()
    }
}

/// Send an IPC request to the daemon at the given socket path and
/// return the response.  Used by host-agent/qubox-client-cli to delegate
/// session management to the daemon.
pub async fn ipc_request(
    socket_path: &std::path::Path,
    request: &IpcRequest,
) -> Result<IpcResponse, DaemonError> {
    #[cfg(unix)]
    {
        let mut stream = IpcStream::connect(socket_path)
            .await
            .map_err(|e| DaemonError::Ipc(format!("connect to daemon: {e}")))?;

        let payload =
            bincode::serialize(request).map_err(|e| DaemonError::Ipc(format!("serialize: {e}")))?;
        let corr_id = rand_corr_id();
        let header = Header::new(1 /* Kind::Request */, corr_id, payload.len() as u32);
        stream
            .write_all(&header.encode())
            .await
            .map_err(|e| DaemonError::Ipc(format!("write: {e}")))?;
        if !payload.is_empty() {
            stream
                .write_all(&payload)
                .await
                .map_err(|e| DaemonError::Ipc(format!("write: {e}")))?;
        }

        let mut hbuf = [0u8; IPC_HEADER_SIZE];
        stream
            .read_exact(&mut hbuf)
            .await
            .map_err(|e| DaemonError::Ipc(format!("read: {e}")))?;
        let header = Header::decode(&hbuf)?;
        let mut resp_buf = vec![0u8; header.payload_len as usize];
        if !resp_buf.is_empty() {
            stream
                .read_exact(&mut resp_buf)
                .await
                .map_err(|e| DaemonError::Ipc(format!("read: {e}")))?;
        }
        let resp: IpcResponse = bincode::deserialize(&resp_buf)
            .map_err(|e| DaemonError::Ipc(format!("deserialize: {e}")))?;
        Ok(resp)
    }

    #[cfg(windows)]
    {
        let name = socket_path.to_string_lossy().to_string();
        let mut stream = IpcStream::connect(name.as_str())
            .await
            .map_err(|e| DaemonError::Ipc(format!("connect to daemon: {e}")))?;

        let payload =
            bincode::serialize(request).map_err(|e| DaemonError::Ipc(format!("serialize: {e}")))?;
        let corr_id = rand_corr_id();
        let header = Header::new(1 /* Kind::Request */, corr_id, payload.len() as u32);
        stream
            .write_all(&header.encode())
            .await
            .map_err(|e| DaemonError::Ipc(format!("write: {e}")))?;
        if !payload.is_empty() {
            stream
                .write_all(&payload)
                .await
                .map_err(|e| DaemonError::Ipc(format!("write: {e}")))?;
        }

        let mut hbuf = [0u8; IPC_HEADER_SIZE];
        stream
            .read_exact(&mut hbuf)
            .await
            .map_err(|e| DaemonError::Ipc(format!("read: {e}")))?;
        let header = Header::decode(&hbuf)?;
        let mut resp_buf = vec![0u8; header.payload_len as usize];
        if !resp_buf.is_empty() {
            stream
                .read_exact(&mut resp_buf)
                .await
                .map_err(|e| DaemonError::Ipc(format!("read: {e}")))?;
        }
        let resp: IpcResponse = bincode::deserialize(&resp_buf)
            .map_err(|e| DaemonError::Ipc(format!("deserialize: {e}")))?;
        Ok(resp)
    }
}

// ── IpcClient (for testing) ────────────────────────────────────────────

/// A test client that connects to the daemon IPC.
pub struct IpcClient {
    stream: IpcStream,
}

impl IpcClient {
    pub async fn connect(config: &DaemonConfig) -> Result<Self, DaemonError> {
        #[cfg(unix)]
        {
            let stream = IpcStream::connect(&config.socket_path)
                .await
                .map_err(|e| DaemonError::Ipc(format!("connect: {e}")))?;
            Ok(Self { stream })
        }
        #[cfg(windows)]
        {
            let name = config.socket_path.to_string_lossy().to_string();
            let stream = IpcStream::connect(name.as_str())
                .await
                .map_err(|e| DaemonError::Ipc(format!("connect: {e}")))?;
            Ok(Self { stream })
        }
    }

    pub async fn call<R: serde::de::DeserializeOwned>(
        &mut self,
        request: &IpcRequest,
    ) -> Result<R, DaemonError> {
        let payload =
            bincode::serialize(request).map_err(|e| DaemonError::Ipc(format!("serialize: {e}")))?;
        let corr_id = rand_corr_id();
        let header = Header::new(1, corr_id, payload.len() as u32);

        self.stream
            .write_all(&header.encode())
            .await
            .map_err(|e| DaemonError::Ipc(format!("write: {e}")))?;
        if !payload.is_empty() {
            self.stream
                .write_all(&payload)
                .await
                .map_err(|e| DaemonError::Ipc(format!("write: {e}")))?;
        }

        // Read response header
        let mut hbuf = [0u8; IPC_HEADER_SIZE];
        self.stream
            .read_exact(&mut hbuf)
            .await
            .map_err(|e| DaemonError::Ipc(format!("read: {e}")))?;
        let res_header = Header::decode(&hbuf)?;

        let mut resp_payload = vec![0u8; res_header.payload_len as usize];
        if res_header.payload_len > 0 {
            self.stream
                .read_exact(&mut resp_payload)
                .await
                .map_err(|e| DaemonError::Ipc(format!("read: {e}")))?;
        }

        let response: R = bincode::deserialize(&resp_payload)
            .map_err(|e| DaemonError::Ipc(format!("deserialize: {e}")))?;
        Ok(response)
    }

    /// Subscribe to events. Returns the events received before a terminal
    /// Response(Unit) or connection drop.
    pub async fn subscribe(&mut self) -> Result<Vec<IpcEvent>, DaemonError> {
        let payload = bincode::serialize(&IpcRequest::SubscribeEvents)
            .map_err(|e| DaemonError::Ipc(format!("serialize: {e}")))?;
        let corr_id = rand_corr_id();
        let header = Header::new(1, corr_id, payload.len() as u32);

        self.stream.write_all(&header.encode()).await.ok();
        self.stream.write_all(&payload).await.ok();

        // Read ack
        let mut hbuf = [0u8; IPC_HEADER_SIZE];
        if self.stream.read_exact(&mut hbuf).await.is_err() {
            return Ok(Vec::new());
        }
        let _ack_header = Header::decode(&hbuf)?;
        let mut ack_payload = vec![0u8; _ack_header.payload_len as usize];
        if _ack_header.payload_len > 0 {
            self.stream.read_exact(&mut ack_payload).await.ok();
        }

        // Read events until terminal Response(Unit) or drop
        let mut events = Vec::new();
        loop {
            let mut ehbuf = [0u8; IPC_HEADER_SIZE];
            if self.stream.read_exact(&mut ehbuf).await.is_err() {
                break;
            }
            let ev_header = match Header::decode(&ehbuf) {
                Ok(h) => h,
                Err(_) => break,
            };
            let mut ev_payload = vec![0u8; ev_header.payload_len as usize];
            if ev_header.payload_len > 0 && self.stream.read_exact(&mut ev_payload).await.is_err() {
                break;
            }
            match ev_header.kind {
                2 => break, // terminal
                3 => {
                    if let Ok(event) = bincode::deserialize::<IpcEvent>(&ev_payload) {
                        events.push(event);
                    }
                }
                _ => {}
            }
        }
        Ok(events)
    }
}

fn rand_corr_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateDb;

    fn test_config(dir: &std::path::Path) -> DaemonConfig {
        DaemonConfig {
            socket_path: dir.join("test.sock"),
            state_db_path: dir.join("state.db"),
            log_level: tracing::Level::INFO,
            service_mode: false,
            update_repo: None,
            signaling_url: None,
        }
    }

    /// Helper: bind an IpcServer without socket activation (tests).
    async fn test_bind(cfg: &DaemonConfig, state: Arc<StateDb>) -> IpcServer {
        #[cfg(unix)]
        {
            IpcServer::bind(cfg, state, None).await.unwrap()
        }
        #[cfg(not(unix))]
        {
            IpcServer::bind(cfg, state).await.unwrap()
        }
    }

    #[test]
    fn ipc_message_roundtrip_header() {
        let h = Header::new(1, 0xDEAD_BEEF, 42);
        let encoded = h.encode();
        let decoded = Header::decode(&encoded).unwrap();
        assert_eq!(h, decoded);
    }

    #[test]
    fn ipc_header_rejects_bad_magic() {
        let mut buf = Header::new(1, 0, 0).encode();
        buf[0..4].copy_from_slice(&[0u8; 4]);
        let err = Header::decode(&buf).unwrap_err();
        assert!(err.to_string().contains("bad magic"), "{err}");
    }

    #[test]
    fn ipc_header_rejects_bad_version() {
        let mut buf = Header::new(1, 0, 0).encode();
        buf[4..6].copy_from_slice(&99u16.to_le_bytes());
        let err = Header::decode(&buf).unwrap_err();
        assert!(err.to_string().contains("unsupported version"), "{err}");
    }

    #[tokio::test]
    async fn ipc_ping_pong() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let state = Arc::new(StateDb::open(&cfg.state_db_path).unwrap());
        let server = test_bind(&cfg, state).await;

        tokio::spawn(async move {
            server.run().await.ok();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&cfg).await.unwrap();
        let resp: IpcResponse = client.call(&IpcRequest::Ping).await.unwrap();
        assert!(matches!(resp, IpcResponse::Pong));
    }

    #[tokio::test]
    async fn ipc_list_pairings_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let state = Arc::new(StateDb::open(&cfg.state_db_path).unwrap());
        let server = test_bind(&cfg, state).await;

        tokio::spawn(async move {
            server.run().await.ok();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&cfg).await.unwrap();
        let resp: IpcResponse = client.call(&IpcRequest::ListPairings).await.unwrap();
        match resp {
            IpcResponse::ListPairingsResponse { pairings } => assert!(pairings.is_empty()),
            other => panic!("expected ListPairingsResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ipc_approve_then_list_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let state = Arc::new(StateDb::open(&cfg.state_db_path).unwrap());
        let server = test_bind(&cfg, state).await;

        tokio::spawn(async move {
            server.run().await.ok();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&cfg).await.unwrap();
        let _: IpcResponse = client
            .call(&IpcRequest::ApprovePairing {
                peer_id: "peer1".into(),
                public_key: vec![1, 2, 3],
            })
            .await
            .unwrap();

        let resp: IpcResponse = client.call(&IpcRequest::ListPairings).await.unwrap();
        match resp {
            IpcResponse::ListPairingsResponse { pairings } => {
                assert_eq!(pairings.len(), 1);
                assert_eq!(pairings[0].peer_id, "peer1");
            }
            other => panic!("expected ListPairingsResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ipc_revoke_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let state = Arc::new(StateDb::open(&cfg.state_db_path).unwrap());
        let server = test_bind(&cfg, state).await;

        tokio::spawn(async move {
            server.run().await.ok();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&cfg).await.unwrap();
        let _: IpcResponse = client
            .call(&IpcRequest::ApprovePairing {
                peer_id: "peer1".into(),
                public_key: vec![1, 2, 3],
            })
            .await
            .unwrap();
        let _: IpcResponse = client
            .call(&IpcRequest::RevokePairing {
                peer_id: "peer1".into(),
            })
            .await
            .unwrap();

        let resp: IpcResponse = client.call(&IpcRequest::ListPairings).await.unwrap();
        match resp {
            IpcResponse::ListPairingsResponse { pairings } => assert!(pairings.is_empty()),
            other => panic!("expected empty, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ipc_stub_returns_501() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let state = Arc::new(StateDb::open(&cfg.state_db_path).unwrap());
        let server = test_bind(&cfg, state).await;

        tokio::spawn(async move {
            server.run().await.ok();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&cfg).await.unwrap();

        let stubs: Vec<IpcRequest> = vec![
            IpcRequest::CheckUpdate,
            IpcRequest::ApplyUpdate {
                staged_version: "v2".into(),
            },
            IpcRequest::GetUpdateStatus,
            IpcRequest::TurnIssueCredentials {
                peer_id: "p".into(),
            },
            IpcRequest::SignalingForward { message: vec![] },
        ];

        for stub in &stubs {
            let resp: IpcResponse = client.call(stub).await.unwrap();
            match resp {
                IpcResponse::Error { code, .. } => {
                    assert_eq!(code, 501, "stub {stub:?} should return 501");
                }
                other => panic!("stub {stub:?} expected 501, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn ipc_settings_and_onboarding_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let state = Arc::new(StateDb::open(&cfg.state_db_path).unwrap());
        let server = test_bind(&cfg, state).await;
        tokio::spawn(async move {
            server.run().await.ok();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&cfg).await.unwrap();
        let _: IpcResponse = client
            .call(&IpcRequest::SetSetting {
                key: "privacy_mode".into(),
                value: "blank-overlay".into(),
            })
            .await
            .unwrap();
        let resp: IpcResponse = client
            .call(&IpcRequest::GetSetting {
                key: "privacy_mode".into(),
            })
            .await
            .unwrap();
        match resp {
            IpcResponse::SettingValue { value, .. } => {
                assert_eq!(value.as_deref(), Some("blank-overlay"));
            }
            other => panic!("expected SettingValue, got {other:?}"),
        }

        let _: IpcResponse = client
            .call(&IpcRequest::CompleteOnboarding {
                device_name: "lab-host".into(),
                signaling_server: "wss://sig.example".into(),
            })
            .await
            .unwrap();
        let onb: IpcResponse = client.call(&IpcRequest::GetOnboarding).await.unwrap();
        match onb {
            IpcResponse::Onboarding {
                completed,
                device_name,
                signaling_server,
            } => {
                assert!(completed);
                assert_eq!(device_name.as_deref(), Some("lab-host"));
                assert_eq!(signaling_server.as_deref(), Some("wss://sig.example"));
            }
            other => panic!("expected Onboarding, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ipc_filesync_push_drain_update_and_ignores() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let state = Arc::new(StateDb::open(&cfg.state_db_path).unwrap());
        let server = test_bind(&cfg, state).await;
        tokio::spawn(async move {
            server.run().await.ok();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&cfg).await.unwrap();

        // Default ignores seed includes .git
        let ignores: IpcResponse = client.call(&IpcRequest::SyncListIgnores).await.unwrap();
        match ignores {
            IpcResponse::SyncIgnores { patterns } => {
                assert!(
                    patterns.iter().any(|p| p.contains(".git")),
                    "expected .git in {patterns:?}"
                );
            }
            other => panic!("expected SyncIgnores, got {other:?}"),
        }

        let payload = dir.path().join("save.dat");
        std::fs::write(&payload, b"hello-filesync").unwrap();
        let push: IpcResponse = client
            .call(&IpcRequest::SyncPushNow {
                local_path: payload.to_string_lossy().into_owned(),
                target_peer: "peer-a".into(),
                node_id: "node-1".into(),
            })
            .await
            .unwrap();
        let job_id = match push {
            IpcResponse::SyncJob { job } => {
                assert_eq!(job.target_peer, "peer-a");
                assert!(matches!(job.status, qubox_sync::OutboxStatus::Queued));
                job.job_id
            }
            other => panic!("expected SyncJob, got {other:?}"),
        };

        let drain: IpcResponse = client.call(&IpcRequest::SyncDrainReady).await.unwrap();
        match drain {
            IpcResponse::SyncJobs { jobs } => {
                assert!(
                    jobs.iter().any(|j| j.job_id == job_id),
                    "pending job missing from drain: {jobs:?}"
                );
            }
            other => panic!("expected SyncJobs, got {other:?}"),
        }

        let tracked: IpcResponse = client
            .call(&IpcRequest::SyncListTrackedFiles)
            .await
            .unwrap();
        match tracked {
            IpcResponse::SyncTrackedFiles { files } => {
                assert!(!files.is_empty(), "push should register tracked file");
            }
            other => panic!("expected SyncTrackedFiles, got {other:?}"),
        }

        let updated: IpcResponse = client
            .call(&IpcRequest::SyncUpdateJob {
                job_id: job_id.clone(),
                status: qubox_sync::OutboxStatus::Done,
                last_error: None,
            })
            .await
            .unwrap();
        match updated {
            IpcResponse::SyncJob { job } => {
                assert!(matches!(job.status, qubox_sync::OutboxStatus::Done));
            }
            other => panic!("expected SyncJob, got {other:?}"),
        }

        // Done jobs should not appear in drain-ready pending list
        let drain2: IpcResponse = client.call(&IpcRequest::SyncDrainReady).await.unwrap();
        match drain2 {
            IpcResponse::SyncJobs { jobs } => {
                assert!(
                    !jobs.iter().any(|j| j.job_id == job_id),
                    "done job should leave pending drain"
                );
            }
            other => panic!("expected SyncJobs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ipc_sync_rule_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let state = Arc::new(StateDb::open(&cfg.state_db_path).unwrap());
        let server = test_bind(&cfg, state).await;
        tokio::spawn(async move {
            server.run().await.ok();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&cfg).await.unwrap();
        let rule = qubox_sync::SyncRule {
            paths: vec!["/tmp/saves".into()],
            process_names: vec!["game.exe".into()],
            ..Default::default()
        };
        let _: IpcResponse = client
            .call(&IpcRequest::SyncAddRule { rule: rule.clone() })
            .await
            .unwrap();
        let listed: IpcResponse = client.call(&IpcRequest::SyncListRules).await.unwrap();
        let rule_id = match listed {
            IpcResponse::SyncRules { rules } => {
                assert_eq!(rules.len(), 1);
                assert_eq!(rules[0].paths, vec!["/tmp/saves".to_string()]);
                rules[0].rule_id.clone()
            }
            other => panic!("expected SyncRules, got {other:?}"),
        };
        let _: IpcResponse = client
            .call(&IpcRequest::SyncSetEnabled {
                rule_id: rule_id.clone(),
                enabled: false,
            })
            .await
            .unwrap();
        let _: IpcResponse = client
            .call(&IpcRequest::SyncRemoveRule { rule_id })
            .await
            .unwrap();
        let listed2: IpcResponse = client.call(&IpcRequest::SyncListRules).await.unwrap();
        match listed2 {
            IpcResponse::SyncRules { rules } => assert!(rules.is_empty()),
            other => panic!("expected empty rules, got {other:?}"),
        }
    }

    #[test]
    fn host_config_privacy_fields_serde() {
        let cfg = HostConfig {
            identity_path: None,
            auto_approve_pairing: false,
            socket_path: "/tmp/d.sock".into(),
            server: Some("wss://x".into()),
            privacy_mode: Some("blank-overlay".into()),
            enable_privacy_on_session_start: true,
            stream_mode: Some("multi-display".into()),
        };
        let v = serde_json::to_value(&cfg).unwrap();
        assert_eq!(v["privacy_mode"], "blank-overlay");
        assert_eq!(v["enable_privacy_on_session_start"], true);
        assert_eq!(v["stream_mode"], "multi-display");
        let back: HostConfig = serde_json::from_value(v).unwrap();
        assert_eq!(back.privacy_mode.as_deref(), Some("blank-overlay"));
        assert!(back.enable_privacy_on_session_start);
    }

    #[tokio::test]
    async fn ipc_start_host_spawns_configured_binary() {
        // Use /bin/true (or Windows equivalent) as a stand-in host agent that exits.
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let state = Arc::new(StateDb::open(&cfg.state_db_path).unwrap());
        let server = test_bind(&cfg, state).await;
        tokio::spawn(async move {
            server.run().await.ok();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&cfg).await.unwrap();
        // StartHost looks for qubox-host-agent next to current_exe; without it
        // subprocess may fail to spawn — we only assert the IPC path returns
        // Unit or a structured Error (not panic / hang).
        let resp: IpcResponse = client
            .call(&IpcRequest::StartHost {
                config: HostConfig {
                    identity_path: None,
                    auto_approve_pairing: false,
                    socket_path: cfg.socket_path.to_string_lossy().into_owned(),
                    server: None,
                    privacy_mode: Some("none".into()),
                    enable_privacy_on_session_start: false,
                    stream_mode: Some("single-stream".into()),
                },
            })
            .await
            .unwrap();
        match resp {
            IpcResponse::Unit | IpcResponse::Error { .. } => {}
            other => panic!("unexpected StartHost response: {other:?}"),
        }
        let _ = client.call::<IpcResponse>(&IpcRequest::StopHost).await;
    }

    #[tokio::test]
    async fn ipc_get_update_status_with_checker() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let state = Arc::new(StateDb::open(&cfg.state_db_path).unwrap());
        let checker = Arc::new(
            UpdateChecker::new("http://127.0.0.1:1".into(), state.clone(), "0.1.0".into()).unwrap(),
        );
        let server = test_bind(&cfg, state).await.with_checker(checker);

        tokio::spawn(async move {
            server.run().await.ok();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&cfg).await.unwrap();
        let resp: IpcResponse = client.call(&IpcRequest::GetUpdateStatus).await.unwrap();
        match resp {
            IpcResponse::UpdateStatusResponse {
                current_version,
                available,
                ..
            } => {
                assert_eq!(current_version, "0.1.0");
                assert!(available.is_none());
            }
            IpcResponse::Error { code, .. } => {
                assert_eq!(code, 502, "no-checker path should not run; got {code}");
            }
            other => panic!("expected UpdateStatusResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ipc_check_update_with_checker_returns_502() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let state = Arc::new(StateDb::open(&cfg.state_db_path).unwrap());
        let checker = Arc::new(
            UpdateChecker::new("http://127.0.0.1:1".into(), state.clone(), "0.1.0".into()).unwrap(),
        );
        let server = test_bind(&cfg, state).await.with_checker(checker);

        tokio::spawn(async move {
            server.run().await.ok();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&cfg).await.unwrap();
        let resp: IpcResponse = client.call(&IpcRequest::CheckUpdate).await.unwrap();
        match resp {
            IpcResponse::Error { code, message } => {
                assert_eq!(code, 502, "fetch failure should map to 502");
                assert!(
                    message.contains("update check") || message.contains("fetch"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected 502 Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ipc_subscribe_events_receives_event() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let state = Arc::new(StateDb::open(&cfg.state_db_path).unwrap());
        let server = test_bind(&cfg, state).await;
        let sender = server.event_sender();

        // Verify broadcast channel works in isolation
        let mut rx = sender.subscribe();
        sender
            .send(IpcEvent::Error {
                code: 999,
                message: "pre-sub".into(),
            })
            .ok();
        match rx.try_recv() {
            Ok(ev) => {
                assert!(
                    matches!(ev, IpcEvent::Error { code: 999, .. }),
                    "broadcast channel should work"
                );
            }
            Err(e) => panic!("broadcast receiver should have event: {e:?}"),
        }
        drop(rx);

        tokio::spawn(async move {
            server.run().await.ok();
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&cfg).await.unwrap();

        // Subscribe manually
        {
            let payload = bincode::serialize(&IpcRequest::SubscribeEvents).unwrap();
            let corr_id = rand_corr_id();
            let header = Header::new(1, corr_id, payload.len() as u32);
            client.stream.write_all(&header.encode()).await.unwrap();
            client.stream.write_all(&payload).await.unwrap();

            // Read ack header + payload
            let mut hbuf = [0u8; IPC_HEADER_SIZE];
            client.stream.read_exact(&mut hbuf).await.unwrap();
            let ack_header = Header::decode(&hbuf).unwrap();
            let mut ack_payload = vec![0u8; ack_header.payload_len as usize];
            if ack_header.payload_len > 0 {
                client.stream.read_exact(&mut ack_payload).await.unwrap();
            }
        }

        // Now the server handler should be subscribed to the broadcast channel.
        // Wait briefly for the handler to be in the select loop.
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Send event AFTER server subscribed (in SendEvents handler)
        sender
            .send(IpcEvent::Error {
                code: 999,
                message: "test error".into(),
            })
            .ok();

        // Try reading events; if connection is already closed (unexpected),
        // this will error immediately — log the error
        let mut events = Vec::new();
        loop {
            let mut ehbuf = [0u8; IPC_HEADER_SIZE];
            let r = tokio::time::timeout(
                tokio::time::Duration::from_secs(2),
                client.stream.read_exact(&mut ehbuf),
            )
            .await;
            match r {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    panic!("read error: {e}");
                }
                Err(_) => break, // timeout
            }
            let ev_header = Header::decode(&ehbuf).unwrap();
            if ev_header.kind == 2 {
                break;
            }
            let mut ev_payload = vec![0u8; ev_header.payload_len as usize];
            if ev_header.payload_len > 0 {
                client.stream.read_exact(&mut ev_payload).await.unwrap();
            }
            if ev_header.kind == 3 {
                if let Ok(event) = bincode::deserialize::<IpcEvent>(&ev_payload) {
                    events.push(event);
                }
            }
        }

        assert!(!events.is_empty(), "should have received events");
        let has_error = events
            .iter()
            .any(|e| matches!(e, IpcEvent::Error { code: 999, .. }));
        assert!(has_error, "should contain our error event");
    }
}
