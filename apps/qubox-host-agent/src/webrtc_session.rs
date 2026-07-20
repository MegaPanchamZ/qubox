//! WebRTC transport for browser-originated sessions.
//!
//! The browser client bundle (`site/lib/qubox-client`) speaks WebRTC over a
//! browser-native `RTCPeerConnection`; the signaling protocol already carries
//! the SDP offer / answer and ICE candidate relay between the two endpoints.
//! This module plugs the host agent into that pipeline: it accepts the
//! browser's SDP offer, generates an SDP answer, exchanges ICE candidates via
//! the existing `RelaySignal` channel, and exposes video / audio tracks plus
//! a data channel for browser→host input events.
//!
//! Phase A (this file):
//!   * PeerConnection bootstrap with ICE servers from the signaling plan
//!   * SDP offer → answer
//!   * Trickle ICE (host → browser via RelaySignal, browser → host via the
//!     same dispatch path)
//!   * A single H.264 video track + an Opus audio track declared up-front so
//!     the browser attaches them to the MediaStream. Phase B wires the real
//!     capture pipeline (PipeWire / DXGI / X11) into `write_video`/`write_audio`.
//!   * A reliable SCTP data channel (`qubox-input`) for `RemoteInputEvent`s
//!     routed from the browser.
//!
//! Everything routes through the host agent's existing `SharedSignalingWriter`
//! — there is no separate WebSocket for WebRTC media control. The ICE/SDP
//! exchange IS the control plane; media flows over the resulting peer
//! connection directly to the browser.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::{watch, Mutex};
use uuid::Uuid;
use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use crate::turn_local;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_credential_type::RTCIceCredentialType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

use qubox_proto::{
    ClientMessage, IceServer, PeerDescriptor, RelaySignal, RemoteInputEvent, SessionSignal,
};

use crate::{send_client_message, SharedSignalingWriter};

/// Registry of in-flight WebRTC sessions keyed by `session_id`. The signaling
/// loop hands incoming `RelaySignal`s to the right session by id.
pub type SessionRegistry = Arc<Mutex<std::collections::HashMap<Uuid, Arc<WebRtcSession>>>>;

/// Per-session counters for input + media (log-friendly; no OTLP required).
#[derive(Default)]
pub struct SessionMetrics {
    pub input_rx: AtomicU64,
    pub input_injected: AtomicU64,
    pub input_parse_err: AtomicU64,
    pub input_inject_err: AtomicU64,
    pub video_frames_written: AtomicU64,
    pub video_bytes_written: AtomicU64,
}

/// One browser → host WebRTC session.
pub struct WebRtcSession {
    pc: Arc<RTCPeerConnection>,
    video_track: Arc<TrackLocalStaticSample>,
    audio_track: Arc<TrackLocalStaticSample>,
    input_channel: Mutex<Option<Arc<RTCDataChannel>>>,
    signaling_writer: SharedSignalingWriter,
    self_peer_id: Uuid,
    session_id: Uuid,
    client: PeerDescriptor,
    codec: qubox_proto::VideoCodec,
    cancel_rx: watch::Receiver<bool>,
    cancel_tx: watch::Sender<bool>,
    pub metrics: Arc<SessionMetrics>,
}

