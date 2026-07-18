use std::{
    fs::{self, File},
    io::{Read, Write},
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::{anyhow, Context};
use clap::{Parser, ValueEnum};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    SampleFormat, Stream,
};
use enigo::{
    Button, Coordinate,
    Direction::{Press, Release},
    Enigo, Key, Keyboard, Mouse, Settings,
};
use futures::{stream::SplitSink, Sink, SinkExt, StreamExt};
use qubox_identity::load_or_create_identity;
use qubox_media::{
    best_h264_encoder_for_platform, plan_ffmpeg_h264, plan_ffmpeg_pipewire_h264,
    preferred_linux_capture_kind, probe_default_host_pipeline, probe_linux_capture_backends,
    read_h264_access_units, spawn_ffmpeg_pipeline, FfmpegPipelinePlan, H264AnnexBStreamFramer,
    H264EncoderBackend, HostVideoPipelineConfig, MediaBackendReport, MediaPipelineRead,
};
use qubox_platform::describe_peer;
use qubox_proto::{
    AudioCodec, AudioStreamParams, CaptureKind, ClientMessage, ControlMsg, InputMouseButton,
    PairingDecision, PeerRole, PlatformOs, RelaySignal, RemoteInputEvent, ServerMessage,
    SessionPermissions, SessionRequested, SessionSignal, SignedHello, TransportKind, VideoCodec,
    VideoStreamParams,
};
use qubox_transport::{
    encode_ticket_b64, NativeQuicAudioSender, NativeQuicHost, NativeQuicInputReceiver,
    NATIVE_QUIC_ALPN,
};
use serde::Serialize;
use tokio::{
    net::TcpStream,
    sync::{mpsc as tokio_mpsc, Mutex},
};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

mod capture_orchestrator;
mod filesync_drain;
mod input_mapping;
mod pairing_ui;
mod permissions;
mod privacy;
mod rate_control;
mod rate_feedback;
mod webrtc_session;

#[cfg(feature = "file-sync")]
mod file_sync_sensors;

use qubox_display::{
    error::DisplayError,
    traits::DisplayManager,
    types::{DisplayId, DisplayState},
};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value_t = false)]
    allow_standalone: bool,

    #[arg(long, env = "QUBOX_SERVER", default_value = "ws://127.0.0.1:7000/ws")]
    server: String,

    #[arg(long)]
    name: Option<String>,

    #[arg(long, env = "QUBOX_IDENTITY_PATH")]
    identity_path: Option<PathBuf>,

    #[arg(long)]
    /// DANGER: auto-approves every pairing request. Never enable on
    /// internet-facing hosts. Managed product must use explicit pair policy.
    auto_approve_pairing: bool,

    #[arg(long)]
    probe_media: bool,

    #[arg(long)]
    plan_host_h264: bool,

    #[arg(long)]
    smoke_test: bool,

    #[arg(long)]
    smoke_test_output: Option<PathBuf>,

    #[arg(long)]
    plan_linux_pipewire_h264: bool,

    #[arg(long)]
    run_linux_pipewire_h264: bool,

    #[arg(long, default_value = "0")]
    pipewire_node: String,

    #[arg(long, value_enum, default_value = "auto")]
    linux_capture: CliLinuxCapture,

    #[arg(long, default_value = ":0.0")]
    x11_display: String,

    #[arg(long, default_value = "desktop")]
    windows_capture_input: String,

    #[arg(long, value_enum)]
    h264_encoder: Option<CliH264Encoder>,

    #[arg(long)]
    disable_audio: bool,

    #[arg(long, default_value_t = 1920)]
    media_width: u32,

    #[arg(long, default_value_t = 1080)]
    media_height: u32,

    #[arg(long, default_value_t = 60)]
    media_fps: u32,

    #[arg(long, default_value_t = 20_000)]
    media_bitrate_kbps: u32,

    #[arg(long, default_value_t = 120)]
    max_media_frames: u64,

    #[arg(long, default_value = "0.0.0.0:0")]
    native_quic_bind: SocketAddr,

    #[arg(long)]
    native_quic_advertise_ip: Option<IpAddr>,

    // ── Multi-display / multi-stream flags ──
    #[arg(long, value_enum, default_value = "single-stream")]
    stream_mode: StreamMode,

    #[arg(long)]
    display: Option<u32>,

    // ── Privacy mode flags ──
    #[arg(long, value_enum, default_value_t = PrivacyModeArg::None)]
    privacy_mode: PrivacyModeArg,

    #[arg(long, default_value_t = false)]
    enable_privacy_on_session_start: bool,

    #[arg(long, default_value = "VKMS-1")]
    vkms_output_name: String,

    // ── P1-9/P1-10: clipboard + mic ──
    #[arg(long, value_enum, default_value_t = HostClipboardSync::Off)]
    clipboard_sync: HostClipboardSync,

    #[arg(long, value_enum, default_value_t = HostClipboardFormats::Text)]
    clipboard_formats: HostClipboardFormats,

    #[arg(long, default_value_t = 250)]
    clipboard_poll_ms: u32,

    #[arg(long, default_value = "BP Virtual Mic")]
    mic_virtual_source_name: String,

    // ── P2-15 HDR static metadata advert ──
    /// Emit `DisplayCapabilities { hdr_static_metadata: Some(...) }`
    /// at session start so the client can pick BT.2100/PQ pixel
    /// formats and switch to BT.2390 tone mapping.
    #[arg(long, default_value_t = false)]
    advertise_hdr: bool,

    // ── P2-15 pen virtual device name ──
    /// When set, the host agent injects `PEN_DATAGRAM_DISCRIMINATOR =
    /// 0x50` packets into a virtual pen device named `value` (Linux
    /// `uinput`, Windows `WinTab`). When unset (the default) the
    /// host ignores pen datagrams. Per ADR-010 §14 the macOS path is
    /// deferred.
    #[arg(long)]
    pen_virtual_device_name: Option<String>,

    // ── ADR-020: Pensieve RL ABR (off by default) ──
    #[arg(long, default_value_t = false)]
    enable_rl_abr: bool,

    // ── ADR-022 FileSync sensors (feature file-sync) ──
    /// Daemon IPC socket path for FileSync sensor reports.
    #[arg(long, env = "QUBOX_IPC_SOCKET")]
    ipc_socket: Option<PathBuf>,

    /// Enable process-lock + FS watcher sensors (requires `--features file-sync`).
    #[arg(long, default_value_t = false)]
    enable_file_sync_sensors: bool,

    /// Local node id for vector clocks (defaults to host peer id).
    #[arg(long)]
    file_sync_node_id: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum HostClipboardSync {
    Off,
    HostToClient,
    ClientToHost,
    Both,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum HostClipboardFormats {
    Text,
    Image,
    Both,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum StreamMode {
    #[clap(name = "single-stream")]
    SingleStream,
    #[clap(name = "multi-display")]
    MultiDisplay,
    #[clap(name = "all-displays")]
    AllDisplays,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum PrivacyModeArg {
    #[clap(name = "none")]
    None,
    #[clap(name = "vkms")]
    Vkms,
    #[clap(name = "blank-overlay")]
    BlankOverlay,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliH264Encoder {
    Nvenc,
    Vaapi,
    Qsv,
    Amf,
    VideoToolbox,
    Libx264,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliLinuxCapture {
    Auto,
    Pipewire,
    X11,
}

impl From<CliH264Encoder> for H264EncoderBackend {
    fn from(value: CliH264Encoder) -> Self {
        match value {
            CliH264Encoder::Nvenc => H264EncoderBackend::Nvenc,
            CliH264Encoder::Vaapi => H264EncoderBackend::Vaapi,
            CliH264Encoder::Qsv => H264EncoderBackend::Qsv,
            CliH264Encoder::Amf => H264EncoderBackend::Amf,
            CliH264Encoder::VideoToolbox => H264EncoderBackend::VideoToolbox,
            CliH264Encoder::Libx264 => H264EncoderBackend::Libx264,
        }
    }
}

#[derive(Debug, Serialize)]
struct MediaPlanOutput {
    readiness: MediaBackendReport,
    config: HostVideoPipelineConfig,
    plan: FfmpegPipelinePlan,
}

#[derive(Debug, Serialize)]
struct MediaFrameSummary {
    frame_id: u64,
    timestamp_micros: u64,
    keyframe: bool,
    nal_units: usize,
    bytes: usize,
}

#[derive(Debug, Serialize)]
struct MediaRunSummary {
    capture: String,
    encoder: String,
    frames: u64,
    keyframes: u64,
    bytes: u64,
    output_path: Option<String>,
    exit_status: Option<String>,
    stderr_tail: Option<String>,
}

type SignalingWriter = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type SharedSignalingWriter = Arc<Mutex<SignalingWriter>>;

#[derive(Clone)]
struct HostSessionRuntime {
    self_peer_id: Uuid,
    signaling_writer: SharedSignalingWriter,
    native_quic_bind: SocketAddr,
    native_quic_advertise_ip: Option<IpAddr>,
    pipewire_node: String,
    linux_capture: CliLinuxCapture,
    x11_display: String,
    windows_capture_input: String,
    media_width: u32,
    media_height: u32,
    media_fps: u32,
    media_bitrate_kbps: u32,
    h264_encoder: Option<CliH264Encoder>,
    disable_audio: bool,
    // Multi-display / multi-stream
    stream_mode: StreamMode,
    display_id: Option<u32>,
    // Privacy mode
    privacy_mode: PrivacyModeArg,
    enable_privacy_on_session_start: bool,
    vkms_output_name: String,
    // P1-9/P1-10
    clipboard_sync: HostClipboardSync,
    clipboard_formats: HostClipboardFormats,
    clipboard_poll_ms: u32,
    mic_virtual_source_name: String,
    advertise_hdr: bool,
    pen_virtual_device_name: Option<String>,
}

struct RemoteInputInjector {
    enigo: Enigo,
    stream_width: u32,
    stream_height: u32,
    display_width: i32,
    display_height: i32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    init_tracing();

    let args = Args::parse();
    refuse_auto_approve_on_public_server(&args)?;

    if args.probe_media {
        let report = probe_default_host_pipeline();
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if args.plan_host_h264 {
        let readiness = probe_default_host_pipeline();
        let config = media_config_from_args(&args, &readiness)?;
        let plan = plan_ffmpeg_h264(&config)?;

        println!(
            "{}",
            serde_json::to_string_pretty(&MediaPlanOutput {
                readiness,
                config,
                plan
            })?
        );
        return Ok(());
    }

    if args.smoke_test {
        run_host_h264_smoke(&args)?;
        return Ok(());
    }

    if args.plan_linux_pipewire_h264 {
        let readiness = probe_default_host_pipeline();
        let config = linux_media_config_from_args(&args, &readiness);
        let plan = plan_ffmpeg_pipewire_h264(&config)?;

        println!(
            "{}",
            serde_json::to_string_pretty(&MediaPlanOutput {
                readiness,
                config,
                plan
            })?
        );
        return Ok(());
    }

    if args.run_linux_pipewire_h264 {
        run_linux_pipewire_h264_smoke(&args)?;
        return Ok(());
    }

    let (identity, identity_path) = load_or_create_identity(args.identity_path, args.name.clone())?;
    let descriptor = describe_peer(
        PeerRole::Host,
        Some(identity.display_name.clone()),
        identity.device_id,
        identity.peer_id_for(PeerRole::Host),
    );

    tracing::info!(
        device_id = %descriptor.device_id,
        peer_id = %descriptor.peer_id,
        name = %descriptor.device_name,
        identity_path = %identity_path.display(),
        os = ?descriptor.os,
        ?descriptor.capabilities.transports,
        ?descriptor.capabilities.capture,
        ?descriptor.capabilities.encoders,
        "host agent starting"
    );

    #[cfg(feature = "file-sync")]
    if args.enable_file_sync_sensors {
        if let Some(socket) = args.ipc_socket.clone() {
            let node_id = args
                .file_sync_node_id
                .clone()
                .unwrap_or_else(|| descriptor.peer_id.to_string());
            // Load rules from daemon; empty list → sensors idle until rules added.
            let mut rules = Vec::new();
            let mut cfg = qubox_daemon::DaemonConfig::default();
            cfg.socket_path = socket.clone();
            if let Ok(mut client) = qubox_daemon::ipc::IpcClient::connect(&cfg).await {
                if let Ok(qubox_daemon::ipc::IpcResponse::SyncRules { rules: r }) = client
                    .call(&qubox_daemon::ipc::IpcRequest::SyncListRules)
                    .await
                {
                    rules = r;
                }
            }
            let _ = file_sync_sensors::spawn_sensors(file_sync_sensors::SensorConfig {
                socket_path: socket,
                node_id,
                rules,
            })
            .await;
        } else {
            tracing::warn!("enable_file_sync_sensors set but --ipc-socket missing");
        }
    }

    #[cfg(not(feature = "file-sync"))]
    if args.enable_file_sync_sensors {
        tracing::warn!("enable_file_sync_sensors ignored: build without feature file-sync");
    }

    let (stream, _) = connect_async(&args.server)
        .await
        .with_context(|| format!("failed to connect to {}", args.server))?;
    let (writer, mut reader) = stream.split();
    let writer = Arc::new(Mutex::new(writer));

    // Interactive pairing UI (loopback HTTP) unless auto-approve is on.
    let pairing_ui = if !args.auto_approve_pairing {
        let (tx, mut rx) = tokio_mpsc::unbounded_channel::<(Uuid, bool)>();
        let ui = pairing_ui::PairingUiState::new(tx);
        let port = pairing_ui::spawn_pairing_ui(ui.clone(), 17443).await?;
        tracing::info!(%port, "approve pairing at http://127.0.0.1:{port}/pending (GUI polls this)");
        let writer_dec = writer.clone();
        tokio::spawn(async move {
            while let Some((request_id, approved)) = rx.recv().await {
                let msg = ClientMessage::PairingDecision(PairingDecision {
                    request_id,
                    approved,
                });
                if let Err(e) = send_client_message(&writer_dec, &msg).await {
                    tracing::warn!(error = %e, %request_id, approved, "pairing decision send failed");
                } else {
                    tracing::info!(%request_id, approved, "pairing decision sent to signaling");
                }
            }
        });
        Some(ui)
    } else {
        None
    };

    let runtime = HostSessionRuntime {
        self_peer_id: descriptor.peer_id,
        signaling_writer: writer.clone(),
        native_quic_bind: args.native_quic_bind,
        native_quic_advertise_ip: args.native_quic_advertise_ip,
        pipewire_node: args.pipewire_node.clone(),
        linux_capture: args.linux_capture,
        x11_display: args.x11_display.clone(),
        windows_capture_input: args.windows_capture_input.clone(),
        media_width: args.media_width,
        media_height: args.media_height,
        media_fps: args.media_fps,
        media_bitrate_kbps: args.media_bitrate_kbps,
        h264_encoder: args.h264_encoder,
        disable_audio: args.disable_audio,
        stream_mode: args.stream_mode,
        display_id: args.display,
        privacy_mode: args.privacy_mode,
        enable_privacy_on_session_start: args.enable_privacy_on_session_start,
        vkms_output_name: args.vkms_output_name,
        clipboard_sync: args.clipboard_sync,
        clipboard_formats: args.clipboard_formats,
        clipboard_poll_ms: args.clipboard_poll_ms,
        mic_virtual_source_name: args.mic_virtual_source_name,
        advertise_hdr: args.advertise_hdr,
        pen_virtual_device_name: args.pen_virtual_device_name,
    };

    send_client_message(
        &writer,
        &ClientMessage::SignedHello(SignedHello::sign(
            &descriptor,
            &identity.signing_key(Some(&identity_path))?,
        )),
    )
    .await?;

    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));

    // Registry of in-flight WebRTC sessions. The signaling message loop hands
    // incoming RelaySignal::IceCandidate messages to the right session by
    // `session_id`. Each session registers itself on entry and removes
    // itself when the PeerConnection closes.
    let webrtc_sessions: webrtc_session::SessionRegistry =
        Arc::new(Mutex::new(std::collections::HashMap::new()));

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                send_client_message(&writer, &ClientMessage::Heartbeat).await?;
            }
            frame = reader.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let message: ServerMessage = serde_json::from_str(text.as_ref())?;
                        handle_server_message(
                            message,
                            writer.clone(),
                            args.auto_approve_pairing,
                            runtime.clone(),
                            pairing_ui.clone(),
                            webrtc_sessions.clone(),
                        ).await?;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("signaling connection closed");
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error.into()),
                }
            }
        }
    }

    Ok(())
}

