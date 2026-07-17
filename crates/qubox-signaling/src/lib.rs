use std::{
    collections::HashMap,
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{header, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use qubox_proto::{
    ClientMessage, ErrorMessage, IceServer, PairingDecision, PairingGrant, PairingRequest,
    PairingRequested, PeerDescriptor, PeerRole, PresenceEvent, RelaySignal, ServerMessage,
    SessionCredential, SessionPermissions, SessionPlan, SessionRequested, StartSessionRequest,
    TransportKind, VideoCodec, Welcome,
};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};
use uuid::Uuid;

pub mod cluster;
#[cfg(feature = "webtransport")]
pub mod webtransport;

/// Environment variable that holds the server secret used to sign
/// `SessionCredential` HMACs and gate TURN issuance. When absent the
/// server picks a random secret at startup (with a loud warning) so
/// existing sessions are not forward-portable across restarts.
const SIGNALING_SECRET_ENV: &str = "QUBOX_SIGNALING_SECRET";

/// Environment variable that, when set to `1`/`true`, REJECTS any peer
/// whose handshake is an unsigned `Hello` (must be `SignedHello`).
/// Default is OFF (unsigned accepted) so LAN and existing tests keep
/// working. Production servers should set
/// `QUBOX_REQUIRE_SIGNED_HELLO=1` — once both the host and the
/// client speak `SignedHello`, the auth chain is closed end-to-end.
const REQUIRE_SIGNED_HELLO_ENV: &str = "QUBOX_REQUIRE_SIGNED_HELLO";

#[derive(Clone)]
pub struct SignalingState {
    peers: Arc<RwLock<HashMap<Uuid, ConnectedPeer>>>,
    pending_pairings: Arc<RwLock<HashMap<Uuid, PendingPairing>>>,
    sessions: Arc<RwLock<HashMap<Uuid, ActiveSession>>>,
    ice_servers: Arc<Vec<IceServer>>,
    pairing_store: PairingStore,
    turn_relays: Arc<RwLock<HashMap<Uuid, TurnRelayEntry>>>,
    /// Server-side secret used to bind `SessionCredential`s to two
    /// device pubkeys and to gate TURN issuance. Loaded from
    /// `QUBOX_SIGNALING_SECRET`; random per-process if missing.
    server_secret: Arc<Vec<u8>>,
    /// When `false`, unsigned `Hello` handshakes are rejected at the
    /// websocket boundary with a clear error so the caller can fall
    /// back to `SignedHello`. Default is `true` (i.e. unsigned is
    /// currently ALLOWED) to avoid breaking the LAN self-host mode
    /// and existing test harnesses. Production servers are expected
    /// to construct their state with `allow_unsigned_hello(false)` —
    /// see `with_options_and_secret_and_policy`.
    allow_unsigned_hello: bool,
    /// Short-lived share codes → host + permissions.
    share_links: Arc<RwLock<HashMap<String, ShareLinkEntry>>>,
    /// Optional device→tenant lookup (Open = self-host; Enforced via trait).
    enrollment: EnrollmentPolicy,
    /// Optional Redis cluster bus for multi-instance deployments.
    cluster: Option<Arc<cluster::ClusterBus>>,
}

/// How the server resolves tenant membership for connecting peers.
#[derive(Clone, Default)]
pub enum EnrollmentPolicy {
    /// Self-host / tests: any peer joins the nil tenant; no external check.
    #[default]
    Open,
    /// Every SignedHello must resolve via `lookup` to a non-revoked
    /// enrolled device. Presence/hosts/pairing are scoped to that
    /// device's tenant. Integrators supply `lookup` (e.g. private Cloud).
    Managed {
        lookup: Arc<dyn DeviceEnrollmentLookup>,
    },
}

/// Result of looking up a device in an external enrollment store.
#[derive(Debug, Clone)]
pub struct EnrolledDevice {
    pub tenant_id: Uuid,
    pub account_id: Uuid,
    pub revoked: bool,
}

/// Async device enrollment lookup (implemented outside the stock Open binary).
pub trait DeviceEnrollmentLookup: Send + Sync + 'static {
    fn lookup(
        &self,
        device_id: Uuid,
        public_key: [u8; 32],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<EnrolledDevice>, String>> + Send + '_>,
    >;
}

impl EnrollmentPolicy {
    pub fn is_managed(&self) -> bool {
        matches!(self, Self::Managed { .. })
    }
}

#[derive(Debug, Clone)]
struct ShareLinkEntry {
    host_peer_id: Uuid,
    #[allow(dead_code)]
    host_label: String,
    #[allow(dead_code)]
    permissions: SessionPermissions,
    expires_unix_ms: u64,
}

/// A peer-advertised TURN relay address.
#[derive(Debug, Clone)]
struct TurnRelayEntry {
    relay_address: SocketAddr,
    updated_at_unix_millis: u64,
}

impl Default for SignalingState {
    fn default() -> Self {
        let allow_unsigned = !env_bool(REQUIRE_SIGNED_HELLO_ENV).unwrap_or(false);
        Self {
            peers: Arc::new(RwLock::new(HashMap::new())),
            pending_pairings: Arc::new(RwLock::new(HashMap::new())),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            ice_servers: Arc::new(Vec::new()),
            pairing_store: PairingStore::memory(),
            turn_relays: Arc::new(RwLock::new(HashMap::new())),
            server_secret: Arc::new(generate_test_server_secret()),
            allow_unsigned_hello: allow_unsigned,
            share_links: Arc::new(RwLock::new(HashMap::new())),
            enrollment: EnrollmentPolicy::Open,
            cluster: None,
        }
    }
}

/// Parse `name` as a boolean env var. Accepts `1`, `true`, `yes`, `on`
/// (case-insensitive) as truthy; `0`, `false`, `no`, `off`, empty as
/// falsy. Treats unset variables as a `None` so the caller can apply
/// its own default.
fn env_bool(name: &str) -> Option<bool> {
    let raw = std::env::var(name).ok()?;
    let v = raw.trim().to_ascii_lowercase();
    Some(match v.as_str() {
        "" | "0" | "false" | "no" | "off" => false,
        "1" | "true" | "yes" | "on" => true,
        _ => return None,
    })
}

struct ConnectedPeer {
    descriptor: PeerDescriptor,
    outbound: mpsc::UnboundedSender<ServerMessage>,
    /// Public key from the `SignedHello` (or `None` for legacy
    /// unsigned `Hello` peers, who are auth-downgraded but still
    /// registered for backward compatibility on the LAN self-host mode).
    public_key: Option<[u8; 32]>,
    /// Tenant namespace for managed isolation (`Uuid::nil()` = self-host).
    tenant_id: Uuid,
}

#[derive(Debug, Clone)]
struct PendingPairing {
    request_id: Uuid,
    host_peer_id: Uuid,
    client: PeerDescriptor,
    client_label: String,
}

#[derive(Debug, Clone)]
struct ActiveSession {
    host_peer_id: Uuid,
    client_peer_id: Uuid,
    #[allow(dead_code)]
    transport: TransportKind,
    #[allow(dead_code)]
    codec: VideoCodec,
    #[allow(dead_code)]
    host_credential: SessionCredential,
    #[allow(dead_code)]
    client_credential: SessionCredential,
    expires_unix_millis: u64,
    #[allow(dead_code)]
    permissions: SessionPermissions,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PairingStoreState {
    pairings: Vec<PairingGrant>,
}

#[derive(Clone)]
struct PairingStore {
    path: Option<PathBuf>,
    state: Arc<RwLock<PairingStoreState>>,
}

impl PairingStore {
    fn memory() -> Self {
        Self {
            path: None,
            state: Arc::new(RwLock::new(PairingStoreState::default())),
        }
    }

    fn from_path(path: PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let state = if path.exists() {
            serde_json::from_str(&fs::read_to_string(&path)?)?
        } else {
            PairingStoreState::default()
        };

        Ok(Self {
            path: Some(path),
            state: Arc::new(RwLock::new(state)),
        })
    }

    async fn is_paired(&self, host_peer_id: Uuid, client_peer_id: Uuid) -> bool {
        self.state.read().await.pairings.iter().any(|grant| {
            grant.host_peer_id == host_peer_id && grant.client_peer_id == client_peer_id
        })
    }

    async fn add_pairing(&self, grant: PairingGrant) -> anyhow::Result<()> {
        let snapshot = {
            let mut state = self.state.write().await;

            if !state.pairings.contains(&grant) {
                state.pairings.push(grant);
            }

            state.clone()
        };

        self.persist(&snapshot)
    }

    async fn remove_pairing(
        &self,
        host_peer_id: Uuid,
        client_peer_id: Uuid,
    ) -> anyhow::Result<bool> {
        let (snapshot, removed) = {
            let mut state = self.state.write().await;
            let before = state.pairings.len();
            state.pairings.retain(|g| {
                !(g.host_peer_id == host_peer_id && g.client_peer_id == client_peer_id)
            });
            let removed = state.pairings.len() != before;
            (state.clone(), removed)
        };
        if removed {
            self.persist(&snapshot)?;
        }
        Ok(removed)
    }

    #[allow(dead_code)]
    async fn list_pairings(&self) -> Vec<PairingGrant> {
        self.state.read().await.pairings.clone()
    }

    /// Persist pairings via temp file + fsync + rename (atomic on POSIX).
    /// SECURITY: grants are still plaintext JSON; encrypt path or use a secrets
    /// manager for multi-tenant production. Do not store in world-writable dirs.
    fn persist(&self, state: &PairingStoreState) -> anyhow::Result<()> {
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("json.tmp");
            {
                use std::io::Write;
                let mut f = fs::File::create(&tmp)?;
                f.write_all(serde_json::to_string_pretty(state)?.as_bytes())?;
                f.sync_all()?;
            }
            fs::rename(&tmp, path)?;
            if let Some(parent) = path.parent() {
                if let Ok(dir) = fs::File::open(parent) {
                    let _ = dir.sync_all();
                }
            }
        }
        Ok(())
    }
}

pub fn load_pairings_from_path(path: PathBuf) -> anyhow::Result<Vec<PairingGrant>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let state: PairingStoreState = serde_json::from_str(&fs::read_to_string(&path)?)?;
    Ok(state.pairings)
}

impl SignalingState {
    pub fn with_options(
        pairing_store_path: Option<PathBuf>,
        ice_servers: Vec<IceServer>,
    ) -> anyhow::Result<Self> {
        Self::with_options_and_secret(pairing_store_path, ice_servers, load_server_secret())
    }

    pub fn with_options_and_secret(
        pairing_store_path: Option<PathBuf>,
        ice_servers: Vec<IceServer>,
        server_secret: Vec<u8>,
    ) -> anyhow::Result<Self> {
        Self::with_options_and_secret_and_policy(
            pairing_store_path,
            ice_servers,
            server_secret,
            // Production-aligned default: require SignedHello unless the
            // caller (typically the signaling-server binary) says
            // otherwise. Tests can opt back in via this constructor.
            !env_bool(REQUIRE_SIGNED_HELLO_ENV).unwrap_or(true),
        )
    }

