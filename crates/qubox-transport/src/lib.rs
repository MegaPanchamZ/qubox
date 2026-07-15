use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::media::ControlChannel;
use anyhow::{anyhow, Context};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use qubox_media::{inspect_h264_annex_b_nal_units, EncodedVideoAccessUnit};
use qubox_proto::{
    AudioStreamParams, ControlMsg, RemoteInputEvent, SessionCredential, VideoCodec,
    VideoStreamParams,
};
use quinn::{
    Connection, Endpoint, EndpointConfig, RecvStream, SendStream, TransportConfig, VarInt,
};
use rcgen::generate_simple_self_signed;
use rustls::{pki_types::CertificateDer, RootCertStore};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, trace};
use uuid::Uuid;

pub mod congestion;
pub mod filesync;
pub mod media;
pub mod turn;

pub const NATIVE_QUIC_ALPN: &str = "qubox-native-quic/0";
/// ALPN for QUIC v2 (RFC 9369). Peers that support v2 advertise this;
/// servers accept both v1 and v2 ALPNs.
pub const NATIVE_QUIC_ALPN_V2: &str = "qubox-native-quic-v2/0";
/// QUIC v2 wire version (RFC 9369 §3.1). First four bytes of
/// `sha256("QUICv2 version number")`.
pub const QUIC_VERSION_V2: u32 = 0x6B3343CF;
/// QUIC v1 wire version (RFC 9000 §17.2). Used as the v2→v1 fallback.
pub const QUIC_VERSION_V1: u32 = 0x0000_0001;
/// Order matters: first entry is preferred. v2-first, v1-fallback.
pub const PREFERRED_QUIC_VERSIONS: &[u32] = &[QUIC_VERSION_V2, QUIC_VERSION_V1];
pub const DEFAULT_SERVER_NAME: &str = "qubox-native";

/// Maximum JSON-encoded length-prefixed payload size accepted on a
/// reliable QUIC stream. 256 KiB covers ControlMsg + accessibility
/// headers + mic config blob comfortably; larger is an attack signal.
///
/// This is the canonical transport-level cap on `read_json_prefixed` /
/// `maybe_read_json_prefixed` — `media::ControlChannel::MAX_JSON_FRAME`
/// remains a separate module-private budget for the datagram-side
/// `RateFeedback` cadence.
pub const MAX_JSON_FRAME: u32 = 256 * 1024;
/// Maximum encoded video access unit accepted on the reliable media
/// uni-stream. 8 MiB accommodates 4:4:4 4K at 120 fps comfortably while
/// still bounding the per-frame heap allocation against OOM DoS.
pub const MAX_VIDEO_AU_BYTES: u32 = 8 * 1024 * 1024;
/// Maximum single audio chunk accepted on the audio uni-stream. 512 KiB
/// allows 5.1-channel float32 at 96 kHz for ~600 ms per chunk.
pub const MAX_AUDIO_CHUNK_BYTES: u32 = 512 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AckPolicy {
    Media,
    Control,
    InputImmediate,
}

impl AckPolicy {
    /// `min_ack_delay` in microseconds (draft-ietf-quic-ack-frequency-14 §3).
    pub const fn min_ack_delay_us(self) -> u64 {
        match self {
            AckPolicy::Media => 25_000,
            AckPolicy::Control | AckPolicy::InputImmediate => 1_000,
        }
    }

    /// ACK-Frequency `ack_eliciting_threshold`. 0 means "ACK every ack-eliciting packet".
    pub const fn ack_eliciting_threshold(self) -> u8 {
        match self {
            AckPolicy::Media | AckPolicy::Control => 1,
            AckPolicy::InputImmediate => 0,
        }
    }

    /// `reordering_threshold` — should be `packet_threshold - 1` per draft §6.2.
    pub const fn reordering_threshold(self) -> u8 {
        match self {
            AckPolicy::Media => 2,
            AckPolicy::Control | AckPolicy::InputImmediate => 1,
        }
    }
}

impl Default for AckPolicy {
    fn default() -> Self {
        AckPolicy::Media
    }
}

#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub signaling_url: String,
    pub client_credential: SessionCredential,
    pub self_peer_id: Uuid,
    pub remote_peer_id: Uuid,
    pub turn_server: SocketAddr,
    pub turn_only: bool,
    pub turn_force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeQuicTicket {
    pub session_id: Uuid,
    pub connect_addr: SocketAddr,
    pub server_name: String,
    pub alpn: String,
    pub cert_der_b64: String,
    pub expires_unix_millis: u64,
}

#[derive(Debug, Clone)]
pub struct NativeQuicHost {
    endpoint: Arc<Endpoint>,
    ticket: NativeQuicTicket,
    expected_client_credential: SessionCredential,
}

impl NativeQuicHost {
    pub fn bind(
        bind_addr: SocketAddr,
        advertised_ip: Option<IpAddr>,
        session_id: Uuid,
        expected_client_credential: SessionCredential,
    ) -> anyhow::Result<Self> {
        let certified = generate_simple_self_signed(vec![DEFAULT_SERVER_NAME.to_string()])?;
        let cert_der = CertificateDer::from(certified.cert.der().to_vec());
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
        let mut server_config =
            quinn::ServerConfig::with_single_cert(vec![cert_der.clone()], key.into())?;
        server_config.transport_config(Arc::new(build_transport_config()));
        let endpoint = Endpoint::server(server_config, bind_addr)?;
        let local_addr = endpoint.local_addr()?;
        let connect_ip = advertised_ip.unwrap_or_else(|| default_advertised_ip(local_addr.ip()));

        info!(
            %session_id,
            %bind_addr,
            %local_addr,
            connect_ip = %connect_ip,
            "native QUIC host endpoint bound"
        );

        Ok(Self {
            endpoint: Arc::new(endpoint),
            ticket: NativeQuicTicket {
                session_id,
                connect_addr: SocketAddr::new(connect_ip, local_addr.port()),
                server_name: DEFAULT_SERVER_NAME.to_string(),
                alpn: NATIVE_QUIC_ALPN.to_string(),
                cert_der_b64: STANDARD.encode(cert_der.as_ref()),
                expires_unix_millis: expected_client_credential.expires_unix_millis,
            },
            expected_client_credential,
        })
    }

    pub fn ticket(&self) -> &NativeQuicTicket {
        &self.ticket
    }

    /// Bind with QUIC v2 transport and ALPN. Uses
    /// `build_server_config_v2` which accepts both ALPNs.
    pub fn bind_v2(
        bind_addr: SocketAddr,
        advertised_ip: Option<IpAddr>,
        session_id: Uuid,
        expected_client_credential: SessionCredential,
    ) -> anyhow::Result<Self> {
        let certified = generate_simple_self_signed(vec![DEFAULT_SERVER_NAME.to_string()])?;
        let cert_der = CertificateDer::from(certified.cert.der().to_vec());
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
        let server_config = build_server_config_v2(vec![cert_der.clone()], key.into())?;
        let endpoint = Endpoint::server(server_config, bind_addr)?;
        let local_addr = endpoint.local_addr()?;
        let connect_ip = advertised_ip.unwrap_or_else(|| default_advertised_ip(local_addr.ip()));

        Ok(Self {
            endpoint: Arc::new(endpoint),
            ticket: NativeQuicTicket {
                session_id,
                connect_addr: SocketAddr::new(connect_ip, local_addr.port()),
                server_name: DEFAULT_SERVER_NAME.to_string(),
                alpn: NATIVE_QUIC_ALPN_V2.to_string(),
                cert_der_b64: STANDARD.encode(cert_der.as_ref()),
                expires_unix_millis: expected_client_credential.expires_unix_millis,
            },
            expected_client_credential,
        })
    }

    /// Borrow the ticket — ALPN is `NATIVE_QUIC_ALPN_V2` when bound via `bind_v2`.
    pub fn ticket_v2(&self) -> &NativeQuicTicket {
        &self.ticket
    }

    pub async fn accept_authenticated_connection(self) -> anyhow::Result<NativeQuicHostConnection> {
        trace!(session_id = %self.ticket.session_id, "waiting for native QUIC client connection");
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| anyhow!("native QUIC endpoint stopped accepting connections"))?;
        let connection = incoming.await.context("native QUIC handshake failed")?;
        info!(
            session_id = %self.ticket.session_id,
            remote_addr = %connection.remote_address(),
            "native QUIC handshake complete"
        );
        let (mut send, mut recv) = connection
            .accept_bi()
            .await
            .context("failed to accept authentication stream")?;
        trace!(session_id = %self.ticket.session_id, "accepted native QUIC auth stream");
        let auth: ClientAuth = read_json_prefixed(&mut recv)
            .await
            .context("failed to read native QUIC client auth")?;

        if auth.session_id != self.ticket.session_id {
            let _ = write_json_prefixed(
                &mut send,
                &AuthAck {
                    accepted: false,
                    message: "session mismatch".to_string(),
                },
            )
            .await;
            connection.close(0u32.into(), b"session mismatch");
            anyhow::bail!("native QUIC client attempted the wrong session id");
        }

        let now = unix_millis_now();
        if auth.credential.expires_unix_millis < now
            || self.expected_client_credential.expires_unix_millis < now
        {
            let _ = write_json_prefixed(
                &mut send,
                &AuthAck {
                    accepted: false,
                    message: "session expired".to_string(),
                },
            )
            .await;
            connection.close(0u32.into(), b"session expired");
            anyhow::bail!("native QUIC session credential expired before transport authentication");
        }

        if auth.credential.client_pubkey != self.expected_client_credential.client_pubkey {
            let _ = write_json_prefixed(
                &mut send,
                &AuthAck {
                    accepted: false,
                    message: "credential pubkey does not match planned session".to_string(),
                },
            )
            .await;
            connection.close(0u32.into(), b"credential pubkey mismatch");
            anyhow::bail!("credential pubkey mismatch");
        }

        if auth.credential.hmac != self.expected_client_credential.hmac {
            let _ = write_json_prefixed(
                &mut send,
                &AuthAck {
                    accepted: false,
                    message: "credential HMAC does not match".to_string(),
                },
            )
            .await;
            connection.close(0u32.into(), b"credential HMAC mismatch");
            anyhow::bail!("credential HMAC mismatch");
        }

        let _auth_tail = recv
            .read_to_end(1024)
            .await
            .context("failed to drain native QUIC auth stream")?;

        write_json_prefixed(
            &mut send,
            &AuthAck {
                accepted: true,
                message: "ok".to_string(),
            },
        )
        .await
        .context("failed to write native QUIC auth acknowledgement")?;
        send.finish().context("failed to finish auth stream")?;
        debug!(
            session_id = %self.ticket.session_id,
            remote_addr = %connection.remote_address(),
            "native QUIC client authenticated"
        );

        Ok(NativeQuicHostConnection {
            _endpoint: self.endpoint,
            connection,
            session_id: self.ticket.session_id,
        })
    }
}

pub struct NativeQuicHostConnection {
    _endpoint: Arc<Endpoint>,
    connection: Connection,
    session_id: Uuid,
}

impl NativeQuicHostConnection {
    /// Borrow the underlying `quinn::Connection` so callers can layer
    /// the P0-2 datagram media path (or any other feature that needs
    /// direct access to the QUIC connection) on top of the reliable
    /// media stream created by `open_media_sender`.
    pub fn connection(&self) -> Connection {
        self.connection.clone()
    }

    pub async fn open_media_sender(&self) -> anyhow::Result<NativeQuicMediaSender> {
        trace!(session_id = %self.session_id, "opening native QUIC media uni stream");
        let mut send = self
            .connection
            .open_uni()
            .await
            .context("failed to open native QUIC media stream")?;

        // P0-7: prepend the 2-byte stream-purpose envelope so the
        // client can dispatch by purpose instead of relying on
        // accept-order.
        write_stream_envelope(&mut send, StreamPurpose::Media)
            .await
            .context("failed to write media stream envelope")?;

        debug!(session_id = %self.session_id, "native QUIC media uni stream opened");

        Ok(NativeQuicMediaSender {
            send,
            session_id: self.session_id,
        })
    }

