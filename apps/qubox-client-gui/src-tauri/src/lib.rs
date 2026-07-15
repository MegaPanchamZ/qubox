//! # Qubox Tauri GUI launcher (Phase E)
//!
//! This module replaces the original `client_gui` stub with a
//! production launcher. The GUI talks to the `qubox-client-cli` binary via
//! **subprocess** (per ADR-008): the GUI spawns the CLI, parses NDJSON
//! telemetry from its stdout, forwards it to React as Tauri events, and
//! owns the subprocess lifecycle (start / cancel / health).
//!
//! ## Layout
//!
//! * [`Tauri commands`] exposed to React: `list_known_hosts`,
//!   `discover_lan_hosts`, `start_session`, `cancel_session`,
//!   `list_active_sessions`, `accept_pairing`, `reject_pairing`,
//!   `get_settings`, `set_setting`.
//! * [`Tauri events`] emitted to React: `session://started`,
//!   `session://telemetry`, `session://ended`,
//!   `session://pairing-requested`, `session://host-discovered`,
//!   `session://stderr`, `daemon://state-changed`.
//! * Process management uses `tokio::process::Command`; one
//!   `SessionHandle` per active session holds the `Child` and a
//!   cancellation `oneshot::Sender`.
//!
//! ## Backward compatibility
//!
//! The pre-Phase-E `start_session` command is preserved as a no-op
//! stub so the existing `qubox_client_cli::start_session` import keeps
//! compiling. New code uses `start_session_subprocess` instead.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use qubox_client_cli::telemetry::TelemetryEvent;
use qubox_client_cli::{
    start_session as launch_client_session, ClientSessionLaunchConfig, SessionTarget,
    DEFAULT_SIGNALING_SERVER,
};
use qubox_daemon::default_daemon_socket_path;
use qubox_daemon::ipc::{IpcClient, IpcEvent, IpcRequest, IpcResponse};
use qubox_identity::load_or_create_identity;
use qubox_signaling::load_pairings_from_path;
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State,
};
use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex};
use uuid::Uuid;

// ── Public types ─────────────────────────────────────────────────────

/// A host persisted in `pairings.json`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct KnownHost {
    host_peer_id: String,
    display_name: Option<String>,
}

/// A host discovered via signaling, returned by `discover_lan_hosts`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveredHost {
    peer_id: String,
    device_name: String,
    transports: Vec<String>,
}

/// Options for `start_session`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionOptions {
    #[serde(default)]
    mic: bool,
    #[serde(default)]
    clipboard_sync: Option<String>,
    #[serde(default)]
    stats_overlay: bool,
    #[serde(default)]
    show_privacy_indicator: Option<bool>,
    #[serde(default)]
    skip_window: bool,
    #[serde(default)]
    max_stream_frames: Option<u64>,
}

/// A snapshot of an active session.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ActiveSession {
    session_id: String,
    host_id: String,
    pid: Option<u32>,
    started_at: u64,
}

/// Daemon-backed settings bundle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    signaling_server: Option<String>,
    auto_approve_pairing: bool,
    bitrate_kbps: Option<u32>,
    fps_cap: Option<u32>,
    decoder_backend: Option<String>,
    mic_enabled: bool,
    clipboard_sync: Option<String>,
    stats_overlay: bool,
}

// ── Process management ──────────────────────────────────────────────

/// Handle for a single running `qubox-client-cli` subprocess.
struct SessionHandle {
    session_id: String,
    host_id: String,
    pid: Option<u32>,
    started_at: u64,
    kill_tx: Option<oneshot::Sender<()>>,
}

/// Process registry: one entry per active session, guarded by a tokio Mutex.
#[derive(Default)]
struct SessionRegistry {
    sessions: HashMap<String, SessionHandle>,
}

/// Resolves the path of the `qubox-client-cli` binary. In `cargo run` /
/// `cargo build` we get the exact path via `CARGO_BIN_EXE_qubox-client-cli`.
/// In production (installed) we fall back to a `which`-style lookup
/// resolved by the OS.
fn resolve_qubox_client_cli_path() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_qubox-client-cli") {
        return PathBuf::from(path);
    }

    if let Ok(path) = std::env::var("QUBOX_CLIENT_CLI") {
        return PathBuf::from(path);
    }

    if let Ok(path) = which_qubox_client_cli() {
        return path;
    }

    PathBuf::from("qubox-client-cli")
}