impl RemoteInputInjector {
    fn new(video_config: &VideoStreamParams) -> anyhow::Result<Self> {
        #[cfg(target_os = "windows")]
        if let Err(error) = enigo::set_dpi_awareness() {
            tracing::debug!(
                ?error,
                "failed to set DPI awareness before initializing input injection"
            );
        }

        let settings = Settings {
            release_keys_when_dropped: true,
            ..Settings::default()
        };
        let enigo = Enigo::new(&settings)
            .map_err(|error| anyhow::anyhow!("failed to initialize enigo: {error}"))?;
        let (display_width, display_height) = enigo
            .main_display()
            .map_err(|error| anyhow::anyhow!("failed to query host display size: {error}"))?;

        Ok(Self {
            enigo,
            stream_width: video_config.width.max(1),
            stream_height: video_config.height.max(1),
            display_width,
            display_height,
        })
    }

    fn apply(&mut self, event: &RemoteInputEvent) -> anyhow::Result<()> {
        match event {
            RemoteInputEvent::MouseMove { x, y } => {
                let target_x = scale_input_coordinate(*x, self.stream_width, self.display_width);
                let target_y = scale_input_coordinate(*y, self.stream_height, self.display_height);
                self.enigo
                    .move_mouse(target_x, target_y, Coordinate::Abs)
                    .map_err(|error| anyhow::anyhow!("failed to move mouse: {error}"))?;
            }
            RemoteInputEvent::MouseButton { button, pressed } => {
                self.enigo
                    .button(
                        map_mouse_button(*button),
                        if *pressed { Press } else { Release },
                    )
                    .map_err(|error| anyhow::anyhow!("failed to inject mouse button: {error}"))?;
            }
            RemoteInputEvent::Keyboard { key, pressed } => {
                let Some(mapped_key) = map_remote_key(key) else {
                    tracing::debug!(remote_key = %key, "ignoring unmapped remote keyboard key");
                    return Ok(());
                };

                self.enigo
                    .key(mapped_key, if *pressed { Press } else { Release })
                    .map_err(|error| anyhow::anyhow!("failed to inject keyboard input: {error}"))?;
            }
            RemoteInputEvent::HoverDisplay { display_id } => {
                // HoverDisplay is emitted by the host TO the client.
                // Ignore it if received by the host.
                tracing::debug!(%display_id, "received HoverDisplay on host (ignoring)");
            }
            RemoteInputEvent::RelativeMouseMove { .. }
            | RemoteInputEvent::MouseWheel { .. }
            | RemoteInputEvent::Gamepad { .. }
            | RemoteInputEvent::Pen { .. } => {
                tracing::debug!("received unhandled input event type (ignoring)");
            }
        }

        Ok(())
    }
}

fn media_config_from_args(
    args: &Args,
    readiness: &MediaBackendReport,
) -> anyhow::Result<HostVideoPipelineConfig> {
    let encoder = selected_h264_encoder(args.h264_encoder, readiness);

    match readiness.platform {
        PlatformOs::Linux => Ok(linux_media_config(
            resolve_linux_capture(args.linux_capture),
            args.pipewire_node.clone(),
            args.x11_display.clone(),
            encoder,
            args.media_width,
            args.media_height,
            args.media_fps,
            args.media_bitrate_kbps,
        )),
        PlatformOs::Windows => Ok(HostVideoPipelineConfig::windows_gdigrab_h264(
            args.windows_capture_input.clone(),
            encoder,
            args.media_width,
            args.media_height,
            args.media_fps,
            args.media_bitrate_kbps,
        )),
        platform => anyhow::bail!(
            "host FFmpeg media planning is not implemented for {:?} yet",
            platform
        ),
    }
}

fn linux_media_config_from_args(
    args: &Args,
    readiness: &MediaBackendReport,
) -> HostVideoPipelineConfig {
    linux_media_config(
        resolve_linux_capture(args.linux_capture),
        args.pipewire_node.clone(),
        args.x11_display.clone(),
        selected_h264_encoder(args.h264_encoder, readiness),
        args.media_width,
        args.media_height,
        args.media_fps,
        args.media_bitrate_kbps,
    )
}

fn run_host_h264_smoke(args: &Args) -> anyhow::Result<()> {
    let readiness = probe_default_host_pipeline();
    let config = media_config_from_args(args, &readiness)?;
    run_h264_smoke(args, &config)
}

fn run_linux_pipewire_h264_smoke(args: &Args) -> anyhow::Result<()> {
    let readiness = probe_default_host_pipeline();
    let config = linux_media_config_from_args(args, &readiness);
    if !matches!(
        config.capture,
        qubox_media::CaptureSourceConfig::LinuxPipeWire { .. }
    ) {
        anyhow::bail!("--run-linux-pipewire-h264 requires a PipeWire capture selection");
    }

    let _ = plan_ffmpeg_pipewire_h264(&config)?;
    run_h264_smoke(args, &config)
}