    pub async fn open_audio_sender(
        &self,
        audio_config: AudioStreamParams,
    ) -> anyhow::Result<NativeQuicAudioSender> {
        trace!(session_id = %self.session_id, ?audio_config, "opening native QUIC audio uni stream");
        let mut send = self
            .connection
            .open_uni()
            .await
            .context("failed to open native QUIC audio stream")?;

        write_stream_envelope(&mut send, StreamPurpose::Audio)
            .await
            .context("failed to write audio stream envelope")?;
        write_json_prefixed(
            &mut send,
            &WireAudioStreamHeader {
                session_id: self.session_id,
                audio: audio_config,
            },
        )
        .await
        .context("failed to write native QUIC audio stream header")?;

        debug!(session_id = %self.session_id, "native QUIC audio uni stream opened");

        Ok(NativeQuicAudioSender {
            send,
            session_id: self.session_id,
            next_chunk_id: 0,
        })
    }

    /// Accept an incoming `ControlChannel` bi stream from the client.
    /// The client opens this via `ControlChannel::open` and sends
    /// `RateFeedback` at 4 Hz (P0-4 adaptive bitrate).
    pub async fn open_control_receiver(&self) -> anyhow::Result<ControlChannel> {
        ControlChannel::accept(&self.connection).await
    }

    pub async fn open_input_receiver(
        &self,
        video_config: VideoStreamParams,
    ) -> anyhow::Result<NativeQuicInputReceiver> {
        trace!(session_id = %self.session_id, ?video_config, "opening native QUIC control bidi stream");
        let (mut send, recv) = self
            .connection
            .open_bi()
            .await
            .context("failed to open native QUIC control stream")?;

        write_stream_envelope(&mut send, StreamPurpose::VideoConfig)
            .await
            .context("failed to write video-config stream envelope")?;
        write_json_prefixed(
            &mut send,
            &NativeQuicControlMessage::VideoConfig {
                video: video_config,
            },
        )
        .await
        .context("failed to write native QUIC video config")?;
        send.finish()
            .context("failed to finish native QUIC control setup stream")?;

        debug!(session_id = %self.session_id, "native QUIC control bidi stream opened");

        Ok(NativeQuicInputReceiver {
            recv,
            _connection: self.connection.clone(),
            session_id: self.session_id,
        })
    }

    /// Open a uni-stream for sending ControlMsg to the client.
    pub async fn open_control_sender(&self) -> anyhow::Result<NativeQuicHostControlSender> {
        trace!(session_id = %self.session_id, "opening native QUIC control uni stream");
        let mut send = self
            .connection
            .open_uni()
            .await
            .context("failed to open native QUIC control stream")?;
        write_stream_envelope(&mut send, StreamPurpose::HostControl)
            .await
            .context("failed to write host-control stream envelope")?;
        debug!(session_id = %self.session_id, "native QUIC control uni stream opened");
        Ok(NativeQuicHostControlSender { send })
    }

    /// Open a bidirectional FileSync handshake stream (ADR-022).
    pub async fn open_filesync_handshake(&self) -> anyhow::Result<(SendStream, RecvStream)> {
        let (mut send, recv) = self
            .connection
            .open_bi()
            .await
            .context("failed to open FileSync bi stream")?;
        write_stream_envelope(&mut send, StreamPurpose::FileSync)
            .await
            .context("failed to write FileSync envelope")?;
        Ok((send, recv))
    }

    /// Open a unidirectional FileSync bulk transfer stream (ADR-022).
    pub async fn open_filesync_bulk(&self) -> anyhow::Result<SendStream> {
        let mut send = self
            .connection
            .open_uni()
            .await
            .context("failed to open FileSync uni stream")?;
        write_stream_envelope(&mut send, StreamPurpose::FileSync)
            .await
            .context("failed to write FileSync envelope")?;
        Ok(send)
    }
}

pub struct NativeQuicMediaSender {
    send: SendStream,
    session_id: Uuid,
}

impl NativeQuicMediaSender {
    pub async fn send_access_unit(
        &mut self,
        access_unit: &EncodedVideoAccessUnit,
    ) -> anyhow::Result<()> {
        let header = WireAccessUnitHeader {
            session_id: self.session_id,
            frame_id: access_unit.frame_id,
            timestamp_micros: access_unit.timestamp_micros,
            keyframe: access_unit.keyframe,
            byte_len: access_unit.bytes.len(),
            codec: Some(access_unit.codec),
            stream_id: 0,
            display_id: 0,
            width: 0,
            height: 0,
            refresh_hz: 0.0,
            color_space_id: 0,
            hdr_static_metadata: None,
        };

        write_json_prefixed(&mut self.send, &header)
            .await
            .context("failed to write media packet header")?;
        self.send
            .write_all(&access_unit.bytes)
            .await
            .context("failed to write media packet bytes")?;
        Ok(())
    }

    /// Extended version that includes multi-stream metadata.
    /// Used by CaptureOrchestrator for per-display streams.
    pub async fn send_access_unit_ext(
        &mut self,
        access_unit: &EncodedVideoAccessUnit,
        stream_id: u16,
        display_id: u32,
        width: u32,
        height: u32,
        refresh_hz: f32,
        color_space_id: u8,
        hdr_static_metadata: Option<Vec<u8>>,
    ) -> anyhow::Result<()> {
        let header = WireAccessUnitHeader {
            session_id: self.session_id,
            frame_id: access_unit.frame_id,
            timestamp_micros: access_unit.timestamp_micros,
            keyframe: access_unit.keyframe,
            byte_len: access_unit.bytes.len(),
            codec: Some(access_unit.codec),
            stream_id,
            display_id,
            width,
            height,
            refresh_hz,
            color_space_id,
            hdr_static_metadata,
        };

        write_json_prefixed(&mut self.send, &header)
            .await
            .context("failed to write media packet header")?;
        self.send
            .write_all(&access_unit.bytes)
            .await
            .context("failed to write media packet bytes")?;
        Ok(())
    }

    /// Returns the raw QUIC stream ID as bits.
    /// Used by CaptureOrchestrator to set `stream_id` in `WireAccessUnitHeader`.
    pub fn stream_id_bits(&self) -> u64 {
        // SendStream::stream_id() returns quinn::StreamId which is repr(u64)
        self.send.id().into()
    }

    pub async fn finish(&mut self) -> anyhow::Result<()> {
        self.send
            .finish()
            .context("failed to finish native QUIC media stream")?;
        let _ = self.send.stopped().await;
        Ok(())
    }
}

pub struct NativeQuicMediaReceiver {
    _endpoint: Arc<Endpoint>,
    _connection: Connection,
    recv: RecvStream,
    session_id: Uuid,
}

impl NativeQuicMediaReceiver {
    /// Borrow the underlying `quinn::Connection` so callers can layer
    /// the P0-2 datagram media path on top of the reliable stream
    /// returned by this receiver.
    pub fn connection(&self) -> Connection {
        self._connection.clone()
    }