/// Minimal `which`-style helper: probe `PATH` for a binary called
/// `qubox-client-cli`. Cross-platform.
fn which_qubox_client_cli() -> Result<PathBuf, ()> {
    let name = if cfg!(windows) {
        "qubox-client-cli.exe"
    } else {
        "qubox-client-cli"
    };
    let path_var = std::env::var_os("PATH").ok_or(())?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(())
}

// ── Tauri commands ──────────────────────────────────────────────────

/// List hosts persisted in the workspace's `pairings.json`.
#[tauri::command]
fn get_known_hosts() -> Result<Vec<KnownHost>, String> {
    let local_state_dir = resolve_local_state_dir()?;
    let client_identity_path = local_state_dir.join("client-id.json");
    let pairings_path = local_state_dir.join("pairings.json");
    let (identity, _) = load_or_create_identity(Some(client_identity_path), None)
        .map_err(|error| error.to_string())?;
    let mut hosts = load_pairings_from_path(pairings_path)
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|pairing| pairing.client_peer_id == identity.client_peer_id)
        .map(|pairing| KnownHost {
            host_peer_id: pairing.host_peer_id.to_string(),
            display_name: None,
        })
        .collect::<Vec<_>>();

    hosts.sort_by(|left, right| left.host_peer_id.cmp(&right.host_peer_id));
    Ok(hosts)
}

/// Spawn `qubox-client-cli list-hosts --json-telemetry` for `~3s` and parse
/// NDJSON `host_discovered` events.
#[tauri::command]
async fn discover_lan_hosts() -> Result<Vec<DiscoveredHost>, String> {
    let cli = resolve_qubox_client_cli_path();
    let server =
        std::env::var("QUBOX_SERVER").unwrap_or_else(|_| DEFAULT_SIGNALING_SERVER.to_string());

    let mut command = Command::new(&cli);
    command
        .arg("--server")
        .arg(&server)
        .arg("list-hosts")
        .arg("--json-telemetry")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to spawn qubox-client-cli: {error}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "qubox-client-cli did not expose stdout".to_string())?;

    let mut discovered: Vec<DiscoveredHost> = Vec::new();
    let mut lines = TokioBufReader::new(stdout).lines();
    let collect_task = tokio::spawn(async move {
        let mut hosts = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(TelemetryEvent::HostDiscovered {
                peer_id,
                device_name,
                transports,
            }) = serde_json::from_str::<TelemetryEvent>(&line)
            {
                hosts.push(DiscoveredHost {
                    peer_id,
                    device_name,
                    transports,
                });
            }
        }
        hosts
    });

    tokio::time::sleep(Duration::from_secs(3)).await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    if let Ok(hosts) = collect_task.await {
        discovered = hosts;
    }
    discovered.sort_by(|left, right| left.peer_id.cmp(&right.peer_id));
    Ok(discovered)
}

/// Legacy in-process stub preserved for the original import. The
/// production GUI uses [`start_session_subprocess`] instead. This
/// function spawns a thread that just calls the in-process stub,
/// which currently `bail!()`s — kept for backward compat.
#[tauri::command]
fn start_session(host_id: String) -> Result<(), String> {
    let host_id =
        Uuid::parse_str(&host_id).map_err(|error| format!("invalid host id {host_id}: {error}"))?;
    let identity_path = resolve_local_state_dir()?.join("client-id.json");
    let config = ClientSessionLaunchConfig {
        server: std::env::var("QUBOX_SERVER")
            .unwrap_or_else(|_| DEFAULT_SIGNALING_SERVER.to_string()),
        identity_path: Some(identity_path),
        name: None,
        mute_playback: true,
        max_stream_frames: 0,
    };

    thread::Builder::new()
        .name(format!("bp-gui-session-{}", host_id.as_simple()))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build();

            match runtime {
                Ok(runtime) => {
                    if let Err(error) = runtime.block_on(launch_client_session(
                        config,
                        SessionTarget::HostId(host_id),
                    )) {
                        eprintln!("client-gui session launch failed: {error:?}");
                    }
                }
                Err(error) => {
                    eprintln!("client-gui failed to create a Tokio runtime: {error}");
                }
            }
        })
        .map_err(|error| format!("failed to spawn client session thread: {error}"))?;

    Ok(())
}