impl WebRtcSession {
    pub async fn new(
        signaling_writer: SharedSignalingWriter,
        self_peer_id: Uuid,
        session_id: Uuid,
        client: PeerDescriptor,
        ice_servers: &[IceServer],
        codec: qubox_proto::VideoCodec,
    ) -> Result<Arc<Self>> {
        let mut media_engine = webrtc::api::media_engine::MediaEngine::default();
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: MIME_TYPE_OPUS.to_owned(),
                        clock_rate: 48000,
                        channels: 2,
                        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                        rtcp_feedback: vec![],
                    },
                    payload_type: 111,
                    ..Default::default()
                },
                RTPCodecType::Audio,
            )
            .map_err(|e| anyhow!("register opus codec: {e}"))?;

        let video_rtcp_feedback = vec![
            RTCPFeedback {
                typ: "goog-remb".to_owned(),
                parameter: "".to_owned(),
            },
            RTCPFeedback {
                typ: "ccm".to_owned(),
                parameter: "fir".to_owned(),
            },
            RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: "".to_owned(),
            },
            RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: "pli".to_owned(),
            },
        ];

        // Register H.264 in the same Baseline / level 3.1 / packetization-mode=1
        // flavour the browser offered; we'll pick the matching PT during
        // negotiation.
        let h264_levels = [
            (
                "42001f",
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f",
                103,
            ),
            (
                "42e01f",
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
                109,
            ),
            (
                "4d001f",
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=4d001f",
                117,
            ),
        ];
        for (profile_level_id, sdp_fmtp_line, payload_type) in h264_levels {
            media_engine
                .register_codec(
                    RTCRtpCodecParameters {
                        capability: RTCRtpCodecCapability {
                            mime_type: MIME_TYPE_H264.to_owned(),
                            clock_rate: 90000,
                            channels: 0,
                            sdp_fmtp_line: sdp_fmtp_line.to_owned(),
                            rtcp_feedback: video_rtcp_feedback.clone(),
                        },
                        payload_type,
                        ..Default::default()
                    },
                    RTPCodecType::Video,
                )
                .map_err(|e| anyhow!("register h264 codec {profile_level_id}: {e}"))?;
        }

        let api = APIBuilder::new().with_media_engine(media_engine).build();

        let creds = turn_local::mint_for_session(session_id, self_peer_id, ice_servers);
        if let Some(reason) = creds.skip_reason {
            tracing::warn!(
                session_id = %session_id,
                %reason,
                "TURN credentials missing; relay candidates will fail to authenticate"
            );
        }
        let config = RTCConfiguration {
            ice_servers: creds
                .servers
                .iter()
                .map(|s| RTCIceServer {
                    urls: s.urls.clone(),
                    username: s.username.clone().unwrap_or_default(),
                    credential: s.credential.clone().unwrap_or_default(),
                    credential_type: RTCIceCredentialType::Password,
                })
                .collect(),
            ..Default::default()
        };

        let pc = Arc::new(api.new_peer_connection(config).await?);

        let video_codec = RTCRtpCodecCapability {
            mime_type: "video/H264".to_string(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"
                .to_string(),
            ..Default::default()
        };
        let video_track = Arc::new(TrackLocalStaticSample::new(
            video_codec,
            "video".to_string(),
            "qubox".to_string(),
        ));
        pc.add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        let audio_codec = RTCRtpCodecCapability {
            mime_type: "audio/opus".to_string(),
            clock_rate: 48000,
            channels: 2,
            ..Default::default()
        };
        let audio_track = Arc::new(TrackLocalStaticSample::new(
            audio_codec,
            "audio".to_string(),
            "qubox".to_string(),
        ));
        pc.add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let metrics = Arc::new(SessionMetrics::default());
        let session = Arc::new(Self {
            pc: pc.clone(),
            video_track,
            audio_track,
            input_channel: Mutex::new(None),
            signaling_writer,
            self_peer_id,
            session_id,
            client,
            codec,
            cancel_rx: cancel_rx.clone(),
            cancel_tx,
            metrics: metrics.clone(),
        });

        tracing::info!(
            session_id = %session_id,
            client_peer = %session.client.peer_id,
            codec = ?codec,
            "session.start"
        );

        // Trickle ICE: every time the host gathers a candidate, push it back to
        // the browser over the same signaling socket. The browser will call
        // `addIceCandidate` and the DTLS handshake completes once both sides
        // settle on a nominated pair.
        let writer_for_ice = session.signaling_writer.clone();
        let session_id_for_ice = session.session_id;
        let client_peer_for_ice = session.client.peer_id;
        let self_peer_for_ice = session.self_peer_id;
        pc.on_ice_candidate(Box::new(move |candidate| {
            let writer = writer_for_ice.clone();
            let session_id = session_id_for_ice;
            let client_peer = client_peer_for_ice;
            let self_peer = self_peer_for_ice;
            Box::pin(async move {
                let Some(candidate) = candidate else {
                    return;
                };
                let cand_str = candidate.to_string();
                let typ = if cand_str.contains("typ relay") {
                    "relay"
                } else if cand_str.contains("typ srflx") {
                    "srflx"
                } else if cand_str.contains("typ host") {
                    "host"
                } else {
                    "other"
                };
                tracing::debug!(
                    session_id = %session_id,
                    candidate_type = typ,
                    "webrtc local ice candidate"
                );
                let json = match candidate.to_json() {
                    Ok(j) => j,
                    Err(err) => {
                        tracing::warn!(?err, "failed to marshal ice candidate; dropping");
                        return;
                    }
                };
                if let Err(err) = send_client_message(
                    &writer,
                    &ClientMessage::RelaySignal(RelaySignal {
                        session_id,
                        from_peer_id: self_peer,
                        to_peer_id: client_peer,
                        signal: SessionSignal::IceCandidate {
                            candidate: json.candidate,
                            sdp_mid: json.sdp_mid,
                            sdp_mline_index: json.sdp_mline_index.map(|v| v as u16),
                        },
                    }),
                )
                .await
                {
                    tracing::warn!(?err, "failed to relay ICE candidate to browser");
                }
            })
        }));

        // Connection-state observer: log transitions so we can correlate
        // browser-side "negotiating_webrtc" → "live" reports with host-side
        // DTLS/ICE state.
        let session_id_for_state = session.session_id;
        pc.on_peer_connection_state_change(Box::new(move |state| {
            tracing::info!(
                session_id = %session_id_for_state,
                state = ?state,
                "webrtc.pc_state"
            );
            Box::pin(async move {
                if state == RTCPeerConnectionState::Failed {
                    tracing::warn!(
                        session_id = %session_id_for_state,
                        "webrtc peer connection entered Failed state"
                    );
                }
            })
        }));

        let session_id_for_ice_state = session.session_id;
        pc.on_ice_connection_state_change(Box::new(move |state: RTCIceConnectionState| {
            tracing::info!(
                session_id = %session_id_for_ice_state,
                state = ?state,
                "webrtc.ice_state"
            );
            Box::pin(async {})
        }));

        // The browser side creates the data channel ("qubox-input") inside the
        // SDP offer. We hook into `on_data_channel` to receive input events.
        let input_slot = Arc::new(Mutex::new(None::<Arc<RTCDataChannel>>));
        let input_slot_for_cb = input_slot.clone();
        let metrics_for_dc = metrics.clone();
        let session_id_for_dc = session.session_id;
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let label = dc.label().to_string();
            tracing::info!(
                session_id = %session_id_for_dc,
                label = %label,
                "browser opened data channel"
            );

            let dc_for_msg = dc.clone();
            // Prefer enigo/XTEST on X11 — uinput devices often never attach
            // to the X server (missing from `xinput list`), so ABS writes
            // succeed but the cursor never moves.
            let injector = match crate::input_inject::HostInputInjector::open_best() {
                Ok(inj) => {
                    tracing::info!(
                        session_id = %session_id_for_dc,
                        backend = inj.backend_name(),
                        "input injector ready"
                    );
                    Some(Arc::new(inj))
                }
                Err(err) => {
                    tracing::warn!(
                        session_id = %session_id_for_dc,
                        ?err,
                        "no input injector; events logged only"
                    );
                    None
                }
            };
            let injector_for_msg = injector.clone();
            let metrics_for_msg = metrics_for_dc.clone();
            let sid = session_id_for_dc;
            dc.on_message(Box::new(move |msg| {
                let data = msg.data.clone();
                metrics_for_msg.input_rx.fetch_add(1, Ordering::Relaxed);
                let parsed = match serde_json::from_slice::<RemoteInputEvent>(&data) {
                    Ok(ev) => ev,
                    Err(err) => {
                        metrics_for_msg
                            .input_parse_err
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            session_id = %sid,
                            ?err,
                            len = data.len(),
                            "ignoring malformed input event"
                        );
                        return Box::pin(async {});
                    }
                };
                let rx = metrics_for_msg.input_rx.load(Ordering::Relaxed);
                if rx <= 5 || rx % 50 == 0 {
                    tracing::info!(
                        session_id = %sid,
                        input_rx = rx,
                        ?parsed,
                        "received remote input event"
                    );
                } else {
                    tracing::trace!(session_id = %sid, ?parsed, "received remote input event");
                }
                if let Some(inj) = injector_for_msg.as_ref() {
                    match inj.dispatch(&parsed) {
                        Ok(()) => {
                            metrics_for_msg
                                .input_injected
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        Err(err) => {
                            metrics_for_msg
                                .input_inject_err
                                .fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                session_id = %sid,
                                ?err,
                                "input dispatch failed"
                            );
                        }
                    }
                }
                let _ = dc_for_msg;
                Box::pin(async {})
            }));

            // Stash the handle so the host can later `send_text` back over the
            // same channel (clipboard, status updates).
            let slot = input_slot_for_cb.clone();
            let dc_clone = dc.clone();
            Box::pin(async move {
                *slot.lock().await = Some(dc_clone);
            })
        }));

        // Periodic session stats (every 2s while alive).
        let metrics_tick = metrics.clone();
        let mut cancel_for_stats = cancel_rx.clone();
        let sid_stats = session.session_id;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    _ = cancel_for_stats.changed() => {
                        if *cancel_for_stats.borrow() {
                            break;
                        }
                    }
                    _ = interval.tick() => {
                        tracing::info!(
                            session_id = %sid_stats,
                            input_rx = metrics_tick.input_rx.load(Ordering::Relaxed),
                            input_injected = metrics_tick.input_injected.load(Ordering::Relaxed),
                            input_parse_err = metrics_tick.input_parse_err.load(Ordering::Relaxed),
                            input_inject_err = metrics_tick.input_inject_err.load(Ordering::Relaxed),
                            video_frames = metrics_tick.video_frames_written.load(Ordering::Relaxed),
                            video_bytes = metrics_tick.video_bytes_written.load(Ordering::Relaxed),
                            "session.stats"
                        );
                    }
                }
            }
            tracing::info!(
                session_id = %sid_stats,
                input_rx = metrics_tick.input_rx.load(Ordering::Relaxed),
                input_injected = metrics_tick.input_injected.load(Ordering::Relaxed),
                video_frames = metrics_tick.video_frames_written.load(Ordering::Relaxed),
                "session.end"
            );
        });

        let _ = input_slot;

        Ok(session)
    }

    /// Apply the browser's SDP offer and produce an SDP answer to send back.
    pub async fn handle_offer(&self, sdp: String) -> Result<String> {
        let offer = RTCSessionDescription::offer(sdp)
            .map_err(|e| anyhow!("invalid SDP offer from browser: {e}"))?;
        self.pc
            .set_remote_description(offer)
            .await
            .map_err(|e| anyhow!("set_remote_description failed: {e}"))?;
        let answer = self
            .pc
            .create_answer(None)
            .await
            .map_err(|e| anyhow!("create_answer failed: {e}"))?;
        self.pc
            .set_local_description(answer.clone())
            .await
            .map_err(|e| anyhow!("set_local_description failed: {e}"))?;
        Ok(answer.sdp)
    }

    /// Apply an ICE candidate received from the browser via `RelaySignal`.
    pub async fn add_ice_candidate(
        &self,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) -> Result<()> {
        self.pc
            .add_ice_candidate(RTCIceCandidateInit {
                candidate,
                sdp_mid,
                sdp_mline_index,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow!("add_ice_candidate failed: {e}"))
    }

    /// Send a single H.264 access unit (Annex-B format: SPS+PPS+IDR/P slice)
    /// onto the video track. The browser's MediaStream sees it as a frame.
    pub async fn write_video(&self, annex_b: bytes::Bytes) -> Result<()> {
        let nbytes = annex_b.len() as u64;
        let sample = webrtc::media::Sample {
            data: annex_b,
            duration: Duration::from_millis(33),
            ..Default::default()
        };
        self.video_track
            .write_sample(&sample)
            .await
            .map_err(|e| anyhow!("video_track.write_sample failed: {e}"))?;
        self.metrics
            .video_frames_written
            .fetch_add(1, Ordering::Relaxed);
        self.metrics
            .video_bytes_written
            .fetch_add(nbytes, Ordering::Relaxed);
        Ok(())
    }

    /// Send a 20 ms Opus frame onto the audio track.
    pub async fn write_audio(&self, opus_frame: bytes::Bytes) -> Result<()> {
        let sample = webrtc::media::Sample {
            data: opus_frame,
            duration: Duration::from_millis(20),
            ..Default::default()
        };
        self.audio_track
            .write_sample(&sample)
            .await
            .map_err(|e| anyhow!("audio_track.write_sample failed: {e}"))
    }

    pub fn session_id(&self) -> Uuid {
        self.session_id
    }

    pub fn codec(&self) -> qubox_proto::VideoCodec {
        self.codec
    }

    /// Watch channel the capture pipeline uses to learn the session
    /// should shut down (signaling disconnect, kill received, etc).
    /// Producers watch this and stop writing samples.
    pub fn cancel_rx(&self) -> watch::Receiver<bool> {
        self.cancel_rx.clone()
    }

    pub async fn close(&self) -> Result<()> {
        let _ = self.cancel_tx.send(true);
        self.pc.close().await.ok();
        Ok(())
    }

    pub async fn send_relay_signal(&self, signal: SessionSignal) -> Result<()> {
        send_client_message(
            &self.signaling_writer,
            &ClientMessage::RelaySignal(RelaySignal {
                session_id: self.session_id,
                from_peer_id: self.self_peer_id,
                to_peer_id: self.client.peer_id,
                signal,
            }),
        )
        .await
    }
}

