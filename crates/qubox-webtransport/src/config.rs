use std::net::SocketAddr;

use wtransport::Identity;

/// Configuration for the WebTransport server endpoint.
pub struct WebTransportConfig {
    /// Address to bind the HTTP/3 + WebTransport listener on.
    pub listen_addr: SocketAddr,
    /// TLS identity (cert chain + private key) for wtransport.
    pub identity: Identity,
    /// SHA-256(DER) hash shipped to the browser via `serverCertificateHashes`.
    pub cert_hash: [u8; 32],
}

impl WebTransportConfig {
    /// Create a new config, generating a self-signed cert.
    pub fn new(listen_addr: SocketAddr) -> anyhow::Result<Self> {
        let (cert_der, key_der, cert_hash) = crate::cert::generate_self_signed()?;

        let certificate = wtransport::tls::Certificate::from_der(cert_der)
            .map_err(|e| anyhow::anyhow!("invalid certificate: {e}"))?;
        let private_key = wtransport::tls::PrivateKey::from_der_pkcs8(key_der);
        let identity = Identity::new(
            wtransport::tls::CertificateChain::single(certificate),
            private_key,
        );

        Ok(Self {
            listen_addr,
            identity,
            cert_hash,
        })
    }
}