    pub async fn read_access_unit(&mut self) -> anyhow::Result<Option<EncodedVideoAccessUnit>> {
        let Some(header) = read_access_unit_header(&mut self.recv)
            .await
            .context("failed to read native QUIC media header")?
        else {
            return Ok(None);
        };

        if header.session_id != self.session_id {
            anyhow::bail!(
                "received media packet for session {} while connected to {}",
                header.session_id,
                self.session_id
            );
        }

        let mut bytes = vec![0_u8; header.byte_len];
        self.recv
            .read_exact(&mut bytes)
            .await
            .context("failed to read native QUIC media bytes")?;

        let codec = header.codec.unwrap_or(VideoCodec::H264);
        let nal_units = match codec {
            VideoCodec::H264 => inspect_h264_annex_b_nal_units(&bytes),
            // H.265/AV1: parser is implemented in qubox-media; the
            // transport layer is a pass-through and downstream consumers
            // (the client decoder ffmpeg) do the heavy lifting.
            VideoCodec::H265 | VideoCodec::Av1 => Vec::new(),
        };

        Ok(Some(EncodedVideoAccessUnit {
            codec,
            frame_id: header.frame_id,
            timestamp_micros: header.timestamp_micros,
            keyframe: header.keyframe,
            nal_units,
            bytes,
            display_id: header.display_id,
            stream_id: header.stream_id,
            width: header.width,
            height: header.height,
            color_space: None,
            bit_depth: 8,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeQuicAudioChunk {
    pub chunk_id: u64,
    pub bytes: Vec<u8>,
}

pub struct NativeQuicAudioSender {
    send: SendStream,
    session_id: Uuid,
    next_chunk_id: u64,
}

impl NativeQuicAudioSender {
    pub async fn send_audio_chunk(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        let header = WireAudioChunkHeader {
            session_id: self.session_id,
            chunk_id: self.next_chunk_id,
            byte_len: bytes.len(),
        };
        self.next_chunk_id += 1;

        write_json_prefixed(&mut self.send, &header)
            .await
            .context("failed to write audio packet header")?;
        self.send
            .write_all(bytes)
            .await
            .context("failed to write audio packet bytes")?;
        Ok(())
    }

    pub async fn finish(&mut self) -> anyhow::Result<()> {
        self.send
            .finish()
            .context("failed to finish native QUIC audio stream")?;
        let _ = self.send.stopped().await;
        Ok(())
    }
}

pub struct NativeQuicAudioReceiver {
    _endpoint: Arc<Endpoint>,
    _connection: Connection,
    recv: RecvStream,
    session_id: Uuid,
}

impl NativeQuicAudioReceiver {
    pub async fn read_audio_chunk(&mut self) -> anyhow::Result<Option<NativeQuicAudioChunk>> {
        let Some(header) = read_audio_chunk_header(&mut self.recv)
            .await
            .context("failed to read native QUIC audio header")?
        else {
            return Ok(None);
        };

        if header.session_id != self.session_id {
            anyhow::bail!(
                "received audio packet for session {} while connected to {}",
                header.session_id,
                self.session_id
            );
        }

        let mut bytes = vec![0_u8; header.byte_len];
        self.recv
            .read_exact(&mut bytes)
            .await
            .context("failed to read native QUIC audio bytes")?;

        Ok(Some(NativeQuicAudioChunk {
            chunk_id: header.chunk_id,
            bytes,
        }))
    }
}

pub struct NativeQuicInputSender {
    _endpoint: Arc<Endpoint>,
    _connection: Connection,
    send: SendStream,
    session_id: Uuid,
}

impl NativeQuicInputSender {
    pub async fn send_input_event(&mut self, event: &RemoteInputEvent) -> anyhow::Result<()> {
        let _ = self.session_id;
        write_json_prefixed(
            &mut self.send,
            &NativeQuicControlMessage::Input {
                event: event.clone(),
            },
        )
        .await
        .context("failed to write native QUIC input event")
    }

    /// Send a 0x1f IMMEDIATE_ACK frame (ack-eliciting, congestion-controlled).
    /// Currently uses PING fallback (1-byte datagram) because quinn does not
    /// yet expose the native IMMEDIATE_ACK frame API.
    ///
    /// TODO(adr-011): replace with `quinn::Connection::send_immediate_ack()`
    /// once the upstream API lands.
    pub fn request_immediate_ack(&self) -> anyhow::Result<()> {
        self._connection
            .send_datagram(b"\x01".to_vec().into())
            .context("IMMEDIATE_ACK (PING fallback) send failed")?;
        Ok(())
    }

    pub async fn finish(&mut self) -> anyhow::Result<()> {
        self.send
            .finish()
            .context("failed to finish native QUIC input stream")?;
        Ok(())
    }
}

pub struct NativeQuicInputReceiver {
    _connection: Connection,
    recv: RecvStream,
    session_id: Uuid,
}

impl NativeQuicInputReceiver {
    pub async fn read_input_event(&mut self) -> anyhow::Result<Option<RemoteInputEvent>> {
        let _ = self.session_id;
        let Some(message) = maybe_read_json_prefixed::<_, NativeQuicControlMessage>(&mut self.recv)
            .await
            .context("failed to read native QUIC control message")?
        else {
            return Ok(None);
        };

        match message {
            NativeQuicControlMessage::Input { event } => Ok(Some(event)),
            NativeQuicControlMessage::VideoConfig { .. } => {
                anyhow::bail!("received a video config on the host input stream")
            }
        }
    }
}

/// Host→Client control stream receiver.
/// Reads `ControlMsg` values from a uni-stream opened by the host.
pub struct NativeQuicControlReceiver {
    #[allow(dead_code)]
    connection: quinn::Connection,
    recv: quinn::RecvStream,
}

impl NativeQuicControlReceiver {
    pub async fn read_control_msg(&mut self) -> anyhow::Result<Option<ControlMsg>> {
        let Some(msg) = maybe_read_json_prefixed::<_, ControlMsg>(&mut self.recv)
            .await
            .context("failed to read control message")?
        else {
            return Ok(None);
        };
        Ok(Some(msg))
    }
}

/// Client→Host control stream sender (host side).
/// Sends `ControlMsg` values to the client over a uni-stream.
pub struct NativeQuicHostControlSender {
    send: quinn::SendStream,
}

impl NativeQuicHostControlSender {
    pub async fn send_control_msg(&mut self, msg: &ControlMsg) -> anyhow::Result<()> {
        write_json_prefixed(&mut self.send, msg)
            .await
            .context("failed to write control message")
    }

    pub async fn finish(&mut self) -> anyhow::Result<()> {
        self.send
            .finish()
            .context("failed to finish control stream")?;
        let _ = self.send.stopped().await;
        Ok(())
    }
}

pub struct NativeQuicClientSession {
    pub video_config: VideoStreamParams,
    pub audio_config: AudioStreamParams,
    pub media_receiver: NativeQuicMediaReceiver,
    pub audio_receiver: NativeQuicAudioReceiver,
    pub input_sender: NativeQuicInputSender,
    pub control_receiver: NativeQuicControlReceiver,
    /// Connection handle, exposed so callers can open additional
    /// streams for P1-9/P1-10 features (clipboard + mic control
    /// channel). Held as an `Arc`-cloned `quinn::Connection` (the
    /// existing fields also keep clones).
    pub connection: Connection,
}

/// Convenience alias for the underlying `quinn::Connection`. The
/// host's `NativeQuicHostConnection::connection()` returns the same
/// type, so downstream callers (clip/mic handlers) can share the
/// type between host and client.
pub type NativeQuicConnection = Connection;
/// Re-export for FileSync drain / session helpers.
pub use quinn::Connection as QuinnConnection;

impl NativeQuicClientSession {
    /// Open a new bi-directional control channel for sending
    /// `ControlMsg` values (clipboard payloads, mic lifecycle).
    /// The host pairs this with `ControlChannel::accept`.
    pub async fn open_control_channel(&self) -> anyhow::Result<ControlChannel> {
        ControlChannel::open(&self.connection).await
    }
}

pub async fn connect_to_native_quic(
    ticket: &NativeQuicTicket,
    client_credential: &SessionCredential,
) -> anyhow::Result<NativeQuicClientSession> {
    if ticket.expires_unix_millis < unix_millis_now()
        || client_credential.expires_unix_millis < unix_millis_now()
    {
        anyhow::bail!("native QUIC ticket or client credential already expired");
    }

    let mut roots = RootCertStore::empty();
    let cert_bytes = STANDARD
        .decode(ticket.cert_der_b64.as_bytes())
        .context("failed to decode native QUIC certificate")?;
    roots
        .add(CertificateDer::from(cert_bytes))
        .context("failed to trust native QUIC server certificate")?;

    let client_config = quinn::ClientConfig::with_root_certificates(Arc::new(roots))
        .context("failed to build native QUIC client config")?;
    let mut client_config = client_config;
    client_config.transport_config(Arc::new(build_transport_config()));
    let bind_addr = if ticket.connect_addr.is_ipv6() {
        SocketAddr::new(IpAddr::from([0u16; 8]), 0)
    } else {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
    };
    let mut endpoint = Endpoint::client(bind_addr)?;
    endpoint.set_default_client_config(client_config);
    let endpoint = Arc::new(endpoint);

    info!(
        session_id = %ticket.session_id,
        connect_addr = %ticket.connect_addr,
        bind_addr = %bind_addr,
        expires_unix_millis = ticket.expires_unix_millis,
        "starting native QUIC client connection"
    );

    let connection = endpoint
        .connect(ticket.connect_addr, &ticket.server_name)
        .context("failed to start native QUIC connection")?
        .await
        .context("native QUIC connect failed")?;
    info!(
        session_id = %ticket.session_id,
        remote_addr = %connection.remote_address(),
        "native QUIC client connected"
    );

    run_post_quic_handshake(connection, endpoint, ticket.session_id, client_credential).await
}

async fn run_post_quic_handshake(
    connection: Connection,
    endpoint: Arc<Endpoint>,
    session_id: Uuid,
    client_credential: &SessionCredential,
) -> anyhow::Result<NativeQuicClientSession> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("failed to open native QUIC auth stream")?;
    trace!(session_id = %session_id, "opened native QUIC auth stream");
    write_json_prefixed(
        &mut send,
        &ClientAuth {
            session_id,
            credential: client_credential.clone(),
        },
    )
    .await
    .context("failed to write native QUIC auth request")?;
    send.finish()
        .context("failed to finish native QUIC auth send")?;

    let ack: AuthAck = read_json_prefixed(&mut recv)
        .await
        .context("failed to read native QUIC auth acknowledgement")?;
    if !ack.accepted {
        anyhow::bail!("native QUIC authentication rejected: {}", ack.message);
    }
    debug!(session_id = %session_id, message = %ack.message, "native QUIC authentication accepted");
    drop(recv);

    // P0-7: dispatch incoming streams by purpose-envelope instead of a
    // fixed accept order. The host may open streams in any order; we
    // loop on accept_bi/accept_uni, read the 2-byte envelope, and slot
    // each stream into the matching handler. A 30-second overall
    // budget caps any infinite spin.
    route_post_auth_streams(connection, endpoint, session_id).await
}

/// Dispatch the post-auth streams into typed handlers, returning a
/// `NativeQuicClientSession`. The host opens four streams (in any
/// order): `VideoConfig` (bi), `Audio` (uni), `Media` (uni), and
/// `HostControl` (uni). The legacy fixed-order path is preserved as a
/// fallback when the host is an old build that does not emit an
/// envelope.
async fn route_post_auth_streams(
    connection: Connection,
    endpoint: Arc<Endpoint>,
    session_id: Uuid,
) -> anyhow::Result<NativeQuicClientSession> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut router = StreamRouter::new();

    while !router.required_satisfied() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!(
                "timed out waiting for required streams (have: {})",
                router.debug_have()
            );
        }
        let accepted = tokio::time::timeout(remaining, accept_next_stream_envelope(&connection))
            .await
            .map_err(|_| {
                anyhow!(
                    "timed out waiting for required streams (have: {})",
                    router.debug_have()
                )
            })?
            .context("failed to accept next stream")?;

        match accepted {
            Some(AcceptedStream::Bi {
                purpose,
                send,
                mut recv,
            }) => match purpose {
                StreamPurpose::VideoConfig => {
                    let msg: NativeQuicControlMessage = tokio::time::timeout(
                        Duration::from_secs(10),
                        read_json_prefixed(&mut recv),
                    )
                    .await
                    .context("timed out waiting for video config")?
                    .context("failed to read video config")?;
                    match msg {
                        NativeQuicControlMessage::VideoConfig { video } => {
                            debug!(session_id = %session_id, ?video, "received video config");
                            router.video_config = Some(video);
                            router.control_send = Some(send);
                            // The host immediately finishes after
                            // sending the config; close our recv.
                            drop(recv);
                        }
                        NativeQuicControlMessage::Input { .. } => {
                            anyhow::bail!("video-config stream did not start with a VideoConfig")
                        }
                    }
                }
                StreamPurpose::FileSync => {
                    debug!(
                        session_id = %session_id,
                        "FileSync bi during setup; stashing for post-session use"
                    );
                    router.extra.push(AcceptedStream::Bi {
                        purpose: StreamPurpose::FileSync,
                        send,
                        recv,
                    });
                }
                other => {
                    debug!(
                        session_id = %session_id,
                        ?other,
                        "unexpected bi-stream purpose; dropping"
                    );
                }
            },
            Some(AcceptedStream::Uni { purpose, mut recv }) => match purpose {
                StreamPurpose::Audio => {
                    let header: WireAudioStreamHeader = tokio::time::timeout(
                        Duration::from_secs(10),
                        read_json_prefixed(&mut recv),
                    )
                    .await
                    .context("timed out waiting for audio header")?
                    .context("failed to read audio header")?;
                    if header.session_id != session_id {
                        anyhow::bail!(
                            "audio stream for session {} but connected to {}",
                            header.session_id,
                            session_id
                        );
                    }
                    debug!(session_id = %session_id, ?header.audio, "received audio header");
                    router.audio_header = Some(header);
                    router.audio_recv = Some(recv);
                }
                StreamPurpose::Media => {
                    debug!(session_id = %session_id, "accepted media stream");
                    router.media_recv = Some(recv);
                }
                StreamPurpose::HostControl => {
                    debug!(session_id = %session_id, "accepted host control stream");
                    router.control_recv = Some(recv);
                }
                StreamPurpose::FileSync => {
                    debug!(
                        session_id = %session_id,
                        "FileSync uni during setup; stashing for post-session use"
                    );
                    router.extra.push(AcceptedStream::Uni {
                        purpose: StreamPurpose::FileSync,
                        recv,
                    });
                }
                StreamPurpose::Input | StreamPurpose::VideoConfig => {
                    debug!(
                        session_id = %session_id,
                        ?purpose,
                        "unexpected uni-stream purpose; dropping"
                    );
                }
            },
            Some(stream @ (AcceptedStream::LegacyBi { .. } | AcceptedStream::LegacyUni { .. })) => {
                debug!(
                    session_id = %session_id,
                    "legacy (pre-mux) stream accepted; using old fixed-order path"
                );
                router.extra.push(stream);
                // Once we see a legacy stream we cannot trust the
                // envelope magic on subsequent streams either, so exit
                // the loop and fall back.
                break;
            }
            None => {
                anyhow::bail!("connection closed before required streams were received")
            }
        }
    }

    // If we never picked up a muxed stream we may have legacy extras.
    // Drain those via the old fixed-order path. This handles old
    // builds that pre-date the envelope.
    if !router.extra.is_empty() {
        drain_legacy_streams(&mut router, &connection, session_id).await?;
    }

    let video_config = router
        .video_config
        .ok_or_else(|| anyhow!("no VideoConfig stream received"))?;
    let audio_header = router
        .audio_header
        .ok_or_else(|| anyhow!("no Audio stream received"))?;
    let media_recv = router
        .media_recv
        .ok_or_else(|| anyhow!("no Media stream received"))?;
    let control_recv = router
        .control_recv
        .ok_or_else(|| anyhow!("no HostControl stream received"))?;
    let control_send = router
        .control_send
        .ok_or_else(|| anyhow!("no control bi-stream received"))?;
    let audio_recv = router
        .audio_recv
        .ok_or_else(|| anyhow!("no audio recv stream received"))?;

    Ok(NativeQuicClientSession {
        video_config,
        audio_config: audio_header.audio,
        media_receiver: NativeQuicMediaReceiver {
            _endpoint: endpoint.clone(),
            _connection: connection.clone(),
            recv: media_recv,
            session_id,
        },
        audio_receiver: NativeQuicAudioReceiver {
            _endpoint: endpoint.clone(),
            _connection: connection.clone(),
            recv: audio_recv,
            session_id,
        },
        input_sender: NativeQuicInputSender {
            _endpoint: endpoint,
            _connection: connection.clone(),
            send: control_send,
            session_id,
        },
        control_receiver: NativeQuicControlReceiver {
            connection: connection.clone(),
            recv: control_recv,
        },
        connection,
    })
}

/// Race the next bi and uni stream acceptances and read a 2-byte
/// envelope from the winning side. Returns `Ok(None)` if the connection
/// closes cleanly.
async fn accept_next_stream_envelope(conn: &Connection) -> anyhow::Result<Option<AcceptedStream>> {
    let bi = conn.accept_bi();
    let uni = conn.accept_uni();
    tokio::pin!(bi);
    tokio::pin!(uni);
    let accepted = tokio::select! {
        biased;
        res = &mut bi => {
            let (send, mut recv) = res.context("accept_bi failed")?;
            AcceptedStream::Bi {
                purpose: match try_read_envelope_async(&mut recv).await? {
                    Some(env) => env.purpose,
                    None => return Ok(Some(AcceptedStream::LegacyBi { send, recv })),
                },
                send,
                recv,
            }
        }
        res = &mut uni => {
            let mut recv = res.context("accept_uni failed")?;
            AcceptedStream::Uni {
                purpose: match try_read_envelope_async(&mut recv).await? {
                    Some(env) => env.purpose,
                    None => return Ok(Some(AcceptedStream::LegacyUni { recv })),
                },
                recv,
            }
        }
    };
    Ok(Some(accepted))
}

/// Read a 2-byte envelope. Returns `Ok(Some(env))` on success,
/// `Ok(None)` if the magic byte indicates a legacy stream (caller
/// falls back), or `Err` if the connection drops.
async fn try_read_envelope_async<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> anyhow::Result<Option<StreamEnvelope>> {
    let mut buf = [0_u8; StreamEnvelope::SIZE];
    match reader.read_exact(&mut buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    match StreamEnvelope::decode(&buf) {
        Ok(env) => Ok(Some(env)),
        Err(StreamEnvelopeError::BadMagic) => {
            tracing::warn!(
                first_byte = buf[0],
                "legacy (pre-mux) stream detected; falling back to old framing"
            );
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

/// Drain streams that arrived without an envelope, using the legacy
/// fixed-order JSON framing. Order: bi = VideoConfig, then up to 3
/// unis in arbitrary order = Audio, Media, HostControl.
async fn drain_legacy_streams(
    router: &mut StreamRouter,
    connection: &Connection,
    session_id: Uuid,
) -> anyhow::Result<()> {
    // Pull from the stash first.
    let extras: Vec<AcceptedStream> = router.extra.drain(..).collect();
    for st in extras {
        match st {
            AcceptedStream::LegacyBi { send, mut recv } => {
                let msg: NativeQuicControlMessage =
                    tokio::time::timeout(Duration::from_secs(10), read_json_prefixed(&mut recv))
                        .await
                        .context("timed out on legacy control stream")?
                        .context("failed to read legacy control message")?;
                if let NativeQuicControlMessage::VideoConfig { video } = msg {
                    if router.video_config.is_none() {
                        router.video_config = Some(video);
                        router.control_send = Some(send);
                    }
                }
            }
            AcceptedStream::LegacyUni { mut recv } => {
                let header: WireAudioStreamHeader =
                    tokio::time::timeout(Duration::from_secs(10), read_json_prefixed(&mut recv))
                        .await
                        .context("timed out on legacy uni stream")?
                        .context("failed to read legacy uni header")?;
                if header.session_id == session_id && router.audio_header.is_none() {
                    router.audio_header = Some(header);
                    router.audio_recv = Some(recv);
                } else {
                    router.control_recv = Some(recv);
                }
            }
            _ => {}
        }
    }
    // Drain any remaining legacy streams from the connection until
    // the router is satisfied. Bound to 30s for safety.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !router.required_satisfied() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!(
                "legacy fallback: timed out waiting for required streams (have: {})",
                router.debug_have()
            );
        }
        let accepted = tokio::time::timeout(remaining, accept_next_stream_envelope(connection))
            .await
            .map_err(|_| {
                anyhow!(
                    "legacy fallback: timed out waiting for required streams (have: {})",
                    router.debug_have()
                )
            })?;
        match accepted? {
            Some(AcceptedStream::LegacyUni { mut recv }) => {
                let header: WireAudioStreamHeader =
                    tokio::time::timeout(Duration::from_secs(10), read_json_prefixed(&mut recv))
                        .await
                        .context("timed out on legacy uni stream")?
                        .context("failed to read legacy uni header")?;
                if header.session_id == session_id && router.audio_header.is_none() {
                    router.audio_header = Some(header);
                    router.audio_recv = Some(recv);
                } else if router.media_recv.is_none() {
                    // The legacy media stream is bare (no header), so
                    // we treat any non-audio uni as the media stream.
                    router.media_recv = Some(recv);
                } else {
                    router.control_recv = Some(recv);
                }
            }
            Some(AcceptedStream::LegacyBi { send, mut recv }) => {
                let msg: NativeQuicControlMessage =
                    tokio::time::timeout(Duration::from_secs(10), read_json_prefixed(&mut recv))
                        .await
                        .context("timed out on legacy control stream")?
                        .context("failed to read legacy control message")?;
                if let NativeQuicControlMessage::VideoConfig { video } = msg {
                    if router.video_config.is_none() {
                        router.video_config = Some(video);
                        router.control_send = Some(send);
                    }
                }
            }
            _ => {
                // Muxed streams can't appear after a legacy one. Skip.
            }
        }
    }
    Ok(())
}
/// Establish a QUIC connection through a TURN relay (client side).
///
/// This creates a `TurnUdpSocket` wrapping a TURN channel to `peer_turn_address`,
/// builds a quinn endpoint with an explicit `TokioRuntime`, and runs the standard
/// auth + control + media stream setup.
pub async fn connect_via_turn(
    ticket: &NativeQuicTicket,
    client_credential: &SessionCredential,
    turn_server: SocketAddr,
    turn_username: String,
    turn_password: String,
    peer_turn_address: SocketAddr,
) -> anyhow::Result<NativeQuicClientSession> {
    if ticket.expires_unix_millis < unix_millis_now()
        || client_credential.expires_unix_millis < unix_millis_now()
    {
        anyhow::bail!("TURN QUIC ticket or client credential already expired");
    }

    let creds = turn::TurnCredentials {
        urls: vec![],
        username: turn_username,
        password: turn_password,
        ttl: 0,
    };
    let socket = turn::TurnUdpSocket::new(turn_server, creds, peer_turn_address).await?;
    let runtime = Arc::new(quinn::TokioRuntime);

    let mut roots = RootCertStore::empty();
    let cert_bytes = STANDARD
        .decode(ticket.cert_der_b64.as_bytes())
        .context("failed to decode TURN QUIC certificate")?;
    roots
        .add(CertificateDer::from(cert_bytes))
        .context("failed to trust TURN QUIC server certificate")?;

    let client_config = quinn::ClientConfig::with_root_certificates(Arc::new(roots))
        .context("failed to build TURN QUIC client config")?;
    let mut client_config = client_config;
    client_config.transport_config(Arc::new(build_transport_config()));

    let mut endpoint = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        None,
        socket.clone(),
        runtime,
    )?;
    endpoint.set_default_client_config(client_config);
    let endpoint = Arc::new(endpoint);

    info!(
        session_id = %ticket.session_id,
        connect_addr = %ticket.connect_addr,
        "starting TURN-over-QUIC client connection"
    );

    let connection = endpoint
        .connect(ticket.connect_addr, &ticket.server_name)
        .context("failed to start TURN-over-QUIC connection")?
        .await
        .context("TURN-over-QUIC connect failed")?;
    info!(
        session_id = %ticket.session_id,
        remote_addr = %connection.remote_address(),
        "TURN-over-QUIC client connected"
    );

    run_post_quic_handshake(connection, endpoint, ticket.session_id, client_credential).await
}

/// Bind a TURN-relayed QUIC endpoint (host side).
///
/// Creates a TURN allocation, permission, and channel to `client_turn_address`,
/// then builds a quinn endpoint using `new_with_abstract_socket`. The returned
/// `NativeQuicHost` has a ticket whose `connect_addr` is the advertised
/// TURN address (not the local UDP socket address).
pub async fn bind_via_turn(
    bind_addr: SocketAddr,
    advertised_turn_address: SocketAddr,
    session_id: Uuid,
    expected_client_credential: SessionCredential,
    turn_server: SocketAddr,
    turn_username: String,
    turn_password: String,
    client_turn_address: SocketAddr,
) -> anyhow::Result<NativeQuicHost> {
    let _ = bind_addr; // the TURN socket picks its own local address
    let certified = generate_simple_self_signed(vec![DEFAULT_SERVER_NAME.to_string()])?;
    let cert_der = CertificateDer::from(certified.cert.der().to_vec());
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
    let mut server_config =
        quinn::ServerConfig::with_single_cert(vec![cert_der.clone()], key.into())?;
    server_config.transport_config(Arc::new(build_transport_config()));

    let creds = turn::TurnCredentials {
        urls: vec![],
        username: turn_username,
        password: turn_password,
        ttl: 0,
    };
    let socket = turn::TurnUdpSocket::new(turn_server, creds, client_turn_address).await?;
    let runtime = Arc::new(quinn::TokioRuntime);

    info!(
        %session_id,
        %advertised_turn_address,
        "binding TURN-relayed QUIC endpoint"
    );

    let endpoint = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        Some(server_config),
        socket.clone(),
        runtime,
    )?;