/// Dispatch an incoming `SessionSignal` from the browser to the right session.
/// Returns `Ok(true)` if a session handled it, `Ok(false)` if no session
/// matched (e.g., the session already closed).
pub async fn dispatch_signal(
    registry: &SessionRegistry,
    session_id: Uuid,
    signal: SessionSignal,
) -> Result<bool> {
    // Retry briefly: when the host registers a session it does so after
    // some setup latency (ffmpeg spawn, NAL parser warmup). The browser
    // may fire the SDP offer over the relay before the registry sees the
    // insert. Without retry we'd silently drop the offer and the peer
    // connection would never advance past ice-connecting.
    let mut session = registry.lock().await.get(&session_id).cloned();
    if session.is_none() {
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            session = registry.lock().await.get(&session_id).cloned();
            if session.is_some() {
                break;
            }
        }
    }
    let Some(session) = session else {
        tracing::warn!(
            %session_id,
            ?signal,
            "no session in registry after 1s; signaling dropped"
        );
        return Ok(false);
    };
    match signal {
        SessionSignal::SdpOffer { sdp } => {
            tracing::info!(%session_id, "applying browser SDP offer and crafting answer");
            let answer = session.handle_offer(sdp).await?;
            session
                .send_relay_signal(SessionSignal::SdpAnswer { sdp: answer })
                .await?;
        }
        SessionSignal::SdpAnswer { sdp } => {
            // The host is the answerer; an inbound SdpAnswer is unexpected but
            // tolerate it (e.g., during SDP renegotiation).
            tracing::debug!(%session_id, "ignoring SDP answer (host is answerer)");
            let _ = sdp;
        }
        SessionSignal::IceCandidate {
            candidate,
            sdp_mid,
            sdp_mline_index,
        } => {
            session
                .add_ice_candidate(candidate, sdp_mid, sdp_mline_index)
                .await?;
        }
        SessionSignal::NativeQuicTicket { .. } => {
            tracing::warn!(%session_id, "host got NativeQuicTicket on a WebRTC session; ignoring");
        }
        SessionSignal::WebTransportTicket { .. } => {
            tracing::warn!(%session_id, "host got WebTransportTicket on a WebRTC session; ignoring");
        }
        SessionSignal::Ready => {}
    }
    Ok(true)
}

