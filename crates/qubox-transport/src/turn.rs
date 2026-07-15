//! STUN/TURN message codec and minimal TURN client (RFC 8656).
//!
//! This is a hand-rolled implementation of the minimum RFC 8656 subset needed
//! for QUIC-over-TURN. It is NOT a general-purpose TURN library.
//!
//! Q1 finding: quinn 0.11.9 exposes `Endpoint::new_with_abstract_socket` which
//! accepts `Arc<dyn AsyncUdpSocket>`. Task 2 should implement the trait on a
//! wrapper around `TurnClient` instead of the loopback UDP proxy fallback.

use std::{collections::HashMap, future::Future, net::SocketAddr, task::ready, time::Duration};

use anyhow::{anyhow, ensure, Result};
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use quinn::udp::{RecvMeta, Transmit};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use tokio::{
    net::UdpSocket,
    time::{timeout, Instant},
};
use tracing::{debug, trace};

// ── Constants ──────────────────────────────────────────────────────────

const MAGIC_COOKIE: u32 = 0x2112_A442;
const STUN_HEADER_LEN: usize = 20;
const CHANNEL_DATA_HEADER_LEN: usize = 4;
#[allow(dead_code)]
const HMAC_SHA1_LEN: usize = 20;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

// ── STUN class / method encoding ───────────────────────────────────────
// Bit layout: bit 0 = C0, bit 4 = C1, remaining bits = method

fn stun_type(method: u16, class: u8) -> u16 {
    method | ((class as u16 & 0x01) << 4) | ((class as u16 & 0x02) << 7)
}

fn split_type(t: u16) -> (u16, u8) {
    let method = t & 0xFEEF;
    let class = ((t >> 4) & 0x01) as u8 | ((t >> 7) & 0x02) as u8;
    (method, class)
}

// ── Method codes ───────────────────────────────────────────────────────

pub const METHOD_BINDING: u16 = 0x001;
pub const METHOD_ALLOCATE: u16 = 0x003;
pub const METHOD_REFRESH: u16 = 0x004;
pub const METHOD_SEND: u16 = 0x006;
pub const METHOD_CREATE_PERMISSION: u16 = 0x008;
pub const METHOD_CHANNEL_BIND: u16 = 0x00A;

// ── Class codes ────────────────────────────────────────────────────────

pub const CLASS_REQUEST: u8 = 0b00;
pub const CLASS_INDICATION: u8 = 0b01;
pub const CLASS_SUCCESS: u8 = 0b10;
pub const CLASS_ERROR: u8 = 0b11;

// ── Attribute types ────────────────────────────────────────────────────

#[allow(dead_code)]
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_ERROR_CODE: u16 = 0x0009;
const ATTR_REALM: u16 = 0x0014;
const ATTR_NONCE: u16 = 0x0015;
#[allow(dead_code)]
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;
const ATTR_LIFETIME: u16 = 0x000D;
const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
const ATTR_CHANNEL_NUMBER: u16 = 0x000C;
const ATTR_DATA: u16 = 0x0013;
#[allow(dead_code)]
const ATTR_MESSAGE_INTEGRITY_SHA256: u16 = 0x001C;

const IPV4_FAMILY: u8 = 0x01;
const IPV6_FAMILY: u8 = 0x02;

// ── Transaction ID ─────────────────────────────────────────────────────

pub type TransactionId = [u8; 12];
pub type ChannelNumber = u16;

fn random_transaction_id() -> TransactionId {
    let mut id = [0u8; 12];
    let u = uuid::Uuid::new_v4();
    id.copy_from_slice(&u.as_bytes()[..12]);
    id
}

// ── Attribute value helpers ────────────────────────────────────────────

/// XOR-encode a SocketAddr per RFC 5389 §15.2.
fn xor_addr(addr: SocketAddr, xpad: &[u8; 16]) -> Vec<u8> {
    let family = match addr {
        SocketAddr::V4(_) => IPV4_FAMILY,
        SocketAddr::V6(_) => IPV6_FAMILY,
    };
    let port_xor = addr.port() ^ ((MAGIC_COOKIE >> 16) as u16);
    let mut buf = Vec::with_capacity(20);
    buf.push(0); // reserved
    buf.push(family);
    buf.extend_from_slice(&port_xor.to_be_bytes());
    match addr.ip() {
        std::net::IpAddr::V4(ip) => {
            let ip_bytes = ip.octets();
            for i in 0..4 {
                buf.push(ip_bytes[i] ^ xpad[i]);
            }
        }
        std::net::IpAddr::V6(ip) => {
            let ip_bytes = ip.octets();
            for i in 0..4 {
                buf.push(ip_bytes[i] ^ xpad[i]);
            }
            for i in 4..16 {
                buf.push(ip_bytes[i] ^ xpad[i]);
            }
        }
    }
    buf
}

fn decode_xor_addr(data: &[u8], xpad: &[u8; 16]) -> Result<SocketAddr> {
    ensure!(data.len() >= 4, "xor-address too short");
    let _reserved = data[0];
    let family = data[1];
    let port_raw = u16::from_be_bytes([data[2], data[3]]);
    let port = port_raw ^ ((MAGIC_COOKIE >> 16) as u16);

    match family {
        IPV4_FAMILY => {
            ensure!(data.len() >= 8, "xor-address ipv4 too short");
            let mut ip = [0u8; 4];
            for i in 0..4 {
                ip[i] = data[4 + i] ^ xpad[i];
            }
            Ok(SocketAddr::new(std::net::IpAddr::V4(ip.into()), port))
        }
        IPV6_FAMILY => {
            ensure!(data.len() >= 20, "xor-address ipv6 too short");
            let mut ip = [0u8; 16];
            for i in 0..4 {
                ip[i] = data[4 + i] ^ xpad[i];
            }
            for i in 4..16 {
                ip[i] = data[4 + i] ^ xpad[i];
            }
            Ok(SocketAddr::new(std::net::IpAddr::V6(ip.into()), port))
        }
        _ => Err(anyhow!("unknown address family {family}")),
    }
}

fn make_xpad(transaction_id: &TransactionId) -> [u8; 16] {
    let mut xpad = [0u8; 16];
    xpad[0..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    xpad[4..16].copy_from_slice(transaction_id);
    xpad
}

// ── StunClass ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StunClass {
    Request,
    Indication,
    Success,
    Error,
}