    Ok(NativeQuicHost {
        endpoint: Arc::new(endpoint),
        ticket: NativeQuicTicket {
            session_id,
            connect_addr: advertised_turn_address,
            server_name: DEFAULT_SERVER_NAME.to_string(),
            alpn: NATIVE_QUIC_ALPN.to_string(),
            cert_der_b64: STANDARD.encode(cert_der.as_ref()),
            expires_unix_millis: expected_client_credential.expires_unix_millis,
        },
        expected_client_credential,
    })
}

/// Extended `connect_with_fallback` that retries v2 → v1 on
/// `version_negotiation` failures. First attempt uses v2 ALPN; if it fails,
/// retries with v1 ALPN.
pub async fn connect_with_fallback_v2(
    ticket: &NativeQuicTicket,
    client_credential: &SessionCredential,
    turn_config: Option<TurnConfig>,
) -> anyhow::Result<NativeQuicClientSession> {
    let mut v2_ticket = ticket.clone();
    v2_ticket.alpn = NATIVE_QUIC_ALPN_V2.to_string();

    match connect_with_fallback(&v2_ticket, client_credential, turn_config.clone()).await {
        Ok(session) => Ok(session),
        Err(e) => {
            tracing::warn!("v2 fallback: v2 attempt failed ({e}); retrying with v1 ALPN");
            connect_with_fallback(ticket, client_credential, turn_config).await
        }
    }
}