    /// Full-control constructor. Use this from the signaling-server
    /// binary to override the env-var default (e.g. disable
    /// `allow_unsigned_hello` on production builds).
    pub fn with_options_and_secret_and_policy(
        pairing_store_path: Option<PathBuf>,
        ice_servers: Vec<IceServer>,
        server_secret: Vec<u8>,
        allow_unsigned_hello: bool,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            peers: Arc::new(RwLock::new(HashMap::new())),
            pending_pairings: Arc::new(RwLock::new(HashMap::new())),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            ice_servers: Arc::new(ice_servers),
            pairing_store: match pairing_store_path {
                Some(path) => PairingStore::from_path(path)?,
                None => PairingStore::memory(),
            },
            turn_relays: Arc::new(RwLock::new(HashMap::new())),
            server_secret: Arc::new(server_secret),
            allow_unsigned_hello,
            share_links: Arc::new(RwLock::new(HashMap::new())),
            enrollment: EnrollmentPolicy::Open,
            cluster: None,
        })
    }

    /// Attach enrollment policy (device → tenant lookup). Stock server stays Open.
    pub fn with_enrollment(mut self, enrollment: EnrollmentPolicy) -> Self {
        self.enrollment = enrollment;
        self
    }

    pub fn enrollment(&self) -> &EnrollmentPolicy {
        &self.enrollment
    }

    /// Enable Redis multi-instance coordination.
    pub fn with_cluster(mut self, bus: Arc<cluster::ClusterBus>) -> Self {
        self.cluster = Some(bus);
        self
    }

    pub fn cluster_enabled(&self) -> bool {
        self.cluster.is_some()
    }

    /// Number of static ICE server entries advertised in session plans.
    pub fn ice_server_count(&self) -> usize {
        self.ice_servers.len()
    }

    /// Start Redis pub/sub delivery into local WebSocket peers.
    pub fn start_cluster_listener(&self, redis_url: String) {
        let Some(bus) = self.cluster.clone() else {
            return;
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<(Uuid, ServerMessage)>();
        bus.spawn_listener(redis_url, tx);
        let peers = self.peers.clone();
        tokio::spawn(async move {
            while let Some((peer_id, message)) = rx.recv().await {
                if peer_id.is_nil() {
                    // Presence fanout: deliver to all local peers.
                    let g = peers.read().await;
                    for peer in g.values() {
                        let _ = peer.outbound.send(message.clone());
                    }
                } else {
                    let g = peers.read().await;
                    if let Some(peer) = g.get(&peer_id) {
                        let _ = peer.outbound.send(message);
                    }
                }
            }
        });
    }

    pub fn with_pairing_store(path: PathBuf) -> anyhow::Result<Self> {
        Self::with_options(Some(path), Vec::new())
    }

    /// Manually override the unsigned-Hello policy (e.g. tests that
    /// want to exercise the rejection path).
    pub fn set_allow_unsigned_hello(&mut self, allow: bool) {
        self.allow_unsigned_hello = allow;
    }

    /// `true` iff unsigned `Hello` handshakes are currently permitted.
    pub fn allows_unsigned_hello(&self) -> bool {
        self.allow_unsigned_hello
    }

    /// Borrow the server secret for callers that need to issue
    /// `SessionCredential` HMACs out-of-band (e.g. the TURN handler).
    pub fn server_secret(&self) -> &[u8] {
        &self.server_secret
    }

    /// Resolve a connected peer's registered Ed25519 public key (if it
    /// connected via `SignedHello`). Returns `None` for unknown peers
    /// or peers that presented an unsigned `Hello`.
    pub async fn peer_pubkey(&self, peer_id: Uuid) -> Option<[u8; 32]> {
        self.peers.read().await.get(&peer_id)?.public_key
    }

    pub fn router(self) -> Router {
        macro_rules! base_routes {
            ($router:expr) => {
                $router
                    .route("/healthz", get(healthz))
                    // Not under /v1/* — managed Caddy routes /v1/* to accounts API.
                    .route("/status", get(status_handler))
                    .route("/v1/status", get(status_handler))
                    .route("/ws", get(ws_handler))
                    .route(
                        "/v1/turn/relay-address",
                        post(publish_relay_address_handler),
                    )
                    .route(
                        "/v1/turn/relay-address/{peer_id}",
                        get(get_relay_address_handler),
                    )
            };
        }

        let r = base_routes!(Router::new());

        #[cfg(feature = "webtransport")]
        let r = r.route("/v1/webtransport/cert", get(webtransport::cert_handler));

        r.with_state(self)
    }

    async fn register(
        &self,
        descriptor: PeerDescriptor,
        public_key: Option<[u8; 32]>,
        tenant_id: Uuid,
        outbound: mpsc::UnboundedSender<ServerMessage>,
    ) -> anyhow::Result<()> {
        let mut peers = self.peers.write().await;

        if peers.contains_key(&descriptor.peer_id) {
            bail!("peer {} is already connected", descriptor.peer_id);
        }

        let peer_id = descriptor.peer_id;
        let desc_for_cluster = descriptor.clone();
        peers.insert(
            peer_id,
            ConnectedPeer {
                descriptor,
                outbound,
                public_key,
                tenant_id,
            },
        );
        drop(peers);

        if let Some(bus) = &self.cluster {
            if let Err(e) = bus
                .register_peer(peer_id, tenant_id, &desc_for_cluster, public_key)
                .await
            {
                warn!(?e, %peer_id, "cluster register_peer failed");
            }
        }

        Ok(())
    }

    async fn resolve_enrollment(
        &self,
        device_id: Uuid,
        public_key: [u8; 32],
    ) -> anyhow::Result<Uuid> {
        match &self.enrollment {
            EnrollmentPolicy::Open => Ok(Uuid::nil()),
            EnrollmentPolicy::Managed { lookup } => {
                let info = lookup
                    .lookup(device_id, public_key)
                    .await
                    .map_err(|e| anyhow!("enrollment lookup failed: {e}"))?
                    .ok_or_else(|| {
                        anyhow!("device {device_id} is not enrolled for this cloud tenant")
                    })?;
                if info.revoked {
                    bail!("device {device_id} has been revoked");
                }
                Ok(info.tenant_id)
            }
        }
    }

    async fn peer_tenant(&self, peer_id: Uuid) -> Option<Uuid> {
        self.peers.read().await.get(&peer_id).map(|p| p.tenant_id)
    }

    async fn unregister(&self, peer_id: Uuid) -> Option<PeerDescriptor> {
        let removed = self.peers.write().await.remove(&peer_id);
        if let Some(ref peer) = removed {
            if let Some(bus) = &self.cluster {
                let was_host = peer.descriptor.role == PeerRole::Host;
                if let Err(e) = bus.unregister_peer(peer_id, peer.tenant_id, was_host).await {
                    warn!(?e, %peer_id, "cluster unregister_peer failed");
                }
            }
        }
        removed.map(|peer| peer.descriptor)
    }

    async fn remove_sessions_for(&self, peer_id: Uuid) {
        self.sessions.write().await.retain(|_, session| {
            session.host_peer_id != peer_id && session.client_peer_id != peer_id
        });
    }

    async fn prune_expired_sessions(&self) {
        let now = unix_millis_after(Duration::ZERO);
        self.sessions
            .write()
            .await
            .retain(|_, session| session.expires_unix_millis > now);
    }

    /// Publish or update a peer's TURN relay address.
    async fn set_turn_relay(&self, peer_id: Uuid, relay_address: SocketAddr) {
        self.turn_relays.write().await.insert(
            peer_id,
            TurnRelayEntry {
                relay_address,
                updated_at_unix_millis: unix_millis_after(Duration::ZERO),
            },
        );
    }

    /// Look up a peer's TURN relay address, if one is registered.
    async fn get_turn_relay(&self, peer_id: Uuid) -> Option<SocketAddr> {
        self.turn_relays
            .read()
            .await
            .get(&peer_id)
            .map(|entry| entry.relay_address)
    }

    /// Remove a peer's TURN relay address (e.g. on disconnect).
    #[allow(dead_code)]
    async fn remove_turn_relay(&self, peer_id: Uuid) {
        self.turn_relays.write().await.remove(&peer_id);
    }

    /// Remove TURN relay entries older than `max_age`.
    #[allow(dead_code)]
    async fn prune_turn_relays(&self, max_age: Duration) {
        let cutoff = unix_millis_after(Duration::ZERO)
            .saturating_sub(max_age.as_millis().min(u128::from(u64::MAX)) as u64);
        self.turn_relays
            .write()
            .await
            .retain(|_, entry| entry.updated_at_unix_millis > cutoff);
    }

    async fn list_hosts(&self, viewer_tenant: Uuid) -> Vec<PeerDescriptor> {
        let mut hosts: Vec<PeerDescriptor> = {
            let peers = self.peers.read().await;
            peers
                .values()
                .filter(|peer| {
                    peer.descriptor.role == PeerRole::Host && peer.tenant_id == viewer_tenant
                })
                .map(|peer| peer.descriptor.clone())
                .collect()
        };
        if let Some(bus) = &self.cluster {
            if let Ok(remote) = bus.list_hosts(viewer_tenant).await {
                for d in remote {
                    if !hosts.iter().any(|h| h.peer_id == d.peer_id) {
                        hosts.push(d);
                    }
                }
            }
        }
        hosts
    }

    async fn send_to(&self, peer_id: Uuid, message: ServerMessage) -> anyhow::Result<()> {
        {
            let peers = self.peers.read().await;
            if let Some(peer) = peers.get(&peer_id) {
                return peer
                    .outbound
                    .send(message)
                    .map_err(|_| anyhow!("peer {peer_id} is no longer writable"));
            }
        }
        if let Some(bus) = &self.cluster {
            return bus.deliver(peer_id, message).await;
        }
        Err(anyhow!("unknown peer {peer_id}"))
    }

    async fn broadcast_presence(
        &self,
        descriptor: PeerDescriptor,
        tenant_id: Uuid,
        connected: bool,
    ) {
        let message = ServerMessage::Presence(PresenceEvent {
            peer: descriptor.clone(),
            connected,
        });

        {
            let peers = self.peers.read().await;
            for (peer_id, peer) in peers.iter() {
                if *peer_id == descriptor.peer_id {
                    continue;
                }
                // Tenant isolation: never leak presence across workspaces.
                if peer.tenant_id != tenant_id {
                    continue;
                }

                let _ = peer.outbound.send(message.clone());
            }
        }

        if let Some(bus) = &self.cluster {
            // Other instances filter by tenant when applying if needed;
            // presence event carries the peer descriptor for clients.
            let _ = bus.publish_presence(message).await;
        }
    }

    async fn request_pairing(
        &self,
        client: PeerDescriptor,
        request: PairingRequest,
    ) -> anyhow::Result<()> {
        let client_tenant = self
            .peer_tenant(client.peer_id)
            .await
            .ok_or_else(|| anyhow!("client {} is not connected", client.peer_id))?;

        // Host may be local or on another instance.
        let host_ok = {
            let peers = self.peers.read().await;
            if let Some(host) = peers.get(&request.host_peer_id) {
                if host.descriptor.role != PeerRole::Host {
                    bail!("target {} is not a host", request.host_peer_id);
                }
                if host.tenant_id != client_tenant {
                    bail!("host and client are not in the same tenant");
                }
                true
            } else {
                false
            }
        };
        if !host_ok {
            if let Some(bus) = &self.cluster {
                let remote = bus
                    .get_peer(request.host_peer_id)
                    .await?
                    .ok_or_else(|| anyhow!("host {} is not connected", request.host_peer_id))?;
                if remote.descriptor.role != PeerRole::Host {
                    bail!("target {} is not a host", request.host_peer_id);
                }
                if remote.tenant_id != client_tenant {
                    bail!("host and client are not in the same tenant");
                }
            } else {
                bail!("host {} is not connected", request.host_peer_id);
            }
        }

        let pending = PendingPairing {
            request_id: request.request_id,
            host_peer_id: request.host_peer_id,
            client: client.clone(),
            client_label: request.client_label,
        };

        self.pending_pairings
            .write()
            .await
            .insert(pending.request_id, pending.clone());

        self.send_to(
            pending.host_peer_id,
            ServerMessage::PairingRequested(PairingRequested {
                request_id: pending.request_id,
                host_peer_id: pending.host_peer_id,
                client,
                client_label: pending.client_label,
            }),
        )
        .await
    }

    async fn decide_pairing(
        &self,
        host: PeerDescriptor,
        decision: PairingDecision,
    ) -> anyhow::Result<Option<PairingGrant>> {
        let pending = self
            .pending_pairings
            .write()
            .await
            .remove(&decision.request_id)
            .ok_or_else(|| anyhow!("pairing request {} is unknown", decision.request_id))?;

        if host.role != PeerRole::Host {
            bail!("only hosts can approve pairing requests");
        }

        if pending.host_peer_id != host.peer_id {
            bail!(
                "pairing request {} belongs to another host",
                decision.request_id
            );
        }

        if !decision.approved {
            let _ = self
                .send_to(
                    pending.client.peer_id,
                    ServerMessage::PairingRejected {
                        request_id: decision.request_id,
                        reason: "host rejected pairing".to_string(),
                    },
                )
                .await;
            return Ok(None);
        }

        let grant = PairingGrant {
            host_peer_id: pending.host_peer_id,
            client_peer_id: pending.client.peer_id,
        };

        self.pairing_store.add_pairing(grant.clone()).await?;
        if let Some(bus) = &self.cluster {
            let _ = bus
                .put_pairing(grant.host_peer_id, grant.client_peer_id)
                .await;
        }

        let _ = self
            .send_to(
                pending.client.peer_id,
                ServerMessage::PairingEstablished(grant.clone()),
            )
            .await;

        Ok(Some(grant))
    }

    async fn is_paired_cluster(&self, host_peer_id: Uuid, client_peer_id: Uuid) -> bool {
        if self
            .pairing_store
            .is_paired(host_peer_id, client_peer_id)
            .await
        {
            return true;
        }
        if let Some(bus) = &self.cluster {
            return bus
                .is_paired(host_peer_id, client_peer_id)
                .await
                .unwrap_or(false);
        }
        false
    }

    async fn start_session(
        &self,
        client: PeerDescriptor,
        request: StartSessionRequest,
    ) -> anyhow::Result<SessionPlan> {
        self.prune_expired_sessions().await;

        let (client_tenant, client_pubkey) = {
            let peers = self.peers.read().await;
            let c = peers
                .get(&client.peer_id)
                .ok_or_else(|| anyhow!("client {} is not connected", client.peer_id))?;
            (c.tenant_id, c.public_key)
        };

        let (host_descriptor, host_pubkey) = {
            let peers = self.peers.read().await;
            if let Some(host) = peers.get(&request.target_host_id) {
                if host.descriptor.role != PeerRole::Host {
                    bail!("target {} is not a host", request.target_host_id);
                }
                if host.tenant_id != client_tenant {
                    bail!("host and client are not in the same tenant");
                }
                (host.descriptor.clone(), host.public_key)
            } else if let Some(bus) = &self.cluster {
                drop(peers);
                let remote = bus
                    .get_peer(request.target_host_id)
                    .await?
                    .ok_or_else(|| anyhow!("host {} is not connected", request.target_host_id))?;
                if remote.descriptor.role != PeerRole::Host {
                    bail!("target {} is not a host", request.target_host_id);
                }
                if remote.tenant_id != client_tenant {
                    bail!("host and client are not in the same tenant");
                }
                (remote.descriptor, remote.public_key)
            } else {
                bail!("host {} is not connected", request.target_host_id);
            }
        };

        if !self
            .is_paired_cluster(request.target_host_id, client.peer_id)
            .await
        {
            bail!(
                "client {} is not paired with host {}",
                client.peer_id,
                request.target_host_id
            );
        }

        let transport = negotiate_transport(&client, &host_descriptor, request.requested_transport)
            .ok_or_else(|| anyhow!("host and client do not share a transport"))?;
        let codec = negotiate_codec(&client, &host_descriptor, request.preferred_codec)
            .ok_or_else(|| anyhow!("host and client do not share a codec"))?;

        // Both peers must have a `SignedHello` pubkey on record for
        // the credential to be bound. The transport layer verifies the
        // HMAC over (session_id, host_pk, client_pk, exp), so we cannot
        // issue a meaningful credential if either side is anonymous.
        let host_pubkey = host_pubkey.ok_or_else(|| {
            anyhow!(
                "host {} did not send a SignedHello; cannot bind session credential",
                request.target_host_id
            )
        })?;
        let client_pubkey = client_pubkey.ok_or_else(|| {
            anyhow!(
                "client {} did not send a SignedHello; cannot bind session credential",
                client.peer_id
            )
        })?;

        let client_peer_id = client.peer_id;
        let issued_unix_millis = unix_millis_after(Duration::ZERO);
        let expires_unix_millis = issued_unix_millis.saturating_add(SESSION_TOKEN_TTL_MILLIS);
        let host_credential = SessionCredential::issue(
            &self.server_secret,
            request.session_id,
            host_pubkey,
            client_pubkey,
            issued_unix_millis,
            expires_unix_millis,
        );
        let client_credential = host_credential.clone();
        let ice_servers = (*self.ice_servers).clone();

        let permissions = request.permissions.clone();
        let sync_only = request.sync_only;
        let video = request.video;

        self.send_to(
            request.target_host_id,
            ServerMessage::SessionRequested(Box::new(SessionRequested {
                session_id: request.session_id,
                client,
                transport,
                codec,
                host_credential: host_credential.clone(),
                client_credential: client_credential.clone(),
                ice_servers: ice_servers.clone(),
                video,
                permissions: permissions.clone(),
                sync_only,
            })),
        )
        .await?;

        self.sessions.write().await.insert(
            request.session_id,
            ActiveSession {
                host_peer_id: request.target_host_id,
                client_peer_id,
                transport,
                codec,
                host_credential: host_credential.clone(),
                client_credential: client_credential.clone(),
                expires_unix_millis,
                permissions: permissions.clone(),
            },
        );
        if let Some(bus) = &self.cluster {
            let _ = bus
                .put_session(
                    request.session_id,
                    &cluster::RemoteSession {
                        host_peer_id: request.target_host_id,
                        client_peer_id,
                        expires_unix_millis,
                    },
                )
                .await;
        }

        Ok(SessionPlan {
            session_id: request.session_id,
            target_host_id: request.target_host_id,
            transport,
            codec,
            client_credential,
            ice_servers,
            permissions,
            sync_only,
        })
    }

    async fn revoke_pairing(
        &self,
        actor: PeerDescriptor,
        host_peer_id: Uuid,
        client_peer_id: Uuid,
    ) -> anyhow::Result<()> {
        if actor.peer_id != host_peer_id {
            bail!("only host {} may revoke this grant", host_peer_id);
        }
        let removed = self
            .pairing_store
            .remove_pairing(host_peer_id, client_peer_id)
            .await?;
        if !removed {
            bail!("no pairing between {} and {}", host_peer_id, client_peer_id);
        }
        // Drop active sessions for this pair.
        {
            let mut sessions = self.sessions.write().await;
            sessions.retain(|_, s| {
                !(s.host_peer_id == host_peer_id && s.client_peer_id == client_peer_id)
            });
        }
        let msg = ServerMessage::PairingRevoked {
            host_peer_id,
            client_peer_id,
        };
        let _ = self.send_to(client_peer_id, msg.clone()).await;
        let _ = self.send_to(host_peer_id, msg).await;
        Ok(())
    }

    async fn kick_session(
        &self,
        actor: PeerDescriptor,
        session_id: Uuid,
        reason: String,
    ) -> anyhow::Result<()> {
        let session = self
            .sessions
            .write()
            .await
            .remove(&session_id)
            .ok_or_else(|| anyhow!("session {} not found", session_id))?;
        if actor.peer_id != session.host_peer_id && actor.peer_id != session.client_peer_id {
            // put back if unauthorized
            self.sessions.write().await.insert(session_id, session);
            bail!("peer is not a participant in session {}", session_id);
        }
        let msg = ServerMessage::SessionKicked {
            session_id,
            reason: if reason.is_empty() {
                "kicked".into()
            } else {
                reason
            },
        };
        let _ = self.send_to(session.host_peer_id, msg.clone()).await;
        let _ = self.send_to(session.client_peer_id, msg).await;
        Ok(())
    }

    async fn create_share_link(
        &self,
        host: PeerDescriptor,
        ttl_secs: u64,
        permissions: SessionPermissions,
    ) -> anyhow::Result<(String, u64, String)> {
        if host.role != PeerRole::Host {
            bail!("only hosts can create share links");
        }
        let ttl = if ttl_secs == 0 {
            900
        } else {
            ttl_secs.min(86_400)
        };
        let now = unix_millis_after(Duration::ZERO);
        let expires = now.saturating_add(ttl.saturating_mul(1000));
        // 8 hex chars from random
        let mut raw = [0u8; 4];
        OsRng.fill_bytes(&mut raw);
        let code = hex::encode(raw);
        self.share_links.write().await.insert(
            code.clone(),
            ShareLinkEntry {
                host_peer_id: host.peer_id,
                host_label: host.device_name.clone(),
                permissions,
                expires_unix_ms: expires,
            },
        );
        let url_hint = format!("qubox://pair?code={code}");
        Ok((code, expires, url_hint))
    }

    async fn redeem_share_link(
        &self,
        client: PeerDescriptor,
        code: String,
        client_label: String,
    ) -> anyhow::Result<()> {
        let entry = {
            let mut links = self.share_links.write().await;
            let e = links
                .get(&code)
                .cloned()
                .ok_or_else(|| anyhow!("unknown or expired share code"))?;
            if e.expires_unix_ms <= unix_millis_after(Duration::ZERO) {
                links.remove(&code);
                bail!("share code expired");
            }
            // one-time redeem
            links.remove(&code);
            e
        };
        let request = PairingRequest {
            request_id: Uuid::new_v4(),
            host_peer_id: entry.host_peer_id,
            client_label: if client_label.is_empty() {
                client.device_name.clone()
            } else {
                client_label
            },
        };
        self.request_pairing(client, request).await
    }

    async fn relay_signal(&self, peer: PeerDescriptor, signal: RelaySignal) -> anyhow::Result<()> {
        if signal.from_peer_id != peer.peer_id {
            bail!("relay signal source does not match the connected peer");
        }

        self.prune_expired_sessions().await;

        let (host_peer_id, client_peer_id, expires_unix_millis) = {
            if let Some(session) = self.sessions.read().await.get(&signal.session_id).cloned() {
                (
                    session.host_peer_id,
                    session.client_peer_id,
                    session.expires_unix_millis,
                )
            } else if let Some(bus) = &self.cluster {
                let remote = bus
                    .get_session(signal.session_id)
                    .await?
                    .ok_or_else(|| anyhow!("session {} is not active", signal.session_id))?;
                (
                    remote.host_peer_id,
                    remote.client_peer_id,
                    remote.expires_unix_millis,
                )
            } else {
                bail!("session {} is not active", signal.session_id);
            }
        };

        if expires_unix_millis <= unix_millis_after(Duration::ZERO) {
            self.sessions.write().await.remove(&signal.session_id);
            bail!("session {} has expired", signal.session_id);
        }

        let peer_is_client_to_host =
            peer.peer_id == client_peer_id && signal.to_peer_id == host_peer_id;
        let peer_is_host_to_client =
            peer.peer_id == host_peer_id && signal.to_peer_id == client_peer_id;

        if !peer_is_client_to_host && !peer_is_host_to_client {
            bail!(
                "peer {} is not a participant in session {}",
                peer.peer_id,
                signal.session_id
            );
        }

        if !self.is_paired_cluster(host_peer_id, client_peer_id).await {
            bail!(
                "peer {} is not paired with target {}",
                peer.peer_id,
                signal.to_peer_id
            );
        }

        debug!(
            session_id = %signal.session_id,
            from_peer_id = %signal.from_peer_id,
            to_peer_id = %signal.to_peer_id,
            session_expires = expires_unix_millis,
            "relaying session signal"
        );

        self.send_to(signal.to_peer_id, ServerMessage::Signal(signal))
            .await
    }
}