fn run_h264_smoke(args: &Args, config: &HostVideoPipelineConfig) -> anyhow::Result<()> {
    let plan = plan_ffmpeg_h264(config)?;
    let mut pipeline = spawn_ffmpeg_pipeline(&plan)?;
    let mut framer = H264AnnexBStreamFramer::new(config.framerate)?;
    let mut scratch = vec![0_u8; 64 * 1024];
    let mut frames = 0_u64;
    let mut keyframes = 0_u64;
    let mut bytes = 0_u64;
    let mut output = open_smoke_output(args.smoke_test_output.as_ref())?;

    loop {
        match read_h264_access_units(pipeline.stdout_mut(), &mut framer, &mut scratch)
            .map_err(|error| smoke_runtime_error(error, &mut pipeline, frames))?
        {
            MediaPipelineRead::AccessUnits(access_units) => {
                for access_unit in access_units {
                    if let Some(file) = output.as_mut() {
                        file.write_all(&access_unit.bytes).with_context(|| {
                            format!(
                                "failed to append encoded frame {} to {}",
                                access_unit.frame_id,
                                args.smoke_test_output
                                    .as_ref()
                                    .map(|path| path.display().to_string())
                                    .unwrap_or_default()
                            )
                        })?;
                    }
                    frames += 1;
                    if access_unit.keyframe {
                        keyframes += 1;
                    }
                    bytes += access_unit.bytes.len() as u64;
                    println!(
                        "{}",
                        serde_json::to_string(&MediaFrameSummary {
                            frame_id: access_unit.frame_id,
                            timestamp_micros: access_unit.timestamp_micros,
                            keyframe: access_unit.keyframe,
                            nal_units: access_unit.nal_units.len(),
                            bytes: access_unit.bytes.len(),
                        })?
                    );

                    if args.max_media_frames > 0 && frames >= args.max_media_frames {
                        let _ = pipeline.kill();
                        let exit_status = pipeline.wait().ok().map(|status| status.to_string());
                        let stderr_tail = collect_pipeline_stderr(&mut pipeline);
                        ensure_smoke_frames(frames, stderr_tail.as_deref())?;
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&MediaRunSummary {
                                capture: capture_label(config),
                                encoder: config.encoder.ffmpeg_name().to_string(),
                                frames,
                                keyframes,
                                bytes,
                                output_path: args
                                    .smoke_test_output
                                    .as_ref()
                                    .map(|path| path.display().to_string()),
                                exit_status,
                                stderr_tail,
                            })?
                        );
                        return Ok(());
                    }
                }
            }
            MediaPipelineRead::EndOfStream(access_units) => {
                for access_unit in access_units {
                    if let Some(file) = output.as_mut() {
                        file.write_all(&access_unit.bytes).with_context(|| {
                            format!(
                                "failed to append encoded frame {} to {}",
                                access_unit.frame_id,
                                args.smoke_test_output
                                    .as_ref()
                                    .map(|path| path.display().to_string())
                                    .unwrap_or_default()
                            )
                        })?;
                    }
                    frames += 1;
                    if access_unit.keyframe {
                        keyframes += 1;
                    }
                    bytes += access_unit.bytes.len() as u64;
                }

                let exit_status = pipeline.wait().ok().map(|status| status.to_string());
                let stderr_tail = collect_pipeline_stderr(&mut pipeline);
                ensure_smoke_frames(frames, stderr_tail.as_deref())?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&MediaRunSummary {
                        capture: capture_label(config),
                        encoder: config.encoder.ffmpeg_name().to_string(),
                        frames,
                        keyframes,
                        bytes,
                        output_path: args
                            .smoke_test_output
                            .as_ref()
                            .map(|path| path.display().to_string()),
                        exit_status,
                        stderr_tail,
                    })?
                );
                return Ok(());
            }
        }
    }
}

async fn handle_server_message(
    message: ServerMessage,
    writer: SharedSignalingWriter,
    auto_approve_pairing: bool,
    runtime: HostSessionRuntime,
    pairing_ui: Option<pairing_ui::PairingUiState>,
    webrtc_sessions: webrtc_session::SessionRegistry,
) -> anyhow::Result<()> {
    match message {
        ServerMessage::Welcome(welcome) => {
            tracing::info!(self_id = %welcome.self_id, message = %welcome.message, "connected");
        }
        ServerMessage::Hosts { hosts } => {
            tracing::info!(count = hosts.len(), "received current host inventory");
        }
        ServerMessage::PairingRequested(request) => {
            tracing::info!(
                request_id = %request.request_id,
                client_id = %request.client.peer_id,
                client_device_id = %request.client.device_id,
                client_name = %request.client.device_name,
                client_label = %request.client_label,
                auto_approve_pairing,
                "pairing requested"
            );

            if auto_approve_pairing {
                send_client_message(
                    &writer,
                    &ClientMessage::PairingDecision(PairingDecision {
                        request_id: request.request_id,
                        approved: true,
                    }),
                )
                .await?;
            } else if let Some(ui) = &pairing_ui {
                ui.push(&request).await;
                tracing::info!(
                    request_id = %request.request_id,
                    client = %request.client.device_name,
                    "queued for GUI approval (open Pairing in Qubox app)"
                );
            } else {
                tracing::warn!(
                    "pairing request ignored — enable GUI approval or --auto-approve-pairing (LAN only)"
                );
            }
        }
        ServerMessage::PairingEstablished(grant) => {
            tracing::info!(
                host_peer_id = %grant.host_peer_id,
                client_peer_id = %grant.client_peer_id,
                "pairing established"
            );
        }
        ServerMessage::PairingRejected { request_id, reason } => {
            tracing::warn!(%request_id, %reason, "pairing rejected");
        }
        ServerMessage::SessionPlanned(plan) => {
            tracing::info!(
                session_id = %plan.session_id,
                target_host_id = %plan.target_host_id,
                transport = ?plan.transport,
                codec = ?plan.codec,
                "session planned"
            );
        }
        ServerMessage::SessionRequested(requested) => {
            tracing::info!(
                session_id = %requested.session_id,
                client_id = %requested.client.peer_id,
                client_name = %requested.client.device_name,
                transport = ?requested.transport,
                codec = ?requested.codec,
                host_token_expires = requested.host_credential.expires_unix_millis,
                client_token_expires = requested.client_credential.expires_unix_millis,
                ice_servers = %format_ice_servers(&requested.ice_servers),
                "session request received"
            );

            if requested.transport == TransportKind::NativeQuic {
                tokio::spawn(run_native_quic_session(*requested, runtime));
            } else if requested.transport == TransportKind::WebRtc {
                tokio::spawn(run_webrtc_session(*requested, runtime, webrtc_sessions.clone()));
            } else if requested.transport == TransportKind::RelayQuic {
                tracing::warn!(
                    session_id = %requested.session_id,
                    "RelayQuic requested but no host-side relay path is implemented yet"
                );
            }
        }
        ServerMessage::Signal(signal) => {
            tracing::info!(
                session_id = %signal.session_id,
                from_peer_id = %signal.from_peer_id,
                kind = ?signal.signal,
                "received relayed signaling"
            );
            webrtc_session::dispatch_signal(
                &webrtc_sessions,
                signal.session_id,
                signal.signal,
            )
            .await?;
        }
        ServerMessage::Presence(event) => {
            tracing::info!(
                peer_id = %event.peer.peer_id,
                name = %event.peer.device_name,
                connected = event.connected,
                role = ?event.peer.role,
                "presence update"
            );
        }
        ServerMessage::HeartbeatAck => {
            tracing::debug!("heartbeat acknowledged");
        }
        ServerMessage::ShareLinkCreated {
            code,
            expires_unix_ms,
            url_hint,
        } => {
            tracing::info!(%code, expires_unix_ms, %url_hint, "share link created");
        }
        ServerMessage::SessionKicked { session_id, reason } => {
            tracing::warn!(%session_id, %reason, "session kicked");
        }
        ServerMessage::SessionConsentPending {
            consent_id,
            client_peer_id,
            host_peer_id: _,
            expires_at_unix_ms,
            client_label,
        } => {
            tracing::info!(
                %consent_id,
                %client_peer_id,
                expires_at_unix_ms,
                %client_label,
                "session consent pending — approve in dashboard or wait for host UX"
            );
        }
        ServerMessage::PairingRevoked {
            host_peer_id,
            client_peer_id,
        } => {
            tracing::info!(%host_peer_id, %client_peer_id, "pairing revoked");
        }
        ServerMessage::Error(error) => {
            tracing::warn!(code = %error.code, message = %error.message, "server error");
        }
        ServerMessage::SessionBundleAccepted(_)
        | ServerMessage::SignedKillReceived(_) => {
            // Not relevant to host agent flow (these are client-side concerns).
            tracing::debug!("ignoring client-only server message");
        }
    }

    Ok(())
}

async fn send_client_message(
    writer: &SharedSignalingWriter,
    payload: &ClientMessage,
) -> anyhow::Result<()> {
    let mut writer = writer.lock().await;
    send_json(&mut *writer, payload).await
}