/// Spawn `qubox-client-cli start-session --host <id> --json-telemetry ...` as
/// a real subprocess. Returns the new session id. Streamed events are
/// emitted to React as Tauri events keyed by `session://*`.
#[tauri::command]
async fn start_session_subprocess(
    host_id: String,
    options: Option<SessionOptions>,
    app: AppHandle,
    registry: State<'_, Arc<Mutex<SessionRegistry>>>,
) -> Result<String, String> {
    let host_uuid =
        Uuid::parse_str(&host_id).map_err(|error| format!("invalid host id {host_id}: {error}"))?;
    let opts = options.unwrap_or(SessionOptions {
        mic: false,
        clipboard_sync: None,
        stats_overlay: false,
        show_privacy_indicator: None,
        skip_window: false,
        max_stream_frames: None,
    });

    let cli = resolve_qubox_client_cli_path();
    let server =
        std::env::var("QUBOX_SERVER").unwrap_or_else(|_| DEFAULT_SIGNALING_SERVER.to_string());
    let session_id = Uuid::new_v4().to_string();

    let mut command = Command::new(&cli);
    command
        .arg("--server")
        .arg(&server)
        .arg("start-session")
        .arg("--host")
        .arg(host_uuid.to_string())
        .arg("--json-telemetry")
        .arg("--datagram-media")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    if opts.mic {
        command.arg("--mic");
    }
    if let Some(clip) = opts.clipboard_sync.as_deref() {
        command.arg("--clipboard-sync").arg(clip);
    }
    if opts.stats_overlay {
        command.arg("--stats-overlay");
    }
    if let Some(privacy) = opts.show_privacy_indicator {
        if !privacy {
            command.arg("--no-privacy-indicator");
        }
    }
    if opts.skip_window {
        command.arg("--skip-window");
    }
    if let Some(max_frames) = opts.max_stream_frames {
        command
            .arg("--max-stream-frames")
            .arg(max_frames.to_string());
    }

    let mut child: Child = command
        .spawn()
        .map_err(|error| format!("failed to spawn qubox-client-cli: {error}"))?;
    let pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "qubox-client-cli did not expose stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "qubox-client-cli did not expose stderr".to_string())?;

    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    {
        let mut guard = registry.lock().await;
        guard.sessions.insert(
            session_id.clone(),
            SessionHandle {
                session_id: session_id.clone(),
                host_id: host_id.clone(),
                pid,
                started_at,
                kill_tx: Some(kill_tx),
            },
        );
    }

    let session_id_for_task = session_id.clone();
    let app_for_task = app.clone();
    let registry_for_task = registry.inner().clone();
    tokio::spawn(async move {
        run_session_pipeline(
            session_id_for_task,
            host_id,
            app_for_task,
            child,
            stdout,
            stderr,
            kill_rx,
            registry_for_task,
        )
        .await;
    });

    // Notify UI that FileSync outbox may drain while session is up.
    let app_drain = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if let Ok(mut client) = connect_daemon().await {
            if let Ok(IpcResponse::SyncJobs { jobs }) = client
                .call::<IpcResponse>(&IpcRequest::SyncDrainReady)
                .await
            {
                let _ = app_drain.emit(
                    "filesync://drain-ready",
                    serde_json::json!({ "pending": jobs.len() }),
                );
            }
        }
    });

    Ok(session_id)
}

/// Cancel a running session by killing its subprocess.
#[tauri::command]
async fn cancel_session(
    session_id: String,
    app: AppHandle,
    registry: State<'_, Arc<Mutex<SessionRegistry>>>,
) -> Result<(), String> {
    let mut guard = registry.lock().await;
    if let Some(mut handle) = guard.sessions.remove(&session_id) {
        if let Some(tx) = handle.kill_tx.take() {
            let _ = tx.send(());
        }
        drop(guard);
        let payload = serde_json::json!({ "session_id": session_id, "reason": "user_cancelled" });
        let _ = app.emit("session://ended", payload);
        Ok(())
    } else {
        Err(format!("no active session with id {session_id}"))
    }
}

/// Return the list of currently active session ids.
#[tauri::command]
async fn list_active_sessions(
    registry: State<'_, Arc<Mutex<SessionRegistry>>>,
) -> Result<Vec<ActiveSession>, String> {
    let guard = registry.lock().await;
    Ok(guard
        .sessions
        .values()
        .map(|h| ActiveSession {
            session_id: h.session_id.clone(),
            host_id: h.host_id.clone(),
            pid: h.pid,
            started_at: h.started_at,
        })
        .collect())
}

/// Accept a pending pairing by `host_id` via the daemon IPC.
#[tauri::command]
async fn accept_pairing(host_id: String, public_key: Option<Vec<u8>>) -> Result<(), String> {
    let config = build_daemon_config();
    let mut client = IpcClient::connect(&config)
        .await
        .map_err(|error| format!("failed to connect to daemon: {error}"))?;
    let key = public_key.unwrap_or_default();
    let _resp: IpcResponse = client
        .call(&IpcRequest::ApprovePairing {
            peer_id: host_id,
            public_key: key,
        })
        .await
        .map_err(|error| format!("daemon call failed: {error}"))?;
    Ok(())
}