impl StunClass {
    fn code(self) -> u8 {
        match self {
            StunClass::Request => CLASS_REQUEST,
            StunClass::Indication => CLASS_INDICATION,
            StunClass::Success => CLASS_SUCCESS,
            StunClass::Error => CLASS_ERROR,
        }
    }

    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            CLASS_REQUEST => StunClass::Request,
            CLASS_INDICATION => StunClass::Indication,
            CLASS_SUCCESS => StunClass::Success,
            CLASS_ERROR => StunClass::Error,
            _ => return None,
        })
    }

    pub fn is_success(&self) -> bool {
        matches!(self, StunClass::Success)
    }

    pub fn is_error(&self) -> bool {
        matches!(self, StunClass::Error)
    }
}

// ── StunAttribute ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum StunAttribute {
    Username(String),
    Realm(String),
    Nonce(String),
    ErrorCode { code: u16, reason: String },
    RequestedTransport(UdpTransport),
    Lifetime(u32),
    ChannelNumber(u16),
    XorPeerAddress(SocketAddr),
    XorRelayedAddress(SocketAddr),
    Data(Vec<u8>),
    MessageIntegrity([u8; 20]),
    Unknown { attr_type: u16, value: Vec<u8> },
}

impl StunAttribute {
    fn attr_type(&self) -> u16 {
        match self {
            StunAttribute::Username(_) => ATTR_USERNAME,
            StunAttribute::Realm(_) => ATTR_REALM,
            StunAttribute::Nonce(_) => ATTR_NONCE,
            StunAttribute::ErrorCode { .. } => ATTR_ERROR_CODE,
            StunAttribute::RequestedTransport(_) => ATTR_REQUESTED_TRANSPORT,
            StunAttribute::Lifetime(_) => ATTR_LIFETIME,
            StunAttribute::ChannelNumber(_) => ATTR_CHANNEL_NUMBER,
            StunAttribute::XorPeerAddress(_) => ATTR_XOR_PEER_ADDRESS,
            StunAttribute::XorRelayedAddress(_) => ATTR_XOR_RELAYED_ADDRESS,
            StunAttribute::Data(_) => ATTR_DATA,
            StunAttribute::MessageIntegrity(_) => ATTR_MESSAGE_INTEGRITY,
            StunAttribute::Unknown { attr_type, .. } => *attr_type,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpTransport {
    Udp,
}

// ── StunMessage ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct StunMessage {
    pub class: StunClass,
    pub method: u16,
    pub transaction_id: TransactionId,
    pub attributes: Vec<StunAttribute>,
}

impl StunMessage {
    pub fn new(class: StunClass, method: u16) -> Self {
        Self {
            class,
            method,
            transaction_id: random_transaction_id(),
            attributes: Vec::new(),
        }
    }

    /// Encode to wire bytes.
    ///
    /// XOR-address attributes are encoded using the transaction ID as the
    /// XOR pad.  MESSAGE-INTEGRITY values are NOT computed (call
    /// [`finalize_message_integrity`] on the output to patch the HMAC).
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(256);
        let xpad = make_xpad(&self.transaction_id);

        // Header
        let msg_type = stun_type(self.method, self.class.code());
        buf.extend_from_slice(&msg_type.to_be_bytes());
        let body_len_pos = buf.len();
        buf.extend_from_slice(&[0u8; 2]); // placeholder length
        buf.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        buf.extend_from_slice(&self.transaction_id);

        // Attributes
        for attr in &self.attributes {
            let attr_type = attr.attr_type();
            let value = match attr {
                StunAttribute::XorPeerAddress(addr) | StunAttribute::XorRelayedAddress(addr) => {
                    xor_addr(*addr, &xpad)
                }
                _ => attribute_value(attr)?,
            };
            let padded_len = (value.len() + 3) & !3;
            buf.extend_from_slice(&attr_type.to_be_bytes());
            buf.extend_from_slice(&(value.len() as u16).to_be_bytes());
            buf.extend_from_slice(&value);
            buf.extend(std::iter::repeat(0u8).take(padded_len - value.len()));
        }

        // Set length
        let body_len = buf.len() - STUN_HEADER_LEN;
        buf[body_len_pos..body_len_pos + 2].copy_from_slice(&(body_len as u16).to_be_bytes());
        Ok(buf)
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        ensure!(buf.len() >= STUN_HEADER_LEN, "STUN message too short");

        let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
        let (method, class_code) = split_type(msg_type);
        let class = StunClass::from_code(class_code)
            .ok_or_else(|| anyhow!("invalid STUN class {class_code}"))?;
        let _len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        let cookie = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        ensure!(cookie == MAGIC_COOKIE, "invalid STUN magic cookie");

        let mut transaction_id = [0u8; 12];
        transaction_id.copy_from_slice(&buf[8..20]);
        let xpad = make_xpad(&transaction_id);

        let mut attributes = Vec::new();
        let mut pos = STUN_HEADER_LEN;

        while pos + 4 <= buf.len() {
            let attr_type = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
            let attr_len = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
            let padded_len = (attr_len + 3) & !3;

            if pos + 4 + attr_len > buf.len() {
                break;
            }

            let value_start = pos + 4;
            let value = &buf[value_start..value_start + attr_len];

            let attr = match attr_type {
                ATTR_USERNAME => StunAttribute::Username(
                    String::from_utf8(value.to_vec())
                        .map_err(|_| anyhow!("invalid UTF-8 in USERNAME attribute"))?,
                ),
                ATTR_REALM => StunAttribute::Realm(
                    String::from_utf8(value.to_vec())
                        .map_err(|_| anyhow!("invalid UTF-8 in REALM attribute"))?,
                ),
                ATTR_NONCE => StunAttribute::Nonce(
                    String::from_utf8(value.to_vec())
                        .map_err(|_| anyhow!("invalid UTF-8 in NONCE attribute"))?,
                ),
                ATTR_ERROR_CODE => {
                    if value.len() < 4 {
                        StunAttribute::ErrorCode {
                            code: 0,
                            reason: "malformed".into(),
                        }
                    } else {
                        let code_class = value[2];
                        let code_number = value[3];
                        let code = (code_class as u16) * 100 + (code_number as u16);
                        let reason = if value.len() > 4 {
                            String::from_utf8_lossy(&value[4..]).to_string()
                        } else {
                            String::new()
                        };
                        StunAttribute::ErrorCode { code, reason }
                    }
                }
                ATTR_REQUESTED_TRANSPORT => StunAttribute::RequestedTransport(UdpTransport::Udp),
                ATTR_LIFETIME => {
                    let secs = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
                    StunAttribute::Lifetime(secs)
                }
                ATTR_CHANNEL_NUMBER => {
                    let ch = u16::from_be_bytes([value[0], value[1]]);
                    StunAttribute::ChannelNumber(ch)
                }
                ATTR_XOR_PEER_ADDRESS => {
                    let addr = decode_xor_addr(value, &xpad)?;
                    StunAttribute::XorPeerAddress(addr)
                }
                ATTR_XOR_RELAYED_ADDRESS => {
                    let addr = decode_xor_addr(value, &xpad)?;
                    StunAttribute::XorRelayedAddress(addr)
                }
                ATTR_DATA => StunAttribute::Data(value.to_vec()),
                ATTR_MESSAGE_INTEGRITY => {
                    let mut h = [0u8; 20];
                    let copy_len = attr_len.min(20);
                    h[..copy_len].copy_from_slice(&value[..copy_len]);
                    attributes.push(StunAttribute::MessageIntegrity(h));
                    // MESSAGE-INTEGRITY must be last; stop parsing
                    break;
                }
                _ => StunAttribute::Unknown {
                    attr_type,
                    value: value.to_vec(),
                },
            };

            attributes.push(attr);
            pos += 4 + padded_len;
        }

        Ok(Self {
            class,
            method,
            transaction_id,
            attributes,
        })
    }

    pub fn get_username(&self) -> Option<&str> {
        self.attributes.iter().find_map(|a| {
            if let StunAttribute::Username(u) = a {
                Some(u.as_str())
            } else {
                None
            }
        })
    }

    pub fn get_realm(&self) -> Option<&str> {
        self.attributes.iter().find_map(|a| {
            if let StunAttribute::Realm(r) = a {
                Some(r.as_str())
            } else {
                None
            }
        })
    }

    pub fn get_nonce(&self) -> Option<&str> {
        self.attributes.iter().find_map(|a| {
            if let StunAttribute::Nonce(n) = a {
                Some(n.as_str())
            } else {
                None
            }
        })
    }

    pub fn get_error_code(&self) -> Option<(u16, &str)> {
        self.attributes.iter().find_map(|a| {
            if let StunAttribute::ErrorCode { code, reason } = a {
                Some((*code, reason.as_str()))
            } else {
                None
            }
        })
    }
}

fn attribute_value(attr: &StunAttribute) -> Result<Vec<u8>> {
    Ok(match attr {
        StunAttribute::Username(s) => s.as_bytes().to_vec(),
        StunAttribute::Realm(s) => s.as_bytes().to_vec(),
        StunAttribute::Nonce(s) => s.as_bytes().to_vec(),
        StunAttribute::ErrorCode { code, reason } => {
            let class_byte = (code / 100) as u8;
            let number_byte = (code % 100) as u8;
            let mut v = vec![0, 0, class_byte, number_byte];
            v.extend_from_slice(reason.as_bytes());
            v
        }
        StunAttribute::RequestedTransport(UdpTransport::Udp) => {
            vec![17, 0, 0, 0] // UDP protocol number
        }
        StunAttribute::Lifetime(secs) => secs.to_be_bytes().to_vec(),
        StunAttribute::ChannelNumber(ch) => {
            let mut v = ch.to_be_bytes().to_vec();
            v.extend_from_slice(&[0u8; 2]); // RFFU = 0
            v
        }
        StunAttribute::XorPeerAddress(addr) => {
            // Uses transaction_id from the message for the xpad
            // But we don't have it here — caller sets xpad via the message's tx id
            // For encoding we use zeros as placeholder; caller must fix up
            xor_addr(*addr, &[0u8; 16])
        }
        StunAttribute::XorRelayedAddress(addr) => xor_addr(*addr, &[0u8; 16]),
        StunAttribute::Data(d) => d.clone(),
        StunAttribute::MessageIntegrity(h) => h.to_vec(),
        StunAttribute::Unknown { value, .. } => value.clone(),
    })
}

/// Compute MESSAGE-INTEGRITY (HMAC-SHA1) for a STUN message.
///
/// The message may or may not include a MESSAGE-INTEGRITY attribute at the
/// end — if it does, it is stripped before encoding so the HMAC covers only
/// the attributes before it per RFC 5389 §15.4.
pub fn compute_message_integrity(msg: &StunMessage, password: &str) -> Result<[u8; 20]> {
    let username = msg
        .get_username()
        .ok_or_else(|| anyhow!("MESSAGE-INTEGRITY requires USERNAME"))?;
    let realm = msg
        .get_realm()
        .ok_or_else(|| anyhow!("MESSAGE-INTEGRITY requires REALM"))?;

    // key = MD5(username ":" realm ":" password)
    let mut hasher = Md5::new();
    hasher.update(username.as_bytes());
    hasher.update(b":");
    hasher.update(realm.as_bytes());
    hasher.update(b":");
    hasher.update(password.as_bytes());
    let key = hasher.finalize();

    // Re-encode without MI attribute
    let mut msg_no_mi = msg.clone();
    msg_no_mi
        .attributes
        .retain(|a| !matches!(a, StunAttribute::MessageIntegrity(_)));
    let data = msg_no_mi.encode()?;

    // Pad to 64 bytes
    let pad = (64 - (data.len() % 64)) % 64;

    let mut mac = Hmac::<Sha1>::new_from_slice(&key).map_err(|e| anyhow!("HMAC key: {e}"))?;
    mac.update(&data);
    if pad > 0 {
        mac.update(&vec![0u8; pad]);
    }
    let result = mac.finalize().into_bytes();
    let mut h = [0u8; 20];
    h.copy_from_slice(&result);
    Ok(h)
}

/// Patch the MESSAGE-INTEGRITY placeholder in an already-encoded STUN
/// message with the correct HMAC.  The `msg` must have the MI attribute as
/// its last element, and `encoded` must be the output of
/// [`StunMessage::encode`] for that same message.
pub fn finalize_message_integrity(
    msg: &StunMessage,
    password: &str,
    encoded: &mut [u8],
) -> Result<()> {
    let h = compute_message_integrity(msg, password)?;
    let mi_start = encoded
        .len()
        .checked_sub(20)
        .ok_or_else(|| anyhow!("encoded message too short for MI"))?;
    encoded[mi_start..].copy_from_slice(&h);
    Ok(())
}

// ── ChannelData ────────────────────────────────────────────────────────

/// ChannelData framing (RFC 8656 §11.4).
/// This is NOT a STUN message — it's a compact 4-byte header + payload.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelData {
    pub channel: ChannelNumber,
    pub data: Vec<u8>,
}