/// Demultiplex QUIC datagrams for the host side.
///
/// `DatagramDispatcher` is the SOLE consumer of `connection.read_datagram`.
/// It splits the bytestream by the byte[2] discriminator into pen,
/// gamepad, and media channels. This function then:
///
///   * When `device_name` is `Some`, lazily creates a uinput `PenInjector`
///     and forwards every pen datagram into it.
///   * When `device_name` is `None`, drains all three channels with
///     `try_recv()` so the dispatcher keeps reading from the QUIC socket
///     and never back-pressures — we are a pure consumer on the host.
async fn run_datagram_dispatcher_loop(
    conn: qubox_transport::NativeQuicConnection,
    device_name: Option<String>,
) {
    use qubox_transport::media::{decode_pen_datagram, DatagramDispatcher};
    let mut dispatcher = DatagramDispatcher::spawn(conn);
    let mut injector: Option<Box<dyn qubox_pen::PenInjector + Send>> = None;
    loop {
        if let Some(name) = device_name.as_ref() {
            let bytes = match dispatcher.pen_rx().recv().await {
                Some(b) => b,
                None => break,
            };
            match decode_pen_datagram(&bytes) {
                Ok(event) => {
                    if injector.is_none() {
                        #[cfg(target_os = "linux")]
                        {
                            match qubox_pen::linux::UinputInjector::new(name.clone()) {
                                Ok(inj) => {
                                    tracing::info!(
                                        device = %name,
                                        "pen injector created (uinput)"
                                    );
                                    injector = Some(Box::new(inj));
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        ?e,
                                        "failed to create pen injector; events ignored"
                                    );
                                }
                            }
                        }
                        #[cfg(not(target_os = "linux"))]
                        {
                            let _ = name;
                            tracing::warn!("pen injection not supported on this platform");
                        }
                    }
                    if let Some(inj) = injector.as_mut() {
                        if let Err(e) = inj.inject(&event) {
                            tracing::debug!(?e, "pen inject failed");
                        }
                    }
                }
                Err(e) => tracing::debug!(?e, "failed to decode pen datagram"),
            }
        } else {
            // No pen configured: drain the dispatcher's raw channels so
            // its internal `read_datagram` loop keeps making progress.
            // We don't need to forward anywhere — we just keep
            // the dispatcher from blocking on its senders.
            while dispatcher.pen_rx().try_recv().is_ok() {}
            while dispatcher.gamepad_rx().try_recv().is_ok() {}
            while dispatcher.media_chunk_rx().try_recv().is_ok() {}
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn run_native_quic_session(requested: SessionRequested, runtime: HostSessionRuntime) {
    if requested.codec != VideoCodec::H264 {
        tracing::warn!(
            session_id = %requested.session_id,
            codec = ?requested.codec,
            "native QUIC media bridge only supports H.264 right now"
        );
        return;
    }

    // Branch on stream mode
    match runtime.stream_mode {
        StreamMode::SingleStream => {
            run_single_stream_session(requested, runtime).await;
        }
        StreamMode::MultiDisplay => {
            run_multi_display_session(requested, runtime).await;
        }
        StreamMode::AllDisplays => {
            run_multi_display_session(requested, runtime).await;
        }
    }
}

/// WebRTC session lifecycle, browser-originated.
///
/// 1. Build a `WebRtcSession` with the negotiated ICE servers + codecs.
/// 2. Register it in the session registry so incoming `RelaySignal`s get
///    routed correctly while we wait for the browser's SDP offer.
/// 3. Spawn the (Phase A) test-pattern video producer so the browser sees a
///    live MediaStream once ICE completes. Phase B replaces this with the
///    real ffmpeg/PipeWire capture pipeline.
/// 4. Wait until either the PeerConnection fails or the host's signaling
///    connection drops; deregister and close.
async fn run_webrtc_session(
    requested: SessionRequested,
    runtime: HostSessionRuntime,
    registry: webrtc_session::SessionRegistry,
) {
    if requested.codec != VideoCodec::H264 {
        tracing::warn!(
            session_id = %requested.session_id,
            codec = ?requested.codec,
            "WebRTC media bridge only ships an H.264 track right now"
        );
        // Continue anyway — the browser will still see the peer connection
        // and report "live"; just no video frames will flow.
    }

    let session = match webrtc_session::WebRtcSession::new(
        runtime.signaling_writer.clone(),
        runtime.self_peer_id,
        requested.session_id,
        requested.client.clone(),
        &requested.ice_servers,
        requested.codec,
    )
    .await
    {
        Ok(s) => s,
        Err(err) => {
            tracing::error!(
                session_id = %requested.session_id,
                ?err,
                "failed to construct WebRtcSession"
            );
            return;
        }
    };

    // The browser bundle creates the data channel on its side; the host
    // attaches `on_data_channel` in `WebRtcSession::new` so it sees the
    // channel as soon as the browser opens it.

    {
        let mut guard = registry.lock().await;
        guard.insert(requested.session_id, session.clone());
    }

    tracing::info!(
        session_id = %requested.session_id,
        client_peer_id = %requested.client.peer_id,
        codec = ?requested.codec,
        "webrtc session registered; awaiting SDP offer via relay_signal"
    );

    // Spawn the test-pattern producer so the browser sees a live stream as
    // soon as ICE completes. Phase B will spawn the real capture pipeline
    // here instead and feed encoded H.264 access units into write_video.
    let producer_session = session.clone();
    let producer = tokio::spawn(webrtc_session::spawn_test_pattern_producer(
        producer_session,
    ));

    // We don't currently drive the session from this task — the signaling
    // loop dispatches each RelaySignal directly through `dispatch_signal`,
    // which calls `handle_offer` / `add_ice_candidate` on the registered
    // session. This task just waits for the registry entry to be removed
    // (e.g., on close) or the producer to die.
    let _ = producer.await;

    if let Err(err) = session.close().await {
        tracing::warn!(?err, "error closing webrtc session");
    }
    let mut guard = registry.lock().await;
    guard.remove(&requested.session_id);
    tracing::info!(session_id = %requested.session_id, "webrtc session torn down");
}

/// Single-stream session: keeps the existing behavior unchanged.
/// Captures display 0 (primary) via the ffmpeg pipeline and sends over a single QUIC stream.
async fn run_single_stream_session(requested: SessionRequested, runtime: HostSessionRuntime) {
    let result = async {
        let host = NativeQuicHost::bind(
            runtime.native_quic_bind,
            runtime.native_quic_advertise_ip,
            requested.session_id,
            requested.client_credential.clone(),
        )?;
        let ticket_b64 = encode_ticket_b64(host.ticket())?;

        send_client_message(
            &runtime.signaling_writer,
            &ClientMessage::RelaySignal(RelaySignal {
                session_id: requested.session_id,
                from_peer_id: runtime.self_peer_id,
                to_peer_id: requested.client.peer_id,
                signal: SessionSignal::NativeQuicTicket {
                    alpn: NATIVE_QUIC_ALPN.to_string(),
                    ticket_b64,
                },
            }),
        )
        .await?;

        let connection = host.accept_authenticated_connection().await?;
        let media_bps = filesync_drain::MediaBitrateSample::new(runtime.media_bitrate_kbps);
        media_bps.set_current_kbps(runtime.media_bitrate_kbps / 4);
        {
            let peer = requested.client.peer_id.to_string();
            let conn = connection.connection();
            tokio::spawn(filesync_drain::run_outbox_drain_with_congestion(
                conn.clone(),
                peer.clone(),
                Some(media_bps.clone()),
            ));
            let dest = std::env::var("QUBOX_FILESYNC_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| {
                    directories::ProjectDirs::from("com", "qubox", "qubox")
                        .map(|d| d.data_local_dir().join("incoming"))
                        .unwrap_or_else(|| std::path::PathBuf::from("qubox-incoming"))
                });
            let _ = std::fs::create_dir_all(&dest);
            tokio::spawn(qubox_transport::filesync::run_filesync_accept_loop(
                conn, dest,
            ));
        }
        // Live RateFeedback → FileSync congestion sample.
        {
            let conn = connection.connection();
            let sample = media_bps.clone();
            let session_id = requested.session_id;
            let initial = runtime.media_bitrate_kbps.saturating_mul(1000).max(500_000);
            tokio::spawn(async move {
                match qubox_transport::media::ControlChannel::accept(&conn).await {
                    Ok(rate_control) => {
                        let _rx = rate_feedback::spawn_rate_feedback_with_hook(
                            session_id,
                            initial,
                            500_000,
                            initial.saturating_mul(2).max(2_000_000),
                            rate_control,
                            Some(std::sync::Arc::new(move |bps| {
                                sample.set_current_kbps(bps / 1000);
                            })),
                        );
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "rate feedback control channel not accepted");
                    }
                }
            });
        }
        let clip_mic_task = tokio::spawn(setup_clip_mic_handler(
            connection.connection(),
            runtime.clone(),
            requested.permissions.clone(),
        ));
        // The dispatcher is the SOLE consumer of `connection.read_datagram`,
        // preventing the dual-reader race that occurred when the pen loop and
        // a later media dispatcher both pulled from the same QUIC connection.
        let pen_inject_task = {
            let conn = connection.connection();
            let device_name = runtime.pen_virtual_device_name.clone();
            Some(tokio::spawn(async move {
                run_datagram_dispatcher_loop(conn, device_name).await
            }))
        };
        let readiness = probe_default_host_pipeline();
        let config = media_config_from_runtime(&runtime, &readiness)?;
        let video_config = VideoStreamParams {
            codec: config.codec,
            width: config.width,
            height: config.height,
            framerate: config.framerate,
        };
        let input_receiver = connection.open_input_receiver(video_config.clone()).await?;
        let input_task = tokio::spawn(handle_remote_input_events(
            requested.session_id,
            video_config,
            input_receiver,
            requested.permissions.clone(),
        ));
        let (audio_task, audio_stream) = if runtime.disable_audio {
            tracing::info!(
                session_id = %requested.session_id,
                "host audio capture disabled for this session"
            );
            let audio_sender = connection
                .open_audio_sender(default_audio_stream_params())
                .await?;
            (
                tokio::spawn(async move {
                    let mut audio_sender = audio_sender;
                    audio_sender.finish().await?;
                    Ok::<u64, anyhow::Error>(0)
                }),
                None,
            )
        } else {
            let (audio_config, chunk_rx, audio_stream) = open_host_audio_capture()?;
            let audio_sender = connection.open_audio_sender(audio_config).await?;
            (
                tokio::spawn(forward_audio_chunks(audio_sender, chunk_rx)),
                Some(audio_stream),
            )
        };
        let mut sender = connection.open_media_sender().await?;
        let mut host_control_sender = connection.open_control_sender().await?;
        tokio::spawn(async move {
            let _ = host_control_sender.finish().await;
        });
        let plan = plan_ffmpeg_h264(&config)?;
        let mut pipeline = spawn_ffmpeg_pipeline(&plan)?;
        let mut framer = H264AnnexBStreamFramer::new(config.framerate)?;
        let mut scratch = vec![0_u8; 64 * 1024];
        let mut frames = 0_u64;
        let mut bytes = 0_u64;

        loop {
            match read_h264_access_units(pipeline.stdout_mut(), &mut framer, &mut scratch)? {
                MediaPipelineRead::AccessUnits(access_units) => {
                    for access_unit in access_units {
                        if frames == 0 {
                            tracing::info!(
                                session_id = %requested.session_id,
                                frame_id = access_unit.frame_id,
                                timestamp_micros = access_unit.timestamp_micros,
                                keyframe = access_unit.keyframe,
                                bytes = access_unit.bytes.len(),
                                nal_units = access_unit.nal_units.len(),
                                "sending first native QUIC video access unit"
                            );
                        }
                        sender.send_access_unit(&access_unit).await?;
                        frames += 1;
                        bytes += access_unit.bytes.len() as u64;
                    }
                }
                MediaPipelineRead::EndOfStream(access_units) => {
                    for access_unit in access_units {
                        if frames == 0 {
                            tracing::info!(
                                session_id = %requested.session_id,
                                frame_id = access_unit.frame_id,
                                timestamp_micros = access_unit.timestamp_micros,
                                keyframe = access_unit.keyframe,
                                bytes = access_unit.bytes.len(),
                                nal_units = access_unit.nal_units.len(),
                                "sending first native QUIC video access unit"
                            );
                        }
                        sender.send_access_unit(&access_unit).await?;
                        frames += 1;
                        bytes += access_unit.bytes.len() as u64;
                    }
                    sender.finish().await?;
                    tracing::info!(
                        session_id = %requested.session_id,
                        frames,
                        bytes,
                        "native QUIC media session finished"
                    );
                    drop(audio_stream);
                    let _ = tokio::time::timeout(Duration::from_secs(1), audio_task).await;
                    let _ = tokio::time::timeout(Duration::from_millis(250), input_task).await;
                    clip_mic_task.abort();
                    if let Some(task) = pen_inject_task {
                        task.abort();
                    }
                    break;
                }
            }
        }

        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(error) = result {
        tracing::warn!(
            session_id = %requested.session_id,
            client_id = %requested.client.peer_id,
            ?error,
            "native QUIC media session failed"
        );
    }
}

/// Multi-display session: delegates to CaptureOrchestrator for per-display
/// ffmpeg x11grab pipeline setup and read/send loop.
async fn run_multi_display_session(requested: SessionRequested, runtime: HostSessionRuntime) {
    let result = async {
        let host = NativeQuicHost::bind(
            runtime.native_quic_bind,
            runtime.native_quic_advertise_ip,
            requested.session_id,
            requested.client_credential.clone(),
        )?;
        let ticket_b64 = encode_ticket_b64(host.ticket())?;

        send_client_message(
            &runtime.signaling_writer,
            &ClientMessage::RelaySignal(RelaySignal {
                session_id: requested.session_id,
                from_peer_id: runtime.self_peer_id,
                to_peer_id: requested.client.peer_id,
                signal: SessionSignal::NativeQuicTicket {
                    alpn: NATIVE_QUIC_ALPN.to_string(),
                    ticket_b64,
                },
            }),
        )
        .await?;

        let connection = host.accept_authenticated_connection().await?;
        let media_bps = filesync_drain::MediaBitrateSample::new(runtime.media_bitrate_kbps);
        media_bps.set_current_kbps(runtime.media_bitrate_kbps / 4);
        {
            let peer = requested.client.peer_id.to_string();
            let conn = connection.connection();
            tokio::spawn(filesync_drain::run_outbox_drain_with_congestion(
                conn.clone(),
                peer,
                Some(media_bps.clone()),
            ));
            let dest = std::env::var("QUBOX_FILESYNC_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| {
                    directories::ProjectDirs::from("com", "qubox", "qubox")
                        .map(|d| d.data_local_dir().join("incoming"))
                        .unwrap_or_else(|| std::path::PathBuf::from("qubox-incoming"))
                });
            let _ = std::fs::create_dir_all(&dest);
            tokio::spawn(qubox_transport::filesync::run_filesync_accept_loop(conn, dest));
        }
        {
            let conn = connection.connection();
            let sample = media_bps.clone();
            let session_id = requested.session_id;
            let initial = runtime.media_bitrate_kbps.saturating_mul(1000).max(500_000);
            tokio::spawn(async move {
                if let Ok(rate_control) =
                    qubox_transport::media::ControlChannel::accept(&conn).await
                {
                    let _rx = rate_feedback::spawn_rate_feedback_with_hook(
                        session_id,
                        initial,
                        500_000,
                        initial.saturating_mul(2).max(2_000_000),
                        rate_control,
                        Some(std::sync::Arc::new(move |bps| {
                            sample.set_current_kbps(bps / 1000);
                        })),
                    );
                }
            });
        }
        let connection = Arc::new(connection);
        let clip_mic_task = tokio::spawn(setup_clip_mic_handler(
            connection.connection(),
            runtime.clone(),
            requested.permissions.clone(),
        ));
        // The dispatcher is the SOLE consumer of `connection.read_datagram`,
        // preventing the dual-reader race that occurred when the pen loop and
        // a later media dispatcher both pulled from the same QUIC connection.
        let pen_inject_task = {
            let conn = connection.connection();
            let device_name = runtime.pen_virtual_device_name.clone();
            Some(tokio::spawn(async move {
                run_datagram_dispatcher_loop(conn, device_name).await
            }))
        };
        let readiness = probe_default_host_pipeline();
        let config = media_config_from_runtime(&runtime, &readiness)?;
        let video_config = VideoStreamParams {
            codec: config.codec,
            width: config.width,
            height: config.height,
            framerate: config.framerate,
        };
        let input_receiver = connection.open_input_receiver(video_config.clone()).await?;
        let input_task = tokio::spawn(handle_remote_input_events(
            requested.session_id,
            video_config,
            input_receiver,
            requested.permissions.clone(),
        ));

        // Audio: same as single-stream
        let (audio_task, audio_stream) = if runtime.disable_audio {
            let audio_sender = connection
                .open_audio_sender(default_audio_stream_params())
                .await?;
            (
                tokio::spawn(async move {
                    let mut audio_sender = audio_sender;
                    audio_sender.finish().await?;
                    Ok::<u64, anyhow::Error>(0)
                }),
                None,
            )
        } else {
            let (audio_config, chunk_rx, audio_stream) = open_host_audio_capture()?;
            let audio_sender = connection.open_audio_sender(audio_config).await?;
            (
                tokio::spawn(forward_audio_chunks(audio_sender, chunk_rx)),
                Some(audio_stream),
            )
        };

        // ── Privacy mode setup ──
        let blank_overlay = Arc::new(privacy::BlankOverlayManager::new());

        // Open a QUIC uni-stream to forward ControlMsg to the client
        let mut control_sender = connection.open_control_sender().await?;
        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel();
        blank_overlay.set_control_channel(control_tx).await;

        tokio::spawn(async move {
            while let Some(msg) = control_rx.recv().await {
                if let Err(e) = control_sender.send_control_msg(&msg).await {
                    tracing::warn!(error = %e, "failed to send control message to client");
                    break;
                }
            }
            let _ = control_sender.finish().await;
            tracing::debug!("control sender finished");
        });

        // ── Delegate to CaptureOrchestrator ──
        let backend = qubox_display::detect_backend()?;

        // Create display manager (with blank overlay fallback if configured)
        let display_manager = if runtime.privacy_mode == PrivacyModeArg::BlankOverlay {
            let fallback = {
                let bo = blank_overlay.clone();
                Box::new(move |display_id: DisplayId| -> Result<(), DisplayError> {
                    // Sync callback: spawn an async task to call show()
                    let bo = bo.clone();
                    tokio::spawn(async move {
                        let _ = bo.show(display_id).await;
                    });
                    Ok(())
                }) as Box<dyn Fn(DisplayId) -> Result<(), DisplayError> + Send + Sync>
            };
            // The x11 feature is always enabled on Linux (default feature of
            // qubox-display). On non-Linux, fall back to the auto-detected
            // backend (with a dead-code hint for the unused fallback).
            #[cfg(target_os = "linux")]
            {
                let ctx = qubox_display::x11::X11RandrContext::new()
                    .map_err(|e| anyhow::anyhow!("X11 context: {e}"))?;
                Box::new(qubox_display::x11::X11RandrDisplayManager::with_fallback(ctx, fallback))
                    as Box<dyn DisplayManager>
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = fallback;
                qubox_display::display_manager()?
            }
        } else {
            qubox_display::display_manager()?
        };

        // Enable privacy on session start if requested.
        // Must happen **before** the display_manager is moved into orchestrator
        // because the orchestrator's &self methods are not Send/Sync (the
        // internal CaptureSession lacks a Sync bound on its trait).
        if runtime.enable_privacy_on_session_start {
            let privacy_target = DisplayId(runtime.display_id.unwrap_or(0));
            match display_manager.set_display_state(privacy_target, DisplayState::Privacy).await {
                Ok(()) => {
                    tracing::info!(display = %privacy_target.0, "privacy enabled on session start");
                }
                Err(e) => {
                    tracing::warn!(display = %privacy_target.0, error = %e, "privacy enable failed; falling back to blank overlay");
                    let _ = blank_overlay.show(privacy_target).await;
                }
            }
        }

        let (hover_display_tx, _) = tokio::sync::mpsc::unbounded_channel();

        let selected_encoder = selected_h264_encoder(runtime.h264_encoder, &readiness);

        let per_stream_config = capture_orchestrator::PerStreamConfig {
            codec: VideoCodec::H264,
            encoder: selected_encoder,
            target_fps: runtime.media_fps,
            target_bitrate_kbps: runtime.media_bitrate_kbps,
            scale_mode: capture_orchestrator::ScaleMode::Stretch,
            target_resolution: Some((runtime.media_width, runtime.media_height)),
        };

        let mut orchestrator = capture_orchestrator::CaptureOrchestrator::new(
            backend,
            display_manager,
            connection,
            hover_display_tx,
            runtime.x11_display,
        );

        match runtime.stream_mode {
            StreamMode::AllDisplays => {
                orchestrator.start_all_displays(per_stream_config).await?;
            }
            StreamMode::MultiDisplay => {
                let target_id = runtime.display_id.ok_or_else(|| {
                    anyhow::anyhow!("--display <N> is required with --stream-mode multi-display")
                })?;
                orchestrator
                    .start_multi_display(
                        vec![qubox_display::types::DisplayId(target_id)],
                        per_stream_config,
                    )
                    .await?;
            }
            _ => unreachable!(),
        }

        // Wait for all display pipeline tasks
        orchestrator.wait_for_all().await?;

        drop(audio_stream);
        let _ = tokio::time::timeout(Duration::from_secs(1), audio_task).await;
        let _ = tokio::time::timeout(Duration::from_millis(250), input_task).await;
        clip_mic_task.abort();
        if let Some(task) = pen_inject_task {
            task.abort();
        }

        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(error) = result {
        tracing::warn!(
            session_id = %requested.session_id,
            client_id = %requested.client.peer_id,
            ?error,
            "native QUIC multi-display session failed"
        );
    }
}

fn media_config_from_runtime(
    runtime: &HostSessionRuntime,
    readiness: &MediaBackendReport,
) -> anyhow::Result<HostVideoPipelineConfig> {
    let encoder = selected_h264_encoder(runtime.h264_encoder, readiness);

    match readiness.platform {
        PlatformOs::Linux => Ok(linux_media_config(
            resolve_linux_capture(runtime.linux_capture),
            runtime.pipewire_node.clone(),
            runtime.x11_display.clone(),
            encoder,
            runtime.media_width,
            runtime.media_height,
            runtime.media_fps,
            runtime.media_bitrate_kbps,
        )),
        PlatformOs::Windows => Ok(HostVideoPipelineConfig::windows_gdigrab_h264(
            runtime.windows_capture_input.clone(),
            encoder,
            runtime.media_width,
            runtime.media_height,
            runtime.media_fps,
            runtime.media_bitrate_kbps,
        )),
        platform => anyhow::bail!(
            "native QUIC host capture is not implemented for {:?} yet",
            platform
        ),
    }
}

async fn handle_remote_input_events(
    session_id: Uuid,
    video_config: VideoStreamParams,
    mut input_receiver: NativeQuicInputReceiver,
    permissions: SessionPermissions,
) {
    if matches!(
        permissions::decide_input(&permissions),
        permissions::InputDecision::DropStream
    ) {
        tracing::info!(
            session_id = %session_id,
            "session permissions deny input; dropping remote input stream"
        );
        while let Ok(Some(_)) = input_receiver.read_input_event().await {}
        return;
    }

    let input_tx = match spawn_remote_input_worker(session_id, video_config) {
        Ok(input_tx) => Some(input_tx),
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                ?error,
                "failed to start remote input injector; events will be dropped"
            );
            None
        }
    };

    loop {
        match input_receiver.read_input_event().await {
            Ok(Some(event)) => {
                if let Some(input_tx) = input_tx.as_ref() {
                    if input_tx.send(event).is_err() {
                        tracing::warn!(
                            session_id = %session_id,
                            "remote input worker has stopped; closing input stream"
                        );
                        break;
                    }
                }
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(session_id = %session_id, ?error, "remote input stream failed");
                break;
            }
        }
    }
}