/// Reject a pending pairing by `host_id` via the daemon IPC.
#[tauri::command]
async fn reject_pairing(host_id: String) -> Result<(), String> {
    let config = build_daemon_config();
    let mut client = IpcClient::connect(&config)
        .await
        .map_err(|error| format!("failed to connect to daemon: {error}"))?;
    let _resp: IpcResponse = client
        .call(&IpcRequest::RevokePairing { peer_id: host_id })
        .await
        .map_err(|error| format!("daemon call failed: {error}"))?;
    Ok(())
}

fn host_pairing_base_url() -> String {
    // Port file written by host-agent pairing_ui
    if let Some(dir) = directories::ProjectDirs::from("app", "Qubox", "qubox") {
        let path = dir.data_local_dir().join("host_pairing_port");
        if let Ok(s) = std::fs::read_to_string(&path) {
            let port = s.trim();
            if !port.is_empty() {
                return format!("http://127.0.0.1:{port}");
            }
        }
    }
    "http://127.0.0.1:17443".to_string()
}

/// Pending pairing requests from the **host agent** (cloud/self-host).
#[tauri::command]
async fn list_host_pending_pairings() -> Result<serde_json::Value, String> {
    let url = format!("{}/pending", host_pairing_base_url());
    let client = reqwest::Client::new();
    let res = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .map_err(|e| format!("host pairing UI unreachable ({e}). Is qubox-host-agent running?"))?;
    if !res.status().is_success() {
        return Err(format!("host pairing UI HTTP {}", res.status()));
    }
    res.json().await.map_err(|e| format!("parse pending: {e}"))
}

/// Approve/reject a host-side pairing request (sends PairingDecision on signaling).
#[tauri::command]
async fn host_pairing_decide(request_id: String, approved: bool) -> Result<(), String> {
    let url = format!("{}/decide", host_pairing_base_url());
    let client = reqwest::Client::new();
    let res = client
        .post(&url)
        .json(&serde_json::json!({
            "request_id": request_id,
            "approved": approved,
        }))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| format!("host decide: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("host decide HTTP {}", res.status()));
    }
    Ok(())
}