impl SignalingState {
    /// Generate or retrieve the WebTransport cert SHA-256(DER) hash.
    #[cfg(feature = "webtransport")]
    pub async fn webtransport_cert_hash(&self) -> anyhow::Result<serde_json::Value> {
        use qubox_webtransport::cert;
        let (_, _, hash) = cert::generate_self_signed()?;
        Ok(serde_json::json!({
            "hash": hex::encode(hash),
            "algorithm": "sha-256",
        }))
    }
}

async fn healthz() -> &'static str {
    "ok"
}

async fn status_handler(
    State(state): State<SignalingState>,
    headers: axum::http::HeaderMap,
) -> Json<serde_json::Value> {
    let admin_token = std::env::var("QUBOX_ADMIN_TOKEN").ok();
    let is_admin = if let Some(ref token) = admin_token {
        if let Some(auth_header) = headers.get(axum::http::header::AUTHORIZATION) {
            if let Ok(auth_str) = auth_header.to_str() {
                let check_token = auth_str.strip_prefix("Bearer ").unwrap_or(auth_str);
                !check_token.is_empty() && check_token == token
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    if is_admin {
        let peers = state.peers.read().await;
        let mut hosts = 0usize;
        let mut clients = 0usize;
        let mut signed = 0usize;
        for p in peers.values() {
            if p.public_key.is_some() {
                signed += 1;
            }
            match p.descriptor.role {
                PeerRole::Host => hosts += 1,
                PeerRole::Client => clients += 1,
            }
        }
        let peer_count = peers.len();
        drop(peers);
        let sessions = state.sessions.read().await.len();
        let share_links = state.share_links.read().await.len();
        Json(serde_json::json!({
            "service": "signaling",
            "ok": true,
            "enrollment_managed": state.enrollment().is_managed(),
            "cluster": state.cluster_enabled(),
            "ice_servers": state.ice_server_count(),
            "allow_unsigned_hello": state.allows_unsigned_hello(),
            "peers": peer_count,
            "hosts": hosts,
            "clients": clients,
            "signed_peers": signed,
            "active_sessions": sessions,
            "share_links": share_links,
            "ts_unix_ms": unix_millis_after(Duration::ZERO),
        }))
    } else {
        Json(serde_json::json!({
            "service": "signaling",
            "ok": true,
            "enrollment_managed": state.enrollment().is_managed(),
            "cluster": state.cluster_enabled(),
            "ts_unix_ms": unix_millis_after(Duration::ZERO),
        }))
    }
}

async fn ws_handler(websocket: WebSocketUpgrade, State(state): State<SignalingState>) -> Response {
    websocket.on_upgrade(move |socket| handle_socket(socket, state))
}

#[derive(Serialize, Deserialize)]
struct PublishRelayRequest {
    peer_id: Uuid,
    relay_address: SocketAddr,
}

/// `POST /v1/turn/relay-address`
///
/// A host publishes its TURN relayed address so clients can look it up.
/// The bearer must (a) be an HMAC-bound `SessionCredential` whose
/// pubkey chain names the body's `peer_id`, OR (b) be a bare peer UUID
/// matching the body's `peer_id` (legacy soft-compat — clients should
/// migrate to (a)).
async fn publish_relay_address_handler(
    State(state): State<SignalingState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<PublishRelayRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    validate_relay_publish_bearer(&headers, &state, req.peer_id).await?;

    state.set_turn_relay(req.peer_id, req.relay_address).await;
    Ok(Json(serde_json::json!({"status": "ok"})))
}

/// `GET /v1/turn/relay-address/:peer_id`
///
/// A client fetches the host's TURN relayed address. Requires an
/// HMAC-bound `SessionCredential` bearer whose pubkey chain names the
/// *target* peer's Ed25519 public key. Without auth, an unauthenticated
/// party could enumerate every connected peer's relay address.
async fn get_relay_address_handler(
    State(state): State<SignalingState>,
    headers: axum::http::HeaderMap,
    Path(peer_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    validate_get_relay_bearer(&headers, &state, peer_id).await?;
    match state.get_turn_relay(peer_id).await {
        Some(addr) => Ok(Json(serde_json::json!({
            "peer_id": peer_id,
            "relay_address": addr.to_string(),
        }))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "no relay address for peer"})),
        )),
    }
}