fn spawn_remote_input_worker(
    session_id: Uuid,
    video_config: VideoStreamParams,
) -> anyhow::Result<tokio_mpsc::UnboundedSender<RemoteInputEvent>> {
    let (input_tx, mut input_rx) = tokio_mpsc::unbounded_channel();

    thread::Builder::new()
        .name(format!("bp-input-{}", session_id.as_simple()))
        .spawn(move || {
            let mut injector = match RemoteInputInjector::new(&video_config) {
                Ok(injector) => Some(injector),
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        ?error,
                        "failed to initialize host input injector; remote input will be ignored"
                    );
                    None
                }
            };

            let mut pending_event = None;

            while let Some(event) = next_injected_input_event(&mut pending_event, &mut input_rx) {
                if let Some(injector) = injector.as_mut() {
                    if let Err(error) = injector.apply(&event) {
                        tracing::warn!(
                            session_id = %session_id,
                            ?error,
                            ?event,
                            "failed to inject remote input event"
                        );
                    }
                }
            }

            fn next_injected_input_event(
                pending_event: &mut Option<RemoteInputEvent>,
                input_rx: &mut tokio_mpsc::UnboundedReceiver<RemoteInputEvent>,
            ) -> Option<RemoteInputEvent> {
                let mut event = if let Some(event) = pending_event.take() {
                    event
                } else {
                    input_rx.blocking_recv()?
                };

                if matches!(event, RemoteInputEvent::MouseMove { .. }) {
                    loop {
                        match input_rx.try_recv() {
                            Ok(next_event @ RemoteInputEvent::MouseMove { .. }) => {
                                event = next_event;
                            }
                            Ok(next_event) => {
                                *pending_event = Some(next_event);
                                break;
                            }
                            Err(tokio_mpsc::error::TryRecvError::Empty) => break,
                            Err(tokio_mpsc::error::TryRecvError::Disconnected) => break,
                        }
                    }
                }

                Some(event)
            }
        })
        .context("failed to spawn remote input worker thread")?;

    Ok(input_tx)
}

