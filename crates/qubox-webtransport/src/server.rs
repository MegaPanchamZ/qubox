use std::net::SocketAddr;

use anyhow::Context;
use wtransport::{Endpoint, ServerConfig};

use crate::config::WebTransportConfig;

/// WebTransport server wrapping the `wtransport` endpoint.
pub struct WebTransportServer {
    config: WebTransportConfig,
}

impl WebTransportServer {
    pub fn new(config: WebTransportConfig) -> Self {
        Self { config }
    }

    /// Returns the SHA-256(DER) hash for `serverCertificateHashes`.
    pub fn cert_hash(&self) -> [u8; 32] {
        self.config.cert_hash
    }

    /// Bind and accept WebTransport sessions.
    pub async fn run(&self) -> anyhow::Result<()> {
        let hash_hex = hex::encode(self.config.cert_hash);
        tracing::info!(
            listen_addr = %self.config.listen_addr,
            cert_hash = %hash_hex,
            "WebTransport server starting"
        );

        let wt_config = ServerConfig::builder()
            .with_bind_address(self.config.listen_addr)
            .with_identity(self.config.identity.clone_identity())
            .build();

        let endpoint =
            Endpoint::server(wt_config).context("failed to create wtransport endpoint")?;

        tracing::info!(
            addr = %self.config.listen_addr,
            "WebTransport server listening"
        );

        loop {
            let incoming = endpoint.accept().await;
            let request = incoming
                .await
                .context("failed to accept incoming session")?;

            tokio::spawn(async move {
                if let Err(e) = crate::session::handle(request).await {
                    tracing::warn!(error = %e, "WebTransport session exited");
                }
            });
        }
    }
}

/// Standalone entry point — runs the server until failure.
pub async fn run(listen: SocketAddr) -> anyhow::Result<()> {
    let config = WebTransportConfig::new(listen)?;
    let server = WebTransportServer::new(config);
    server.run().await
}