/// Create a share link via client-cli.
#[tauri::command]
fn create_share_link(ttl_secs: u64) -> Result<String, String> {
    let cli = resolve_qubox_client_cli_path();
    let output = std::process::Command::new(&cli)
        .args(["create-share-link", "--ttl-secs", &ttl_secs.to_string()])
        .output()
        .map_err(|e| format!("spawn client-cli: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[tauri::command]
fn redeem_share_link(code: String, client_label: Option<String>) -> Result<String, String> {
    let cli = resolve_qubox_client_cli_path();
    let mut cmd = std::process::Command::new(&cli);
    cmd.args(["redeem-share-link", "--code", &code]);
    if let Some(label) = client_label {
        cmd.args(["--client-label", &label]);
    }
    let output = cmd.output().map_err(|e| format!("spawn client-cli: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[tauri::command]
fn kick_session(session_id: String, reason: Option<String>) -> Result<(), String> {
    let cli = resolve_qubox_client_cli_path();
    let mut cmd = std::process::Command::new(&cli);
    cmd.args(["kick-session", "--session", &session_id]);
    if let Some(r) = reason {
        cmd.args(["--reason", &r]);
    }
    let output = cmd.output().map_err(|e| format!("spawn client-cli: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(())
}

#[tauri::command]
async fn get_settings() -> Result<Settings, String> {
    let mut settings = Settings {
        signaling_server: Some(
            std::env::var("QUBOX_SERVER").unwrap_or_else(|_| DEFAULT_SIGNALING_SERVER.to_string()),
        ),
        auto_approve_pairing: false,
        bitrate_kbps: Some(20_000),
        fps_cap: Some(60),
        decoder_backend: Some("ffmpeg".into()),
        mic_enabled: false,
        clipboard_sync: Some("off".into()),
        stats_overlay: true,
    };

    if let Ok(mut client) = connect_daemon().await {
        if let Ok(IpcResponse::SettingsMap { entries }) =
            client.call::<IpcResponse>(&IpcRequest::ListSettings).await
        {
            for (k, v) in entries {
                match k.as_str() {
                    "signaling_server" => settings.signaling_server = Some(v),
                    "auto_approve_pairing" => {
                        settings.auto_approve_pairing = v == "1" || v.eq_ignore_ascii_case("true")
                    }
                    "bitrate_kbps" => settings.bitrate_kbps = v.parse().ok(),
                    "fps_cap" => settings.fps_cap = v.parse().ok(),
                    "decoder_backend" => settings.decoder_backend = Some(v),
                    "mic_enabled" => {
                        settings.mic_enabled = v == "1" || v.eq_ignore_ascii_case("true")
                    }
                    "clipboard_sync" => settings.clipboard_sync = Some(v),
                    "stats_overlay" => {
                        settings.stats_overlay = v != "0" && !v.eq_ignore_ascii_case("false")
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(settings)
}

/// Persist a setting via the daemon IPC.
#[tauri::command]
async fn set_setting(key: String, value: String) -> Result<(), String> {
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::SetSetting { key, value })
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::Unit => Ok(()),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

#[tauri::command]
async fn get_onboarding() -> Result<serde_json::Value, String> {
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::GetOnboarding)
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::Onboarding {
            completed,
            device_name,
            signaling_server,
        } => Ok(serde_json::json!({
            "completed": completed,
            "deviceName": device_name,
            "signalingServer": signaling_server,
        })),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

#[tauri::command]
async fn complete_onboarding(device_name: String, signaling_server: String) -> Result<(), String> {
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::CompleteOnboarding {
            device_name,
            signaling_server,
        })
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::Unit => Ok(()),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

const DEFAULT_CLOUD_SIGNALING: &str = "wss://signal.qubox.app/ws";
const DEFAULT_CLOUD_ACCOUNTS: &str = "https://signal.qubox.app";

/// Enroll this machine with Qubox Cloud using a dashboard enroll code.
/// Uses the local device identity (same as `qubox-client-cli cloud-enroll`).
#[tauri::command]
async fn cloud_enroll(
    code: String,
    display_name: Option<String>,
    accounts_url: Option<String>,
) -> Result<serde_json::Value, String> {
    let code = code.trim().to_uppercase();
    if code.len() < 6 {
        return Err("enroll code looks too short".into());
    }
    let name = display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let (identity, _) =
        load_or_create_identity(None, name.clone()).map_err(|e| format!("identity: {e}"))?;
    let display = name.unwrap_or_else(|| identity.display_name.clone());
    let base = accounts_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_CLOUD_ACCOUNTS)
        .trim_end_matches('/')
        .to_string();
    let url = format!("{base}/v1/public/enroll");
    let body = serde_json::json!({
        "code": code,
        "device_id": identity.device_id,
        "display_name": display,
        "public_key_hex": hex::encode(identity.public_key),
        "role": "both",
    });
    let client = reqwest::Client::new();
    let res = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("cloud enroll failed ({status}): {text}"));
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({ "raw": text }));
    Ok(serde_json::json!({
        "ok": true,
        "deviceId": identity.device_id,
        "displayName": display,
        "signalingServer": DEFAULT_CLOUD_SIGNALING,
        "accountsUrl": base,
        "enrollment": parsed,
    }))
}

#[tauri::command]
async fn sync_drain_ready() -> Result<Vec<serde_json::Value>, String> {
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::SyncDrainReady)
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::SyncJobs { jobs } => jobs
            .into_iter()
            .map(|j| {
                serde_json::to_value(serde_json::json!({
                    "jobId": j.job_id,
                    "fileId": j.file_id,
                    "targetPeer": j.target_peer,
                    "status": format!("{:?}", j.status),
                    "retryCount": j.retry_count,
                }))
                .map_err(|e| e.to_string())
            })
            .collect(),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

#[tauri::command]
async fn start_host_agent(server: Option<String>) -> Result<(), String> {
    let mut client = connect_daemon().await?;
    let socket = default_daemon_socket_path().to_string_lossy().into_owned();
    // Load privacy prefs from daemon settings (Host mode GUI).
    let privacy_mode = match client
        .call::<IpcResponse>(&IpcRequest::GetSetting {
            key: "privacy_mode".into(),
        })
        .await
    {
        Ok(IpcResponse::SettingValue { value, .. }) => value.filter(|v| !v.is_empty()),
        _ => None,
    };
    let enable_privacy = matches!(
        privacy_mode.as_deref(),
        Some("blank-overlay") | Some("vkms")
    );
    let stream_mode = match client
        .call::<IpcResponse>(&IpcRequest::GetSetting {
            key: "stream_mode".into(),
        })
        .await
    {
        Ok(IpcResponse::SettingValue { value, .. }) => value.filter(|v| !v.is_empty()),
        _ => None,
    };
    match client
        .call::<IpcResponse>(&IpcRequest::StartHost {
            config: qubox_daemon::ipc::HostConfig {
                identity_path: None,
                auto_approve_pairing: false,
                socket_path: socket,
                server: server.or_else(|| std::env::var("QUBOX_SERVER").ok()),
                privacy_mode,
                enable_privacy_on_session_start: enable_privacy,
                stream_mode,
            },
        })
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::Unit => Ok(()),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

#[tauri::command]
async fn stop_host_agent() -> Result<(), String> {
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::StopHost)
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::Unit => Ok(()),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

#[tauri::command]
async fn sync_list_ignores() -> Result<Vec<String>, String> {
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::SyncListIgnores)
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::SyncIgnores { patterns } => Ok(patterns),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

#[tauri::command]
async fn sync_add_ignore(pattern: String) -> Result<Vec<String>, String> {
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::SyncAddIgnore { pattern })
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::SyncIgnores { patterns } => Ok(patterns),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

#[tauri::command]
async fn sync_remove_ignore(pattern: String) -> Result<Vec<String>, String> {
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::SyncRemoveIgnore { pattern })
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::SyncIgnores { patterns } => Ok(patterns),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

#[tauri::command]
async fn sync_apply_ignore_preset(name: String) -> Result<Vec<String>, String> {
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::SyncApplyIgnorePreset { name })
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::SyncIgnores { patterns } => Ok(patterns),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

#[tauri::command]
async fn sync_list_conflicts() -> Result<Vec<serde_json::Value>, String> {
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::SyncListConflicts)
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::SyncConflicts { conflicts } => conflicts
            .into_iter()
            .map(|c| {
                serde_json::to_value(serde_json::json!({
                    "conflictId": c.conflict_id,
                    "fileId": c.file_id,
                    "localPath": c.local_path,
                    "remotePath": c.remote_path,
                    "peerId": c.peer_id,
                    "createdAtUnix": c.created_at_unix,
                }))
                .map_err(|e| e.to_string())
            })
            .collect(),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

#[tauri::command]
async fn sync_list_rules() -> Result<Vec<serde_json::Value>, String> {
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::SyncListRules)
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::SyncRules { rules } => rules
            .into_iter()
            .map(|r| {
                serde_json::to_value(serde_json::json!({
                    "ruleId": r.rule_id,
                    "paths": r.paths,
                    "processNames": r.process_names,
                    "peerIds": r.peer_ids,
                    "enabled": r.enabled,
                    "maxFileBytes": r.max_file_bytes,
                    "ignoreGlobs": r.ignore_globs,
                }))
                .map_err(|e| e.to_string())
            })
            .collect(),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

#[tauri::command]
async fn sync_list_jobs() -> Result<Vec<serde_json::Value>, String> {
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::SyncListJobs)
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::SyncJobs { jobs } => jobs
            .into_iter()
            .map(|j| {
                serde_json::to_value(serde_json::json!({
                    "jobId": j.job_id,
                    "fileId": j.file_id,
                    "targetPeer": j.target_peer,
                    "status": format!("{:?}", j.status),
                    "retryCount": j.retry_count,
                }))
                .map_err(|e| e.to_string())
            })
            .collect(),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

#[tauri::command]
async fn sync_resolve_conflict(conflict_id: String, resolution: String) -> Result<(), String> {
    use qubox_sync::ConflictResolution;
    let resolution = match resolution.as_str() {
        "keep-local" => ConflictResolution::KeepLocal,
        "keep-remote" => ConflictResolution::KeepRemote,
        "keep-both" => ConflictResolution::KeepBoth,
        _ => return Err("invalid resolution".into()),
    };
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::SyncResolveConflict {
            conflict_id,
            resolution,
        })
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::Unit => Ok(()),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

#[tauri::command]
async fn sync_add_rule(
    paths: Vec<String>,
    process_names: Vec<String>,
    peer_ids: Vec<String>,
    ignore_globs: Vec<String>,
) -> Result<(), String> {
    let rule = qubox_sync::SyncRule {
        rule_id: String::new(),
        paths,
        process_names,
        peer_ids,
        enabled: true,
        max_file_bytes: 256 * 1024 * 1024,
        ignore_globs,
    };
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::SyncAddRule { rule })
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::Unit => Ok(()),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

#[tauri::command]
async fn sync_push_now(
    local_path: String,
    target_peer: String,
    node_id: String,
) -> Result<(), String> {
    let mut client = connect_daemon().await?;
    match client
        .call::<IpcResponse>(&IpcRequest::SyncPushNow {
            local_path,
            target_peer,
            node_id,
        })
        .await
        .map_err(|e| e.to_string())?
    {
        IpcResponse::SyncJob { .. } | IpcResponse::Unit => Ok(()),
        IpcResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected {other:?}")),
    }
}

// ── Session pipeline ────────────────────────────────────────────────

/// Background task: read NDJSON from stdout, stderr from the child,
/// and watch for cancellation. On exit, clean up the registry and
/// emit `session://ended`.
async fn run_session_pipeline<R, S>(
    session_id: String,
    host_id: String,
    app: AppHandle,
    mut child: Child,
    stdout: R,
    stderr: S,
    mut kill_rx: oneshot::Receiver<()>,
    registry: Arc<Mutex<SessionRegistry>>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    S: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let started_payload = serde_json::json!({
        "session_id": session_id,
        "host_id": host_id,
    });
    let _ = app.emit("session://started", &started_payload);

    let app_for_stdout = app.clone();
    let session_for_stdout = session_id.clone();
    let stdout_task = tokio::spawn(async move {
        let mut reader = TokioBufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            forward_telemetry_line(&app_for_stdout, &session_for_stdout, &line);
        }
    });

    let app_for_stderr = app.clone();
    let session_for_stderr = session_id.clone();
    let stderr_task = tokio::spawn(async move {
        let mut reader = TokioBufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let payload = serde_json::json!({
                "session_id": session_for_stderr,
                "line": line,
                "level": "info",
            });
            let _ = app_for_stderr.emit("session://stderr", payload);
        }
    });

    let exit_reason = tokio::select! {
        result = child.wait() => {
            match result {
                Ok(status) => {
                    if status.success() { "completed".to_string() }
                    else { format!("exit_status:{}", status.code().unwrap_or(-1)) }
                }
                Err(error) => format!("wait_failed:{error}"),
            }
        }
        _ = &mut kill_rx => {
            let _ = child.kill().await;
            "user_cancelled".to_string()
        }
    };

    let _ = stdout_task.await;
    let _ = stderr_task.await;

    {
        let mut guard = registry.lock().await;
        guard.sessions.remove(&session_id);
    }
    let payload = serde_json::json!({
        "session_id": session_id,
        "reason": exit_reason,
    });
    let _ = app.emit("session://ended", payload);
}

/// Parse one NDJSON line from `qubox-client-cli` and forward it to React.
/// Lines that fail to parse are emitted verbatim under
/// `session://telemetry` so the GUI can surface them in the log view.
fn forward_telemetry_line(app: &AppHandle, session_id: &str, line: &str) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let event: Result<TelemetryEvent, _> = serde_json::from_str(trimmed);
    let payload = match event {
        Ok(event) => {
            let mapped = match &event {
                TelemetryEvent::HostDiscovered { .. } => "session://host-discovered",
                TelemetryEvent::PairingRequested { .. } => "session://pairing-requested",
                _ => "session://telemetry",
            };
            let body = serde_json::json!({
                "session_id": session_id,
                "event": event,
            });
            let _ = app.emit(mapped, body);
            return;
        }
        Err(error) => serde_json::json!({
            "session_id": session_id,
            "raw": trimmed,
            "parse_error": error.to_string(),
        }),
    };
    let _ = app.emit("session://telemetry", payload);
}

// ── Local state directory resolution ────────────────────────────────

fn resolve_local_state_dir() -> Result<PathBuf, String> {
    for root in candidate_search_roots() {
        for ancestor in root.ancestors() {
            let candidate = ancestor.join(".local");
            if candidate.is_dir() {
                return Ok(candidate);
            }
        }
    }

    Err("failed to locate the workspace .local directory".to_string())
}

fn candidate_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Ok(current_dir) = std::env::current_dir() {
        roots.push(current_dir);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    roots.push(manifest_dir.clone());

    if let Some(parent) = manifest_dir.parent() {
        roots.push(parent.to_path_buf());
    }

    roots
}

fn build_daemon_config() -> qubox_daemon::DaemonConfig {
    let mut config = qubox_daemon::DaemonConfig::default();
    if let Ok(path) = std::env::var("QUBOX_DAEMON_SOCKET") {
        config.socket_path = PathBuf::from(path);
    }
    config
}

async fn connect_daemon() -> Result<IpcClient, String> {
    let config = build_daemon_config();
    IpcClient::connect(&config)
        .await
        .map_err(|error| format!("daemon connect failed: {error}"))
}

// ── Optional: read IPcEvent stream and forward to React ─────────────

/// Spawn a background task that subscribes to daemon `IpcEvent`s and
/// forwards them as `daemon://state-changed` events. The task is
/// tolerant of the daemon being down: it retries on failure and
/// silently exits if it cannot reconnect after a short window.
fn spawn_daemon_bridge(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let config = build_daemon_config();
        let mut client = match IpcClient::connect(&config).await {
            Ok(client) => client,
            Err(_) => {
                return;
            }
        };
        let events: Vec<IpcEvent> = match client.subscribe().await {
            Ok(events) => events,
            Err(_) => return,
        };
        for event in events {
            let payload = serde_json::json!({ "event": event });
            let _ = app.emit("daemon://state-changed", payload);
        }
    });
}

// ── Tauri entrypoint ────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let registry: Arc<Mutex<SessionRegistry>> = Arc::new(Mutex::new(SessionRegistry::default()));

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .manage(registry)
        .setup(|app| {
            let handle = app.handle().clone();
            spawn_daemon_bridge(handle.clone());

            let show_i = MenuItem::with_id(app, "show", "Show Qubox", true, None::<&str>)?;
            let hosts_i = MenuItem::with_id(app, "hosts", "Hosts", true, None::<&str>)?;
            let host_start =
                MenuItem::with_id(app, "host_start", "Start host agent", true, None::<&str>)?;
            let host_stop =
                MenuItem::with_id(app, "host_stop", "Stop host agent", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu =
                Menu::with_items(app, &[&show_i, &hosts_i, &host_start, &host_stop, &quit_i])?;

            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().unwrap_or_else(|| {
                    tauri::image::Image::new_owned(vec![0u8; 4 * 32 * 32], 32, 32)
                }))
                .menu(&menu)
                .tooltip("Qubox")
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "show" | "hosts" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "host_start" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = start_host_agent(None).await;
                            let _ = app.emit(
                                "daemon://state-changed",
                                serde_json::json!({"host":"starting"}),
                            );
                        });
                    }
                    "host_stop" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = stop_host_agent().await;
                            let _ = app.emit(
                                "daemon://state-changed",
                                serde_json::json!({"host":"stopped"}),
                            );
                        });
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_known_hosts,
            discover_lan_hosts,
            start_session,
            start_session_subprocess,
            cancel_session,
            list_active_sessions,
            accept_pairing,
            reject_pairing,
            list_host_pending_pairings,
            host_pairing_decide,
            get_settings,
            create_share_link,
            redeem_share_link,
            kick_session,
            set_setting,
            get_onboarding,
            complete_onboarding,
            cloud_enroll,
            sync_drain_ready,
            start_host_agent,
            stop_host_agent,
            sync_list_ignores,
            sync_add_ignore,
            sync_remove_ignore,
            sync_apply_ignore_preset,
            sync_list_conflicts,
            sync_list_rules,
            sync_list_jobs,
            sync_resolve_conflict,
            sync_add_rule,
            sync_push_now,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_search_roots_include_manifest_dir() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let roots = candidate_search_roots();

        assert!(roots.iter().any(|root| root == manifest_dir));
    }

    #[test]
    fn resolve_qubox_client_cli_path_falls_back_safely() {
        let path = resolve_qubox_client_cli_path();
        assert!(!path.as_os_str().is_empty());
    }

    #[test]
    fn session_registry_insert_and_remove() {
        let mut registry = SessionRegistry::default();
        let host_id = "00000000-0000-0000-0000-000000000000".to_string();
        let session_id = "11111111-1111-1111-1111-111111111111".to_string();
        let (tx, _rx) = oneshot::channel();
        registry.sessions.insert(
            session_id.clone(),
            SessionHandle {
                session_id: session_id.clone(),
                host_id: host_id.clone(),
                pid: None,
                started_at: 0,
                kill_tx: Some(tx),
            },
        );
        assert_eq!(registry.sessions.len(), 1);
        let handle = registry.sessions.remove(&session_id).unwrap();
        assert_eq!(handle.host_id, host_id);
        assert!(registry.sessions.is_empty());
    }

    #[test]
    fn forward_telemetry_line_does_not_panic_on_unparsed_payload() {
        let json = serde_json::from_str::<serde_json::Value>("this is not json");
        assert!(json.is_err());
    }
}