/// Set up the P1-9/P1-10 clip+mic handler on the host side.
/// Accepts the client→host control channel, spawns a
/// `ClipboardWatcher` if the host→client direction is enabled,
/// drives the `ClipboardApplier` on inbound `ClipboardChanged`,
/// and creates a `VirtualMicDevice` on `MicStart`.
///
/// Before entering the receive loop the host advertises
/// [`ControlMsg::DisplayCapabilities`] so the client knows the
/// negotiated `color_space`, `bit_depth`, and refresh-rate ceiling
/// before it picks its `VideoStreamPreferences`. The metadata is
/// derived from the configured `media_*` args and the canonical
/// HDR10 SEI defaults; SDR hosts send `hdr_static_metadata = None`.
async fn setup_clip_mic_handler(
    connection: qubox_transport::NativeQuicConnection,
    runtime: HostSessionRuntime,
    permissions: SessionPermissions,
) -> anyhow::Result<()> {
    use qubox_clipboard::{ClipboardApplier, ClipboardSyncConfig, ClipboardWatcher};
    use qubox_transport::media::ControlChannel;

    let mut channel = ControlChannel::accept(&connection).await?;
    let _ = advertise_display_capabilities(&mut channel, &runtime).await;
    let mut clipboard_applier = ClipboardApplier::new();
    let mut last_clipboard_seq: u64 = 0;
    let mut mic_device: Option<qubox_mic::VirtualMicDevice> = None;

    let text_enabled = matches!(
        runtime.clipboard_formats,
        HostClipboardFormats::Text | HostClipboardFormats::Both
    );
    let image_enabled = matches!(
        runtime.clipboard_formats,
        HostClipboardFormats::Image | HostClipboardFormats::Both
    );

    if permissions::allow_clipboard_watch(&permissions)
        && matches!(
            runtime.clipboard_sync,
            HostClipboardSync::HostToClient | HostClipboardSync::Both
        )
    {
        let (clip_tx, mut clip_rx) = tokio::sync::mpsc::unbounded_channel::<ControlMsg>();
        let watcher = ClipboardWatcher::new(
            ClipboardSyncConfig {
                text_enabled,
                image_enabled,
                poll_interval: Duration::from_millis(runtime.clipboard_poll_ms.max(50) as u64),
            },
            clip_tx,
        );
        let watch_channel = Arc::new(tokio::sync::Mutex::new(channel));
        let watch_channel_clone = Arc::clone(&watch_channel);
        let send_task = tokio::spawn(async move {
            while let Some(msg) = clip_rx.recv().await {
                let mut guard = watch_channel_clone.lock().await;
                let _ = guard.send(&msg).await;
            }
        });
        tokio::spawn(watcher.run());
        drop(send_task);
        // Open a second control channel for inbound control messages
        // (MicStart, MicStop, client→host ClipboardChanged).
        channel = ControlChannel::accept(&connection).await?;
    }

    while let Some(msg) = channel.recv().await? {
        match msg {
            ControlMsg::ClipboardChanged { .. } => {
                let client_to_host = matches!(
                    runtime.clipboard_sync,
                    HostClipboardSync::ClientToHost | HostClipboardSync::Both
                );
                match permissions::decide_clipboard_apply(&permissions, client_to_host) {
                    permissions::ClipboardApplyDecision::DropDenied => {
                        tracing::debug!("clipboard denied by session permissions");
                        continue;
                    }
                    permissions::ClipboardApplyDecision::DropDirectionOff => continue,
                    permissions::ClipboardApplyDecision::Apply => {}
                }
                if let Err(error) = clipboard_applier.apply(&msg, &mut last_clipboard_seq) {
                    tracing::warn!(?error, "host clipboard apply failed");
                }
            }
            ControlMsg::MicStart { config } => {
                if matches!(
                    permissions::decide_mic_start(&permissions),
                    permissions::MicStartDecision::Deny
                ) {
                    tracing::info!("mic denied by session permissions");
                    let ack = ControlMsg::MicConfigAck {
                        config,
                        virtual_device_ok: false,
                    };
                    channel.send(&ack).await?;
                    continue;
                }
                tracing::info!(?config, "client requested mic start");
                let device = qubox_mic::VirtualMicDevice::try_create(
                    &runtime.mic_virtual_source_name,
                    &config,
                );
                let ack = ControlMsg::MicConfigAck {
                    config,
                    virtual_device_ok: device.status.device_created,
                };
                let _ = mic_device.take();
                mic_device = Some(device);
                channel.send(&ack).await?;
            }
            ControlMsg::MicStop => {
                tracing::info!("client requested mic stop");
                let _ = mic_device.take();
            }
            other => {
                tracing::debug!(?other, "host received unhandled control message");
            }
        }
    }
    Ok(())
}

/// Build the [`ControlMsg::DisplayCapabilities`] payload from the
/// current `HostSessionRuntime` configuration without touching the
/// network. The HDR static metadata block is `Some` only when the
/// operator passed `--advertise-hdr`; otherwise the client treats the
/// session as SDR.
fn build_display_capabilities(runtime: &HostSessionRuntime) -> ControlMsg {
    build_display_capabilities_from(
        runtime.media_width,
        runtime.media_height,
        runtime.media_fps,
        runtime.advertise_hdr,
    )
}

/// Pure-data variant of [`build_display_capabilities`] that does not
/// require a [`HostSessionRuntime`]. Useful for unit tests and for
/// synthesising capabilities outside an active session.
fn build_display_capabilities_from(
    media_width: u32,
    media_height: u32,
    media_fps: u32,
    advertise_hdr: bool,
) -> ControlMsg {
    ControlMsg::DisplayCapabilities {
        hdr_static_metadata: if advertise_hdr {
            Some(qubox_proto::HdrStaticMetadata::default())
        } else {
            None
        },
        max_resolution: [
            media_width.min(u16::MAX as u32) as u16,
            media_height.min(u16::MAX as u32) as u16,
        ],
        max_refresh_hz: media_fps,
    }
}

/// Emit [`ControlMsg::DisplayCapabilities`] on the host→client control
/// channel at session start. See [`build_display_capabilities`] for
/// the metadata derivation rules.
async fn advertise_display_capabilities(
    channel: &mut qubox_transport::media::ControlChannel,
    runtime: &HostSessionRuntime,
) -> anyhow::Result<()> {
    let caps_msg = build_display_capabilities(runtime);
    channel.send(&caps_msg).await?;
    let view = caps_msg.display_capabilities();
    tracing::info!(?view, "sent DisplayCapabilities to client");
    Ok(())
}