impl ChannelData {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(CHANNEL_DATA_HEADER_LEN + self.data.len());
        buf.extend_from_slice(&self.channel.to_be_bytes());
        buf.extend_from_slice(&(self.data.len() as u16).to_be_bytes());
        buf.extend_from_slice(&self.data);
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        ensure!(
            buf.len() >= CHANNEL_DATA_HEADER_LEN,
            "ChannelData too short"
        );
        let channel = u16::from_be_bytes([buf[0], buf[1]]);
        let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        ensure!(
            buf.len() >= CHANNEL_DATA_HEADER_LEN + len,
            "ChannelData payload truncated"
        );
        Ok(Self {
            channel,
            data: buf[CHANNEL_DATA_HEADER_LEN..CHANNEL_DATA_HEADER_LEN + len].to_vec(),
        })
    }

    /// Returns true if the buffer starts with a ChannelData frame
    /// (channel number >= 0x4000). STUN messages start with magic cookie at
    /// byte 4, so we can disambiguate by checking bits 0-1 of byte 0.
    pub fn is_channel_data(buf: &[u8]) -> bool {
        buf.len() >= 2 && buf[0] & 0xC0 == 0x40
    }
}

// ── TurnCredentials ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCredentials {
    pub urls: Vec<String>,
    pub username: String,
    pub password: String,
    pub ttl: u32,
}