/// Verify the `Authorization: Bearer ...` header for the relay-publish
/// path. Two bearer formats are accepted; each must bind to the body's
/// `expected_peer_id`:
///
/// 1. `Bearer base64(json(SessionCredential))` — preferred. The
///    credential is verified against the server's HMAC secret and
///    must contain an Ed25519 pubkey matching the `expected_peer_id`'s
///    currently-registered pubkey (looked up via the connections table).
///    This binds the credential to a specific peer UUID.
///
/// 2. `Bearer <peer_uuid>` — legacy. The UUID must equal
///    `expected_peer_id` (no surprises). Production deployments should
///    consider this a soft-deprecation and rotate clients onto (a).
async fn validate_relay_publish_bearer(
    headers: &axum::http::HeaderMap,
    state: &SignalingState,
    expected_peer_id: Uuid,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let token = bearer_token(headers)?.ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "missing or invalid Authorization header"})),
        )
    })?;

    // (1) HMAC-bound SessionCredential. Bind cred ↔ peer_id ↔ pubkey.
    if let Some(cred) = decode_session_credential(&token) {
        let now = unix_millis_after(Duration::ZERO);
        if cred.verify(state.server_secret(), now) && cred_has_nonzero_pubkeys(&cred) {
            match state.peer_pubkey(expected_peer_id).await {
                Some(pk) if pk == cred.host_pubkey || pk == cred.client_pubkey => {
                    debug!(
                        session_id = %cred.session_id,
                        peer_id = %expected_peer_id,
                        "accepted HMAC-bound SessionCredential bearer bound to peer pubkey"
                    );
                    return Ok(());
                }
                Some(_) => {
                    return Err((
                        StatusCode::FORBIDDEN,
                        Json(serde_json::json!({
                            "error": "credential pubkeys do not name the publisher peer"
                        })),
                    ));
                }
                None => {
                    // HMAC-verified but the publisher peer has not
                    // registered a pubkey via SignedHello; reject
                    // rather than fall through to the legacy UUID
                    // path, which would otherwise let an attacker
                    // forge a publish by claiming a peer_id whose
                    // pubkey the server has never seen.
                    return Err((
                        StatusCode::FORBIDDEN,
                        Json(serde_json::json!({
                            "error": "peer must register via SignedHello to publish TURN address"
                        })),
                    ));
                }
            }
        }
    }

    // (2) Legacy bare-UUID bearer (soft-compat).
    let token_peer_id: Uuid = token.parse().map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "Authorization bearer must be either a base64 SessionCredential JSON or a peer UUID"
            })),
        )
    })?;

    if token_peer_id != expected_peer_id {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Bearer does not match peer_id"})),
        ));
    }

    Ok(())
}

/// Verify the `Authorization: Bearer ...` header for the relay-GET
/// path. Requires an HMAC-bound `SessionCredential` whose pubkeys name
/// the target peer's currently-registered Ed25519 pubkey. The
/// requester is the OTHER named peer on the same session credential.
/// This stops unauthenticated enumeration of every relay address.
async fn validate_get_relay_bearer(
    headers: &axum::http::HeaderMap,
    state: &SignalingState,
    target_peer_id: Uuid,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let token = bearer_token(headers)?.ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "missing or invalid Authorization header"})),
        )
    })?;

    let cred = decode_session_credential(&token).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "GET relay-address requires an HMAC-bound SessionCredential bearer (legacy UUID not accepted)"
            })),
        )
    })?;

    let now = unix_millis_after(Duration::ZERO);
    if !cred.verify(state.server_secret(), now) || !cred_has_nonzero_pubkeys(&cred) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "SessionCredential failed HMAC verification or has expired"
            })),
        ));
    }

    let target_pk = state.peer_pubkey(target_peer_id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "target peer is not connected"})),
        )
    })?;

    if target_pk != cred.host_pubkey && target_pk != cred.client_pubkey {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "credential does not name the target peer"
            })),
        ));
    }

    Ok(())
}

fn bearer_token(
    headers: &axum::http::HeaderMap,
) -> Result<Option<String>, (StatusCode, Json<serde_json::Value>)> {
    Ok(headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.starts_with("Bearer "))
        .map(|v| v[7..].to_string())
        .filter(|v| !v.is_empty()))
}

fn decode_session_credential(token: &str) -> Option<SessionCredential> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(token.as_bytes())
        .ok()?;
    serde_json::from_slice::<SessionCredential>(&bytes).ok()
}

fn cred_has_nonzero_pubkeys(cred: &SessionCredential) -> bool {
    cred.host_pubkey != [0u8; 32] || cred.client_pubkey != [0u8; 32]
}

// Back-compat alias used by other call sites / docs.
#[allow(dead_code)]
async fn validate_bearer(
    headers: &axum::http::HeaderMap,
    state: &SignalingState,
    expected_peer_id: Uuid,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    validate_relay_publish_bearer(headers, state, expected_peer_id).await
}