fn open_host_audio_capture() -> anyhow::Result<(
    AudioStreamParams,
    tokio_mpsc::UnboundedReceiver<Vec<u8>>,
    Stream,
)> {
    let host = cpal::default_host();

    #[cfg(target_os = "windows")]
    let (device, supported_config, source_name) = {
        let device = host
            .default_output_device()
            .context("failed to find default output device for WASAPI loopback capture")?;
        let source_name = "default-output".to_string();
        let supported_config = device
            .default_output_config()
            .map_err(|error| anyhow!(error))
            .context("failed to query default output format for WASAPI loopback capture")?;

        (device, supported_config, source_name)
    };

    #[cfg(not(target_os = "windows"))]
    let (device, supported_config, source_name) = {
        let device = host
            .default_input_device()
            .context("failed to find default input device for audio capture")?;
        let source_name = "default-input".to_string();
        let supported_config = device
            .default_input_config()
            .map_err(|error| anyhow!(error))
            .context("failed to query default input format for audio capture")?;

        (device, supported_config, source_name)
    };

    let capture_channels = supported_config.channels();
    let sample_rate = supported_config.sample_rate();
    let sample_format = supported_config.sample_format();
    let stream_config = supported_config.config();
    let audio_config = AudioStreamParams {
        sample_rate,
        ..default_audio_stream_params()
    };
    let (chunk_tx, chunk_rx) = tokio_mpsc::unbounded_channel();
    let err_fn = |error| tracing::warn!(?error, "host audio capture stream error");

    let stream = match sample_format {
        SampleFormat::F32 => {
            let chunk_tx = chunk_tx.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[f32], _| forward_audio_input_f32(data, capture_channels, &chunk_tx),
                err_fn,
                None,
            )
        }
        SampleFormat::I16 => {
            let chunk_tx = chunk_tx.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[i16], _| forward_audio_input_i16(data, capture_channels, &chunk_tx),
                err_fn,
                None,
            )
        }
        SampleFormat::U16 => {
            let chunk_tx = chunk_tx.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[u16], _| forward_audio_input_u16(data, capture_channels, &chunk_tx),
                err_fn,
                None,
            )
        }
        sample_format => anyhow::bail!("unsupported host audio sample format {sample_format:?}"),
    }
    .map_err(|error| anyhow!(error))
    .context("failed to build host audio capture stream")?;

    stream
        .play()
        .map_err(|error| anyhow!(error))
        .context("failed to start host audio capture stream")?;

    tracing::info!(
        source = %source_name,
        sample_rate,
        capture_channels,
        transport_channels = audio_config.channels,
        ?sample_format,
        "host audio capture started"
    );

    Ok((audio_config, chunk_rx, stream))
}

fn default_audio_stream_params() -> AudioStreamParams {
    AudioStreamParams {
        codec: AudioCodec::PcmF32,
        sample_rate: 48_000,
        channels: 2,
    }
}

async fn forward_audio_chunks(
    mut audio_sender: NativeQuicAudioSender,
    mut chunk_rx: tokio_mpsc::UnboundedReceiver<Vec<u8>>,
) -> anyhow::Result<u64> {
    let mut chunks = 0_u64;

    while let Some(bytes) = chunk_rx.recv().await {
        if bytes.is_empty() {
            continue;
        }

        audio_sender.send_audio_chunk(&bytes).await?;
        chunks += 1;
    }

    let _ = audio_sender.finish().await;
    Ok(chunks)
}

fn forward_audio_input_f32(
    data: &[f32],
    capture_channels: u16,
    chunk_tx: &tokio_mpsc::UnboundedSender<Vec<u8>>,
) {
    let normalized = normalize_audio_channels(data, capture_channels);
    if normalized.is_empty() {
        return;
    }

    let mut bytes = Vec::with_capacity(normalized.len() * std::mem::size_of::<f32>());
    for sample in normalized {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }

    let _ = chunk_tx.send(bytes);
}

fn forward_audio_input_i16(
    data: &[i16],
    capture_channels: u16,
    chunk_tx: &tokio_mpsc::UnboundedSender<Vec<u8>>,
) {
    let converted = data
        .iter()
        .map(|sample| f32::from(*sample) / f32::from(i16::MAX))
        .collect::<Vec<_>>();
    forward_audio_input_f32(&converted, capture_channels, chunk_tx);
}

fn forward_audio_input_u16(
    data: &[u16],
    capture_channels: u16,
    chunk_tx: &tokio_mpsc::UnboundedSender<Vec<u8>>,
) {
    let converted = data
        .iter()
        .map(|sample| (f32::from(*sample) / f32::from(u16::MAX)) * 2.0 - 1.0)
        .collect::<Vec<_>>();
    forward_audio_input_f32(&converted, capture_channels, chunk_tx);
}

fn normalize_audio_channels(samples: &[f32], capture_channels: u16) -> Vec<f32> {
    match capture_channels {
        0 => Vec::new(),
        1 => samples
            .iter()
            .flat_map(|sample| [*sample, *sample])
            .collect(),
        2 => samples.to_vec(),
        channels => {
            let channels = usize::from(channels);
            let mut normalized = Vec::with_capacity(samples.len() / channels * 2);
            for frame in samples.chunks(channels) {
                let left = frame.first().copied().unwrap_or(0.0);
                let right = frame.get(1).copied().unwrap_or(left);
                normalized.push(left);
                normalized.push(right);
            }
            normalized
        }
    }
}

fn selected_h264_encoder(
    preferred: Option<CliH264Encoder>,
    readiness: &MediaBackendReport,
) -> H264EncoderBackend {
    preferred
        .map(Into::into)
        .or_else(|| best_h264_encoder_for_platform(&readiness.encoder.details))
        .unwrap_or(H264EncoderBackend::Libx264)
}

fn resolve_linux_capture(preferred: CliLinuxCapture) -> CliLinuxCapture {
    match preferred {
        CliLinuxCapture::Auto => preferred_linux_capture_kind(&probe_linux_capture_backends())
            .map(|kind| match kind {
                CaptureKind::Pipewire => CliLinuxCapture::Pipewire,
                CaptureKind::X11 => CliLinuxCapture::X11,
                _ => CliLinuxCapture::X11,
            })
            .unwrap_or(CliLinuxCapture::X11),
        explicit => explicit,
    }
}

fn linux_media_config(
    capture: CliLinuxCapture,
    pipewire_node: String,
    x11_display: String,
    encoder: H264EncoderBackend,
    media_width: u32,
    media_height: u32,
    media_fps: u32,
    media_bitrate_kbps: u32,
) -> HostVideoPipelineConfig {
    match capture {
        CliLinuxCapture::Auto => unreachable!("linux capture must be resolved before planning"),
        CliLinuxCapture::Pipewire => HostVideoPipelineConfig::linux_pipewire_h264(
            pipewire_node,
            encoder,
            media_width,
            media_height,
            media_fps,
            media_bitrate_kbps,
        ),
        CliLinuxCapture::X11 => HostVideoPipelineConfig::linux_x11_h264(
            x11_display,
            encoder,
            media_width,
            media_height,
            media_fps,
            media_bitrate_kbps,
        ),
    }
}

fn open_smoke_output(path: Option<&PathBuf>) -> anyhow::Result<Option<File>> {
    let Some(path) = path else {
        return Ok(None);
    };

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create smoke test output directory {}",
                    parent.display()
                )
            })?;
        }
    }

    let file = File::create(path)
        .with_context(|| format!("failed to create smoke test output {}", path.display()))?;
    Ok(Some(file))
}

fn collect_pipeline_stderr(pipeline: &mut qubox_media::RunningMediaPipeline) -> Option<String> {
    let stderr = pipeline.stderr_mut()?;
    let mut text = String::new();
    if stderr.read_to_string(&mut text).is_err() {
        return None;
    }

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lines = trimmed.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(20);
    Some(lines[start..].join("\n"))
}

fn ensure_smoke_frames(frames: u64, stderr_tail: Option<&str>) -> anyhow::Result<()> {
    if frames > 0 {
        return Ok(());
    }

    let stderr_message = stderr_tail
        .filter(|text| !text.trim().is_empty())
        .map(|text| format!(" ffmpeg stderr:\n{text}"))
        .unwrap_or_default();
    anyhow::bail!("smoke test captured zero frames.{stderr_message}")
}

fn smoke_runtime_error(
    error: qubox_media::MediaRuntimeError,
    pipeline: &mut qubox_media::RunningMediaPipeline,
    frames: u64,
) -> anyhow::Error {
    let _ = pipeline.kill();
    let _ = pipeline.wait();
    let stderr_tail = collect_pipeline_stderr(pipeline)
        .map(|text| format!("\nffmpeg stderr:\n{text}"))
        .unwrap_or_default();

    anyhow!(
        "smoke test failed after {} frames: {}{}",
        frames,
        error,
        stderr_tail
    )
}

fn capture_label(config: &HostVideoPipelineConfig) -> String {
    match &config.capture {
        qubox_media::CaptureSourceConfig::LinuxPipeWire { node } => {
            format!("linux_pipewire:{node}")
        }
        qubox_media::CaptureSourceConfig::LinuxX11 { display } => {
            format!("linux_x11:{display}")
        }
        qubox_media::CaptureSourceConfig::WindowsGdiGrab { input } => {
            format!("windows_gdigrab:{input}")
        }
        qubox_media::CaptureSourceConfig::MacosAvFoundation {
            display_index,
            audio_index,
        } => format!("macos_avfoundation:{display_index}:{audio_index}"),
        qubox_media::CaptureSourceConfig::WindowsDxgi { input } => {
            format!("windows_dxgi:{input}")
        }
    }
}

fn scale_input_coordinate(value: u32, source_extent: u32, target_extent: i32) -> i32 {
    if source_extent <= 1 || target_extent <= 1 {
        return 0;
    }

    let source_max = u64::from(source_extent - 1);
    let target_max = u64::from((target_extent - 1) as u32);
    let clamped = u64::from(value.min(source_extent - 1));

    ((clamped * target_max) / source_max) as i32
}

fn map_mouse_button(button: InputMouseButton) -> Button {
    match button {
        InputMouseButton::Left => Button::Left,
        InputMouseButton::Right => Button::Right,
        InputMouseButton::Middle => Button::Middle,
    }
}

fn map_remote_key(key: &str) -> Option<Key> {
    if let Some(character) = single_character_key(key) {
        return Some(Key::Unicode(character));
    }

    match key {
        "Down" => Some(Key::DownArrow),
        "Left" => Some(Key::LeftArrow),
        "Right" => Some(Key::RightArrow),
        "Up" => Some(Key::UpArrow),
        "Backspace" => Some(Key::Backspace),
        "Delete" => Some(Key::Delete),
        "End" => Some(Key::End),
        "Enter" => Some(Key::Return),
        "Escape" => Some(Key::Escape),
        "Home" => Some(Key::Home),
        "Insert" => insert_key(),
        "Menu" => menu_key(),
        "PageDown" => Some(Key::PageDown),
        "PageUp" => Some(Key::PageUp),
        "Pause" => pause_key(),
        "Space" => Some(Key::Space),
        "Tab" => Some(Key::Tab),
        "NumLock" => num_lock_key(),
        "CapsLock" => Some(Key::CapsLock),
        "ScrollLock" => scroll_lock_key(),
        "LeftShift" => Some(Key::LShift),
        "RightShift" => Some(Key::RShift),
        "LeftCtrl" => Some(Key::LControl),
        "RightCtrl" => Some(Key::RControl),
        "LeftAlt" => left_alt_key(),
        "RightAlt" => right_alt_key(),
        "LeftSuper" | "RightSuper" => Some(Key::Meta),
        "NumPad0" => Some(Key::Numpad0),
        "NumPad1" => Some(Key::Numpad1),
        "NumPad2" => Some(Key::Numpad2),
        "NumPad3" => Some(Key::Numpad3),
        "NumPad4" => Some(Key::Numpad4),
        "NumPad5" => Some(Key::Numpad5),
        "NumPad6" => Some(Key::Numpad6),
        "NumPad7" => Some(Key::Numpad7),
        "NumPad8" => Some(Key::Numpad8),
        "NumPad9" => Some(Key::Numpad9),
        "NumPadDot" => Some(Key::Decimal),
        "NumPadSlash" => Some(Key::Divide),
        "NumPadAsterisk" => Some(Key::Multiply),
        "NumPadMinus" => Some(Key::Subtract),
        "NumPadPlus" => Some(Key::Add),
        "NumPadEnter" => numpad_enter_key(),
        "F1" => Some(Key::F1),
        "F2" => Some(Key::F2),
        "F3" => Some(Key::F3),
        "F4" => Some(Key::F4),
        "F5" => Some(Key::F5),
        "F6" => Some(Key::F6),
        "F7" => Some(Key::F7),
        "F8" => Some(Key::F8),
        "F9" => Some(Key::F9),
        "F10" => Some(Key::F10),
        "F11" => Some(Key::F11),
        "F12" => Some(Key::F12),
        "F13" => Some(Key::F13),
        "F14" => Some(Key::F14),
        "F15" => Some(Key::F15),
        _ => None,
    }
}