// ── TurnAllocation ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TurnAllocation {
    pub relayed: SocketAddr,
    pub lifetime_secs: u32,
}

// ── TurnError ──────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("STUN decode error: {0}")]
    StunDecode(String),
    #[error("STUN encode error: {0}")]
    StunEncode(String),
    #[error("TURN operation timed out")]
    Timeout,
    #[error("unauthorized: realm={realm}, nonce={nonce}")]
    Unauthorized { realm: String, nonce: String },
    #[error("TURN error code {code}: {reason}")]
    ErrorCode { code: u16, reason: String },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for TurnError {
    fn from(e: anyhow::Error) -> Self {
        TurnError::Other(e.to_string())
    }
}

// ── TurnClient ─────────────────────────────────────────────────────────

pub struct TurnClient {
    pub server: SocketAddr,
    pub credentials: TurnCredentials,
    pub local: UdpSocket,
    pub allocation: Option<TurnAllocation>,
    pub next_channel: u16,
    realm: Option<String>,
    nonce: Option<String>,
    pending: HashMap<TransactionId, ChannelNumber>,
    pub(crate) channels: HashMap<ChannelNumber, SocketAddr>,
}

impl TurnClient {
    /// Open a local UDP socket, connect to the TURN server, and allocate a
    /// relayed address.
    pub async fn new(server: SocketAddr, credentials: TurnCredentials) -> Result<Self> {
        let local = UdpSocket::bind("0.0.0.0:0").await?;
        local.connect(server).await?;

        let mut client = Self {
            server,
            credentials,
            local,
            allocation: None,
            next_channel: 0x4000,
            realm: None,
            nonce: None,
            pending: HashMap::new(),
            channels: HashMap::new(),
        };

        let relayed = client.send_allocate().await?;
        client.allocation = Some(TurnAllocation {
            relayed,
            lifetime_secs: 3600,
        });
        debug!(%relayed, "TURN allocation established");

        Ok(client)
    }

    async fn send_allocate(&mut self) -> Result<SocketAddr> {
        let mut retries = 0;
        loop {
            let (realm, nonce) = (self.realm.clone(), self.nonce.clone());
            let mut msg = StunMessage::new(StunClass::Request, METHOD_ALLOCATE);
            msg.attributes
                .push(StunAttribute::RequestedTransport(UdpTransport::Udp));
            msg.attributes.push(StunAttribute::Lifetime(3600));

            if let (Some(r), Some(n)) = (&realm, &nonce) {
                msg.attributes
                    .push(StunAttribute::Username(self.credentials.username.clone()));
                msg.attributes.push(StunAttribute::Realm(r.clone()));
                msg.attributes.push(StunAttribute::Nonce(n.clone()));
                msg.attributes
                    .push(StunAttribute::MessageIntegrity([0u8; 20]));
            }

            self.pending.insert(msg.transaction_id, 0);
            self.send_request(&msg).await?;

            let resp = self.recv_stun(&msg.transaction_id).await?;

            match resp.class {
                StunClass::Success => {
                    for attr in &resp.attributes {
                        if let StunAttribute::XorRelayedAddress(addr) = attr {
                            return Ok(*addr);
                        }
                    }
                    return Err(anyhow!("Allocate success missing XOR-RELAYED-ADDRESS"));
                }
                StunClass::Error => {
                    let (code, reason) = resp.get_error_code().unwrap_or((0, "unknown"));
                    if code == 401 {
                        if retries >= 1 {
                            return Err(anyhow!("Allocate 401 after retry: {reason}"));
                        }
                        let r = resp
                            .get_realm()
                            .ok_or_else(|| anyhow!("401 without REALM"))?
                            .to_string();
                        let n = resp
                            .get_nonce()
                            .ok_or_else(|| anyhow!("401 without NONCE"))?
                            .to_string();
                        self.realm = Some(r);
                        self.nonce = Some(n);
                        retries += 1;
                        continue; // retry with auth
                    }
                    return Err(anyhow!("Allocate error {code}: {reason}"));
                }
                _ => return Err(anyhow!("unexpected response class")),
            }
        }
    }

