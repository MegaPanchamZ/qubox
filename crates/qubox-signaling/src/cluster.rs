//! Redis-backed multi-instance coordination for signaling.
//!
//! Local WebSocket connections stay on the accepting process. Shared
//! state (peer registry, sessions, pairings) and cross-instance
//! `ServerMessage` delivery use Redis.
//!
//! Keys:
//! - `qubox:peer:{peer_id}` — JSON peer record (TTL)
//! - `qubox:hosts:{tenant_id}` — SET of host peer ids
//! - `qubox:session:{session_id}` — JSON session (TTL)
//! - `qubox:pair:{host}:{client}` — "1" when paired
//!
//! Channels:
//! - `qubox:inst:{instance_id}` — deliver envelope to a specific instance

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use qubox_proto::{PeerDescriptor, PeerRole, ServerMessage};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

const PEER_TTL_SECS: i64 = 90;
const SESSION_TTL_SECS: i64 = 600;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemotePeer {
    pub instance_id: String,
    pub tenant_id: Uuid,
    pub descriptor: PeerDescriptor,
    pub public_key: Option<[u8; 32]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSession {
    pub host_peer_id: Uuid,
    pub client_peer_id: Uuid,
    pub expires_unix_millis: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeliverEnvelope {
    peer_id: Uuid,
    message: ServerMessage,
}

#[derive(Clone)]
pub struct ClusterBus {
    instance_id: String,
    conn: ConnectionManager,
}

impl ClusterBus {
    pub async fn connect(redis_url: &str, instance_id: String) -> anyhow::Result<Arc<Self>> {
        let client = redis::Client::open(redis_url).context("redis url")?;
        let conn = ConnectionManager::new(client)
            .await
            .context("redis connection manager")?;
        info!(%instance_id, "redis cluster bus connected");
        Ok(Arc::new(Self { instance_id, conn }))
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    fn peer_key(peer_id: Uuid) -> String {
        format!("qubox:peer:{peer_id}")
    }

    fn hosts_key(tenant_id: Uuid) -> String {
        format!("qubox:hosts:{tenant_id}")
    }

    fn session_key(session_id: Uuid) -> String {
        format!("qubox:session:{session_id}")
    }

    fn pair_key(host: Uuid, client: Uuid) -> String {
        format!("qubox:pair:{host}:{client}")
    }

    fn inst_channel(instance_id: &str) -> String {
        format!("qubox:inst:{instance_id}")
    }

    pub async fn register_peer(
        &self,
        peer_id: Uuid,
        tenant_id: Uuid,
        descriptor: &PeerDescriptor,
        public_key: Option<[u8; 32]>,
    ) -> anyhow::Result<()> {
        let rec = RemotePeer {
            instance_id: self.instance_id.clone(),
            tenant_id,
            descriptor: descriptor.clone(),
            public_key,
        };
        let json = serde_json::to_string(&rec)?;
        let mut conn = self.conn.clone();
        let key = Self::peer_key(peer_id);
        let _: () = conn.set_ex(key, json, PEER_TTL_SECS as u64).await?;
        if descriptor.role == PeerRole::Host {
            let _: () = conn
                .sadd(Self::hosts_key(tenant_id), peer_id.to_string())
                .await?;
        }
        Ok(())
    }

    pub async fn touch_peer(&self, peer_id: Uuid) -> anyhow::Result<()> {
        let mut conn = self.conn.clone();
        let _: bool = conn.expire(Self::peer_key(peer_id), PEER_TTL_SECS).await?;
        Ok(())
    }

    pub async fn unregister_peer(
        &self,
        peer_id: Uuid,
        tenant_id: Uuid,
        was_host: bool,
    ) -> anyhow::Result<()> {
        let mut conn = self.conn.clone();
        let _: () = conn.del(Self::peer_key(peer_id)).await?;
        if was_host {
            let _: () = conn
                .srem(Self::hosts_key(tenant_id), peer_id.to_string())
                .await?;
        }
        Ok(())
    }

    pub async fn get_peer(&self, peer_id: Uuid) -> anyhow::Result<Option<RemotePeer>> {
        let mut conn = self.conn.clone();
        let raw: Option<String> = conn.get(Self::peer_key(peer_id)).await?;
        Ok(match raw {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    pub async fn list_hosts(&self, tenant_id: Uuid) -> anyhow::Result<Vec<PeerDescriptor>> {
        let mut conn = self.conn.clone();
        let ids: Vec<String> = conn.smembers(Self::hosts_key(tenant_id)).await?;
        let mut out = Vec::new();
        for id in ids {
            let Ok(peer_id) = Uuid::parse_str(&id) else {
                continue;
            };
            if let Some(p) = self.get_peer(peer_id).await? {
                if p.descriptor.role == PeerRole::Host && p.tenant_id == tenant_id {
                    out.push(p.descriptor);
                }
            } else {
                let _: () = conn.srem(Self::hosts_key(tenant_id), id).await?;
            }
        }
        Ok(out)
    }

    pub async fn put_session(
        &self,
        session_id: Uuid,
        session: &RemoteSession,
    ) -> anyhow::Result<()> {
        let mut conn = self.conn.clone();
        let json = serde_json::to_string(session)?;
        let _: () = conn
            .set_ex(Self::session_key(session_id), json, SESSION_TTL_SECS as u64)
            .await?;
        Ok(())
    }

    pub async fn get_session(&self, session_id: Uuid) -> anyhow::Result<Option<RemoteSession>> {
        let mut conn = self.conn.clone();
        let raw: Option<String> = conn.get(Self::session_key(session_id)).await?;
        Ok(match raw {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    pub async fn put_pairing(&self, host: Uuid, client: Uuid) -> anyhow::Result<()> {
        let mut conn = self.conn.clone();
        let _: () = conn
            .set_ex(Self::pair_key(host, client), "1", 86_400u64)
            .await?;
        Ok(())
    }

    pub async fn is_paired(&self, host: Uuid, client: Uuid) -> anyhow::Result<bool> {
        let mut conn = self.conn.clone();
        let v: Option<String> = conn.get(Self::pair_key(host, client)).await?;
        Ok(v.is_some())
    }

    pub async fn remove_pairing(&self, host: Uuid, client: Uuid) -> anyhow::Result<()> {
        let mut conn = self.conn.clone();
        let _: () = conn.del(Self::pair_key(host, client)).await?;
        Ok(())
    }

    /// Deliver a server message to a peer that may live on another instance.
    pub async fn deliver(&self, peer_id: Uuid, message: ServerMessage) -> anyhow::Result<()> {
        let Some(remote) = self.get_peer(peer_id).await? else {
            return Err(anyhow!("peer {peer_id} not in cluster registry"));
        };
        let env = DeliverEnvelope { peer_id, message };
        let payload = serde_json::to_string(&env)?;
        let mut conn = self.conn.clone();
        let channel = Self::inst_channel(&remote.instance_id);
        let _: i64 = conn.publish(channel, payload).await?;
        Ok(())
    }

    /// Broadcast presence to all instances (including self via redis).
    pub async fn publish_presence(&self, message: ServerMessage) -> anyhow::Result<()> {
        let env = DeliverEnvelope {
            peer_id: Uuid::nil(), // fanout marker
            message,
        };
        let payload = serde_json::to_string(&env)?;
        let mut conn = self.conn.clone();
        let _: i64 = conn.publish("qubox:presence".to_string(), payload).await?;
        Ok(())
    }

    /// Spawn pub/sub listener; delivers into `local_tx` as (peer_id, msg).
    /// peer_id = nil means presence fanout (caller should broadcast locally).
    pub fn spawn_listener(
        self: Arc<Self>,
        redis_url: String,
        local_tx: mpsc::UnboundedSender<(Uuid, ServerMessage)>,
    ) {
        let instance_id = self.instance_id.clone();
        tokio::spawn(async move {
            let client = match redis::Client::open(redis_url.as_str()) {
                Ok(c) => c,
                Err(e) => {
                    warn!(?e, "cluster listener failed to open redis");
                    return;
                }
            };
            loop {
                if let Err(e) = run_pubsub_loop(&client, &instance_id, &local_tx).await {
                    warn!(?e, "cluster pubsub loop error; reconnecting");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        });
    }
}

async fn run_pubsub_loop(
    client: &redis::Client,
    instance_id: &str,
    local_tx: &mpsc::UnboundedSender<(Uuid, ServerMessage)>,
) -> anyhow::Result<()> {
    let mut pubsub = client.get_async_pubsub().await?;
    let inst_ch = format!("qubox:inst:{instance_id}");
    pubsub.subscribe(&inst_ch).await?;
    pubsub.subscribe("qubox:presence").await?;
    info!(%instance_id, "cluster pubsub subscribed");
    let mut stream = pubsub.on_message();
    use futures::StreamExt;
    while let Some(msg) = stream.next().await {
        let payload: String = match msg.get_payload() {
            Ok(p) => p,
            Err(e) => {
                warn!(?e, "bad pubsub payload");
                continue;
            }
        };
        let env: DeliverEnvelope = match serde_json::from_str(&payload) {
            Ok(e) => e,
            Err(e) => {
                warn!(?e, "bad deliver envelope");
                continue;
            }
        };
        let _ = local_tx.send((env.peer_id, env.message));
    }
    Ok(())
}