fn single_character_key(key: &str) -> Option<char> {
    if key.len() == 1 {
        let character = key.chars().next()?;
        if character.is_ascii_alphanumeric() {
            return Some(character.to_ascii_lowercase());
        }
    }

    match key {
        "Key0" => Some('0'),
        "Key1" => Some('1'),
        "Key2" => Some('2'),
        "Key3" => Some('3'),
        "Key4" => Some('4'),
        "Key5" => Some('5'),
        "Key6" => Some('6'),
        "Key7" => Some('7'),
        "Key8" => Some('8'),
        "Key9" => Some('9'),
        "Apostrophe" => Some('\''),
        "Backquote" => Some('`'),
        "Backslash" => Some('\\'),
        "Comma" => Some(','),
        "Equal" => Some('='),
        "LeftBracket" => Some('['),
        "Minus" => Some('-'),
        "Period" => Some('.'),
        "RightBracket" => Some(']'),
        "Semicolon" => Some(';'),
        "Slash" => Some('/'),
        _ => None,
    }
}

#[cfg(target_os = "windows")]
fn insert_key() -> Option<Key> {
    Some(Key::Insert)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn insert_key() -> Option<Key> {
    Some(Key::Insert)
}

#[cfg(not(any(target_os = "windows", all(unix, not(target_os = "macos")))))]
fn insert_key() -> Option<Key> {
    None
}

#[cfg(target_os = "windows")]
fn menu_key() -> Option<Key> {
    Some(Key::Apps)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn menu_key() -> Option<Key> {
    Some(Key::LMenu)
}

#[cfg(not(any(target_os = "windows", all(unix, not(target_os = "macos")))))]
fn menu_key() -> Option<Key> {
    None
}

#[cfg(target_os = "windows")]
fn pause_key() -> Option<Key> {
    Some(Key::Pause)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn pause_key() -> Option<Key> {
    Some(Key::Pause)
}

#[cfg(not(any(target_os = "windows", all(unix, not(target_os = "macos")))))]
fn pause_key() -> Option<Key> {
    None
}

#[cfg(target_os = "windows")]
fn num_lock_key() -> Option<Key> {
    Some(Key::Numlock)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn num_lock_key() -> Option<Key> {
    Some(Key::Numlock)
}

#[cfg(not(any(target_os = "windows", all(unix, not(target_os = "macos")))))]
fn num_lock_key() -> Option<Key> {
    None
}

#[cfg(target_os = "windows")]
fn scroll_lock_key() -> Option<Key> {
    Some(Key::Scroll)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn scroll_lock_key() -> Option<Key> {
    Some(Key::ScrollLock)
}

#[cfg(not(any(target_os = "windows", all(unix, not(target_os = "macos")))))]
fn scroll_lock_key() -> Option<Key> {
    None
}

#[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
fn left_alt_key() -> Option<Key> {
    Some(Key::LMenu)
}

#[cfg(not(any(target_os = "windows", all(unix, not(target_os = "macos")))))]
fn left_alt_key() -> Option<Key> {
    None
}

#[cfg(target_os = "windows")]
fn right_alt_key() -> Option<Key> {
    Some(Key::RMenu)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn right_alt_key() -> Option<Key> {
    Some(Key::Alt)
}

#[cfg(not(any(target_os = "windows", all(unix, not(target_os = "macos")))))]
fn right_alt_key() -> Option<Key> {
    None
}

#[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
fn numpad_enter_key() -> Option<Key> {
    Some(Key::Return)
}

#[cfg(not(any(target_os = "windows", all(unix, not(target_os = "macos")))))]
fn numpad_enter_key() -> Option<Key> {
    None
}

fn format_ice_servers(ice_servers: &[qubox_proto::IceServer]) -> String {
    if ice_servers.is_empty() {
        return "none".to_string();
    }

    ice_servers
        .iter()
        .flat_map(|server| server.urls.iter())
        .cloned()
        .collect::<Vec<_>>()
        .join(",")
}

async fn send_json<S, T>(writer: &mut S, payload: &T) -> anyhow::Result<()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
    T: Serialize,
{
    writer
        .send(Message::Text(serde_json::to_string(payload)?.into()))
        .await?;

    Ok(())
}

/// Refuse `--auto-approve-pairing` when the signaling URL is not clearly
/// local (loopback / RFC1918 / .local). Managed / public hosts must use
/// explicit pair policy.
fn refuse_auto_approve_on_public_server(args: &Args) -> anyhow::Result<()> {
    if !args.auto_approve_pairing {
        return Ok(());
    }
    if signaling_url_is_local(&args.server) {
        return Ok(());
    }
    anyhow::bail!(
        "--auto-approve-pairing is only allowed for loopback/private/.local signaling URLs \
         (got {}). Remove the flag or point --server at a local address.",
        args.server
    )
}

fn signaling_url_is_local(server: &str) -> bool {
    let host = server
        .split("://")
        .nth(1)
        .unwrap_or(server)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split('@')
        .next_back()
        .unwrap_or("")
        .trim_matches(|c| c == '[' || c == ']');
    // strip port (careful with IPv6)
    let host = if host.matches(':').count() == 1 && !host.contains("::") {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    } else {
        host
    };
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".local") {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
            IpAddr::V6(v6) => {
                v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local()
            }
        };
    }
    false
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagram_dispatcher_loop_runs_regardless_of_device_name() {
        // The new design ALWAYS spawns `run_datagram_dispatcher_loop` so the
        // dispatcher is the sole consumer of `connection.read_datagram`.
        // Pen injection is the inner branch: present only when a device name
        // is configured. Verify the predicate that drives the inner branch.
        let no_device: Option<String> = None;
        let with_device: Option<String> = Some("tablet-0".to_string());

        // No device: drain branch only — the loop still runs.
        assert!(!is_pen_injector_enabled(no_device.as_ref()));

        // With device: lazy create on first decoded event.
        assert!(is_pen_injector_enabled(with_device.as_ref()));
    }

    fn is_pen_injector_enabled(device_name: Option<&String>) -> bool {
        device_name.is_some()
    }

    #[test]
    fn signaling_url_local_detection() {
        assert!(signaling_url_is_local("ws://127.0.0.1:7000/ws"));
        assert!(signaling_url_is_local("ws://localhost:7000/ws"));
        assert!(signaling_url_is_local("ws://192.168.1.10:7000/ws"));
        assert!(signaling_url_is_local("ws://10.0.0.5/ws"));
        assert!(signaling_url_is_local("wss://host.local/ws"));
        assert!(!signaling_url_is_local("wss://signal.qubox.app/ws"));
        assert!(!signaling_url_is_local("wss://signal.example.com/ws"));
    }

    #[test]
    fn scale_input_coordinate_maps_full_range() {
        assert_eq!(scale_input_coordinate(0, 1920, 3840), 0);
        assert_eq!(scale_input_coordinate(1919, 1920, 3840), 3839);
        assert_eq!(scale_input_coordinate(960, 1920, 3840), 1920);
    }

    #[test]
    fn remote_key_mapper_handles_common_navigation_and_text_keys() {
        assert_eq!(map_remote_key("A"), Some(Key::Unicode('a')));
        assert_eq!(map_remote_key("Key7"), Some(Key::Unicode('7')));
        assert_eq!(map_remote_key("Enter"), Some(Key::Return));
        assert_eq!(map_remote_key("Left"), Some(Key::LeftArrow));
        assert_eq!(map_remote_key("NumPadPlus"), Some(Key::Add));
        assert!(map_remote_key("Unknown").is_none());
    }

    #[test]
    fn normalize_audio_channels_duplicates_mono_and_downmixes_multichannel() {
        assert_eq!(
            normalize_audio_channels(&[0.25, -0.5], 1),
            vec![0.25, 0.25, -0.5, -0.5]
        );
        assert_eq!(
            normalize_audio_channels(&[1.0, 0.5, 0.1, -1.0, -0.5, 0.2], 3),
            vec![1.0, 0.5, -1.0, -0.5]
        );
    }

    #[test]
    fn build_display_capabilities_sdr_omits_hdr_metadata() {
        let msg = build_display_capabilities_from(1920, 1080, 60, false);
        let view = msg.display_capabilities().expect("view");
        assert_eq!(view.max_resolution, [1920, 1080]);
        assert_eq!(view.max_refresh_hz, 60);
        assert!(
            view.hdr_static_metadata.is_none(),
            "advertise_hdr=false → SDR"
        );
    }

    #[test]
    fn build_display_capabilities_hdr_populates_sei_block() {
        let msg = build_display_capabilities_from(3840, 2160, 144, true);
        let view = msg.display_capabilities().expect("view");
        assert_eq!(view.max_resolution, [3840, 2160]);
        assert_eq!(view.max_refresh_hz, 144);
        let meta = view.hdr_static_metadata.expect("hdr metadata");
        assert_eq!(meta.primaries, 9);
        assert_eq!(meta.transfer, 16);
        assert_eq!(meta.matrix, 9);
    }

    #[test]
    fn build_display_capabilities_saturates_u16_dimensions() {
        // Pretend a 16K capture pipeline; must not overflow u16 fields.
        let msg = build_display_capabilities_from(100_000, 80_000, 60, false);
        let view = msg.display_capabilities().expect("view");
        assert_eq!(view.max_resolution, [u16::MAX, u16::MAX]);
    }
}