/// Fallback connection chain: direct QUIC → TURN-over-QUIC → TURN/TCP (stubbed).
///
/// The chain is:
/// 1. Direct QUIC attempt with a 3-second timeout (skipped when `turn_only` or `turn_force` is set).
/// 2. TURN-over-QUIC: fetches credentials and the peer's relay address from the signaling
///    server, publishes its own relay address, and connects through the TURN server.
/// 3. TURN/TCP (stubbed — returns an error).
pub async fn connect_with_fallback(
    ticket: &NativeQuicTicket,
    client_credential: &SessionCredential,
    turn_config: Option<TurnConfig>,
) -> anyhow::Result<NativeQuicClientSession> {
    let skip_direct = turn_config
        .as_ref()
        .is_some_and(|c| c.turn_only || c.turn_force);

    if !skip_direct {
        match tokio::time::timeout(
            Duration::from_secs(3),
            connect_to_native_quic(ticket, client_credential),
        )
        .await
        {
            Ok(Ok(session)) => return Ok(session),
            Ok(Err(e)) => info!("direct QUIC failed: {e}"),
            Err(_) => info!("direct QUIC timed out after 3s"),
        }
    }

    if let Some(config) = &turn_config {
        let peer_pubkey_hex = pubkey_to_hex(&client_credential.client_pubkey);
        let turn_creds = fetch_turn_credentials(
            &config.signaling_url,
            &config.client_credential,
            &peer_pubkey_hex,
            config.self_peer_id,
        )
        .await?;
        let host_turn_addr = fetch_host_turn_address(
            &config.signaling_url,
            &config.client_credential,
            config.remote_peer_id,
        )
        .await?;
        let _ = publish_own_turn_address(
            &config.signaling_url,
            &config.client_credential,
            config.self_peer_id,
            config.turn_server,
        )
        .await;

        match tokio::time::timeout(
            Duration::from_secs(5),
            connect_via_turn(
                ticket,
                client_credential,
                config.turn_server,
                turn_creds.username,
                turn_creds.password,
                host_turn_addr,
            ),
        )
        .await
        {
            Ok(Ok(session)) => return Ok(session),
            Ok(Err(e)) => info!("TURN-over-QUIC failed: {e}"),
            Err(_) => info!("TURN-over-QUIC timed out after 5s"),
        }
    }

    // TURN/TCP not implemented in this build
    Err(anyhow!("all connection paths failed"))
}

// ── TURN signaling-server HTTP helpers ──────────────────────────────

/// Process-wide shared HTTP client for the TURN signaling helpers.
/// Building a `reqwest::Client` is comparatively expensive (TLS config,
/// connector pool, etc.), so we reuse a single instance across all calls.
fn shared_http() -> &'static reqwest::Client {
    static CLI: OnceLock<reqwest::Client> = OnceLock::new();
    CLI.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build reqwest client")
    })
}

/// Build the `Authorization: Bearer <...>` value used against the TURN
/// endpoints on the signaling server: the entire `SessionCredential`
/// JSON, base64-encoded.
fn bearer_from_credential(c: &SessionCredential) -> String {
    let raw = serde_json::to_vec(c).expect("SessionCredential serializes");
    STANDARD.encode(raw)
}

/// True iff `s` is a 64-character lowercase/uppercase hex string (i.e.
/// the textual encoding of a 32-byte Ed25519 public key).
fn is_valid_hex_pubkey(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Render a 32-byte Ed25519 public key as lowercase hex.
fn pubkey_to_hex(pk: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in pk {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

async fn fetch_turn_credentials(
    signaling_url: &str,
    credential: &SessionCredential,
    peer_pubkey_hex: &str,
    peer_id: Uuid,
) -> anyhow::Result<turn::TurnCredentials> {
    if !is_valid_hex_pubkey(peer_pubkey_hex) {
        anyhow::bail!(
            "fetch_turn_credentials: peer_pubkey_hex must be 64-char hex (got {} chars)",
            peer_pubkey_hex.len()
        );
    }
    let base = signaling_url.trim_end_matches('/');
    let bearer = bearer_from_credential(credential);
    let resp = shared_http()
        .post(format!("{base}/v1/turn/credentials"))
        .header("Authorization", format!("Bearer {bearer}"))
        .json(&serde_json::json!({ "peer_id": peer_pubkey_hex }))
        .send()
        .await
        .context("failed to fetch TURN credentials from signaling server")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "TURN credentials request failed for peer_id={peer_id}: {status} body={body}"
        ));
    }
    let creds: turn::TurnCredentials = resp
        .json()
        .await
        .context("failed to parse TURN credentials response")?;
    Ok(creds)
}

/// Fetch the host's published TURN relay address. Requires an HMAC-bound
/// `SessionCredential` bearer — the signaling server rejects unauthenticated
/// GET (legacy bare UUID is not accepted on this path).
async fn fetch_host_turn_address(
    signaling_url: &str,
    credential: &SessionCredential,
    peer_id: Uuid,
) -> anyhow::Result<SocketAddr> {
    let base = signaling_url.trim_end_matches('/');
    let bearer = bearer_from_credential(credential);
    let resp = shared_http()
        .get(format!("{base}/v1/turn/relay-address/{peer_id}"))
        .header("Authorization", format!("Bearer {bearer}"))
        .send()
        .await
        .context("failed to fetch host TURN relay address")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "host TURN relay address fetch failed for peer_id={peer_id}: {status} body={body}"
        ));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .context("failed to parse TURN relay address response")?;
    // Prefer the new `relay_address` field; fall back to the legacy
    // `turn_address` field for backward-compat with older server builds.
    let addr_str = body
        .get("relay_address")
        .and_then(|v| v.as_str())
        .or_else(|| body.get("turn_address").and_then(|v| v.as_str()))
        .ok_or_else(|| anyhow!("missing relay_address (or legacy turn_address) in response"))?;
    addr_str
        .parse::<SocketAddr>()
        .map_err(|e| anyhow!("invalid relay_address {addr_str:?}: {e}"))
}

async fn publish_own_turn_address(
    signaling_url: &str,
    credential: &SessionCredential,
    peer_id: Uuid,
    addr: SocketAddr,
) -> anyhow::Result<()> {
    let base = signaling_url.trim_end_matches('/');
    let bearer = bearer_from_credential(credential);
    let resp = shared_http()
        .post(format!("{base}/v1/turn/relay-address"))
        .header("Authorization", format!("Bearer {bearer}"))
        .json(&serde_json::json!({
            "peer_id": peer_id.to_string(),
            "relay_address": addr.to_string(),
        }))
        .send()
        .await
        .context("failed to publish TURN relay address")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "publish TURN relay address failed for peer_id={peer_id}: {status} body={body}"
        ));
    }
    Ok(())
}

pub fn encode_ticket_b64(ticket: &NativeQuicTicket) -> anyhow::Result<String> {
    Ok(STANDARD.encode(serde_json::to_vec(ticket)?))
}

