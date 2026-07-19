//! Localhost control plane for interactive host pairing approval.
//!
//! GUI / tools poll `GET http://127.0.0.1:{port}/pending` and POST decisions
//! to `/decide`. Only bound to loopback.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use qubox_proto::PairingRequested;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPairingView {
    pub request_id: Uuid,
    pub client_peer_id: Uuid,
    pub client_device_id: Uuid,
    pub client_name: String,
    pub client_label: String,
    pub received_at_unix_ms: u64,
}

#[derive(Clone)]
pub struct PairingUiState {
    pending: Arc<Mutex<HashMap<Uuid, PendingPairingView>>>,
    decisions: mpsc::UnboundedSender<(Uuid, bool)>,
    /// Sessions awaiting operator consent (Phase 2 toast gate).
    /// Independent of pairing UI so the operator can approve access
    /// without re-confirming a previously-paired device.
    pending_sessions: Arc<Mutex<HashMap<Uuid, PendingSessionView>>>,
    session_decisions: mpsc::UnboundedSender<(Uuid, bool)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSessionView {
    pub session_id: Uuid,
    pub client_peer_id: Uuid,
    pub client_name: String,
    pub received_at_unix_ms: u64,
}

impl PairingUiState {
    pub fn new(decisions: mpsc::UnboundedSender<(Uuid, bool)>) -> Self {
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            decisions,
            pending_sessions: Arc::new(Mutex::new(HashMap::new())),
            session_decisions: session_tx,
        }
    }

    pub fn new_with_session_decisions(
        decisions: mpsc::UnboundedSender<(Uuid, bool)>,
        session_decisions: mpsc::UnboundedSender<(Uuid, bool)>,
    ) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            decisions,
            pending_sessions: Arc::new(Mutex::new(HashMap::new())),
            session_decisions,
        }
    }

    pub async fn push(&self, request: &PairingRequested) {
        let view = PendingPairingView {
            request_id: request.request_id,
            client_peer_id: request.client.peer_id,
            client_device_id: request.client.device_id,
            client_name: request.client.device_name.clone(),
            client_label: request.client_label.clone(),
            received_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        };
        self.pending.lock().await.insert(request.request_id, view);
    }

    pub async fn remove(&self, request_id: Uuid) {
        self.pending.lock().await.remove(&request_id);
    }

    pub async fn push_session(
        &self,
        session_id: Uuid,
        client_peer_id: Uuid,
        client_name: String,
    ) {
        let view = PendingSessionView {
            session_id,
            client_peer_id,
            client_name,
            received_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        };
        self.pending_sessions.lock().await.insert(session_id, view);
    }

    pub async fn remove_session(&self, session_id: Uuid) {
        self.pending_sessions.lock().await.remove(&session_id);
    }

    pub fn session_decisions_tx(&self) -> mpsc::UnboundedSender<(Uuid, bool)> {
        self.session_decisions.clone()
    }
}

#[derive(Deserialize)]
struct DecideBody {
    request_id: Uuid,
    approved: bool,
}

async fn list_pending(State(state): State<PairingUiState>) -> Json<Vec<PendingPairingView>> {
    let g = state.pending.lock().await;
    let mut v: Vec<_> = g.values().cloned().collect();
    v.sort_by_key(|p| p.received_at_unix_ms);
    Json(v)
}

async fn decide(
    State(state): State<PairingUiState>,
    Json(body): Json<DecideBody>,
) -> Json<serde_json::Value> {
    let _ = state.decisions.send((body.request_id, body.approved));
    state.pending.lock().await.remove(&body.request_id);
    Json(serde_json::json!({ "ok": true }))
}

async fn list_pending_sessions(
    State(state): State<PairingUiState>,
) -> Json<Vec<PendingSessionView>> {
    let g = state.pending_sessions.lock().await;
    let mut v: Vec<_> = g.values().cloned().collect();
    v.sort_by_key(|p| p.received_at_unix_ms);
    Json(v)
}

#[derive(Deserialize)]
struct DecideSessionBody {
    session_id: Uuid,
    approved: bool,
}

async fn decide_session(
    State(state): State<PairingUiState>,
    Json(body): Json<DecideSessionBody>,
) -> Json<serde_json::Value> {
    let _ = state.session_decisions.send((body.session_id, body.approved));
    state.pending_sessions.lock().await.remove(&body.session_id);
    Json(serde_json::json!({ "ok": true }))
}

/// Bind loopback control server. Returns the bound port.
pub async fn spawn_pairing_ui(state: PairingUiState, preferred_port: u16) -> anyhow::Result<u16> {
    let app = Router::new()
        .route("/pending", get(list_pending))
        .route("/decide", post(decide))
        .route("/pending-sessions", get(list_pending_sessions))
        .route("/decide-session", post(decide_session))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], preferred_port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(_) => {
            // fall back to ephemeral
            tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?
        }
    };
    let port = listener.local_addr()?.port();
    tracing::info!(%port, "host pairing UI control listening on 127.0.0.1");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!(error = %e, "pairing UI server exited");
        }
    });
    // Advertise port for GUI
    if let Some(dir) = directories::ProjectDirs::from("app", "Qubox", "qubox") {
        let path = dir.data_local_dir().join("host_pairing_port");
        let _ = std::fs::create_dir_all(dir.data_local_dir());
        let _ = std::fs::write(&path, port.to_string());
    }
    Ok(port)
}