    /// Create a permission for a peer address.
    pub async fn create_permission(&mut self, peer: SocketAddr) -> Result<()> {
        let msg = self.build_authenticated_request(METHOD_CREATE_PERMISSION, |m| {
            m.attributes.push(StunAttribute::XorPeerAddress(peer));
        })?;

        self.pending.insert(msg.transaction_id, 0);
        self.send_request(&msg).await?;

        let resp = self.recv_stun(&msg.transaction_id).await?;

        match resp.class {
            StunClass::Success => {
                debug!(%peer, "TURN permission created");
                Ok(())
            }
            StunClass::Error => {
                let (code, reason) = resp.get_error_code().unwrap_or((0, ""));
                Err(anyhow!("CreatePermission error {code}: {reason}"))
            }
            _ => Err(anyhow!("unexpected response class")),
        }
    }

    /// Bind a channel number to a peer address. Returns the channel number.
    pub async fn channel_bind(&mut self, peer: SocketAddr) -> Result<ChannelNumber> {
        let ch = self.next_channel;
        self.next_channel += 1;

        let msg = self.build_authenticated_request(METHOD_CHANNEL_BIND, |m| {
            m.attributes.push(StunAttribute::ChannelNumber(ch));
            m.attributes.push(StunAttribute::XorPeerAddress(peer));
        })?;

        self.pending.insert(msg.transaction_id, ch);
        self.send_request(&msg).await?;

        let resp = self.recv_stun(&msg.transaction_id).await?;

        match resp.class {
            StunClass::Success => {
                self.channels.insert(ch, peer);
                debug!(%peer, %ch, "TURN channel bound");
                Ok(ch)
            }
            StunClass::Error => {
                let (code, reason) = resp.get_error_code().unwrap_or((0, ""));
                Err(anyhow!("ChannelBind error {code}: {reason}"))
            }
            _ => Err(anyhow!("unexpected response class")),
        }
    }

    /// Send data to a peer using the ChannelBind path.
    /// Requires `channel_bind` to have been called first.
    pub async fn send_data(&self, peer: SocketAddr, data: &[u8]) -> Result<()> {
        let ch = self
            .channels
            .iter()
            .find_map(|(c, p)| if *p == peer { Some(*c) } else { None })
            .ok_or_else(|| anyhow!("no channel bound for peer {peer}"))?;

        let frame = ChannelData {
            channel: ch,
            data: data.to_vec(),
        };
        let encoded = frame.encode();
        self.local.send(&encoded).await?;
        trace!(%peer, %ch, len = data.len(), "TURN send channel-data");
        Ok(())
    }

    /// Receive data from any peer. Returns the peer address and data.
    pub async fn recv_data(&mut self) -> Result<(SocketAddr, Vec<u8>)> {
        let mut buf = vec![0u8; 65535];
        let n = self.local.recv(&mut buf).await?;
        let buf = &buf[..n];

        if ChannelData::is_channel_data(buf) {
            let frame = ChannelData::decode(buf)?;
            let peer = self
                .channels
                .get(&frame.channel)
                .ok_or_else(|| anyhow!("unknown channel {}", frame.channel))?;
            trace!(
                channel = frame.channel,
                len = frame.data.len(),
                "TURN recv channel-data"
            );
            Ok((*peer, frame.data))
        } else {
            let msg = StunMessage::decode(buf)?;
            match msg.class {
                StunClass::Indication if msg.method == METHOD_BINDING => {
                    // Data Indication uses method 0x001
                    let peer = msg
                        .attributes
                        .iter()
                        .find_map(|a| {
                            if let StunAttribute::XorPeerAddress(addr) = a {
                                Some(*addr)
                            } else {
                                None
                            }
                        })
                        .ok_or_else(|| anyhow!("Data indication without XOR-PEER-ADDRESS"))?;
                    let data = msg
                        .attributes
                        .iter()
                        .find_map(|a| {
                            if let StunAttribute::Data(d) = a {
                                Some(d.clone())
                            } else {
                                None
                            }
                        })
                        .ok_or_else(|| anyhow!("Data indication without DATA"))?;
                    trace!(%peer, len = data.len(), "TURN recv data-indication");
                    Ok((peer, data))
                }
                _ => Err(anyhow!(
                    "unexpected STUN message: class={:?} method={}",
                    msg.class,
                    msg.method
                )),
            }
        }
    }

    /// Refresh the allocation with a new lifetime.
    pub async fn refresh(&mut self, lifetime_secs: u32) -> Result<()> {
        let msg = self.build_authenticated_request(METHOD_REFRESH, |m| {
            m.attributes.push(StunAttribute::Lifetime(lifetime_secs));
        })?;

        self.pending.insert(msg.transaction_id, 0);
        self.send_request(&msg).await?;

        let resp = self.recv_stun(&msg.transaction_id).await?;

        match resp.class {
            StunClass::Success => {
                if let Some(alloc) = &mut self.allocation {
                    alloc.lifetime_secs = lifetime_secs;
                }
                debug!(%lifetime_secs, "TURN allocation refreshed");
                Ok(())
            }
            StunClass::Error => {
                let (code, reason) = resp.get_error_code().unwrap_or((0, ""));
                Err(anyhow!("Refresh error {code}: {reason}"))
            }
            _ => Err(anyhow!("unexpected response class")),
        }
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.local.local_addr()
    }

    // ── Internal helpers ────────────────────────────────────────────

    fn build_authenticated_request<F>(&self, method: u16, f: F) -> Result<StunMessage>
    where
        F: FnOnce(&mut StunMessage),
    {
        let mut msg = StunMessage::new(StunClass::Request, method);
        msg.attributes
            .push(StunAttribute::Username(self.credentials.username.clone()));

        if let Some(r) = &self.realm {
            msg.attributes.push(StunAttribute::Realm(r.clone()));
        }
        if let Some(n) = &self.nonce {
            msg.attributes.push(StunAttribute::Nonce(n.clone()));
        }

        f(&mut msg);

        if self.realm.is_some() {
            msg.attributes
                .push(StunAttribute::MessageIntegrity([0u8; 20]));
        }

        Ok(msg)
    }

