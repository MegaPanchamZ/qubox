use rcgen::{generate_simple_self_signed, CertifiedKey};
use sha2::{Digest, Sha256};

/// Persistent cert path — load on startup, write on first gen.
pub const CERT_PERSIST_PATH: &str = "~/.qubox/webtransport-cert.pem";

/// Generate a self-signed cert valid for `qubox.local` + `localhost`.
/// Returns (DER bytes, PKCS#8 private key DER, SHA-256(DER) hash).
///
/// # Important
/// The hash is SHA-256 of the DER bytes, **not** the SPKI.
/// The browser's `serverCertificateHashes` expects DER-hash (ADR-017 §13).
pub fn generate_self_signed() -> anyhow::Result<(Vec<u8>, Vec<u8>, [u8; 32])> {
    let CertifiedKey { cert, key_pair } = generate_simple_self_signed(
        vec!["qubox.local".into(), "localhost".into()],
    )?;

    let der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();

    // SHA-256 of the DER bytes — NOT the SPKI
    let hash = der_hash(&der);

    Ok((der, key_der, hash))
}

/// Compute SHA-256 over raw DER bytes.
pub fn der_hash(der: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(der);
    hasher.finalize().into()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn cert_hash_is_32_bytes() {
        let (_, _, hash) = generate_self_signed().expect("cert generation");
        assert_eq!(hash.len(), 32);
    }

    #[test]
    fn cert_hash_round_trip() {
        let (der, _, hash) = generate_self_signed().expect("cert generation");
        let computed = der_hash(&der);
        assert_eq!(hash, computed, "hash must be deterministic from DER");
    }

    #[test]
    fn cert_hash_is_not_spki_hash() {
        let (der, _, der_hash) = generate_self_signed().expect("cert generation");

        // Parse DER to extract SPKI (SubjectPublicKeyInfo within TBSCertificate)
        let parsed = x509_parser::parse_x509_certificate(&der)
            .expect("parse DER cert");
        let spki_raw = parsed.1.public_key().raw;

        // SHA-256 of SPKI (the WRONG thing to hash)
        let spki_hash: [u8; 32] = Sha256::digest(spki_raw).into();

        assert_ne!(
            der_hash, spki_hash,
            "hash must be SHA-256(DER), NOT SHA-256(SPKI) — see ADR-017 §13 pitfall #3"
        );
    }
}