async fn handle_socket(socket: WebSocket, state: SignalingState) {
    let (mut writer, mut reader) = socket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<ServerMessage>();

    let writer_task = tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            match serde_json::to_string(&message) {
                Ok(payload) => {
                    if writer.send(Message::Text(payload.into())).await.is_err() {
                        break;
                    }
                }
                Err(error) => {
                    warn!(?error, "failed to serialize server message");
                }
            }
        }
    });

    let mut registered_peer: Option<PeerDescriptor> = None;

    while let Some(frame) = reader.next().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                warn!(?error, "websocket read failed");
                break;
            }
        };

        let payload = match frame {
            Message::Text(text) => text,
            Message::Binary(_) => {
                let _ = outbound_tx.send(ServerMessage::Error(ErrorMessage::new(
                    "unsupported_frame",
                    "binary websocket frames are not supported",
                )));
                continue;
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => break,
        };

        let message = match serde_json::from_str::<ClientMessage>(&payload) {
            Ok(message) => message,
            Err(error) => {
                let _ = outbound_tx.send(ServerMessage::Error(ErrorMessage::new(
                    "invalid_message",
                    format!("failed to decode client message: {error}"),
                )));
                continue;
            }
        };

        match (&registered_peer, message) {
            (None, ClientMessage::Hello(descriptor)) => {
                // Production-safe default: if the server was constructed
                // with `allow_unsigned_hello(false)` (e.g. when
                // `QUBOX_REQUIRE_SIGNED_HELLO=1`), reject the
                // handshake rather than admitting a peer with no pubkey
                // (which would later be unable to obtain a HMAC-bound
                // `SessionCredential`).
                if !state.allows_unsigned_hello() {
                    warn!(
                        device_id = %descriptor.device_id,
                        peer_id = %descriptor.peer_id,
                        "rejecting unsigned Hello; server requires SignedHello"
                    );
                    let _ = outbound_tx.send(ServerMessage::Error(ErrorMessage::new(
                        "signed_hello_required",
                        "server requires a SignedHello handshake (set QUBOX_REQUIRE_SIGNED_HELLO=0 to allow legacy peers)",
                    )));
                    break;
                }
                if state.enrollment().is_managed() {
                    let _ = outbound_tx.send(ServerMessage::Error(ErrorMessage::new(
                        "enrollment_required",
                        "managed signaling requires SignedHello from an enrolled device",
                    )));
                    break;
                }

                let descriptor_for_presence = descriptor.clone();
                let tenant_id = Uuid::nil();

                match state
                    .register(descriptor.clone(), None, tenant_id, outbound_tx.clone())
                    .await
                {
                    Ok(()) => {
                        warn!(
                            device_id = %descriptor.device_id,
                            peer_id = %descriptor.peer_id,
                            "peer connected with unsigned Hello; session credentials will be \
                             rejected because this peer has no pubkey on record"
                        );
                        let _ = outbound_tx.send(ServerMessage::Welcome(Welcome {
                            self_id: descriptor.peer_id,
                            message: "signaling connected (unsigned)".to_string(),
                        }));
                        state
                            .broadcast_presence(descriptor_for_presence, tenant_id, true)
                            .await;
                        registered_peer = Some(descriptor);
                    }
                    Err(error) => {
                        let _ = outbound_tx.send(ServerMessage::Error(ErrorMessage::new(
                            "registration_failed",
                            error.to_string(),
                        )));
                        break;
                    }
                }
            }
            (None, ClientMessage::SignedHello(hello)) => {
                let descriptor = hello.descriptor.clone();
                let descriptor_for_presence = descriptor.clone();

                if !hello.verify() {
                    let _ = outbound_tx.send(ServerMessage::Error(ErrorMessage::new(
                        "signed_hello_invalid",
                        "SignedHello signature did not verify against the embedded public key",
                    )));
                    break;
                }

                let tenant_id = match state
                    .resolve_enrollment(descriptor.device_id, hello.public_key)
                    .await
                {
                    Ok(t) => t,
                    Err(error) => {
                        warn!(
                            device_id = %descriptor.device_id,
                            peer_id = %descriptor.peer_id,
                            %error,
                            "rejecting SignedHello: enrollment check failed"
                        );
                        let _ = outbound_tx.send(ServerMessage::Error(ErrorMessage::new(
                            "not_enrolled",
                            error.to_string(),
                        )));
                        break;
                    }
                };

                match state
                    .register(
                        descriptor.clone(),
                        Some(hello.public_key),
                        tenant_id,
                        outbound_tx.clone(),
                    )
                    .await
                {
                    Ok(()) => {
                        info!(
                            device_id = %descriptor.device_id,
                            peer_id = %descriptor.peer_id,
                            tenant_id = %tenant_id,
                            role = ?descriptor.role,
                            os = ?descriptor.os,
                            name = %descriptor.device_name,
                            public_key = ?hello.public_key,
                            "peer connected (signed hello verified)"
                        );
                        let _ = outbound_tx.send(ServerMessage::Welcome(Welcome {
                            self_id: descriptor.peer_id,
                            message: if tenant_id.is_nil() {
                                "signaling connected".to_string()
                            } else {
                                format!("signaling connected (tenant {tenant_id})")
                            },
                        }));
                        state
                            .broadcast_presence(descriptor_for_presence, tenant_id, true)
                            .await;
                        registered_peer = Some(descriptor);
                    }
                    Err(error) => {
                        let _ = outbound_tx.send(ServerMessage::Error(ErrorMessage::new(
                            "registration_failed",
                            error.to_string(),
                        )));
                        break;
                    }
                }
            }
            (None, _) => {
                let _ = outbound_tx.send(ServerMessage::Error(ErrorMessage::new(
                    "handshake_required",
                    "the first message must be hello or signed_hello",
                )));
                break;
            }
            (Some(_), ClientMessage::Hello(_) | ClientMessage::SignedHello(_)) => {
                let _ = outbound_tx.send(ServerMessage::Error(ErrorMessage::new(
                    "duplicate_hello",
                    "peer is already registered",
                )));
            }
            (Some(peer), ClientMessage::ListHosts) => {
                let tenant = state.peer_tenant(peer.peer_id).await.unwrap_or(Uuid::nil());
                let _ = outbound_tx.send(ServerMessage::Hosts {
                    hosts: state.list_hosts(tenant).await,
                });
            }
            (Some(peer), ClientMessage::RequestPairing(request)) => {
                match state.request_pairing(peer.clone(), request).await {
                    Ok(()) => {}
                    Err(error) => {
                        let _ = outbound_tx.send(ServerMessage::Error(ErrorMessage::new(
                            "pairing_failed",
                            error.to_string(),
                        )));
                    }
                }
            }
            (Some(peer), ClientMessage::PairingDecision(decision)) => {
                match state.decide_pairing(peer.clone(), decision).await {
                    Ok(Some(grant)) => {
                        let _ = outbound_tx.send(ServerMessage::PairingEstablished(grant));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = outbound_tx.send(ServerMessage::Error(ErrorMessage::new(
                            "pairing_decision_failed",
                            error.to_string(),
                        )));
                    }
                }
            }
            (Some(peer), ClientMessage::Heartbeat) => {
                debug!(peer_id = %peer.peer_id, "heartbeat");
                let _ = outbound_tx.send(ServerMessage::HeartbeatAck);
            }
            (Some(peer), ClientMessage::StartSession(request)) => {
                match state.start_session(peer.clone(), request).await {
                    Ok(plan) => {
                        let _ = outbound_tx.send(ServerMessage::SessionPlanned(plan));
                    }
                    Err(error) => {
                        let _ = outbound_tx.send(ServerMessage::Error(ErrorMessage::new(
                            "session_rejected",
                            error.to_string(),
                        )));
                    }
                }
            }
            (Some(peer), ClientMessage::RelaySignal(signal)) => {
                match state.relay_signal(peer.clone(), signal).await {
                    Ok(()) => {}
                    Err(error) => {
                        let _ = outbound_tx.send(ServerMessage::Error(ErrorMessage::new(
                            "relay_failed",
                            error.to_string(),
                        )));
                    }
                }
            }

            (
                Some(peer),
                ClientMessage::RevokePairing {
                    host_peer_id,
                    client_peer_id,
                },
            ) => {
                if let Err(error) = state
                    .revoke_pairing(peer.clone(), host_peer_id, client_peer_id)
                    .await
                {
                    let _ = state
                        .send_to(
                            peer.peer_id,
                            ServerMessage::Error(ErrorMessage::new(
                                "revoke_failed",
                                error.to_string(),
                            )),
                        )
                        .await;
                }
            }
            (Some(peer), ClientMessage::KickSession { session_id, reason }) => {
                if let Err(error) = state.kick_session(peer.clone(), session_id, reason).await {
                    let _ = state
                        .send_to(
                            peer.peer_id,
                            ServerMessage::Error(ErrorMessage::new(
                                "kick_failed",
                                error.to_string(),
                            )),
                        )
                        .await;
                }
            }
            (
                Some(peer),
                ClientMessage::CreateShareLink {
                    ttl_secs,
                    permissions,
                },
            ) => match state
                .create_share_link(peer.clone(), ttl_secs, permissions)
                .await
            {
                Ok((code, expires_unix_ms, url_hint)) => {
                    let _ = state
                        .send_to(
                            peer.peer_id,
                            ServerMessage::ShareLinkCreated {
                                code,
                                expires_unix_ms,
                                url_hint,
                            },
                        )
                        .await;
                }
                Err(error) => {
                    let _ = state
                        .send_to(
                            peer.peer_id,
                            ServerMessage::Error(ErrorMessage::new(
                                "share_link_failed",
                                error.to_string(),
                            )),
                        )
                        .await;
                }
            },
            (Some(peer), ClientMessage::RedeemShareLink { code, client_label }) => {
                if let Err(error) = state
                    .redeem_share_link(peer.clone(), code, client_label)
                    .await
                {
                    let _ = state
                        .send_to(
                            peer.peer_id,
                            ServerMessage::Error(ErrorMessage::new(
                                "redeem_failed",
                                error.to_string(),
                            )),
                        )
                        .await;
                }
            }
        }
    }

    if let Some(peer) = registered_peer {
        let tenant = state.peer_tenant(peer.peer_id).await.unwrap_or(Uuid::nil());
        state.unregister(peer.peer_id).await;
        state.remove_sessions_for(peer.peer_id).await;
        state.broadcast_presence(peer, tenant, false).await;
    }

    writer_task.abort();
}

fn negotiate_transport(
    client: &PeerDescriptor,
    host: &PeerDescriptor,
    requested: Option<TransportKind>,
) -> Option<TransportKind> {
    if let Some(transport) = requested {
        return client
            .capabilities
            .supports_transport(transport)
            .then_some(transport)
            .filter(|transport| host.capabilities.supports_transport(*transport));
    }

    [
        TransportKind::NativeQuic,
        TransportKind::WebRtc,
        TransportKind::RelayQuic,
    ]
    .into_iter()
    .find(|transport| {
        client.capabilities.supports_transport(*transport)
            && host.capabilities.supports_transport(*transport)
    })
}

fn negotiate_codec(
    client: &PeerDescriptor,
    host: &PeerDescriptor,
    preferred: Option<VideoCodec>,
) -> Option<VideoCodec> {
    if let Some(codec) = preferred {
        let client_ok = client.capabilities.decoders.contains(&codec)
            || client.capabilities.encoders.contains(&codec);
        let host_ok = host.capabilities.encoders.contains(&codec)
            || host.capabilities.decoders.contains(&codec);

        if client_ok && host_ok {
            return Some(codec);
        }
    }

    [VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264]
        .into_iter()
        .find(|codec| {
            host.capabilities.encoders.contains(codec)
                && client.capabilities.decoders.contains(codec)
        })
}

/// 10-minute lifetime for a session credential.
const SESSION_TOKEN_TTL_MILLIS: u64 = 10 * 60 * 1_000;

fn unix_millis_after(duration: Duration) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let expires = now + duration;

    expires.as_millis().min(u128::from(u64::MAX)) as u64
}

