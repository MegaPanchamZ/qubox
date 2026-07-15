use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Context;
use axum::{routing, Extension};
use clap::Parser;
use qubox_proto::IceServer;
use qubox_signaling::cluster::ClusterBus;
use qubox_signaling::SignalingState;
use rand_core::{OsRng, RngCore};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

mod enrollment;
mod turn;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, env = "QUBOX_BIND", default_value = "0.0.0.0:7000")]
    bind: SocketAddr,

    #[arg(long, env = "QUBOX_PAIRING_STORE")]
    pairing_store: Option<PathBuf>,

    #[arg(long = "ice-server")]
    ice_servers: Vec<String>,

    /// Env var for the TURN shared secret (default QUBOX_TURN_SECRET).
    #[arg(long = "turn-secret-env", default_value = "QUBOX_TURN_SECRET")]
    turn_secret_env: String,

    /// Env var for the TURN URLs (default QUBOX_TURN_URLS).
    #[arg(long = "turn-urls-env", default_value = "QUBOX_TURN_URLS")]
    turn_urls_env: String,

    /// Env var for the signaling server secret used to sign
    /// SessionCredentials. Loaded from QUBOX_SIGNALING_SECRET
    /// by default; random per-process if missing.
    #[arg(
        long = "signaling-secret-env",
        default_value = "QUBOX_SIGNALING_SECRET"
    )]
    signaling_secret_env: String,

    /// Allow legacy unsigned `Hello` handshakes. Default is `false`
    /// (production-safe: peers must present a `SignedHello`). Set
    /// `--allow-unsigned-hello` for LAN self-host mode or test
    /// harnesses that have not migrated yet.
    #[arg(long = "allow-unsigned-hello", default_value_t = false)]
    allow_unsigned_hello: bool,

    /// Redis URL for multi-instance peer registry + pub/sub
    /// (e.g. redis://127.0.0.1:6379). Empty = single-process mode.
    #[arg(long, env = "QUBOX_REDIS_URL", default_value = "")]
    redis_url: String,

    /// Stable instance id for this process (auto UUID if empty).
    #[arg(long, env = "QUBOX_INSTANCE_ID", default_value = "")]
    instance_id: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let args = Args::parse();
    let listener = tokio::net::TcpListener::bind(args.bind)
        .await
        .context("failed to bind signaling listener")?;

    tracing::info!(address = %args.bind, "signaling server listening");

    let signaling_secret = match std::env::var(&args.signaling_secret_env) {
        Ok(secret) if !secret.is_empty() => secret.into_bytes(),
        _ => {
            tracing::warn!(
                env = %args.signaling_secret_env,
                "QUBOX_SIGNALING_SECRET not set; generated a random secret for this process. \
                 Issued session credentials will NOT survive a restart."
            );
            let mut bytes = vec![0_u8; 32];
            OsRng.fill_bytes(&mut bytes);
            bytes
        }
    };

    let turn_state = Arc::new(turn::TurnState::from_env());
    // Merge CLI ice-servers with TURN/STUN fleet from env (multi-region).
    let mut ice = ice_servers(args.ice_servers);
    for url in turn_state.ice_server_urls() {
        if !ice.iter().any(|s| s.urls.iter().any(|u| u == &url)) {
            ice.push(IceServer {
                urls: vec![url],
                username: None,
                credential: None,
            });
        }
    }
    if turn_state.configured {
        tracing::info!(
            regions = ?turn_state.regions_summary(),
            "TURN/STUN fleet loaded for ICE"
        );
    } else {
        tracing::warn!("TURN not configured (set QUBOX_TURN_SECRET + QUBOX_TURN_URLS or QUBOX_TURN_REGIONS)");
    }

    let mut state = SignalingState::with_options_and_secret_and_policy(
        args.pairing_store,
        ice,
        signaling_secret,
        args.allow_unsigned_hello,
    )?
    .with_enrollment(enrollment::policy_from_env());

    if !args.redis_url.is_empty() {
        let instance_id = if args.instance_id.is_empty() {
            Uuid::new_v4().to_string()
        } else {
            args.instance_id.clone()
        };
        let bus = ClusterBus::connect(&args.redis_url, instance_id)
            .await
            .context("connect redis cluster bus")?;
        state = state.with_cluster(bus);
        state.start_cluster_listener(args.redis_url.clone());
        tracing::info!(redis = %args.redis_url, "multi-instance signaling enabled");
    }

    let state = Arc::new(state);
    tracing::info!(
        allow_unsigned_hello = args.allow_unsigned_hello,
        cluster = state.cluster_enabled(),
        "signaling handshake policy initialised"
    );

    let app = state
        .as_ref()
        .clone()
        .router()
        .route(
            "/v1/turn/credentials",
            routing::post(turn::issue_credential_handler),
        )
        .route("/v1/turn/regions", routing::get(turn::regions_handler))
        .layer(Extension(turn_state))
        .layer(Extension(state));

    axum::serve(listener, app)
        .await
        .context("signaling server exited unexpectedly")?;

    Ok(())
}

fn ice_servers(urls: Vec<String>) -> Vec<IceServer> {
    urls.into_iter()
        .map(|url| IceServer {
            urls: vec![url],
            username: None,
            credential: None,
        })
        .collect()
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,qubox_signaling=debug"));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}