/// Background "test pattern" video producer. Sends a single black H.264 IDR
/// frame repeatedly so the browser's MediaStream has SOMETHING to render while
/// the real capture pipeline is being wired up in Phase B.
///
/// The IDR + SPS + PPS below encodes a 16x16 black frame; `webrtc-rs` will
/// packetise each access unit into RTP. The browser's `videoEl.srcObject`
/// receives the MediaStream and the WebRTC `playing` event fires once ICE
/// connects.
pub async fn spawn_test_pattern_producer(session: Arc<WebRtcSession>) {
    // 16×16 black frame, H.264 Baseline L3.0. Generated with:
    //   ffmpeg -f lavfi -i "color=c=black:s=16x16:r=1" -vframes 1 \
    //          -vcodec libx264 -profile:v baseline -level 3.0      \
    //          -pix_fmt yuv420p -fflags +bitexact -flags:v +bitexact \
    //          out.h264
    // then stripped to SPS + PPS + IDR NAL units only (SEI discarded).
    // The SPS/PPS are emitted once per access unit so the browser decoder
    // initialises immediately on the first RTP packet.
    static BLACK_16X16: &[u8] = &[
        // SPS (NAL type 7): Baseline L3.0, 16×16
        0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0, 0x1E, 0xDD, 0xEC, 0x04, 0x40, 0x00, 0x00, 0x03,
        0x00, 0x40, 0x00, 0x00, 0x03, 0x00, 0x83, 0xC5, 0x8B, 0xE0, // PPS (NAL type 8)
        0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x0F, 0x2C, 0x80,
        // IDR slice (NAL type 5): single black macroblock
        0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x04, 0xBC, 0x98, 0xA0, 0x00, 0x38, 0xA3, 0x80,
    ];
    let frame = bytes::Bytes::copy_from_slice(BLACK_16X16);
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if let Err(err) = session.write_video(frame.clone()).await {
            tracing::warn!(?err, "test pattern producer write failed");
            break;
        }
    }
}

// Suppress unused-imports when this module is added but some helpers are
// still evolving.
#[allow(dead_code)]
fn _unused_ice_init() -> RTCIceCandidateInit {
    RTCIceCandidateInit::default()
}