fn load_server_secret() -> Vec<u8> {
    match std::env::var(SIGNALING_SECRET_ENV) {
        Ok(secret) if !secret.is_empty() => {
            info!(
                env = SIGNALING_SECRET_ENV,
                len = secret.len(),
                "loaded signaling server secret from env"
            );
            secret.into_bytes()
        }
        _ => {
            let mut bytes = vec![0_u8; 32];
            OsRng.fill_bytes(&mut bytes);
            warn!(
                env = SIGNALING_SECRET_ENV,
                "QUBOX_SIGNALING_SECRET not set; generated a random secret for this process. \
                 Issued session credentials will NOT survive a restart. \
                 Set the env var to a 32+ byte random value for production."
            );
            bytes
        }
    }
}

fn generate_test_server_secret() -> Vec<u8> {
    // Tests use a deterministic secret so peer pubkeys stay
    // self-consistent across multiple `SignalingState::default()`.
    b"unit-test-signaling-secret-do-not-use-in-prod".to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use qubox_proto::{
        generate_signing_key, CapabilityProfile, IceServer, PlatformOs, SessionCredential,
        SessionSignal, SignedHello,
    };

    fn descriptor(role: PeerRole, peer_id: Uuid) -> PeerDescriptor {
        PeerDescriptor {
            device_id: Uuid::new_v4(),
            peer_id,
            device_name: format!("{role:?}"),
            role,
            os: PlatformOs::Linux,
            capabilities: CapabilityProfile {
                transports: vec![TransportKind::NativeQuic, TransportKind::WebRtc],
                capture: Vec::new(),
                encoders: vec![VideoCodec::H264, VideoCodec::Av1],
                decoders: vec![VideoCodec::H264, VideoCodec::Av1],
                notes: Vec::new(),
            },
        }
    }

    /// Build a `(descriptor, public_key)` pair with a freshly
    /// generated Ed25519 keypair. Used by tests that need a peer
    /// that can satisfy the new pubkey-required `start_session`.
    fn signed_descriptor(role: PeerRole, peer_id: Uuid) -> (PeerDescriptor, [u8; 32]) {
        let key = generate_signing_key();
        let descriptor = descriptor(role, peer_id);
        let _ = SignedHello::sign(&descriptor, &key);
        (descriptor, key.verifying_key().to_bytes())
    }

    /// Register a peer whose pubkey is exactly `public_key`, returning
    /// its descriptor and a sender that can be discarded. The peer_id
    /// embedded in the descriptor is what callers pass as the body's
    /// `peer_id` for relay-publish / relay-get tests.
    async fn register_signed_peer(
        state: &SignalingState,
        role: PeerRole,
        public_key: [u8; 32],
    ) -> (
        PeerDescriptor,
        tokio::sync::mpsc::UnboundedSender<ServerMessage>,
    ) {
        let _key = generate_signing_key();
        let descriptor = descriptor(role, Uuid::new_v4());
        let (tx, _rx) = mpsc::unbounded_channel::<ServerMessage>();
        // The caller-supplied public_key is what we'll register so the
        // credential/peer binding test gets a deterministic match.
        let pk = public_key;
        state
            .register(descriptor.clone(), Some(pk), Uuid::nil(), tx.clone())
            .await
            .expect("test peer registration");
        (descriptor, tx)
    }

    #[tokio::test]
    async fn list_hosts_is_tenant_isolated() {
        let state = SignalingState::default();
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let host_a = descriptor(PeerRole::Host, Uuid::new_v4());
        let host_b = descriptor(PeerRole::Host, Uuid::new_v4());
        let (tx_a, _) = mpsc::unbounded_channel();
        let (tx_b, _) = mpsc::unbounded_channel();
        state
            .register(host_a.clone(), None, tenant_a, tx_a)
            .await
            .unwrap();
        state
            .register(host_b.clone(), None, tenant_b, tx_b)
            .await
            .unwrap();
        let a_hosts = state.list_hosts(tenant_a).await;
        let b_hosts = state.list_hosts(tenant_b).await;
        assert_eq!(a_hosts.len(), 1);
        assert_eq!(a_hosts[0].peer_id, host_a.peer_id);
        assert_eq!(b_hosts.len(), 1);
        assert_eq!(b_hosts[0].peer_id, host_b.peer_id);
    }

    #[tokio::test]
    async fn pairing_rejects_cross_tenant() {
        let state = SignalingState::default();
        let host = descriptor(PeerRole::Host, Uuid::new_v4());
        let client = descriptor(PeerRole::Client, Uuid::new_v4());
        let (host_tx, _) = mpsc::unbounded_channel();
        let (client_tx, _) = mpsc::unbounded_channel();
        state
            .register(host.clone(), None, Uuid::new_v4(), host_tx)
            .await
            .unwrap();
        state
            .register(client.clone(), None, Uuid::new_v4(), client_tx)
            .await
            .unwrap();
        let err = state
            .request_pairing(
                client,
                PairingRequest {
                    request_id: Uuid::new_v4(),
                    host_peer_id: host.peer_id,
                    client_label: "x".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("same tenant"),
            "expected tenant error, got {err}"
        );
    }

    #[tokio::test]
    async fn session_requires_pairing() {
        let state = SignalingState::default();
        let host = descriptor(PeerRole::Host, Uuid::new_v4());
        let client = descriptor(PeerRole::Client, Uuid::new_v4());
        let (host_tx, _host_rx) = mpsc::unbounded_channel();
        let (client_tx, _client_rx) = mpsc::unbounded_channel();

        state
            .register(host.clone(), None, Uuid::nil(), host_tx)
            .await
            .unwrap();
        state
            .register(client.clone(), None, Uuid::nil(), client_tx)
            .await
            .unwrap();

        let request = StartSessionRequest {
            session_id: Uuid::new_v4(),
            target_host_id: host.peer_id,
            requested_transport: None,
            preferred_codec: None,
            video: None,
            permissions: Default::default(),
            sync_only: false,
        };

        assert!(state
            .start_session(client.clone(), request.clone())
            .await
            .is_err());

        state
            .pairing_store
            .add_pairing(PairingGrant {
                host_peer_id: host.peer_id,
                client_peer_id: client.peer_id,
            })
            .await
            .unwrap();

        // No pubkey → session credential cannot be issued.
        assert!(state.start_session(client, request).await.is_err());
    }

    #[tokio::test]
    async fn signed_session_issues_hmac_bound_credential() {
        let state = SignalingState::default();
        let (host, host_pk) = signed_descriptor(PeerRole::Host, Uuid::new_v4());
        let (client, client_pk) = signed_descriptor(PeerRole::Client, Uuid::new_v4());
        let (host_tx, _host_rx) = mpsc::unbounded_channel();
        let (client_tx, _client_rx) = mpsc::unbounded_channel();

        state
            .register(host.clone(), Some(host_pk), Uuid::nil(), host_tx)
            .await
            .unwrap();
        state
            .register(client.clone(), Some(client_pk), Uuid::nil(), client_tx)
            .await
            .unwrap();
        state
            .pairing_store
            .add_pairing(PairingGrant {
                host_peer_id: host.peer_id,
                client_peer_id: client.peer_id,
            })
            .await
            .unwrap();

        let plan = state
            .start_session(
                client,
                StartSessionRequest {
                    session_id: Uuid::new_v4(),
                    target_host_id: host.peer_id,
                    requested_transport: Some(TransportKind::NativeQuic),
                    preferred_codec: Some(VideoCodec::Av1),
                    video: None,
                    permissions: Default::default(),
                    sync_only: false,
                },
            )
            .await
            .unwrap();

        assert_eq!(plan.client_credential.host_pubkey, host_pk);
        assert_eq!(plan.client_credential.client_pubkey, client_pk);
        assert!(
            plan.client_credential
                .verify(state.server_secret(), unix_millis_after(Duration::ZERO)),
            "issued credential must verify under the server secret"
        );
    }

    #[tokio::test]
    async fn relayed_signal_requires_pairing() {
        let state = SignalingState::default();
        let host = descriptor(PeerRole::Host, Uuid::new_v4());
        let client = descriptor(PeerRole::Client, Uuid::new_v4());
        let (host_tx, _host_rx) = mpsc::unbounded_channel();
        let (client_tx, _client_rx) = mpsc::unbounded_channel();

        state
            .register(host.clone(), None, Uuid::nil(), host_tx)
            .await
            .unwrap();
        state
            .register(client.clone(), None, Uuid::nil(), client_tx)
            .await
            .unwrap();

        let signal = RelaySignal {
            session_id: Uuid::new_v4(),
            from_peer_id: client.peer_id,
            to_peer_id: host.peer_id,
            signal: SessionSignal::SdpOffer {
                sdp: "v=0".to_string(),
            },
        };

        assert!(state
            .relay_signal(client.clone(), signal.clone())
            .await
            .is_err());

        state
            .pairing_store
            .add_pairing(PairingGrant {
                host_peer_id: host.peer_id,
                client_peer_id: client.peer_id,
            })
            .await
            .unwrap();

        // Both peers unsigned → credential cannot bind, so
        // start_session fails. The test then asserts that relay
        // likewise fails because the session never existed.
        assert!(state
            .start_session(
                client.clone(),
                StartSessionRequest {
                    session_id: signal.session_id,
                    target_host_id: host.peer_id,
                    requested_transport: Some(TransportKind::WebRtc),
                    preferred_codec: Some(VideoCodec::H264),
                    video: None,
                    permissions: Default::default(),
                    sync_only: false,
                },
            )
            .await
            .is_err());
        assert!(state.relay_signal(client, signal).await.is_err());
    }

    #[tokio::test]
    async fn relayed_signal_requires_planned_session() {
        let state = SignalingState::default();
        let host = descriptor(PeerRole::Host, Uuid::new_v4());
        let client = descriptor(PeerRole::Client, Uuid::new_v4());
        let (host_tx, _host_rx) = mpsc::unbounded_channel();
        let (client_tx, _client_rx) = mpsc::unbounded_channel();

        state
            .register(host.clone(), None, Uuid::nil(), host_tx)
            .await
            .unwrap();
        state
            .register(client.clone(), None, Uuid::nil(), client_tx)
            .await
            .unwrap();
        state
            .pairing_store
            .add_pairing(PairingGrant {
                host_peer_id: host.peer_id,
                client_peer_id: client.peer_id,
            })
            .await
            .unwrap();

        let signal = RelaySignal {
            session_id: Uuid::new_v4(),
            from_peer_id: client.peer_id,
            to_peer_id: host.peer_id,
            signal: SessionSignal::SdpOffer {
                sdp: "v=0".to_string(),
            },
        };

        assert!(state.relay_signal(client, signal).await.is_err());
    }

    #[tokio::test]
    async fn relayed_signal_rejects_expired_session() {
        let state = SignalingState::default();
        let (host, host_pk) = signed_descriptor(PeerRole::Host, Uuid::new_v4());
        let (client, client_pk) = signed_descriptor(PeerRole::Client, Uuid::new_v4());
        let (host_tx, _host_rx) = mpsc::unbounded_channel();
        let (client_tx, _client_rx) = mpsc::unbounded_channel();

        state
            .register(host.clone(), Some(host_pk), Uuid::nil(), host_tx)
            .await
            .unwrap();
        state
            .register(client.clone(), Some(client_pk), Uuid::nil(), client_tx)
            .await
            .unwrap();
        state
            .pairing_store
            .add_pairing(PairingGrant {
                host_peer_id: host.peer_id,
                client_peer_id: client.peer_id,
            })
            .await
            .unwrap();

        let session_id = Uuid::new_v4();
        state
            .start_session(
                client.clone(),
                StartSessionRequest {
                    session_id,
                    target_host_id: host.peer_id,
                    requested_transport: Some(TransportKind::WebRtc),
                    preferred_codec: Some(VideoCodec::H264),
                    video: None,
                    permissions: Default::default(),
                    sync_only: false,
                },
            )
            .await
            .unwrap();

        state
            .sessions
            .write()
            .await
            .get_mut(&session_id)
            .unwrap()
            .expires_unix_millis = 0;

        let signal = RelaySignal {
            session_id,
            from_peer_id: client.peer_id,
            to_peer_id: host.peer_id,
            signal: SessionSignal::SdpOffer {
                sdp: "v=0".to_string(),
            },
        };

        assert!(state.relay_signal(client, signal).await.is_err());
        assert!(!state.sessions.read().await.contains_key(&session_id));
    }

    #[test]
    fn requested_webrtc_transport_is_honored_when_supported() {
        let host = descriptor(PeerRole::Host, Uuid::new_v4());
        let client = descriptor(PeerRole::Client, Uuid::new_v4());

        assert_eq!(
            negotiate_transport(&client, &host, Some(TransportKind::WebRtc)),
            Some(TransportKind::WebRtc)
        );
    }

    #[tokio::test]
    async fn session_plan_contains_credentials_and_ice_servers() {
        let state = SignalingState::with_options(
            None,
            vec![IceServer {
                urls: vec!["stun:127.0.0.1:3478".to_string()],
                username: None,
                credential: None,
            }],
        )
        .unwrap();
        let (host, host_pk) = signed_descriptor(PeerRole::Host, Uuid::new_v4());
        let (client, client_pk) = signed_descriptor(PeerRole::Client, Uuid::new_v4());
        let (host_tx, mut host_rx) = mpsc::unbounded_channel();
        let (client_tx, _client_rx) = mpsc::unbounded_channel();

        state
            .register(host.clone(), Some(host_pk), Uuid::nil(), host_tx)
            .await
            .unwrap();
        state
            .register(client.clone(), Some(client_pk), Uuid::nil(), client_tx)
            .await
            .unwrap();
        state
            .pairing_store
            .add_pairing(PairingGrant {
                host_peer_id: host.peer_id,
                client_peer_id: client.peer_id,
            })
            .await
            .unwrap();

        let plan = state
            .start_session(
                client,
                StartSessionRequest {
                    session_id: Uuid::new_v4(),
                    target_host_id: host.peer_id,
                    requested_transport: Some(TransportKind::WebRtc),
                    preferred_codec: Some(VideoCodec::H264),
                    video: None,
                    permissions: Default::default(),
                    sync_only: false,
                },
            )
            .await
            .unwrap();
        let requested = match host_rx.recv().await {
            Some(ServerMessage::SessionRequested(requested)) => requested,
            other => panic!("expected session request, got {other:?}"),
        };

        // New credential is HMAC-bound and identical on both sides.
        assert_eq!(plan.client_credential, requested.host_credential);
        assert_eq!(plan.client_credential.hmac.len(), 32);
        assert!(plan
            .client_credential
            .verify(state.server_secret(), unix_millis_after(Duration::ZERO)));
        assert_eq!(plan.ice_servers.len(), 1);
        assert_eq!(requested.ice_servers, plan.ice_servers);
    }

    #[tokio::test]
    async fn turn_relay_set_get_remove() {
        let state = SignalingState::default();
        let peer_id = Uuid::new_v4();
        let addr: SocketAddr = "10.0.0.1:3478".parse().unwrap();

        assert!(state.get_turn_relay(peer_id).await.is_none());

        state.set_turn_relay(peer_id, addr).await;
        assert_eq!(state.get_turn_relay(peer_id).await, Some(addr));

        state.remove_turn_relay(peer_id).await;
        assert!(state.get_turn_relay(peer_id).await.is_none());
    }

    #[tokio::test]
    async fn turn_relay_prune_removes_old_entries() {
        let state = SignalingState::default();
        let peer_id = Uuid::new_v4();
        let addr: SocketAddr = "10.0.0.1:3478".parse().unwrap();

        state.set_turn_relay(peer_id, addr).await;
        // Prune with a generous max_age: entry inserted "now" is
        // definitely younger than 1 hour.
        state.prune_turn_relays(Duration::from_secs(3600)).await;
        assert!(
            state.get_turn_relay(peer_id).await.is_some(),
            "fresh entry should survive a 1-hour prune window"
        );

        // Prune with zero max_age: everything is older than "now".
        state.prune_turn_relays(Duration::ZERO).await;
        assert!(
            state.get_turn_relay(peer_id).await.is_none(),
            "entry should be pruned by 0s max_age"
        );
    }

    #[tokio::test]
    async fn turn_relay_isolation() {
        let state = SignalingState::default();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        state
            .set_turn_relay(a, "10.0.0.1:3478".parse().unwrap())
            .await;
        state
            .set_turn_relay(b, "10.0.0.2:3478".parse().unwrap())
            .await;

        assert_eq!(
            state.get_turn_relay(a).await,
            Some("10.0.0.1:3478".parse().unwrap())
        );
        assert_eq!(
            state.get_turn_relay(b).await,
            Some("10.0.0.2:3478".parse().unwrap())
        );

        state.remove_turn_relay(a).await;
        assert!(state.get_turn_relay(a).await.is_none());
        assert!(state.get_turn_relay(b).await.is_some());
    }

    #[tokio::test]
    async fn publish_relay_address_rejects_missing_auth() {
        let app = SignalingState::default().router();
        let body = serde_json::to_string(&PublishRelayRequest {
            peer_id: Uuid::new_v4(),
            relay_address: "10.0.0.1:3478".parse().unwrap(),
        })
        .unwrap();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/turn/relay-address")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap();
        let res = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(res.status(), 401);
    }

    #[tokio::test]
    async fn publish_relay_address_rejects_wrong_peer_id() {
        let app = SignalingState::default().router();
        let peer_id = Uuid::new_v4();
        let body = serde_json::to_string(&PublishRelayRequest {
            peer_id,
            relay_address: "10.0.0.1:3478".parse().unwrap(),
        })
        .unwrap();
        let wrong_id = Uuid::new_v4();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/turn/relay-address")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {wrong_id}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        let res = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(res.status(), 403);
    }

    #[tokio::test]
    async fn publish_and_get_relay_address_round_trip() {
        // Build ONE state and use it for both requests so the GET
        // actually sees the entry the POST published. Register a
        // peer whose pubkey matches the HMAC credential so that POST
        // (HMAC path) and GET (HMAC-required) both authenticate.
        let state = SignalingState::default();
        let host_key = generate_signing_key();
        let host_pk = host_key.verifying_key().to_bytes();
        let (descriptor, _tx) = register_signed_peer(&state, PeerRole::Host, host_pk).await;

        let issued_unix_millis = unix_millis_after(Duration::ZERO);
        let expires_unix_millis = issued_unix_millis + 60_000;
        let credential = SessionCredential::issue(
            state.server_secret(),
            Uuid::new_v4(),
            host_pk,
            [0xCDu8; 32],
            issued_unix_millis,
            expires_unix_millis,
        );
        let bearer_raw = serde_json::to_vec(&credential).unwrap();
        let bearer = base64::engine::general_purpose::STANDARD.encode(&bearer_raw);

        let body = serde_json::to_string(&PublishRelayRequest {
            peer_id: descriptor.peer_id,
            relay_address: "10.0.0.1:3478".parse().unwrap(),
        })
        .unwrap();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/turn/relay-address")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        let res = tower::ServiceExt::oneshot(state.clone().router(), req)
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // Fetch the relay address — SAME state, HMAC-bearer required.
        let req = axum::http::Request::builder()
            .method("GET")
            .uri(format!("/v1/turn/relay-address/{}", descriptor.peer_id))
            .header("authorization", format!("Bearer {bearer}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let res = tower::ServiceExt::oneshot(state.router(), req)
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            200,
            "GET should return the entry published by POST"
        );
        let json: serde_json::Value = axum::body::to_bytes(res.into_body(), 4096)
            .await
            .map(|b| serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null))
            .unwrap();
        assert_eq!(
            json.get("relay_address").and_then(|v| v.as_str()),
            Some("10.0.0.1:3478")
        );
    }

    #[tokio::test]
    async fn publish_and_get_relay_with_hmac_session_credential() {
        let state = SignalingState::default();
        let host_key = generate_signing_key();
        let host_pk = host_key.verifying_key().to_bytes();
        let (host_descriptor, host_tx) =
            register_signed_peer(&state, PeerRole::Host, host_pk).await;

        let issued_unix_millis = unix_millis_after(Duration::ZERO);
        let expires_unix_millis = issued_unix_millis + 60_000;
        let credential = SessionCredential::issue(
            state.server_secret(),
            Uuid::new_v4(),
            host_pk,
            [0xABu8; 32], // some other pubkey for the client side
            issued_unix_millis,
            expires_unix_millis,
        );
        let bearer_raw = serde_json::to_vec(&credential).unwrap();
        let bearer = base64::engine::general_purpose::STANDARD.encode(&bearer_raw);

        let body = serde_json::to_string(&PublishRelayRequest {
            peer_id: host_descriptor.peer_id,
            relay_address: "192.0.2.5:3478".parse().unwrap(),
        })
        .unwrap();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/turn/relay-address")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        let res = tower::ServiceExt::oneshot(state.clone().router(), req)
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            200,
            "POST with valid HMAC credential should succeed"
        );

        // GET requires a credential whose cred chain names the target peer's
        // pubkey. The credential above does (host_pk), so it satisfies GET.
        let req = axum::http::Request::builder()
            .method("GET")
            .uri(format!(
                "/v1/turn/relay-address/{}",
                host_descriptor.peer_id
            ))
            .header("authorization", format!("Bearer {bearer}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let res = tower::ServiceExt::oneshot(state.router(), req)
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        drop(host_tx);
    }

    #[tokio::test]
    async fn get_relay_address_returns_404_for_unknown_peer() {
        // GET without auth → 401; with valid HMAC bearer but unknown peer → 404.
        // We exercise both paths.
        let state = SignalingState::default();
        let unknown = Uuid::new_v4();

        // 1) No bearer: must be rejected at the auth layer.
        let req = axum::http::Request::builder()
            .method("GET")
            .uri(format!("/v1/turn/relay-address/{unknown}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let res = tower::ServiceExt::oneshot(state.clone().router(), req)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // 2) Valid HMAC bearer, but target peer is unknown → 404.
        let host_key = generate_signing_key();
        let host_pk = host_key.verifying_key().to_bytes();
        let credential = SessionCredential::issue(
            state.server_secret(),
            Uuid::new_v4(),
            host_pk,
            [0xEFu8; 32],
            unix_millis_after(Duration::ZERO),
            unix_millis_after(Duration::ZERO) + 60_000,
        );
        let bearer_raw = serde_json::to_vec(&credential).unwrap();
        let bearer = base64::engine::general_purpose::STANDARD.encode(&bearer_raw);
        let req = axum::http::Request::builder()
            .method("GET")
            .uri(format!("/v1/turn/relay-address/{unknown}"))
            .header("authorization", format!("Bearer {bearer}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let res = tower::ServiceExt::oneshot(state.router(), req)
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    fn make_valid_hmac_credential(secret: &[u8], expires_unix_millis: u64) -> SessionCredential {
        let session_id = Uuid::new_v4();
        let host_key = generate_signing_key();
        let client_key = generate_signing_key();
        let host_pk = host_key.verifying_key().to_bytes();
        let client_pk = client_key.verifying_key().to_bytes();
        SessionCredential::issue(
            secret,
            session_id,
            host_pk,
            client_pk,
            1_000_000,
            expires_unix_millis,
        )
    }

    fn encode_credential(cred: &SessionCredential) -> String {
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(cred).unwrap())
    }

    /// A valid HMAC-bound `SessionCredential` whose host_pubkey matches
    /// the body's `peer_id`'s registered pubkey must authorize the
    /// relay-publish endpoint.
    #[tokio::test]
    async fn validate_bearer_accepts_hmac_session_credential() {
        let state = SignalingState::default();
        let session_id = Uuid::new_v4();
        let host_key = generate_signing_key();
        let host_pk = host_key.verifying_key().to_bytes();
        let (host_descriptor, _host_tx) =
            register_signed_peer(&state, PeerRole::Host, host_pk).await;
        let client_key = generate_signing_key();
        let client_pk = client_key.verifying_key().to_bytes();
        let issued = unix_millis_after(Duration::ZERO);
        let expires = issued + 60_000;
        let credential = SessionCredential::issue(
            state.server_secret(),
            session_id,
            host_pk,
            client_pk,
            issued,
            expires,
        );
        assert!(credential.verify(state.server_secret(), unix_millis_after(Duration::ZERO)));
        let bearer = encode_credential(&credential);
        let app = state.router();
        let body = serde_json::to_string(&PublishRelayRequest {
            peer_id: host_descriptor.peer_id,
            relay_address: "10.0.0.1:3478".parse().unwrap(),
        })
        .unwrap();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/turn/relay-address")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        let res = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(res.status(), 200, "HMAC-bound credential must be accepted");
    }

    /// A valid HMAC-bound `SessionCredential` whose pubkeys do NOT
    /// match the body's `peer_id`'s registered pubkey must be rejected
    /// with 403, even though the credential itself is valid.
    #[tokio::test]
    async fn validate_bearer_rejects_hmac_with_unrelated_peer() {
        let state = SignalingState::default();
        let host_key = generate_signing_key();
        let host_pk = host_key.verifying_key().to_bytes();
        let (host_descriptor, _host_tx) =
            register_signed_peer(&state, PeerRole::Host, host_pk).await;

        // Build a credential that names completely different pubkeys.
        let other_key = generate_signing_key();
        let other_pk = other_key.verifying_key().to_bytes();
        let credential = SessionCredential::issue(
            state.server_secret(),
            Uuid::new_v4(),
            other_pk,
            other_pk,
            unix_millis_after(Duration::ZERO),
            unix_millis_after(Duration::ZERO) + 60_000,
        );
        let bearer = encode_credential(&credential);
        let body = serde_json::to_string(&PublishRelayRequest {
            peer_id: host_descriptor.peer_id,
            relay_address: "10.0.0.1:3478".parse().unwrap(),
        })
        .unwrap();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/turn/relay-address")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        let res = tower::ServiceExt::oneshot(state.router(), req)
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::FORBIDDEN,
            "credential whose pubkeys do not match the peer must not authorize publish"
        );
    }

    /// An HMAC-verified credential presented for a `peer_id` that has
    /// not registered via `SignedHello` must be rejected (prevents
    /// fallback to the legacy UUID path that would otherwise let an
    /// attacker forge publishes for a peer_id the server has never
    /// seen).
    #[tokio::test]
    async fn validate_bearer_rejects_hmac_when_peer_has_no_pubkey() {
        let state = SignalingState::default();
        // peer_id never connects → peer_pubkey() returns None.
        let unauthenticated_peer_id = Uuid::new_v4();
        let credential = make_valid_hmac_credential(
            state.server_secret(),
            unix_millis_after(Duration::ZERO) + 60_000,
        );
        let bearer = encode_credential(&credential);
        let body = serde_json::to_string(&PublishRelayRequest {
            peer_id: unauthenticated_peer_id,
            relay_address: "10.0.0.1:3478".parse().unwrap(),
        })
        .unwrap();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/turn/relay-address")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        let res = tower::ServiceExt::oneshot(state.router(), req)
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::FORBIDDEN,
            "HMAC credential for an unknown peer must not authorize publish"
        );
    }

    /// GET /v1/turn/relay-address/{id} without any bearer must be
    /// rejected to prevent relay-address enumeration.
    #[tokio::test]
    async fn get_relay_address_rejects_missing_auth() {
        let state = SignalingState::default();
        let peer_id = Uuid::new_v4();
        // Pre-populate so we'd otherwise return 200.
        state
            .set_turn_relay(peer_id, "10.0.0.1:3478".parse().unwrap())
            .await;
        let req = axum::http::Request::builder()
            .method("GET")
            .uri(format!("/v1/turn/relay-address/{peer_id}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let res = tower::ServiceExt::oneshot(state.router(), req)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// GET /v1/turn/relay-address/{id} with a credential that does
    /// not name the target peer's pubkey must be rejected.
    #[tokio::test]
    async fn get_relay_address_rejects_unrelated_credential() {
        let state = SignalingState::default();
        let target_key = generate_signing_key();
        let target_pk = target_key.verifying_key().to_bytes();
        let (target_descriptor, _tx) =
            register_signed_peer(&state, PeerRole::Host, target_pk).await;
        state
            .set_turn_relay(target_descriptor.peer_id, "10.0.0.1:3478".parse().unwrap())
            .await;

        // Credential whose pubkeys are completely unrelated.
        let other_key = generate_signing_key();
        let other_pk = other_key.verifying_key().to_bytes();
        let credential = SessionCredential::issue(
            state.server_secret(),
            Uuid::new_v4(),
            other_pk,
            other_pk,
            unix_millis_after(Duration::ZERO),
            unix_millis_after(Duration::ZERO) + 60_000,
        );
        let bearer = encode_credential(&credential);
        let req = axum::http::Request::builder()
            .method("GET")
            .uri(format!(
                "/v1/turn/relay-address/{}",
                target_descriptor.peer_id
            ))
            .header("authorization", format!("Bearer {bearer}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let res = tower::ServiceExt::oneshot(state.router(), req)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    /// GET /v1/turn/relay-address/{id} with the legacy bare-UUID
    /// bearer must be rejected — only HMAC credentials are accepted
    /// on the GET path.
    #[tokio::test]
    async fn get_relay_address_rejects_legacy_uuid_bearer() {
        let state = SignalingState::default();
        let peer_id = Uuid::new_v4();
        state
            .set_turn_relay(peer_id, "10.0.0.1:3478".parse().unwrap())
            .await;
        let req = axum::http::Request::builder()
            .method("GET")
            .uri(format!("/v1/turn/relay-address/{peer_id}"))
            .header("authorization", format!("Bearer {peer_id}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let res = tower::ServiceExt::oneshot(state.router(), req)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// A credential whose expiry is in the past must not satisfy
    /// `SessionCredential::verify`, which means `validate_bearer`
    /// falls through to the legacy UUID path and ultimately rejects
    /// the request because the bearer is not a UUID either.
    #[tokio::test]
    async fn validate_bearer_rejects_expired_hmac_session_credential() {
        let state = SignalingState::default();
        let credential = make_valid_hmac_credential(state.server_secret(), 0);
        let bearer = encode_credential(&credential);
        let app = state.router();
        let peer_id = Uuid::new_v4();
        let body = serde_json::to_string(&PublishRelayRequest {
            peer_id,
            relay_address: "10.0.0.1:3478".parse().unwrap(),
        })
        .unwrap();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/turn/relay-address")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        let res = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert!(
            res.status() == StatusCode::UNAUTHORIZED || res.status() == StatusCode::FORBIDDEN,
            "expired credential must be rejected, got {}",
            res.status()
        );
    }

    /// A bearer that base64-decodes to non-JSON must be rejected with
    /// the format-error message.
    #[tokio::test]
    async fn validate_bearer_rejects_malformed_hmac_session_credential() {
        let state = SignalingState::default();
        let bearer = base64::engine::general_purpose::STANDARD.encode(b"definitely not JSON");
        let app = state.router();
        let peer_id = Uuid::new_v4();
        let body = serde_json::to_string(&PublishRelayRequest {
            peer_id,
            relay_address: "10.0.0.1:3478".parse().unwrap(),
        })
        .unwrap();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/turn/relay-address")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        let res = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            payload["error"],
            "Authorization bearer must be either a base64 SessionCredential JSON or a peer UUID",
        );
    }

    /// The legacy bare-UUID path must still work for clients that
    /// haven't migrated to HMAC-bound credentials.
    #[tokio::test]
    async fn validate_bearer_accepts_legacy_uuid() {
        let app = SignalingState::default().router();
        let peer_id = Uuid::new_v4();
        let body = serde_json::to_string(&PublishRelayRequest {
            peer_id,
            relay_address: "10.0.0.1:3478".parse().unwrap(),
        })
        .unwrap();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/turn/relay-address")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {peer_id}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        let res = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(
            res.status(),
            200,
            "legacy UUID bearer must still be accepted"
        );
    }

    /// A bearer with non-zero pubkeys but a wrong HMAC must be rejected
    /// (falls through to UUID parsing which fails).
    #[tokio::test]
    async fn validate_bearer_rejects_tampered_hmac() {
        let state = SignalingState::default();
        // Issue a credential under the WRONG secret.
        let mut credential = make_valid_hmac_credential(
            b"not-the-server-secret",
            unix_millis_after(Duration::ZERO) + 60_000,
        );
        // Ensure at least one pubkey is non-zero (issue already does).
        assert!(credential.host_pubkey != [0u8; 32] || credential.client_pubkey != [0u8; 32]);
        // Force expiry into the future so the only thing failing
        // is the HMAC.
        credential.expires_unix_millis = unix_millis_after(Duration::ZERO) + 60_000;
        credential.hmac = [0xAAu8; 32];
        let bearer = encode_credential(&credential);
        let app = state.router();
        let peer_id = Uuid::new_v4();
        let body = serde_json::to_string(&PublishRelayRequest {
            peer_id,
            relay_address: "10.0.0.1:3478".parse().unwrap(),
        })
        .unwrap();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/turn/relay-address")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        let res = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "tampered HMAC must not pass"
        );
    }

    #[tokio::test]
    async fn pairing_store_atomic_persist_round_trip() {
        let dir = std::env::temp_dir().join(format!("qubox-pair-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pairings.json");
        let store = PairingStore::from_path(path.clone()).unwrap();
        let grant = PairingGrant {
            host_peer_id: Uuid::new_v4(),
            client_peer_id: Uuid::new_v4(),
        };
        store.add_pairing(grant.clone()).await.unwrap();
        assert!(path.exists());
        let reloaded = load_pairings_from_path(path).unwrap();
        assert!(reloaded.contains(&grant));
        let _ = std::fs::remove_dir_all(dir);
    }
}
