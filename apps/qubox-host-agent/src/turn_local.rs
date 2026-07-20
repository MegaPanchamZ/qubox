//! Local TURN credential minting for the host-agent.
//!
//! The signaling server delivers ICE server *URLs* to the host but does
//! not currently include the per-session HMAC credentials the browser
//! uses (those are issued to the viewer via the BFF bundle). The host
//! therefore mints its own short-term credentials from
//! `QUBOX_TURN_SECRET` (RFC 5389-style static-auth-secret), matching
//! what coturn validates against. The username/password format must
//! stay byte-compatible with
//! `apps/qubox-signaling-server/src/turn.rs::issue_credentials` and
//! `site/lib/session-bundle.ts::getIceAllowlist` so all three sides
//! agree.
//!
//! Username: `<expiry>:<base64(hmac_sha1(secret, expiry))>:peer=<id>:session=<id>`
//! Password: `base64(hmac_sha1(secret, username))`

use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use qubox_proto::IceServer;
use sha1::Sha1;
use uuid::Uuid;

type HmacSha1 = Hmac<Sha1>;

/// Result of resolving ICE servers for a WebRTC session.
pub struct ResolvedIce {
    pub servers: Vec<IceServer>,
    /// `Some(reason)` when at least one `turn:` URL was passed through
    /// without credentials (e.g. host has no `QUBOX_TURN_SECRET`). The
    /// host-side peerconnection will construct successfully but relay
    /// candidates will be rejected by coturn.
    pub skip_reason: Option<String>,
}

/// Inject HMAC credentials into any `turn:` URL that arrives without
/// them. STUN URLs and pre-minted static creds pass through unchanged.
pub fn mint_for_session(session_id: Uuid, peer_id: Uuid, servers: &[IceServer]) -> ResolvedIce {
    let secret = match std::env::var("QUBOX_TURN_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(s) => s,
        None => {
            let has_turn = servers
                .iter()
                .flat_map(|s| s.urls.iter())
                .any(|u| u.starts_with("turn:") || u.starts_with("turns:"));
            return ResolvedIce {
                servers: servers.to_vec(),
                skip_reason: if has_turn {
                    Some("QUBOX_TURN_SECRET unset; turn: servers left without credentials".into())
                } else {
                    None
                },
            };
        }
    };

    let ttl: u64 = std::env::var("QUBOX_TURN_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let expiry = now + ttl;
    let expiry_str = expiry.to_string();
    let expiry_hmac = {
        let mut mac =
            HmacSha1::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
        mac.update(expiry_str.as_bytes());
        STANDARD.encode(mac.finalize().into_bytes())
    };
    let username = format!(
        "{expiry}:{expiry_hmac}:peer={peer}:session={session}",
        expiry = expiry_str,
        expiry_hmac = expiry_hmac,
        peer = peer_id,
        session = session_id,
    );
    let password = {
        let mut mac =
            HmacSha1::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
        mac.update(username.as_bytes());
        STANDARD.encode(mac.finalize().into_bytes())
    };

    let mut out = Vec::with_capacity(servers.len());
    let mut had_turn = false;
    for s in servers {
        // If the signaling already supplied credentials (e.g. via
        // /v1/turn/credentials) honour them as-is.
        if s.username.is_some() && s.credential.is_some() {
            out.push(s.clone());
            continue;
        }
        let needs_creds = s
            .urls
            .iter()
            .any(|u| u.starts_with("turn:") || u.starts_with("turns:"));
        if !needs_creds {
            out.push(s.clone());
            continue;
        }
        had_turn = true;
        out.push(IceServer {
            urls: s.urls.clone(),
            username: Some(username.clone()),
            credential: Some(password.clone()),
        });
    }
    ResolvedIce {
        servers: out,
        skip_reason: if !had_turn { None } else { None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_secret() -> String {
        // Use a per-test value to avoid cross-test interference.
        format!("test_secret_{}", Uuid::new_v4())
    }

    #[test]
    fn mints_creds_for_turn_url_without_them() {
        let secret = tmp_secret();
        // SAFETY: tests in this module run with `--test-threads=1` by
        // default and never read the env from another test.
        unsafe { std::env::set_var("QUBOX_TURN_SECRET", &secret) };
        let servers = vec![IceServer {
            urls: vec!["turn:signal.qubox.app:3478".into()],
            username: None,
            credential: None,
        }];
        let out = mint_for_session(Uuid::new_v4(), Uuid::new_v4(), &servers);
        assert_eq!(out.servers.len(), 1);
        let s = &out.servers[0];
        assert!(s.username.is_some());
        assert!(s.credential.is_some());
        let user = s.username.as_ref().unwrap();
        assert!(user.contains("peer="));
        assert!(user.contains("session="));
        unsafe { std::env::remove_var("QUBOX_TURN_SECRET") };
    }

    #[test]
    fn stun_urls_pass_through() {
        unsafe { std::env::set_var("QUBOX_TURN_SECRET", "x") };
        let servers = vec![IceServer {
            urls: vec!["stun:example.com:3478".into()],
            username: None,
            credential: None,
        }];
        let out = mint_for_session(Uuid::new_v4(), Uuid::new_v4(), &servers);
        assert!(out.servers[0].username.is_none());
        unsafe { std::env::remove_var("QUBOX_TURN_SECRET") };
    }

    #[test]
    fn preexisting_creds_are_honoured() {
        unsafe { std::env::set_var("QUBOX_TURN_SECRET", "x") };
        let servers = vec![IceServer {
            urls: vec!["turn:signal.qubox.app:3478".into()],
            username: Some("pre".into()),
            credential: Some("set".into()),
        }];
        let out = mint_for_session(Uuid::new_v4(), Uuid::new_v4(), &servers);
        assert_eq!(out.servers[0].username.as_deref(), Some("pre"));
        assert_eq!(out.servers[0].credential.as_deref(), Some("set"));
        unsafe { std::env::remove_var("QUBOX_TURN_SECRET") };
    }
}
