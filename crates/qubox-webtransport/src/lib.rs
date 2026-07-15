pub mod cert;
pub mod config;
pub mod server;
pub mod session;

pub use config::WebTransportConfig;
pub use server::WebTransportServer;

/// SHA-256 of the DER-encoded certificate (NOT SPKI — see ADR-017 §13).
pub struct ClientCertHash(pub [u8; 32]);

impl ClientCertHash {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for ClientCertHash {
    fn from(hash: [u8; 32]) -> Self {
        Self(hash)
    }
}

impl From<ClientCertHash> for [u8; 32] {
    fn from(h: ClientCertHash) -> Self {
        h.0
    }
}

impl std::fmt::LowerHex for ClientCertHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