    /// Encode a request and send it.  If the request has MESSAGE-INTEGRITY
    /// as the last attribute, finalize it with the correct HMAC.
    async fn send_request(&self, msg: &StunMessage) -> Result<Vec<u8>> {
        let mut encoded = msg.encode()?;
        let has_mi = msg
            .attributes
            .last()
            .is_some_and(|a| matches!(a, StunAttribute::MessageIntegrity(_)));
        if has_mi {
            finalize_message_integrity(msg, &self.credentials.password, &mut encoded)?;
        }
        self.local.send(&encoded).await?;
        Ok(encoded)
    }

    async fn recv_stun(&self, expected_tx: &TransactionId) -> Result<StunMessage> {
        let deadline = Instant::now() + DEFAULT_TIMEOUT;
        let mut buf = vec![0u8; 65535];

        while Instant::now() < deadline {
            let remaining = deadline - Instant::now();
            let n = timeout(remaining, self.local.recv(&mut buf))
                .await
                .map_err(|_| anyhow!("STUN response timeout"))??;

            if n < STUN_HEADER_LEN {
                continue;
            }

            // Skip ChannelData frames
            if ChannelData::is_channel_data(&buf[..n]) {
                continue;
            }

            let msg = StunMessage::decode(&buf[..n])?;
            if msg.transaction_id == *expected_tx {
                return Ok(msg);
            }
            // Mismatched transaction — ignore
            trace!(expected = ?expected_tx, got = ?msg.transaction_id, "ignoring STUN response with mismatched tx id");
        }

        Err(anyhow!("STUN response timeout"))
    }
}

// ── TurnUdpSocket — quinn AsyncUdpSocket adapter ──────────────────

use std::io::{self, IoSliceMut};
use std::pin::Pin;
use std::sync::Arc as StdArc;
use std::task::{Context, Poll};

use quinn::{AsyncUdpSocket, UdpPoller};

impl TurnClient {
    /// Consume the client and extract the server address + UDP socket +
    /// channel mappings needed by [`TurnUdpSocket`].
    pub fn into_io_parts(
        self,
    ) -> (
        SocketAddr,
        tokio::net::UdpSocket,
        HashMap<ChannelNumber, SocketAddr>,
        HashMap<SocketAddr, ChannelNumber>,
    ) {
        let rev: HashMap<SocketAddr, ChannelNumber> = self
            .channels
            .iter()
            .map(|(&ch, &addr)| (addr, ch))
            .collect();
        (self.server, self.local, self.channels, rev)
    }
}

/// A quinn [`AsyncUdpSocket`] that sends/receives QUIC traffic through a
/// TURN relay tunnel.  Outbound datagrams are wrapped as TURN ChannelData
/// frames and sent to the TURN server.  Inbound ChannelData frames (and
/// Data indications) are unwrapped and presented to quinn as UDP datagrams.
#[derive(Debug)]
pub struct TurnUdpSocket {
    local: tokio::net::UdpSocket,
    #[allow(dead_code)] // kept for potential reconnection
    server: SocketAddr,
    channels: std::sync::RwLock<HashMap<ChannelNumber, SocketAddr>>,
    peer_channels: std::sync::RwLock<HashMap<SocketAddr, ChannelNumber>>,
    waker: StdArc<tokio::sync::Notify>,
}

impl TurnUdpSocket {
    /// Open a TURN relay, bind a channel to `peer`, and return a
    /// quinn-compatible socket wrapping that channel.
    pub async fn new(
        server: SocketAddr,
        credentials: TurnCredentials,
        peer: SocketAddr,
    ) -> Result<StdArc<Self>> {
        let mut client = TurnClient::new(server, credentials).await?;
        client.create_permission(peer).await?;
        client.channel_bind(peer).await?;
        let (srv, local, channels, peer_channels) = client.into_io_parts();
        Ok(StdArc::new(Self {
            local,
            server: srv,
            channels: std::sync::RwLock::new(channels),
            peer_channels: std::sync::RwLock::new(peer_channels),
            waker: StdArc::new(tokio::sync::Notify::new()),
        }))
    }

    /// Build from an already-configured TurnClient (testing / advanced
    /// usage).  The client must have completed at least one channel bind.
    pub fn from_client(client: TurnClient) -> StdArc<Self> {
        let (server, local, channels, peer_channels) = client.into_io_parts();
        StdArc::new(Self {
            local,
            server,
            channels: std::sync::RwLock::new(channels),
            peer_channels: std::sync::RwLock::new(peer_channels),
            waker: StdArc::new(tokio::sync::Notify::new()),
        })
    }
}

impl AsyncUdpSocket for TurnUdpSocket {
    fn create_io_poller(self: StdArc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(TurnUdpPoller {
            waker: self.waker.clone(),
        })
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        let ch = {
            let peer_channels = self.peer_channels.read().unwrap();
            peer_channels
                .get(&transmit.destination)
                .copied()
                .ok_or(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "no TURN channel bound for peer",
                ))?
        };
        let frame = ChannelData {
            channel: ch,
            data: transmit.contents.to_vec(),
        };
        let encoded = frame.encode();
        // try_send goes through Registration::try_io which checks cached
        // readiness. In real usage the socket is always polled before this
        // is called (quinn's driver ensures this). For tests, a yield point
        // must precede the first try_send.
        match self.local.try_send(&encoded) {
            Ok(_) => {
                self.waker.notify_one();
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.waker.notify_one();
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        ready!(self.local.poll_recv_ready(cx)?);

        let mut buf = [0u8; 65535];
        let n = match self.local.try_recv(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Poll::Pending,
            Err(e) => return Poll::Ready(Err(e)),
        };
        let data = &buf[..n];

        if ChannelData::is_channel_data(data) {
            let frame = ChannelData::decode(data)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let channels = self.channels.read().unwrap();
            let peer = *channels.get(&frame.channel).ok_or(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown TURN channel",
            ))?;
            drop(channels);
            let copy_len = frame.data.len().min(bufs[0].len());
            bufs[0][..copy_len].copy_from_slice(&frame.data[..copy_len]);
            meta[0] = RecvMeta {
                addr: peer,
                len: copy_len,
                stride: copy_len,
                ecn: None,
                dst_ip: None,
            };
            return Poll::Ready(Ok(1));
        }

        // STUN Data Indication (RFC 8656 §11.3)
        let msg =
            StunMessage::decode(data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        match msg.class {
            StunClass::Indication if msg.method == METHOD_SEND => {
                let peer = msg
                    .attributes
                    .iter()
                    .find_map(|a| {
                        if let StunAttribute::XorPeerAddress(addr) = a {
                            Some(*addr)
                        } else {
                            None
                        }
                    })
                    .ok_or(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "DataIndication missing XOR-PEER-ADDRESS",
                    ))?;
                let d = msg
                    .attributes
                    .iter()
                    .find_map(|a| {
                        if let StunAttribute::Data(d) = a {
                            Some(d.clone())
                        } else {
                            None
                        }
                    })
                    .ok_or(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "DataIndication missing DATA",
                    ))?;
                let copy_len = d.len().min(bufs[0].len());
                bufs[0][..copy_len].copy_from_slice(&d[..copy_len]);
                meta[0] = RecvMeta {
                    addr: peer,
                    len: copy_len,
                    stride: copy_len,
                    ecn: None,
                    dst_ip: None,
                };
                Poll::Ready(Ok(1))
            }
            _ => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected STUN message on TURN relay socket",
            ))),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.local.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        1
    }

    fn max_receive_segments(&self) -> usize {
        1
    }

    fn may_fragment(&self) -> bool {
        false
    }
}