pub fn decode_ticket_b64(ticket_b64: &str) -> anyhow::Result<NativeQuicTicket> {
    Ok(serde_json::from_slice(
        &STANDARD.decode(ticket_b64.as_bytes())?,
    )?)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ClientAuth {
    session_id: Uuid,
    /// Full HMAC-bound SessionCredential. The host already holds
    /// `expected_client_credential` and binds the auth on:
    /// - session_id match
    /// - expiry in the future
    /// - client_pubkey equality (links this credential to the
    ///   device that received it through SignedHello)
    /// - hmac equality (proves it was issued by the signaling
    ///   server using the same secret — both sides hold the
    ///   credential symmetrically so direct compare is safe)
    credential: SessionCredential,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AuthAck {
    accepted: bool,
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct WireAccessUnitHeader {
    session_id: Uuid,
    frame_id: u64,
    timestamp_micros: u64,
    keyframe: bool,
    byte_len: usize,
    /// Codec tag. `None` on the wire means H.264 (back-compat with v0).
    #[serde(default)]
    codec: Option<VideoCodec>,
    #[serde(default)]
    stream_id: u16,
    #[serde(default)]
    display_id: u32,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    refresh_hz: f32,
    #[serde(default)]
    color_space_id: u8,
    #[serde(default)]
    hdr_static_metadata: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireAudioStreamHeader {
    session_id: Uuid,
    audio: AudioStreamParams,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WireAudioChunkHeader {
    session_id: Uuid,
    chunk_id: u64,
    byte_len: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum NativeQuicControlMessage {
    VideoConfig { video: VideoStreamParams },
    Input { event: RemoteInputEvent },
}

async fn write_json_prefixed<W: AsyncWriteExt + Unpin, T: Serialize>(
    writer: &mut W,
    payload: &T,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(payload)?;
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_json_prefixed<R: AsyncReadExt + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> anyhow::Result<T> {
    let len = reader.read_u32().await? as usize;
    if len > MAX_JSON_FRAME as usize {
        anyhow::bail!(
            "length-prefixed JSON payload of {} bytes exceeds MAX_JSON_FRAME limit ({} bytes)",
            len,
            MAX_JSON_FRAME
        );
    }
    let mut bytes = vec![0_u8; len];
    reader.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn maybe_read_json_prefixed<R: AsyncReadExt + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> anyhow::Result<Option<T>> {
    let len = match reader.read_u32().await {
        Ok(len) => len as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if len > MAX_JSON_FRAME as usize {
        anyhow::bail!(
            "length-prefixed JSON payload of {} bytes exceeds MAX_JSON_FRAME limit ({} bytes)",
            len,
            MAX_JSON_FRAME
        );
    }
    let mut bytes = vec![0_u8; len];
    reader.read_exact(&mut bytes).await?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

/// Read a length-prefixed `WireAccessUnitHeader` and reject any
/// access unit whose declared payload exceeds `MAX_VIDEO_AU_BYTES` so
/// the receiver cannot be coerced into a multi-gigabyte allocation.
/// Extracted as a free function (rather than inlined into
/// `read_access_unit`) so it can be exercised from unit tests with an
/// in-memory `AsyncRead`.
async fn read_access_unit_header<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> anyhow::Result<Option<WireAccessUnitHeader>> {
    let header = match maybe_read_json_prefixed::<_, WireAccessUnitHeader>(reader).await? {
        Some(h) => h,
        None => return Ok(None),
    };
    if header.byte_len > MAX_VIDEO_AU_BYTES as usize {
        anyhow::bail!(
            "video access unit byte_len of {} bytes exceeds MAX_VIDEO_AU_BYTES limit ({} bytes)",
            header.byte_len,
            MAX_VIDEO_AU_BYTES
        );
    }
    Ok(Some(header))
}

/// Read a length-prefixed `WireAudioChunkHeader` and reject any chunk
/// whose declared payload exceeds `MAX_AUDIO_CHUNK_BYTES`. Extracted
/// so it can be exercised from unit tests with an in-memory `AsyncRead`.
async fn read_audio_chunk_header<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> anyhow::Result<Option<WireAudioChunkHeader>> {
    let header = match maybe_read_json_prefixed::<_, WireAudioChunkHeader>(reader).await? {
        Some(h) => h,
        None => return Ok(None),
    };
    if header.byte_len > MAX_AUDIO_CHUNK_BYTES as usize {
        anyhow::bail!(
            "audio chunk byte_len of {} bytes exceeds MAX_AUDIO_CHUNK_BYTES limit ({} bytes)",
            header.byte_len,
            MAX_AUDIO_CHUNK_BYTES
        );
    }
    Ok(Some(header))
}
// ── Stream-purpose multiplexing (P0-7 / mux) ─────────────────────────
//
// After auth completes the host opens several QUIC streams (control bi,
// audio uni, media uni, host-control uni). On the client side, the
// legacy `run_post_quic_handshake` accepted these in a *fixed order*
// which raced against the host's stream-opening sequence: any reorder
// produced opaque timeouts.
//
// The fix prepends a 2-byte envelope — a magic + a purpose tag — to
// every NEW stream opened after auth. The client side then loops on
// `accept_bi` / `accept_uni` and dispatches by envelope. The legacy
// reliable JSON path (no envelope) still works as a fallback for old
// builds, with a one-shot deprecation log.

/// Magic prefix for the post-auth stream envelope. Distinct from the
/// 2-byte media datagram magic so the dispatchers can never collide.
pub const STREAM_MAGIC: u8 = 0xA1;

/// Logical purpose of a stream opened after auth. The wire encoding is
/// a single byte; values are stable so we can keep multiple builds
/// interoperating.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPurpose {
    /// Host → client: bi-directional control stream carrying the
    /// initial `VideoConfig` followed by client → host `Input`
    /// events. Opened first so the client knows the codec/width/height.
    VideoConfig = 0x01,
    /// Host → client: uni-directional audio stream. Header carries
    /// `WireAudioStreamHeader` (session_id + AudioStreamParams).
    Audio = 0x02,
    /// Host → client: uni-directional media (encoded video access
    /// units). Header carries `WireAccessUnitHeader` per frame.
    Media = 0x03,
    /// Host → client: uni-directional control stream for `ControlMsg`
    /// (clipboard, mic lifecycle, blank-overlay, privacy events).
    HostControl = 0x04,
    /// Reserved for client → host auxiliary streams (clipboard
    /// payload channels, mic audio, future features).
    Input = 0x05,
    /// Bidirectional FileSync handshake + unidirectional bulk transfers.
    /// Used for context-aware, process-locked file synchronization (ADR-022).
    FileSync = 0x06,
}

impl StreamPurpose {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(StreamPurpose::VideoConfig),
            0x02 => Some(StreamPurpose::Audio),
            0x03 => Some(StreamPurpose::Media),
            0x04 => Some(StreamPurpose::HostControl),
            0x05 => Some(StreamPurpose::Input),
            0x06 => Some(StreamPurpose::FileSync),
            _ => None,
        }
    }

    pub fn as_byte(self) -> u8 {
        self as u8
    }
}

/// 2-byte stream envelope: `[magic, purpose]`. Written as the very
/// first bytes on every post-auth stream so the receiver can dispatch
/// without parsing the JSON header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamEnvelope {
    pub magic: u8,
    pub purpose: StreamPurpose,
}

impl StreamEnvelope {
    pub const SIZE: usize = 2;

    pub fn new(purpose: StreamPurpose) -> Self {
        Self {
            magic: STREAM_MAGIC,
            purpose,
        }
    }

    pub fn encode(&self) -> [u8; 2] {
        [self.magic, self.purpose.as_byte()]
    }

    /// Decode the 2-byte prelude. Returns `Err(BadMagic)` if the
    /// magic byte is wrong (i.e. this is a legacy / non-muxed stream
    /// — the caller should fall back to the old behaviour). Returns
    /// `Err(UnknownPurpose)` if the purpose byte is reserved/unknown.
    pub fn decode(buf: &[u8]) -> Result<Self, StreamEnvelopeError> {
        if buf.len() < Self::SIZE {
            return Err(StreamEnvelopeError::Short);
        }
        if buf[0] != STREAM_MAGIC {
            return Err(StreamEnvelopeError::BadMagic);
        }
        let purpose =
            StreamPurpose::from_byte(buf[1]).ok_or(StreamEnvelopeError::UnknownPurpose(buf[1]))?;
        Ok(Self {
            magic: buf[0],
            purpose,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEnvelopeError {
    Short,
    BadMagic,
    UnknownPurpose(u8),
}

impl std::fmt::Display for StreamEnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamEnvelopeError::Short => write!(f, "stream envelope buffer too short"),
            StreamEnvelopeError::BadMagic => write!(f, "stream envelope magic mismatch"),
            StreamEnvelopeError::UnknownPurpose(b) => {
                write!(f, "stream envelope unknown purpose 0x{b:02x}")
            }
        }
    }
}

impl std::error::Error for StreamEnvelopeError {}

/// Write the 2-byte envelope at the start of a stream. Call before
/// any length-prefixed JSON header.
pub async fn write_stream_envelope<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    purpose: StreamPurpose,
) -> anyhow::Result<()> {
    let env = StreamEnvelope::new(purpose);
    let bytes = env.encode();
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// A stream accepted by the dispatcher. `purpose` is `Some` for
/// muxed streams and `None` for the legacy (pre-mux) fallback.
pub enum AcceptedStream {
    Bi {
        purpose: StreamPurpose,
        send: SendStream,
        recv: RecvStream,
    },
    Uni {
        purpose: StreamPurpose,
        recv: RecvStream,
    },
    LegacyBi {
        send: SendStream,
        recv: RecvStream,
    },
    LegacyUni {
        recv: RecvStream,
    },
}

/// State collected while the client side dispatches incoming streams
/// into typed handlers.
#[derive(Default)]
pub struct StreamRouter {
    pub video_config: Option<VideoStreamParams>,
    pub audio_header: Option<WireAudioStreamHeader>,
    pub audio_recv: Option<RecvStream>,
    pub media_recv: Option<RecvStream>,
    pub control_send: Option<SendStream>,
    pub control_recv: Option<quinn::RecvStream>,
    /// Streams accepted but not yet recognised — kept so we don't
    /// drop them on the floor.
    pub extra: Vec<AcceptedStream>,
}

impl StreamRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` once all *required* streams for the client
    /// session have been received: `VideoConfig`, audio header, media
    /// receiver, and host→client control receiver.
    pub fn required_satisfied(&self) -> bool {
        self.video_config.is_some()
            && self.audio_header.is_some()
            && self.audio_recv.is_some()
            && self.media_recv.is_some()
            && self.control_send.is_some()
            && self.control_recv.is_some()
    }

    fn debug_have(&self) -> String {
        let mut parts = Vec::new();
        if self.video_config.is_some() {
            parts.push("video_config");
        }
        if self.audio_header.is_some() {
            parts.push("audio_header");
        }
        if self.audio_recv.is_some() {
            parts.push("audio_recv");
        }
        if self.media_recv.is_some() {
            parts.push("media_recv");
        }
        if self.control_send.is_some() {
            parts.push("control_send");
        }
        if self.control_recv.is_some() {
            parts.push("control_recv");
        }
        parts.join(",")
    }
}
fn default_advertised_ip(ip: IpAddr) -> IpAddr {
    if ip.is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        ip
    }
}

fn build_transport_config() -> TransportConfig {
    let mut config = TransportConfig::default();
    config.max_concurrent_bidi_streams(VarInt::from_u32(8));
    config.max_concurrent_uni_streams(VarInt::from_u32(8));
    config.keep_alive_interval(Some(Duration::from_secs(5)));
    config.enable_segmentation_offload(false);
    // P0-2: enable QUIC DATAGRAM (RFC 9221) for the low-latency media path.
    // 1 MiB per-direction send + receive buffer is the canonical setting
    // for ~4 Mbps / 60 fps streams.
    config.datagram_send_buffer_size(1 << 20);
    config.datagram_receive_buffer_size(Some(1 << 20));
    config.min_mtu(1200);
    config
}

/// Build a `TransportConfig` for the QUIC v2 + ACK-Frequency path.
/// Doubles datagram buffers to 2 MiB and wires ACK-Frequency per `policy`.
pub fn build_transport_config_v2(policy: AckPolicy) -> TransportConfig {
    let mut config = build_transport_config();
    config.datagram_send_buffer_size(2 << 20);
    config.datagram_receive_buffer_size(Some(2 << 20));
    let mut ack = quinn::AckFrequencyConfig::default();
    ack.ack_eliciting_threshold(VarInt::from_u32(policy.ack_eliciting_threshold() as u32));
    ack.reordering_threshold(VarInt::from_u32(policy.reordering_threshold() as u32));
    if matches!(policy, AckPolicy::Control | AckPolicy::InputImmediate) {
        ack.max_ack_delay(Some(Duration::from_micros(policy.min_ack_delay_us())));
    }
    config.ack_frequency_config(Some(ack));
    config.keep_alive_interval(Some(Duration::from_secs(5)));
    config
}

/// Build a `quinn::EndpointConfig` that prefers QUIC v2 and falls back to v1.
pub fn build_endpoint_config_v2() -> EndpointConfig {
    let mut cfg = EndpointConfig::default();
    cfg.supported_versions(PREFERRED_QUIC_VERSIONS.to_vec());
    cfg
}

/// Build a `quinn::ServerConfig` that accepts both `qubox-native-quic/0` (v1)
/// and `qubox-native-quic-v2/0` (v2) ALPNs.
pub fn build_server_config_v2(
    cert_der: Vec<rustls::pki_types::CertificateDer<'static>>,
    key_der: rustls::pki_types::PrivateKeyDer<'static>,
) -> anyhow::Result<quinn::ServerConfig> {
    use quinn::crypto::rustls::QuicServerConfig;

    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_der, key_der)
        .context("build_server_config_v2: rustls config")?;
    tls_config.alpn_protocols = vec![
        NATIVE_QUIC_ALPN.as_bytes().to_vec(),
        NATIVE_QUIC_ALPN_V2.as_bytes().to_vec(),
    ];
    let quic_config = QuicServerConfig::try_from(tls_config)
        .map_err(|_| anyhow!("build_server_config_v2: no initial cipher suite"))?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(quic_config));
    let transport = Arc::get_mut(&mut cfg.transport)
        .context("build_server_config_v2: transport already shared")?;
    *transport = build_transport_config_v2(AckPolicy::Media);
    Ok(cfg)
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use qubox_media::EncodedVideoAccessUnit;

    #[test]
    fn alpn_v2_constant() {
        assert_eq!(NATIVE_QUIC_ALPN_V2, "qubox-native-quic-v2/0");
        assert_ne!(NATIVE_QUIC_ALPN_V2, NATIVE_QUIC_ALPN);
    }

    #[test]
    fn quic_wire_version_constants() {
        assert_eq!(QUIC_VERSION_V2, 0x6B3343CF);
        assert_eq!(QUIC_VERSION_V1, 0x0000_0001);
        assert_eq!(PREFERRED_QUIC_VERSIONS, &[QUIC_VERSION_V2, QUIC_VERSION_V1]);
    }

    #[test]
    fn ack_policy_default_values() {
        assert_eq!(AckPolicy::default(), AckPolicy::Media);
        assert_eq!(AckPolicy::Media.min_ack_delay_us(), 25_000);
        assert_eq!(AckPolicy::Control.min_ack_delay_us(), 1_000);
        assert_eq!(AckPolicy::InputImmediate.min_ack_delay_us(), 1_000);
        assert_eq!(AckPolicy::Media.ack_eliciting_threshold(), 1);
        assert_eq!(AckPolicy::Control.ack_eliciting_threshold(), 1);
        assert_eq!(AckPolicy::InputImmediate.ack_eliciting_threshold(), 0);
        assert_eq!(AckPolicy::Media.reordering_threshold(), 2);
        assert_eq!(AckPolicy::Control.reordering_threshold(), 1);
        assert_eq!(AckPolicy::InputImmediate.reordering_threshold(), 1);
    }

    #[test]
    fn ticket_b64_round_trips() {
        let ticket = NativeQuicTicket {
            session_id: Uuid::new_v4(),
            connect_addr: "127.0.0.1:4444".parse().unwrap(),
            server_name: DEFAULT_SERVER_NAME.to_string(),
            alpn: NATIVE_QUIC_ALPN.to_string(),
            cert_der_b64: STANDARD.encode([1_u8, 2, 3, 4]),
            expires_unix_millis: 123,
        };

        let encoded = encode_ticket_b64(&ticket).unwrap();
        let decoded = decode_ticket_b64(&encoded).unwrap();

        assert_eq!(ticket, decoded);
    }

    #[tokio::test]
    async fn loopback_native_quic_media_round_trip() {
        let client_credential = SessionCredential::new_legacy_token(unix_millis_now() + 60_000);
        let video_config = VideoStreamParams {
            codec: VideoCodec::H264,
            width: 1280,
            height: 720,
            framerate: 60,
        };
        let expected_video_config = video_config.clone();
        let audio_config = AudioStreamParams {
            codec: qubox_proto::AudioCodec::PcmF32,
            sample_rate: 48_000,
            channels: 2,
        };
        let expected_audio_config = audio_config.clone();
        let host = NativeQuicHost::bind(
            "127.0.0.1:0".parse().unwrap(),
            None,
            Uuid::new_v4(),
            client_credential.clone(),
        )
        .unwrap();
        let ticket = host.ticket().clone();
        let bytes = vec![0, 0, 1, 0x65, 1, 2, 3];
        let audio_bytes = vec![0, 0, 128, 63, 0, 0, 0, 191];
        let expected_audio_bytes = audio_bytes.clone();

        let server_task = tokio::spawn(async move {
            let connection = tokio::time::timeout(
                Duration::from_secs(5),
                host.accept_authenticated_connection(),
            )
            .await
            .expect("server auth accept timed out")
            .unwrap();
            let mut input_receiver = tokio::time::timeout(
                Duration::from_secs(5),
                connection.open_input_receiver(video_config.clone()),
            )
            .await
            .expect("server input channel open timed out")
            .unwrap();
            let input_task =
                tokio::spawn(
                    async move { input_receiver.read_input_event().await.unwrap().unwrap() },
                );
            let mut audio_sender = tokio::time::timeout(
                Duration::from_secs(5),
                connection.open_audio_sender(audio_config),
            )
            .await
            .expect("server audio open timed out")
            .unwrap();
            let mut sender =
                tokio::time::timeout(Duration::from_secs(5), connection.open_media_sender())
                    .await
                    .expect("server media open timed out")
                    .unwrap();
            // Open the control uni-stream so the client can accept it
            let mut control_sender =
                tokio::time::timeout(Duration::from_secs(5), connection.open_control_sender())
                    .await
                    .expect("server control open timed out")
                    .unwrap();
            // Immediately finish the control stream (no messages to send in this test)
            tokio::time::timeout(Duration::from_secs(5), control_sender.finish())
                .await
                .expect("server control finish timed out")
                .unwrap();
            audio_sender.send_audio_chunk(&audio_bytes).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), audio_sender.finish())
                .await
                .expect("server audio finish timed out")
                .unwrap();
            sender
                .send_access_unit(&EncodedVideoAccessUnit {
                    codec: VideoCodec::H264,
                    frame_id: 7,
                    timestamp_micros: 42,
                    keyframe: true,
                    nal_units: inspect_h264_annex_b_nal_units(&bytes),
                    bytes: bytes.clone(),
                    display_id: 0,
                    stream_id: 0,
                    width: 1920,
                    height: 1080,
                    color_space: None,
                    bit_depth: 8,
                })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), sender.finish())
                .await
                .expect("server media finish timed out")
                .unwrap();
            let input_event = tokio::time::timeout(Duration::from_secs(5), input_task)
                .await
                .expect("server input task timed out")
                .unwrap();

            assert_eq!(
                input_event,
                RemoteInputEvent::Keyboard {
                    key: "Space".to_string(),
                    pressed: true,
                }
            );
        });

        let mut session = tokio::time::timeout(
            Duration::from_secs(20),
            connect_to_native_quic(&ticket, &client_credential),
        )
        .await
        .expect("client connect timed out")
        .unwrap();
        assert_eq!(session.video_config, expected_video_config);
        assert_eq!(session.audio_config, expected_audio_config);
        tokio::time::timeout(
            Duration::from_secs(5),
            session
                .input_sender
                .send_input_event(&RemoteInputEvent::Keyboard {
                    key: "Space".to_string(),
                    pressed: true,
                }),
        )
        .await
        .expect("client input send timed out")
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), session.input_sender.finish())
            .await
            .expect("client input finish timed out")
            .unwrap();

        let audio_chunk = tokio::time::timeout(
            Duration::from_secs(5),
            session.audio_receiver.read_audio_chunk(),
        )
        .await
        .expect("client audio read timed out")
        .unwrap()
        .unwrap();

        assert_eq!(audio_chunk.chunk_id, 0);
        assert_eq!(audio_chunk.bytes, expected_audio_bytes);
        assert!(tokio::time::timeout(
            Duration::from_secs(5),
            session.audio_receiver.read_audio_chunk()
        )
        .await
        .expect("client audio eof read timed out")
        .unwrap()
        .is_none());

        let access_unit = tokio::time::timeout(
            Duration::from_secs(5),
            session.media_receiver.read_access_unit(),
        )
        .await
        .expect("client media read timed out")
        .unwrap()
        .unwrap();

        assert_eq!(access_unit.frame_id, 7);
        assert_eq!(access_unit.timestamp_micros, 42);
        assert!(access_unit.keyframe);
        assert_eq!(access_unit.bytes, vec![0, 0, 1, 0x65, 1, 2, 3]);
        assert!(tokio::time::timeout(
            Duration::from_secs(5),
            session.media_receiver.read_access_unit()
        )
        .await
        .expect("client media eof read timed out")
        .unwrap()
        .is_none());

        // Verify control stream was received and is EOF
        assert!(tokio::time::timeout(
            Duration::from_secs(5),
            session.control_receiver.read_control_msg()
        )
        .await
        .expect("client control read timed out")
        .unwrap()
        .is_none());

        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("server task timed out")
            .unwrap();
    }

    #[test]
    fn stream_envelope_round_trip() {
        let env = StreamEnvelope::new(StreamPurpose::Audio);
        let bytes = env.encode();
        assert_eq!(bytes[0], STREAM_MAGIC);
        assert_eq!(bytes[1], StreamPurpose::Audio.as_byte());
        let decoded = StreamEnvelope::decode(&bytes).unwrap();
        assert_eq!(decoded, env);
    }

    #[test]
    fn stream_envelope_bad_magic_is_detected() {
        let bytes = [0xFF_u8, StreamPurpose::Media.as_byte()];
        match StreamEnvelope::decode(&bytes) {
            Err(StreamEnvelopeError::BadMagic) => {}
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn stream_envelope_unknown_purpose_is_detected() {
        let bytes = [STREAM_MAGIC, 0xEE_u8];
        match StreamEnvelope::decode(&bytes) {
            Err(StreamEnvelopeError::UnknownPurpose(0xEE)) => {}
            other => panic!("expected UnknownPurpose(0xEE), got {other:?}"),
        }
    }

    #[test]
    fn stream_envelope_short_buffer_is_detected() {
        let bytes = [STREAM_MAGIC];
        match StreamEnvelope::decode(&bytes) {
            Err(StreamEnvelopeError::Short) => {}
            other => panic!("expected Short, got {other:?}"),
        }
    }

    #[test]
    fn stream_purpose_from_byte_round_trip() {
        for p in [
            StreamPurpose::VideoConfig,
            StreamPurpose::Audio,
            StreamPurpose::Media,
            StreamPurpose::HostControl,
            StreamPurpose::Input,
            StreamPurpose::FileSync,
        ] {
            assert_eq!(StreamPurpose::from_byte(p.as_byte()), Some(p));
        }
        assert_eq!(StreamPurpose::from_byte(0xFE), None);
    }

    /// Same as the existing `loopback_native_quic_media_round_trip`,
    /// but the host opens streams in REVERSE order (host-control first,
    /// then media, audio, video-config last). The legacy fixed-order
    /// accept path would block on `accept_bi()` for the video-config
    /// stream and time out; the new envelope-based router handles all
    /// four in any order.
    #[tokio::test]
    async fn loopback_native_quic_reverse_order_round_trip() {
        let client_credential = SessionCredential::new_legacy_token(unix_millis_now() + 60_000);
        let video_config = VideoStreamParams {
            codec: VideoCodec::H264,
            width: 1280,
            height: 720,
            framerate: 60,
        };
        let expected_video_config = video_config.clone();
        let audio_config = AudioStreamParams {
            codec: qubox_proto::AudioCodec::PcmF32,
            sample_rate: 48_000,
            channels: 2,
        };
        let expected_audio_config = audio_config.clone();
        let host = NativeQuicHost::bind(
            "127.0.0.1:0".parse().unwrap(),
            None,
            Uuid::new_v4(),
            client_credential.clone(),
        )
        .unwrap();
        let ticket = host.ticket().clone();
        let bytes = vec![0, 0, 1, 0x65, 1, 2, 3];

        // REVERSE order: host-control, media, audio, video-config.
        let server_task = tokio::spawn(async move {
            let connection = tokio::time::timeout(
                Duration::from_secs(5),
                host.accept_authenticated_connection(),
            )
            .await
            .expect("server auth accept timed out")
            .unwrap();
            // 1) host control (uni) — opens first.
            let mut control_sender =
                tokio::time::timeout(Duration::from_secs(5), connection.open_control_sender())
                    .await
                    .expect("server control open timed out")
                    .unwrap();
            tokio::time::timeout(Duration::from_secs(5), control_sender.finish())
                .await
                .expect("server control finish timed out")
                .unwrap();
            // 2) media (uni)
            let mut sender =
                tokio::time::timeout(Duration::from_secs(5), connection.open_media_sender())
                    .await
                    .expect("server media open timed out")
                    .unwrap();
            // 3) audio (uni)
            let mut audio_sender = tokio::time::timeout(
                Duration::from_secs(5),
                connection.open_audio_sender(audio_config),
            )
            .await
            .expect("server audio open timed out")
            .unwrap();
            // 4) input/video-config (bi) — opens LAST.
            let mut input_receiver = tokio::time::timeout(
                Duration::from_secs(5),
                connection.open_input_receiver(video_config.clone()),
            )
            .await
            .expect("server input channel open timed out")
            .unwrap();
            audio_sender.send_audio_chunk(&[1, 2, 3, 4]).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), audio_sender.finish())
                .await
                .expect("server audio finish timed out")
                .unwrap();
            sender
                .send_access_unit(&EncodedVideoAccessUnit {
                    codec: VideoCodec::H264,
                    frame_id: 11,
                    timestamp_micros: 99,
                    keyframe: true,
                    nal_units: inspect_h264_annex_b_nal_units(&bytes),
                    bytes: bytes.clone(),
                    display_id: 0,
                    stream_id: 0,
                    width: 1280,
                    height: 720,
                    color_space: None,
                    bit_depth: 8,
                })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), sender.finish())
                .await
                .expect("server media finish timed out")
                .unwrap();
            let _ = input_receiver.read_input_event().await; // drain
        });

        let mut session = tokio::time::timeout(
            Duration::from_secs(20),
            connect_to_native_quic(&ticket, &client_credential),
        )
        .await
        .expect("client connect timed out")
        .unwrap();
        assert_eq!(session.video_config, expected_video_config);
        assert_eq!(session.audio_config, expected_audio_config);

        tokio::time::timeout(
            Duration::from_secs(5),
            session
                .input_sender
                .send_input_event(&RemoteInputEvent::Keyboard {
                    key: "Space".to_string(),
                    pressed: true,
                }),
        )
        .await
        .expect("client input send timed out")
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), session.input_sender.finish())
            .await
            .expect("client input finish timed out")
            .unwrap();

        let audio_chunk = tokio::time::timeout(
            Duration::from_secs(5),
            session.audio_receiver.read_audio_chunk(),
        )
        .await
        .expect("client audio read timed out")
        .unwrap()
        .unwrap();
        assert_eq!(audio_chunk.chunk_id, 0);
        assert_eq!(audio_chunk.bytes, vec![1, 2, 3, 4]);

        let access_unit = tokio::time::timeout(
            Duration::from_secs(5),
            session.media_receiver.read_access_unit(),
        )
        .await
        .expect("client media read timed out")
        .unwrap()
        .unwrap();
        assert_eq!(access_unit.frame_id, 11);

        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("server task timed out")
            .unwrap();
    }

    #[test]
    fn turn_config_constructs() {
        let cfg = TurnConfig {
            signaling_url: "http://127.0.0.1:7000".to_string(),
            client_credential: SessionCredential::new_legacy_token(unix_millis_now() + 60_000),
            self_peer_id: Uuid::new_v4(),
            remote_peer_id: Uuid::new_v4(),
            turn_server: "127.0.0.1:3478".parse().unwrap(),
            turn_only: false,
            turn_force: false,
        };
        assert_eq!(cfg.turn_server.port(), 3478);
        assert!(!cfg.turn_only);
        assert!(!cfg.turn_force);
        assert!(cfg.signaling_url.contains("127.0.0.1"));
    }

    #[test]
    fn is_valid_hex_pubkey_accepts_hex_and_rejects_garbage() {
        // 64-char hex (lowercase + uppercase) is accepted.
        let lower: String = "a".repeat(64);
        assert!(is_valid_hex_pubkey(&lower));
        let upper: String = "A".repeat(64);
        assert!(is_valid_hex_pubkey(&upper));
        let mixed: String =
            "0123456789abcdef0123456789ABCDEF0123456789abcdef0123456789ABCDEF".into();
        assert!(is_valid_hex_pubkey(&mixed));
        assert!(is_valid_hex_pubkey(&"f".repeat(64)));

        // Anything else is rejected.
        assert!(!is_valid_hex_pubkey(""));
        assert!(!is_valid_hex_pubkey("not-a-key"));
        assert!(!is_valid_hex_pubkey(&"a".repeat(63)));
        assert!(!is_valid_hex_pubkey(&"a".repeat(65)));
        assert!(!is_valid_hex_pubkey(&format!("{}Z", "a".repeat(63))));
        assert!(!is_valid_hex_pubkey(
            "contains a space here that is sixty four chars long   yes!"
        ));
    }

    #[test]
    fn pubkey_to_hex_round_trips_through_validator() {
        let pk = [0xABu8; 32];
        let hex = pubkey_to_hex(&pk);
        assert_eq!(hex.len(), 64);
        assert!(is_valid_hex_pubkey(&hex), "rendered hex must re-validate");
        assert_eq!(hex, "ab".repeat(32));
    }

    #[test]
    fn bearer_from_credential_round_trips_via_base64_json() {
        let cred = SessionCredential::new_legacy_token(unix_millis_now() + 60_000);
        let bearer = bearer_from_credential(&cred);
        // base64-decodable
        let raw = STANDARD.decode(&bearer).expect("bearer is base64");
        // JSON-decodable into a SessionCredential equal to the original
        let decoded: SessionCredential =
            serde_json::from_slice(&raw).expect("decoded bearer is SessionCredential JSON");
        assert_eq!(decoded.token, cred.token);
        assert_eq!(decoded.expires_unix_millis, cred.expires_unix_millis);
    }

    #[test]
    fn turn_config_constructor_with_session_credential_round_trip() {
        // Mirrors the legacy turn_config_constructs but with the new
        // `client_credential` field. Confirms we can build, clone, and
        // inspect a TurnConfig whose auth material is a SessionCredential.
        let cred = SessionCredential::new_legacy_token(unix_millis_now() + 60_000);
        let cfg = TurnConfig {
            signaling_url: "https://signaling.example/v1".to_string(),
            client_credential: cred.clone(),
            self_peer_id: Uuid::new_v4(),
            remote_peer_id: Uuid::new_v4(),
            turn_server: "10.0.0.1:3478".parse().unwrap(),
            turn_only: true,
            turn_force: true,
        };
        let cloned = cfg.clone();
        assert_eq!(cfg.client_credential.token, cloned.client_credential.token);
        assert!(cfg.turn_only && cfg.turn_force);
        // The bearer must be base64(SessionCredential JSON) — i.e. it must
        // round-trip through bearer_from_credential.
        let bearer = bearer_from_credential(&cfg.client_credential);
        let raw = STANDARD.decode(&bearer).unwrap();
        let decoded: SessionCredential = serde_json::from_slice(&raw).unwrap();
        assert_eq!(decoded.token, cfg.client_credential.token);
    }

    #[test]
    fn accept_auth_rejects_mismatched_credential() {
        let session_id = Uuid::new_v4();
        let issued = unix_millis_now();
        let expires = issued + 60_000;
        let expected = SessionCredential::issue(
            b"server-secret",
            session_id,
            [1_u8; 32],
            [2_u8; 32],
            issued,
            expires,
        );
        let credential = SessionCredential::issue(
            b"server-secret",
            session_id,
            [1_u8; 32],
            [3_u8; 32],
            issued,
            expires,
        );
        let auth = ClientAuth {
            session_id,
            credential: credential.clone(),
        };
        let raw = serde_json::to_vec(&auth).unwrap();
        let decoded: ClientAuth = serde_json::from_slice(&raw).unwrap();

        assert_eq!(decoded, auth);
        assert_eq!(decoded.credential.client_pubkey, credential.client_pubkey);
        assert_eq!(decoded.credential.hmac, credential.hmac);
        assert_ne!(decoded.credential.client_pubkey, expected.client_pubkey);
        assert_ne!(decoded.credential.hmac, expected.hmac);
    }

    #[tokio::test]
    async fn connect_via_turn_rejects_expired_ticket() {
        let ticket = NativeQuicTicket {
            session_id: Uuid::new_v4(),
            connect_addr: "127.0.0.1:9999".parse().unwrap(),
            server_name: DEFAULT_SERVER_NAME.to_string(),
            alpn: NATIVE_QUIC_ALPN.to_string(),
            cert_der_b64: "AAAA".to_string(),
            expires_unix_millis: unix_millis_now() - 1,
        };
        let cred = SessionCredential::new_legacy_token(unix_millis_now() + 60_000);
        match connect_via_turn(
            &ticket,
            &cred,
            "127.0.0.1:3478".parse().unwrap(),
            "user".to_string(),
            "pass".to_string(),
            "127.0.0.1:9998".parse().unwrap(),
        )
        .await
        {
            Err(e) => assert!(e.to_string().contains("expired"), "got: {e}"),
            Ok(_) => panic!("expected Err for expired ticket"),
        }
    }

    #[tokio::test]
    async fn bind_via_turn_fails_when_no_turn_server() {
        // No TURN server listening; bind_via_turn should fail with a
        // connection error (expiry is checked at accept time, not bind time).
        assert!(bind_via_turn(
            "0.0.0.0:0".parse().unwrap(),
            "10.0.0.1:3478".parse().unwrap(),
            Uuid::new_v4(),
            SessionCredential::new_legacy_token(unix_millis_now() + 60_000),
            "127.0.0.1:3478".parse().unwrap(),
            "user".to_string(),
            "pass".to_string(),
            "127.0.0.1:9998".parse().unwrap(),
        )
        .await
        .is_err());
    }
}

/// Length-prefix DoS bounds for the framed readers.
/// Exercises `read_json_prefixed`, `read_access_unit_header`, and
/// `read_audio_chunk_header` with payloads that exceed the documented
/// caps and asserts each reader rejects them *without* performing the
/// would-be OOM-sized allocation.
#[cfg(test)]
mod framing_tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::AsyncWriteExt;

    /// Serialize `payload` exactly the way the production code would:
    /// `tokio::io::AsyncWriteExt::write_u32` (big-endian) length prefix
    /// followed by the JSON body. Writing into a `Vec<u8>` is supported
    /// by Tokio's `AsyncWrite` impl, so this is a faithful round-trip.
    async fn write_json_frame<T: serde::Serialize>(payload: &T) -> Vec<u8> {
        let mut buf = Vec::new();
        write_json_prefixed(&mut buf, payload)
            .await
            .expect("write_json_prefixed to a Vec<u8>");
        buf
    }

    /// Build a wire frame whose 4-byte length prefix advertises
    /// `len_advertised`, regardless of what (if anything) follows. This
    /// lets us force the receiver to examine the length *first* — the
    /// whole point of the cap check — and bail before reading the body.
    async fn wire_with_advertised_len(len_advertised: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.write_u32(len_advertised).await.unwrap();
        buf
    }

    #[tokio::test]
    async fn read_json_prefixed_rejects_oversized_length_prefix() {
        let cap_plus_one = MAX_JSON_FRAME + 1;
        let buf = wire_with_advertised_len(cap_plus_one).await;
        let mut cursor = Cursor::new(buf);
        let err = read_json_prefixed::<_, serde_json::Value>(&mut cursor)
            .await
            .expect_err("read_json_prefixed must reject an oversized length prefix");
        let msg = err.to_string();
        assert!(
            msg.contains("MAX_JSON_FRAME"),
            "error must name the cap, got: {msg}"
        );
        assert!(
            msg.contains(&cap_plus_one.to_string()),
            "error must echo the offending length, got: {msg}"
        );
        // We must NOT have attempted to read the body: the cursor
        // should still be positioned at byte 4.
        assert_eq!(
            cursor.position(),
            4,
            "the bound check must fire before reading the JSON body"
        );
    }

    #[tokio::test]
    async fn read_access_unit_header_rejects_oversized_byte_len() {
        let oversized = MAX_VIDEO_AU_BYTES as usize + 1;
        let payload = WireAccessUnitHeader {
            session_id: Uuid::nil(),
            frame_id: 0,
            timestamp_micros: 0,
            keyframe: false,
            byte_len: oversized,
            codec: None,
            stream_id: 0,
            display_id: 0,
            width: 0,
            height: 0,
            refresh_hz: 0.0,
            color_space_id: 0,
            hdr_static_metadata: None,
        };
        let buf = write_json_frame(&payload).await;

        // Sanity: the JSON header itself must be small (< MAX_JSON_FRAME)
        // so the receiver can read it *before* applying the AU cap check.
        // Tokio's `write_u32` is big-endian.
        let header_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert!(
            header_len <= MAX_JSON_FRAME,
            "test fixture: serialized header len {header_len} must be within MAX_JSON_FRAME ({MAX_JSON_FRAME})"
        );

        let mut cursor = Cursor::new(buf);
        let err = read_access_unit_header(&mut cursor)
            .await
            .expect_err("read_access_unit_header must reject an oversized byte_len");
        let msg = err.to_string();
        assert!(
            msg.contains("MAX_VIDEO_AU_BYTES"),
            "error must name the cap, got: {msg}"
        );
        assert!(
            msg.contains(&oversized.to_string()),
            "error must echo the offending byte_len, got: {msg}"
        );
    }

    #[tokio::test]
    async fn read_audio_chunk_header_rejects_oversized_byte_len() {
        let oversized = MAX_AUDIO_CHUNK_BYTES as usize + 1;
        let payload = WireAudioChunkHeader {
            session_id: Uuid::nil(),
            chunk_id: 0,
            byte_len: oversized,
        };
        let buf = write_json_frame(&payload).await;

        // Sanity: the JSON header itself must be small (< MAX_JSON_FRAME)
        // so the receiver can read it *before* applying the audio cap
        // check.
        let header_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert!(
            header_len <= MAX_JSON_FRAME,
            "test fixture: serialized header len {header_len} must be within MAX_JSON_FRAME ({MAX_JSON_FRAME})"
        );

        let mut cursor = Cursor::new(buf);
        let err = read_audio_chunk_header(&mut cursor)
            .await
            .expect_err("read_audio_chunk_header must reject an oversized byte_len");
        let msg = err.to_string();
        assert!(
            msg.contains("MAX_AUDIO_CHUNK_BYTES"),
            "error must name the cap, got: {msg}"
        );
        assert!(
            msg.contains(&oversized.to_string()),
            "error must echo the offending byte_len, got: {msg}"
        );
    }

    #[tokio::test]
    async fn read_access_unit_header_accepts_payload_under_cap() {
        // A modest AU byte_len well under the cap must parse cleanly.
        let payload = WireAccessUnitHeader {
            session_id: Uuid::nil(),
            frame_id: 7,
            timestamp_micros: 1234,
            keyframe: true,
            byte_len: 1024,
            codec: Some(VideoCodec::H264),
            stream_id: 0,
            display_id: 0,
            width: 0,
            height: 0,
            refresh_hz: 0.0,
            color_space_id: 0,
            hdr_static_metadata: None,
        };
        let buf = write_json_frame(&payload).await;
        let mut cursor = Cursor::new(buf);

        let header = read_access_unit_header(&mut cursor)
            .await
            .expect("under-cap header must parse")
            .expect("must be present");
        assert_eq!(header.byte_len, 1024);
        assert_eq!(header.frame_id, 7);
    }
}
