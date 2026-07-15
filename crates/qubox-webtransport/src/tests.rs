use crate::cert;

#[test]
fn client_cert_hash_from_array() {
    let hash = [0xabu8; 32];
    let ch = crate::ClientCertHash::from(hash);
    assert_eq!(ch.as_bytes(), &hash);
    let back: [u8; 32] = ch.into();
    assert_eq!(back, hash);
}

#[test]
fn client_cert_hash_lower_hex() {
    let hash = [0xabu8; 32];
    let ch = crate::ClientCertHash(hash);
    let hex_str = format!("{:x}", ch);
    assert_eq!(hex_str.len(), 64);
    assert!(hex_str.starts_with("ababab"));
    assert!(hex_str.ends_with("ababab"));
}

#[test]
fn cert_hash_not_spki_regression() {
    // Explicitly verify SHA-256(DER) ≠ SHA-256(SPKI)
    let (der, _, der_hash) = cert::generate_self_signed().expect("cert gen");

    let parsed = x509_parser::parse_x509_certificate(&der).expect("parse DER");
    let spki_raw = parsed.1.public_key().raw;

    use sha2::{Digest, Sha256};
    let spki_hash: [u8; 32] = Sha256::digest(spki_raw).into();

    assert_ne!(
        der_hash, spki_hash,
        "SHA-256(DER) must not equal SHA-256(SPKI) — serverCertificateHashes expects DER hash"
    );
}