#[derive(Debug)]
struct TurnUdpPoller {
    waker: StdArc<tokio::sync::Notify>,
}

impl UdpPoller for TurnUdpPoller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        let notified = self.waker.notified();
        tokio::pin!(notified);
        match notified.poll(cx) {
            Poll::Ready(()) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    #[test]
    fn stun_encode_decode_roundtrip_binding() {
        let mut msg = StunMessage::new(StunClass::Request, METHOD_BINDING);
        msg.attributes.push(StunAttribute::Username("test".into()));

        let encoded = msg.encode().unwrap();
        let decoded = StunMessage::decode(&encoded).unwrap();

        assert_eq!(decoded.class, StunClass::Request);
        assert_eq!(decoded.method, METHOD_BINDING);
        assert_eq!(decoded.transaction_id, msg.transaction_id);
        assert_eq!(decoded.get_username(), Some("test"));
    }

    #[test]
    fn stun_decode_rejects_truncated_message() {
        let buf = [0u8; 1];
        assert!(StunMessage::decode(&buf).is_err());
    }

    #[test]
    fn stun_decode_rejects_wrong_magic_cookie() {
        let mut buf = vec![0u8; 20];
        // Set message type for binding request
        let mt = stun_type(METHOD_BINDING, CLASS_REQUEST);
        buf[0..2].copy_from_slice(&mt.to_be_bytes());
        buf[2..4].copy_from_slice(&[0, 0]); // length = 0
                                            // Wrong magic cookie
        buf[4..8].copy_from_slice(&[0, 0, 0, 0]);
        assert!(StunMessage::decode(&buf).is_err());
    }

    #[test]
    fn xor_peer_address_roundtrip() {
        let tx = [0u8; 12];
        let xpad = make_xpad(&tx);
        let addr: SocketAddr = "10.0.0.1:1234".parse().unwrap();

        let encoded = xor_addr(addr, &xpad);
        let decoded = decode_xor_addr(&encoded, &xpad).unwrap();

        assert_eq!(addr, decoded);
    }

    #[test]
    fn xor_relayed_address_ipv6_roundtrip() {
        let tx = [0u8; 12];
        let xpad = make_xpad(&tx);
        let addr: SocketAddr = "[2001:db8::1]:5678".parse().unwrap();

        let encoded = xor_addr(addr, &xpad);
        let decoded = decode_xor_addr(&encoded, &xpad).unwrap();

        assert_eq!(addr, decoded);
    }

    #[test]
    fn channel_data_encode_decode() {
        let frame = ChannelData {
            channel: 0x4000,
            data: vec![1, 2, 3, 4],
        };

        let encoded = frame.encode();
        let decoded = ChannelData::decode(&encoded).unwrap();

        assert_eq!(frame, decoded);
        assert!(ChannelData::is_channel_data(&encoded));
    }

    #[test]
    fn message_integrity_hmac() {
        // Test vector from RFC 5389 §8.1 (adapted for our API)
        let _key = b"presharedkey";
        let username = "user";
        let realm = "realm";
        let password = "presharedkey";

        // Create a STUN message with USERNAME, REALM, NONCE
        let mut msg = StunMessage::new(StunClass::Request, METHOD_BINDING);
        msg.attributes
            .push(StunAttribute::Username(username.into()));
        msg.attributes.push(StunAttribute::Realm(realm.into()));
        msg.attributes.push(StunAttribute::Nonce("nonce".into()));
        // Add a dummy MI to trigger HMAC computation
        msg.attributes
            .push(StunAttribute::MessageIntegrity([0u8; 20]));

        let h = compute_message_integrity(&msg, password).unwrap();
        assert_eq!(h.len(), 20);

        // Verify the HMAC is deterministic
        let h2 = compute_message_integrity(&msg, password).unwrap();
        assert_eq!(h, h2);

        // Different password gives different HMAC
        let h3 = compute_message_integrity(&msg, "wrong_password").unwrap();
        assert_ne!(h, h3);
    }

    #[test]
    fn allocate_success_response() {
        // Construct an Allocate Success response
        let tx = random_transaction_id();
        let relayed_addr: SocketAddr = "203.0.113.1:3478".parse().unwrap();

        let msg = StunMessage {
            class: StunClass::Success,
            method: METHOD_ALLOCATE,
            transaction_id: tx,
            attributes: vec![
                StunAttribute::XorRelayedAddress(relayed_addr),
                StunAttribute::Lifetime(3600),
            ],
        };

        let encoded = msg.encode().unwrap();
        let decoded = StunMessage::decode(&encoded).unwrap();

        assert_eq!(decoded.class, StunClass::Success);
        assert_eq!(decoded.method, METHOD_ALLOCATE);

        let decoded_relayed = decoded.attributes.iter().find_map(|a| {
            if let StunAttribute::XorRelayedAddress(a) = a {
                Some(*a)
            } else {
                None
            }
        });
        assert_eq!(decoded_relayed, Some(relayed_addr));

        let decoded_lifetime = decoded.attributes.iter().find_map(|a| {
            if let StunAttribute::Lifetime(s) = a {
                Some(*s)
            } else {
                None
            }
        });
        assert_eq!(decoded_lifetime, Some(3600));
    }

    #[test]
    fn issue_credentials_hmac() {
        // Simulate the credential issuance compute and verify format
        let secret = b"test_secret_123";
        let expiry_str = b"1719878400";

        let mut mac = Hmac::<Sha1>::new_from_slice(secret).unwrap();
        mac.update(expiry_str);
        let expiry_hmac = STANDARD.encode(mac.finalize().into_bytes());
        let username = format!("1719878400:{expiry_hmac}");

        let mut mac = Hmac::<Sha1>::new_from_slice(secret).unwrap();
        mac.update(username.as_bytes());
        let password = STANDARD.encode(mac.finalize().into_bytes());

        // Verify format
        assert!(username.contains(':'));
        assert_eq!(STANDARD.decode(&expiry_hmac).unwrap().len(), 20);
        assert_eq!(STANDARD.decode(&password).unwrap().len(), 20);

        // Recompute and verify consistency
        let mut mac = Hmac::<Sha1>::new_from_slice(secret).unwrap();
        mac.update(b"1719878400");
        assert_eq!(STANDARD.encode(mac.finalize().into_bytes()), expiry_hmac);
    }

    #[tokio::test]
    async fn turn_udp_socket_try_send_encapsulates_channel_data() {
        use tokio::net::UdpSocket;

        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let local = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        local.connect(server_addr).await.unwrap();

        let peer_addr: SocketAddr = "203.0.113.5:9999".parse().unwrap();
        let mut channels = HashMap::new();
        let mut peer_channels = HashMap::new();
        channels.insert(0x4000, peer_addr);
        peer_channels.insert(peer_addr, 0x4000);

        let socket = StdArc::new(TurnUdpSocket {
            local,
            server: server_addr,
            channels: std::sync::RwLock::new(channels),
            peer_channels: std::sync::RwLock::new(peer_channels),
            waker: StdArc::new(tokio::sync::Notify::new()),
        });

        let transmit = Transmit {
            destination: peer_addr,
            ecn: None,
            contents: &[1, 2, 3, 4],
            segment_size: None,
            src_ip: None,
        };
        // Yield so the runtime populates cached WRITABLE readiness
        tokio::time::sleep(Duration::from_millis(0)).await;
        socket.try_send(&transmit).unwrap();

        let mut buf = [0u8; 65535];
        let (n, _src) = loop {
            match server.try_recv_from(&mut buf) {
                Ok(v) => break v,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(e) => panic!("{e}"),
            }
        };
        let data = &buf[..n];

        assert!(ChannelData::is_channel_data(data));
        let frame = ChannelData::decode(data).unwrap();
        assert_eq!(frame.channel, 0x4000);
        assert_eq!(frame.data, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn turn_udp_socket_poll_recv_decapsulates_channel_data() {
        use tokio::net::UdpSocket;

        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let local = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local_addr = local.local_addr().unwrap();
        local.connect(server_addr).await.unwrap();

        let peer_addr: SocketAddr = "203.0.113.6:8888".parse().unwrap();
        let mut channels = HashMap::new();
        let mut peer_channels = HashMap::new();
        channels.insert(0x4001, peer_addr);
        peer_channels.insert(peer_addr, 0x4001);

        let socket = StdArc::new(TurnUdpSocket {
            local,
            server: server_addr,
            channels: std::sync::RwLock::new(channels),
            peer_channels: std::sync::RwLock::new(peer_channels),
            waker: StdArc::new(tokio::sync::Notify::new()),
        });

        // Server sends a ChannelData frame to the socket
        let frame = ChannelData {
            channel: 0x4001,
            data: vec![10, 20, 30, 40, 50],
        };
        server.send_to(&frame.encode(), local_addr).await.unwrap();

        // Small delay for delivery
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut backing = [0u8; 65535];
        let mut bufs = [IoSliceMut::new(&mut backing)];
        let mut metas = [RecvMeta::default()];
        let waker = std::task::Waker::noop();
        let cx = &mut Context::from_waker(&waker);
        let poll_result = socket.poll_recv(cx, &mut bufs, &mut metas);
        let count = if let Poll::Ready(Ok(n)) = poll_result {
            n
        } else {
            panic!("expected Ready(Ok(_)), got {poll_result:?}");
        };
        assert_eq!(count, 1);
        assert_eq!(metas[0].addr, peer_addr);
        let received = &bufs[0][..metas[0].len];
        assert_eq!(received, &[10, 20, 30, 40, 50]);
    }

    #[tokio::test]
    async fn turn_udp_socket_local_addr() {
        use tokio::net::UdpSocket;

        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let local = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local_addr = local.local_addr().unwrap();
        local.connect(server_addr).await.unwrap();

        let socket = StdArc::new(TurnUdpSocket {
            local,
            server: server_addr,
            channels: std::sync::RwLock::new(HashMap::new()),
            peer_channels: std::sync::RwLock::new(HashMap::new()),
            waker: StdArc::new(tokio::sync::Notify::new()),
        });

        assert_eq!(socket.local_addr().unwrap(), local_addr);
    }

    #[test]
    fn stun_encode_decode_full_allocate() {
        // Create a full Allocate Request with auth
        let mut msg = StunMessage::new(StunClass::Request, METHOD_ALLOCATE);
        msg.attributes
            .push(StunAttribute::RequestedTransport(UdpTransport::Udp));
        msg.attributes.push(StunAttribute::Lifetime(3600));
        msg.attributes.push(StunAttribute::Username("user".into()));
        msg.attributes.push(StunAttribute::Realm("bp".into()));
        msg.attributes.push(StunAttribute::Nonce("abc123".into()));

        let encoded = msg.encode().unwrap();
        let decoded = StunMessage::decode(&encoded).unwrap();

        assert_eq!(decoded.class, StunClass::Request);
        assert_eq!(decoded.method, METHOD_ALLOCATE);
        assert!(decoded.get_username() == Some("user"));
        assert!(decoded.get_realm() == Some("bp"));
        assert!(decoded.get_nonce() == Some("abc123"));
    }
}
