use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, ErrorKind, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command as ProcessCommand, Stdio},
    sync::{
        mpsc::{self, Receiver, Sender, TryRecvError},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

mod filesync_session;

use qubox_client_cli::blank_overlay::{BlankOverlayWindow, OverlayCommand, OverlayController};
use qubox_client_cli::stats_overlay::{self, paint_overlay, StatsCollector, TelemetrySnapshot};
use qubox_client_cli::stream_registry::{StreamEntry, StreamRegistry};
use qubox_client_cli::telemetry::{self as tlm, TelemetryEvent};
use qubox_client_cli::tiled_view::TiledView;

use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand, ValueEnum};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    SampleFormat, Stream,
};
use futures::{stream::SplitStream, Sink, SinkExt, StreamExt};
use minifb::{Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
use qubox_client_cli::render_wgpu::ToneMapKind;
use qubox_clipboard::{ClipboardApplier, ClipboardSyncConfig, ClipboardWatcher};
use qubox_display::{DisplayId, DisplayState};
use qubox_identity::load_or_create_identity;
use qubox_platform::describe_peer;
use qubox_proto::{
    AudioStreamParams, ClientMessage, ControlMsg, InputMouseButton, MicStreamConfig,
    PairingRequest, PeerDescriptor, PeerRole, RelaySignal, RemoteInputEvent, ServerMessage,
    SessionPermissions, SessionPlan, SessionSignal, SignedHello, StartSessionRequest,
    TransportKind, VideoCodec, VideoStreamParams,
};
use qubox_transport::{
    connect_to_native_quic, decode_ticket_b64, NativeQuicAudioReceiver, NativeQuicClientSession,
    NativeQuicControlReceiver, NativeQuicInputSender, NativeQuicMediaReceiver, NativeQuicTicket,
};
use serde::Serialize;
use tokio::{net::TcpStream, sync::mpsc as tokio_mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, env = "QUBOX_SERVER", default_value = "ws://127.0.0.1:7000/ws")]
    server: String,

    /// Managed accounts API base (signup/enroll). Required for `cloud-enroll`
    /// (e.g. https://signal.qubox.app). Leave empty for pure self-host.
    #[arg(long, env = "QUBOX_ACCOUNTS_URL", default_value = "")]
    accounts_url: String,

    #[arg(long)]
    name: Option<String>,

    #[arg(long, env = "QUBOX_IDENTITY_PATH")]
    identity_path: Option<PathBuf>,

    /// Emit structured NDJSON telemetry on stdout for the Tauri GUI.
    /// Human-readable logs are routed to stderr when this is set.
    #[arg(long, default_value_t = false)]
    json_telemetry: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Identity,
    /// Link this device to a Qubox Cloud account using a dashboard enroll code.
    CloudEnroll {
        #[arg(long)]
        code: String,
        #[arg(long)]
        display_name: Option<String>,
    },
    ListHosts,
    Watch,
    Pair {
        #[arg(long)]
        host: String,
    },
    StartSession {
        #[arg(long)]
        host: String,

        #[arg(long)]
        transport: Option<CliTransport>,

        #[arg(long)]
        codec: Option<CliCodec>,

        #[arg(long)]
        mute_playback: bool,

        #[arg(long, default_value_t = 1000)]
        max_stream_frames: u64,

        #[arg(long)]
        skip_window: bool,

        #[arg(long)]
        list_streams: bool,

        #[arg(long)]
        tile: bool,

        #[arg(long)]
        select_stream: Option<u32>,

        #[arg(long, default_value_t = true)]
        show_privacy_indicator: bool,

        #[arg(long, value_enum, default_value_t = CliClipboardSync::Off)]
        clipboard_sync: CliClipboardSync,

        #[arg(long, value_enum, default_value_t = CliClipboardFormats::Text)]
        clipboard_formats: CliClipboardFormats,

        #[arg(long, default_value_t = 250)]
        clipboard_poll_ms: u32,

        #[arg(long, default_value_t = false)]
        mic: bool,

        #[arg(long)]
        mic_device: Option<String>,

        #[arg(long, default_value_t = false)]
        mic_disable_aec: bool,

        #[arg(long, default_value_t = false)]
        mic_disable_ns: bool,

        #[arg(long, default_value_t = 64_000)]
        mic_bitrate_bps: u32,

        #[arg(long, default_value_t = 20)]
        mic_frame_ms: u8,

        /// Choose how the video stream is decoded.
        /// `subprocess` — ffmpeg CLI in a child process (default,
        /// works on every box, slowest).
        /// `sw` — ffmpeg-next in-process software decode + libswscale
        /// BGRA conversion (faster; needs `hw-decode` feature).
        /// `hw` — ffmpeg-next in-process HW-accelerated decode when a
        /// compatible driver is present, SW fallback otherwise
        /// (fastest; needs `hw-decode` feature).
        #[arg(long, value_enum, default_value_t = CliDecoder::Subprocess)]
        decoder: CliDecoder,

        /// Choose how decoded video frames are presented.
        /// `minifb` — CPU blit to a `minifb::Window` (default;
        /// works on every box).
        /// `wgpu` — GPU blit through wgpu (P0-5; needs a display
        /// server + GPU driver).
        #[arg(long, value_enum, default_value_t = CliRenderer::Minifb)]
        renderer: CliRenderer,

        /// HDR tone-mapping operator for the wgpu renderer (ADR-010
        /// §6). `bt2390` is the canonical HDR10-on-SDR operator;
        /// `hable` is the John Hable filmic curve; `srgb-passthrough`
        /// skips tone mapping and prints the framebuffer 1:1. Has no
        /// effect with `--renderer=minifb`. Unknown values fall back
        /// to `bt2390` (the spec default).
        #[arg(long, value_enum, default_value_t = CliToneMap::Bt2390)]
        tone_map: CliToneMap,

        /// Pen / tablet streaming mode (P2-15). `off` disables pen
        /// capture entirely; `client-to-host` spawns the
        /// `PenCoalescer` pump and forwards events to the host over
        /// the QUIC datagram channel with discriminator `0x50`.
        /// Default is `off` per ADR-010 §14.
        #[arg(long, value_enum, default_value_t = CliPenMode::Off)]
        pen: CliPenMode,

        /// ADR-022 Phase C: request a FileSync-only session (no video).
        #[arg(long, default_value_t = false)]
        sync_only: bool,
    },
    RelaySignal {
        #[arg(long)]
        session: Uuid,

        #[arg(long)]
        to: Uuid,

        #[arg(long)]
        kind: CliSignalKind,

        #[arg(long, default_value = "")]
        body: String,
    },
    /// Host: create a short-lived share/pair code.
    CreateShareLink {
        #[arg(long, default_value_t = 900)]
        ttl_secs: u64,
        #[arg(long, default_value_t = true)]
        input: bool,
        #[arg(long, default_value_t = true)]
        clipboard: bool,
        #[arg(long, default_value_t = true)]
        mic: bool,
    },
    /// Client: redeem a share code (requests pairing with the host).
    RedeemShareLink {
        #[arg(long)]
        code: String,
        #[arg(long, default_value = "")]
        client_label: String,
    },
    /// Kick an active session (host or client).
    KickSession {
        #[arg(long)]
        session: Uuid,
        #[arg(long, default_value = "kicked")]
        reason: String,
    },
    /// Host: revoke pairing with a client peer.
    RevokePairing {
        #[arg(long)]
        host_peer_id: Uuid,
        #[arg(long)]
        client_peer_id: Uuid,
    },
    /// ADR-022 FileSync via local daemon IPC (no signaling).
    Sync {
        #[command(subcommand)]
        action: CliSyncAction,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum CliSyncAction {
    ListIgnores,
    AddIgnore {
        pattern: String,
    },
    RemoveIgnore {
        pattern: String,
    },
    SetIgnores {
        #[arg(long = "pattern", required = true)]
        patterns: Vec<String>,
    },
    ApplyPreset {
        name: String,
    },
    ListRules,
    ListJobs,
    ListConflicts,
    ResolveConflict {
        conflict_id: String,
        #[arg(long, value_parser = ["keep-local", "keep-remote", "keep-both"])]
        resolution: String,
    },
    Push {
        path: String,
        #[arg(long)]
        peer: String,
        #[arg(long, default_value = "local")]
        node_id: String,
    },
    /// Start a sync-only session plan (no video) toward a host (Phase C).
    SyncOnlySession {
        #[arg(long)]
        host: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliTransport {
    NativeQuic,
    WebRtc,
    RelayQuic,
}

impl From<CliTransport> for TransportKind {
    fn from(value: CliTransport) -> Self {
        match value {
            CliTransport::NativeQuic => TransportKind::NativeQuic,
            CliTransport::WebRtc => TransportKind::WebRtc,
            CliTransport::RelayQuic => TransportKind::RelayQuic,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliCodec {
    H264,
    H265,
    Av1,
}

impl From<CliCodec> for VideoCodec {
    fn from(value: CliCodec) -> Self {
        match value {
            CliCodec::H264 => VideoCodec::H264,
            CliCodec::H265 => VideoCodec::H265,
            CliCodec::Av1 => VideoCodec::Av1,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliSignalKind {
    SdpOffer,
    SdpAnswer,
    IceCandidate,
    NativeQuicTicket,
    Ready,
}

/// Unified decoder handle for `running_native_quic_viewer`. Wraps the
/// legacy ffmpeg subprocess path and the in-process ffmpeg-next path
/// behind a single `shutdown` API so the call site in `main.rs` does
/// not have to branch on `CliDecoder`.
enum DecoderHandle {
    Subprocess(RunningFrameDecoder),
    #[cfg(feature = "hw-decode")]
    InProcess(qubox_client_cli::decoder_hw::RunningHwFrameDecoder),
}

impl DecoderHandle {
    fn shutdown(self, allow_pipe_end: bool) -> anyhow::Result<()> {
        match self {
            DecoderHandle::Subprocess(decoder) => decoder.shutdown(allow_pipe_end),
            #[cfg(feature = "hw-decode")]
            DecoderHandle::InProcess(decoder) => decoder.shutdown(),
        }
    }
}

fn spawn_chosen_decoder(
    kind: CliDecoder,
    video_config: &VideoStreamParams,
    encoded_rx: Receiver<Vec<u8>>,
    decoded_tx: Sender<Vec<u32>>,
) -> anyhow::Result<DecoderHandle> {
    match kind {
        CliDecoder::Subprocess => Ok(DecoderHandle::Subprocess(RunningFrameDecoder::spawn(
            video_config,
            encoded_rx,
            decoded_tx,
        )?)),
        #[cfg(feature = "hw-decode")]
        CliDecoder::Hw | CliDecoder::Sw => {
            let use_hw = matches!(kind, CliDecoder::Hw);
            let cfg = if use_hw {
                qubox_client_cli::decoder_hw::HwDecoderConfig::for_platform(video_config.clone())
            } else {
                qubox_client_cli::decoder_hw::HwDecoderConfig::software_only(video_config.clone())
            };
            let (hw_tx, hw_rx) =
                qubox_client_cli::decoder_hw::decoded_channel(cfg.decoded_queue_depth);
            std::thread::spawn(move || bridge_decoded_to_minifb(hw_rx, decoded_tx));
            let inner =
                qubox_client_cli::decoder_hw::RunningHwFrameDecoder::spawn(cfg, encoded_rx, hw_tx)?;
            tracing::info!(
                use_hw = use_hw,
                "spawn_chosen_decoder: in-process ffmpeg-next decoder active (P0-3)"
            );
            Ok(DecoderHandle::InProcess(inner))
        }
        #[cfg(not(feature = "hw-decode"))]
        CliDecoder::Hw | CliDecoder::Sw => {
            anyhow::bail!(
                "--decoder hw/sw requires the `hw-decode` feature; rebuild with --features hw-decode"
            );
        }
    }
}

#[cfg(feature = "hw-decode")]
fn bridge_decoded_to_minifb(
    rx: crossbeam_channel::Receiver<qubox_client_cli::frame_pipeline::DecodedFrame>,
    decoded_tx: Sender<Vec<u32>>,
) {
    while let Ok(frame) = rx.recv() {
        match frame.to_minifb_pixels() {
            Ok(pixels) => {
                if decoded_tx.send(pixels).is_err() {
                    break;
                }
            }
            Err(error) => {
                tracing::warn!(?error, "decoded frame bridge failed; skipping frame");
            }
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliClipboardSync {
    Off,
    HostToClient,
    ClientToHost,
    Both,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliDecoder {
    Subprocess,
    Sw,
    Hw,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliRenderer {
    Minifb,
    Wgpu,
}

/// CLI-facing tone-map selector. Mirrors [`ToneMapKind`] but lives
/// in `main.rs` so clap can derive `ValueEnum` here without leaking
/// that derive macro to the renderer library.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliToneMap {
    #[clap(name = "srgb-passthrough")]
    SrgbPassthrough,
    #[clap(name = "hable")]
    Hable,
    #[clap(name = "bt2390")]
    Bt2390,
}

impl From<CliToneMap> for ToneMapKind {
    fn from(value: CliToneMap) -> Self {
        match value {
            CliToneMap::SrgbPassthrough => ToneMapKind::SrgbPassthrough,
            CliToneMap::Hable => ToneMapKind::Hable,
            CliToneMap::Bt2390 => ToneMapKind::Bt2390,
        }
    }
}

/// Pen / stylus streaming mode. Default is `Off` per ADR-010 §14
/// ("TCC `Input Monitoring` permission is fragile in CLI tools on
/// macOS"); the user opts in explicitly to enable the
/// client-to-host direction.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum CliPenMode {
    /// Disable pen capture and QUIC datagram forwarding.
    Off,
    /// Capture pen events locally and forward them to the host
    /// over the QUIC datagram channel (discriminator 0x50).
    ClientToHost,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliClipboardFormats {
    Text,
    Image,
    Both,
}

type SignalingReader = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

struct RunningFrameDecoder {
    child: Child,
    writer_handle: thread::JoinHandle<anyhow::Result<()>>,
    reader_handle: thread::JoinHandle<anyhow::Result<()>>,
    stderr_handle: thread::JoinHandle<anyhow::Result<()>>,
}

#[derive(Clone)]
struct AudioPlaybackHandle {
    queue: Arc<Mutex<VecDeque<f32>>>,
    max_buffered_samples: usize,
}

struct RunningAudioPlayback {
    _stream: Stream,
    handle: AudioPlaybackHandle,
}

impl RunningFrameDecoder {
    fn spawn(
        video_config: &VideoStreamParams,
        encoded_rx: Receiver<Vec<u8>>,
        decoded_tx: Sender<Vec<u32>>,
    ) -> anyhow::Result<Self> {
        let mut child = ProcessCommand::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "warning",
                "-nostdin",
                "-fflags",
                "nobuffer",
                "-flags",
                "low_delay",
                "-probesize",
                "32",
                "-analyzeduration",
                "0",
                "-f",
                "h264",
                "-i",
                "pipe:0",
                "-an",
                "-pix_fmt",
                "bgra",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn ffmpeg decoder; ensure ffmpeg is installed on the client")?;

        let stdin = child.stdin.take().ok_or_else(|| {
            anyhow!("spawned ffmpeg decoder did not expose stdin for H.264 input")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            anyhow!("spawned ffmpeg decoder did not expose stdout for BGRA output")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            anyhow!("spawned ffmpeg decoder did not expose stderr for diagnostics")
        })?;
        let width = video_config.width;
        let height = video_config.height;

        let writer_handle = thread::spawn(move || decoder_writer_loop(stdin, encoded_rx));
        let reader_handle =
            thread::spawn(move || decoder_reader_loop(stdout, width, height, decoded_tx));
        let stderr_handle = thread::spawn(move || decoder_stderr_loop(stderr));

        Ok(Self {
            child,
            writer_handle,
            reader_handle,
            stderr_handle,
        })
    }

    fn shutdown(mut self, allow_pipe_end: bool) -> anyhow::Result<()> {
        let writer_result = self
            .writer_handle
            .join()
            .map_err(|_| anyhow!("ffmpeg decoder writer thread panicked"))?;
        let reader_result = self
            .reader_handle
            .join()
            .map_err(|_| anyhow!("ffmpeg decoder reader thread panicked"))?;
        let stderr_result = self
            .stderr_handle
            .join()
            .map_err(|_| anyhow!("ffmpeg decoder stderr thread panicked"))?;

        let _ = self.child.kill();
        let status = self
            .child
            .wait()
            .context("failed to wait for ffmpeg decoder exit")?;
        tracing::debug!(?status, "ffmpeg decoder process exited");

        if let Err(error) = writer_result {
            if !allow_pipe_end || !is_benign_decoder_pipe_end(&error) {
                return Err(error);
            }
        }
        reader_result?;
        stderr_result?;
        Ok(())
    }
}

impl RunningAudioPlayback {
    fn start(audio_config: &AudioStreamParams) -> anyhow::Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("failed to open the default client audio output device")?;
        let sample_format = device
            .default_output_config()
            .map_err(|error| anyhow!(error))
            .context("failed to query the default client audio output format")?
            .sample_format();
        let stream_config = cpal::StreamConfig {
            channels: audio_config.channels,
            sample_rate: audio_config.sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let handle = AudioPlaybackHandle {
            queue: queue.clone(),
            max_buffered_samples: audio_config.sample_rate as usize
                * audio_config.channels as usize
                * 2,
        };
        let err_fn = |error| eprintln!("client audio playback stream error: {error}");

        let stream = match sample_format {
            SampleFormat::F32 => {
                let queue = queue.clone();
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [f32], _| fill_audio_output_buffer_f32(data, &queue),
                    err_fn,
                    None,
                )
            }
            SampleFormat::I16 => {
                let queue = queue.clone();
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [i16], _| fill_audio_output_buffer_i16(data, &queue),
                    err_fn,
                    None,
                )
            }
            SampleFormat::U16 => {
                let queue = queue.clone();
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [u16], _| fill_audio_output_buffer_u16(data, &queue),
                    err_fn,
                    None,
                )
            }
            sample_format => {
                anyhow::bail!("unsupported client audio sample format {sample_format:?}")
            }
        }
        .map_err(|error| anyhow!(error))
        .context("failed to build the client audio playback stream")?;

        stream
            .play()
            .map_err(|error| anyhow!(error))
            .context("failed to start the client audio playback stream")?;

        Ok(Self {
            _stream: stream,
            handle,
        })
    }

    fn handle(&self) -> AudioPlaybackHandle {
        self.handle.clone()
    }
}

impl AudioPlaybackHandle {
    fn push_chunk(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let samples = decode_audio_chunk_samples(bytes)?;
        let mut queue = self
            .queue
            .lock()
            .map_err(|_| anyhow!("client audio playback queue was poisoned"))?;

        for sample in samples {
            queue.push_back(sample);
        }

        while queue.len() > self.max_buffered_samples {
            let _ = queue.pop_front();
        }

        Ok(())
    }
}

struct WindowLoopOutcome {
    rendered_frames: u64,
    closed_by_user: bool,
    stream_ended: bool,
}

fn load_qubox_env() {
    if let Some(proj_dirs) = directories::ProjectDirs::from("com", "qubox", "qubox") {
        let env_path = proj_dirs.config_dir().join("env");
        if env_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&env_path) {
                for line in content.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                        continue;
                    }
                    let line = if line.starts_with("export ") {
                        &line[7..]
                    } else {
                        line
                    };
                    if let Some((key, val)) = line.split_once('=') {
                        let key = key.trim();
                        let val = val.trim();
                        let val = val.trim_matches(|c| c == '"' || c == '\'');
                        if !key.is_empty() {
                            std::env::set_var(key, val);
                        }
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    load_qubox_env();
    let _ = rustls::crypto::ring::default_provider().install_default();
    init_tracing();
    let args = Args::parse();
    if args.json_telemetry {
        tlm::enable();
    }
    let (identity, identity_path) = load_or_create_identity(args.identity_path, args.name.clone())?;
    let descriptor = describe_peer(
        PeerRole::Client,
        Some(identity.display_name.clone()),
        identity.device_id,
        identity.peer_id_for(PeerRole::Client),
    );

    if matches!(&args.command, Command::Identity) {
        println!(
            "identity={} device={} host_peer={} client_peer={} name={}",
            identity_path.display(),
            identity.device_id,
            identity.host_peer_id,
            identity.client_peer_id,
            identity.display_name
        );
        return Ok(());
    }

    if let Command::CloudEnroll { code, display_name } = &args.command {
        return run_cloud_enroll(
            &args.accounts_url,
            code,
            display_name
                .clone()
                .unwrap_or_else(|| identity.display_name.clone()),
            identity.device_id,
            &identity.public_key,
        )
        .await;
    }

    if let Command::Sync { action } = &args.command {
        return run_cli_sync(action.clone()).await;
    }

    let (stream, _) = connect_async(&args.server)
        .await
        .with_context(|| format!("failed to connect to {}", args.server))?;
    let (mut writer, mut reader) = stream.split();

    send_json(
        &mut writer,
        &ClientMessage::SignedHello(SignedHello::sign(
            &descriptor,
            &identity.signing_key(Some(&identity_path))?,
        )),
    )
    .await?;

    match args.command {
        Command::Identity | Command::CloudEnroll { .. } => {
            unreachable!("exits before websocket connection")
        }
        Command::ListHosts => {
            send_json(&mut writer, &ClientMessage::ListHosts).await?;
            while let Some(frame) = reader.next().await {
                match frame? {
                    Message::Text(text) => {
                        let message: ServerMessage = serde_json::from_str(text.as_ref())?;
                        match message {
                            ServerMessage::Hosts { hosts } => {
                                print_hosts(&hosts);
                                break;
                            }
                            ServerMessage::Welcome(_) | ServerMessage::HeartbeatAck => {}
                            ServerMessage::Error(error) => {
                                anyhow::bail!("{}: {}", error.code, error.message);
                            }
                            _ => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
        Command::Watch => {
            send_json(&mut writer, &ClientMessage::ListHosts).await?;
            let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
            println!("watching presence, pairing, session, and signaling events");

            loop {
                tokio::select! {
                    _ = heartbeat.tick() => {
                        send_json(&mut writer, &ClientMessage::Heartbeat).await?;
                    }
                    frame = reader.next() => {
                        match frame {
                            Some(Ok(Message::Text(text))) => {
                                let message: ServerMessage = serde_json::from_str(text.as_ref())?;
                                match message {
                                    ServerMessage::Hosts { hosts } => print_hosts(&hosts),
                                    ServerMessage::PairingEstablished(grant) => println!(
                                        "pairing established: host={} client={}",
                                        grant.host_peer_id, grant.client_peer_id
                                    ),
                                    ServerMessage::PairingRejected { request_id, reason } => println!(
                                        "pairing rejected: {} {}",
                                        request_id, reason
                                    ),
                                    ServerMessage::Presence(event) => println!(
                                        "presence: {} {} {}",
                                        event.peer.peer_id,
                                        event.peer.device_name,
                                        if event.connected { "online" } else { "offline" }
                                    ),
                                    ServerMessage::SessionPlanned(plan) => println!(
                                        "session planned: {} {:?} {:?} token_expires={} ice_servers={}",
                                        plan.session_id,
                                        plan.transport,
                                        plan.codec,
                                        plan.client_credential.expires_unix_millis,
                                        format_ice_servers(&plan.ice_servers)
                                    ),
                                    ServerMessage::Signal(signal) => println!(
                                        "signal: session={} from={} to={} kind={:?}",
                                        signal.session_id, signal.from_peer_id, signal.to_peer_id, signal.signal
                                    ),
                                    ServerMessage::Error(error) => println!("server error: {} {}", error.code, error.message),
                                    ServerMessage::Welcome(_) | ServerMessage::HeartbeatAck => {}
                                    ServerMessage::PairingRequested(_)
                                    | ServerMessage::SessionRequested(_)
                                    | ServerMessage::ShareLinkCreated { .. }
                                    | ServerMessage::SessionKicked { .. }
                                    | ServerMessage::PairingRevoked { .. }
                    | ServerMessage::SessionConsentPending { .. }
                    | ServerMessage::SessionBundleAccepted(_)
                    | ServerMessage::SignedKillReceived(_) => {}
                                }
                            }
                            Some(Ok(Message::Close(_))) | None => break,
                            Some(Ok(_)) => {}
                            Some(Err(error)) => return Err(error.into()),
                        }
                    }
                }
            }
        }
        Command::Pair { host } => {
            let resolved_host = resolve_host_argument(&mut writer, &mut reader, &host).await?;
            let pairing_request_id = Uuid::new_v4();
            if tlm::is_enabled() {
                tlm::emit(&TelemetryEvent::PairingRequested {
                    host_id: resolved_host.peer_id.to_string(),
                    request_id: pairing_request_id.to_string(),
                });
            }
            send_json(
                &mut writer,
                &ClientMessage::RequestPairing(PairingRequest {
                    request_id: pairing_request_id,
                    host_peer_id: resolved_host.peer_id,
                    client_label: descriptor.device_name.clone(),
                }),
            )
            .await?;

            while let Some(frame) = reader.next().await {
                match frame? {
                    Message::Text(text) => {
                        let message: ServerMessage = serde_json::from_str(text.as_ref())?;
                        match message {
                            ServerMessage::PairingEstablished(grant) => {
                                println!(
                                    "paired client {} with host {}",
                                    grant.client_peer_id, grant.host_peer_id
                                );
                                if tlm::is_enabled() {
                                    tlm::emit(&TelemetryEvent::PairingEstablished {
                                        host_id: grant.host_peer_id.to_string(),
                                        client_id: grant.client_peer_id.to_string(),
                                    });
                                }
                                break;
                            }
                            ServerMessage::PairingRejected { request_id, reason } => {
                                anyhow::bail!("pairing rejected {}: {}", request_id, reason);
                            }
                            ServerMessage::Error(error) => {
                                anyhow::bail!("{}: {}", error.code, error.message);
                            }
                            ServerMessage::Welcome(_) | ServerMessage::HeartbeatAck => {}
                            ServerMessage::Hosts { hosts } => print_hosts(&hosts),
                            ServerMessage::Presence(_)
                            | ServerMessage::PairingRequested(_)
                            | ServerMessage::SessionPlanned(_)
                            | ServerMessage::SessionRequested(_)
                            | ServerMessage::Signal(_)
                            | ServerMessage::ShareLinkCreated { .. }
                            | ServerMessage::SessionKicked { .. }
                            | ServerMessage::PairingRevoked { .. }
                    | ServerMessage::SessionConsentPending { .. }
                    | ServerMessage::SessionBundleAccepted(_)
                    | ServerMessage::SignedKillReceived(_) => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
        Command::StartSession {
            host,
            transport,
            codec,
            mute_playback,
            max_stream_frames,
            skip_window,
            list_streams,
            tile,
            select_stream,
            show_privacy_indicator,
            clipboard_sync,
            clipboard_formats,
            clipboard_poll_ms,
            mic,
            mic_device,
            mic_disable_aec,
            mic_disable_ns,
            mic_bitrate_bps,
            mic_frame_ms,
            decoder,
            renderer,
            tone_map,
            pen,
            sync_only,
        } => {
            let stream_registry = StreamRegistry::new();

            if let Some(sel) = select_stream {
                stream_registry.set_selected_stream(Some(DisplayId(sel)));
            }
            stream_registry.set_tile_mode(tile);
            stream_registry.set_show_privacy_indicator(show_privacy_indicator);

            let resolved_host = resolve_host_argument(&mut writer, &mut reader, &host).await?;
            let requested_transport = transport.map(Into::into);
            let request = StartSessionRequest {
                session_id: Uuid::new_v4(),
                target_host_id: resolved_host.peer_id,
                requested_transport,
                preferred_codec: codec.map(Into::into).or(Some(VideoCodec::H264)),
                video: None,
                permissions: Default::default(),
                sync_only,
            consent_id: None,
            };

            send_json(&mut writer, &ClientMessage::StartSession(request)).await?;

            while let Some(frame) = reader.next().await {
                match frame? {
                    Message::Text(text) => {
                        let message: ServerMessage = serde_json::from_str(text.as_ref())?;
                        match message {
                            ServerMessage::SessionPlanned(plan) => {
                                if plan.transport == TransportKind::NativeQuic {
                                    receive_native_quic_stream(
                                        &mut writer,
                                        &mut reader,
                                        &descriptor,
                                        plan,
                                        mute_playback,
                                        max_stream_frames,
                                        skip_window,
                                        list_streams,
                                        &stream_registry,
                                        clipboard_sync,
                                        clipboard_formats,
                                        clipboard_poll_ms,
                                        mic,
                                        mic_device,
                                        mic_disable_aec,
                                        mic_disable_ns,
                                        mic_bitrate_bps,
                                        mic_frame_ms,
                                        decoder,
                                        renderer,
                                        tone_map,
                                        pen,
                                    )
                                    .await?;
                                } else {
                                    println!(
                                        "session {} -> host {} via {:?} using {:?} token={} token_expires={} ice_servers={}",
                                        plan.session_id,
                                        plan.target_host_id,
                                        plan.transport,
                                        plan.codec,
                                        plan.client_credential.token,
                                        plan.client_credential.expires_unix_millis,
                                        format_ice_servers(&plan.ice_servers)
                                    );
                                }
                                break;
                            }
                            ServerMessage::Error(error) => {
                                anyhow::bail!("{}: {}", error.code, error.message);
                            }
                            ServerMessage::Welcome(_) | ServerMessage::HeartbeatAck => {}
                            ServerMessage::Hosts { hosts } => print_hosts(&hosts),
                            ServerMessage::Presence(_)
                            | ServerMessage::PairingRequested(_)
                            | ServerMessage::PairingEstablished(_)
                            | ServerMessage::PairingRejected { .. }
                            | ServerMessage::SessionRequested(_)
                            | ServerMessage::Signal(_)
                            | ServerMessage::ShareLinkCreated { .. }
                            | ServerMessage::SessionKicked { .. }
                            | ServerMessage::PairingRevoked { .. }
                    | ServerMessage::SessionConsentPending { .. }
                    | ServerMessage::SessionBundleAccepted(_)
                    | ServerMessage::SignedKillReceived(_) => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
        Command::RelaySignal {
            session,
            to,
            kind,
            body,
        } => {
            let signal = match kind {
                CliSignalKind::SdpOffer => SessionSignal::SdpOffer { sdp: body },
                CliSignalKind::SdpAnswer => SessionSignal::SdpAnswer { sdp: body },
                CliSignalKind::IceCandidate => SessionSignal::IceCandidate {
                    candidate: body,
                    sdp_mid: Some("0".to_string()),
                    sdp_mline_index: Some(0),
                },
                CliSignalKind::NativeQuicTicket => SessionSignal::NativeQuicTicket {
                    alpn: "qubox-native-quic/0".to_string(),
                    ticket_b64: body,
                },
                CliSignalKind::Ready => SessionSignal::Ready,
            };

            send_json(
                &mut writer,
                &ClientMessage::RelaySignal(RelaySignal {
                    session_id: session,
                    from_peer_id: descriptor.peer_id,
                    to_peer_id: to,
                    signal,
                }),
            )
            .await?;

            println!("signal relayed toward {}", to);
        }
        Command::CreateShareLink {
            ttl_secs,
            input,
            clipboard,
            mic,
        } => {
            send_json(
                &mut writer,
                &ClientMessage::CreateShareLink {
                    ttl_secs,
                    permissions: SessionPermissions {
                        input,
                        clipboard,
                        mic,
                    },
                },
            )
            .await?;
            while let Some(frame) = reader.next().await {
                match frame? {
                    Message::Text(text) => {
                        let message: ServerMessage = serde_json::from_str(text.as_ref())?;
                        match message {
                            ServerMessage::ShareLinkCreated {
                                code,
                                expires_unix_ms,
                                url_hint,
                            } => {
                                println!(
                                    "share code={code} expires_ms={expires_unix_ms} url={url_hint}"
                                );
                                break;
                            }
                            ServerMessage::Error(error) => {
                                anyhow::bail!("{}: {}", error.code, error.message);
                            }
                            ServerMessage::Welcome(_) | ServerMessage::HeartbeatAck => {}
                            _ => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
        Command::RedeemShareLink { code, client_label } => {
            send_json(
                &mut writer,
                &ClientMessage::RedeemShareLink { code, client_label },
            )
            .await?;
            while let Some(frame) = reader.next().await {
                match frame? {
                    Message::Text(text) => {
                        let message: ServerMessage = serde_json::from_str(text.as_ref())?;
                        match message {
                            ServerMessage::PairingEstablished(grant) => {
                                println!(
                                    "paired via share link: host={} client={}",
                                    grant.host_peer_id, grant.client_peer_id
                                );
                                break;
                            }
                            ServerMessage::PairingRequested(req) => {
                                println!(
                                    "pairing requested via share link: request_id={} host={}",
                                    req.request_id, req.host_peer_id
                                );
                                // keep waiting for establish / reject
                            }
                            ServerMessage::PairingRejected { request_id, reason } => {
                                anyhow::bail!("pairing rejected {}: {}", request_id, reason);
                            }
                            ServerMessage::Error(error) => {
                                anyhow::bail!("{}: {}", error.code, error.message);
                            }
                            ServerMessage::Welcome(_) | ServerMessage::HeartbeatAck => {}
                            _ => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
        Command::KickSession { session, reason } => {
            send_json(
                &mut writer,
                &ClientMessage::KickSession {
                    session_id: session,
                    reason: reason.clone(),
                },
            )
            .await?;
            println!("kick sent for session {session}: {reason}");
        }
        Command::RevokePairing {
            host_peer_id,
            client_peer_id,
        } => {
            send_json(
                &mut writer,
                &ClientMessage::RevokePairing {
                    host_peer_id,
                    client_peer_id,
                },
            )
            .await?;
            println!("revoke sent host={host_peer_id} client={client_peer_id}");
        }
        Command::Sync { .. } => unreachable!("sync handled before signaling connect"),
    }

    let _ = writer.send(Message::Close(None)).await;

    Ok(())
}

async fn run_cloud_enroll(
    accounts_url: &str,
    code: &str,
    display_name: String,
    device_id: uuid::Uuid,
    public_key: &[u8; 32],
) -> anyhow::Result<()> {
    let base = accounts_url.trim().trim_end_matches('/');
    if base.is_empty() {
        anyhow::bail!(
            "cloud-enroll needs an accounts API URL.\n\
             Qubox Cloud:  qubox-client-cli --accounts-url https://signal.qubox.app cloud-enroll --code {code}\n\
             Or:           export QUBOX_ACCOUNTS_URL=https://signal.qubox.app"
        );
    }
    if base.contains("127.0.0.1") || base.contains("localhost") {
        eprintln!(
            "warning: accounts URL is local ({base}); for Qubox Cloud use https://signal.qubox.app"
        );
    }
    let url = format!("{base}/v1/public/enroll");
    let body = serde_json::json!({
        "code": code,
        "device_id": device_id,
        "display_name": display_name,
        "public_key_hex": hex::encode(public_key),
        "role": "both",
    });
    let client = reqwest::Client::new();
    let res = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("cloud enroll failed ({status}): {text}");
    }
    println!("enrolled device {device_id}");
    println!("{text}");
    Ok(())
}

async fn run_cli_sync(action: CliSyncAction) -> anyhow::Result<()> {
    use qubox_daemon::ipc::{IpcClient, IpcRequest, IpcResponse};
    use qubox_daemon::DaemonConfig;
    use qubox_sync::ConflictResolution;

    if let CliSyncAction::SyncOnlySession { host } = &action {
        println!(
            "sync-only session: use `start-session --host {host} --sync-only` once host builds support FileSync-only streams (Phase C). Queuing policy still uses daemon outbox until both peers are online."
        );
        return Ok(());
    }

    let config = DaemonConfig::default();
    let mut client = IpcClient::connect(&config).await?;
    let resp: IpcResponse = match action {
        CliSyncAction::ListIgnores => client.call(&IpcRequest::SyncListIgnores).await?,
        CliSyncAction::AddIgnore { pattern } => {
            client.call(&IpcRequest::SyncAddIgnore { pattern }).await?
        }
        CliSyncAction::RemoveIgnore { pattern } => {
            client
                .call(&IpcRequest::SyncRemoveIgnore { pattern })
                .await?
        }
        CliSyncAction::SetIgnores { patterns } => {
            client
                .call(&IpcRequest::SyncSetIgnores { patterns })
                .await?
        }
        CliSyncAction::ApplyPreset { name } => {
            client
                .call(&IpcRequest::SyncApplyIgnorePreset { name })
                .await?
        }
        CliSyncAction::ListRules => client.call(&IpcRequest::SyncListRules).await?,
        CliSyncAction::ListJobs => client.call(&IpcRequest::SyncListJobs).await?,
        CliSyncAction::ListConflicts => client.call(&IpcRequest::SyncListConflicts).await?,
        CliSyncAction::ResolveConflict {
            conflict_id,
            resolution,
        } => {
            let resolution = match resolution.as_str() {
                "keep-local" => ConflictResolution::KeepLocal,
                "keep-remote" => ConflictResolution::KeepRemote,
                "keep-both" => ConflictResolution::KeepBoth,
                _ => anyhow::bail!("invalid resolution"),
            };
            client
                .call(&IpcRequest::SyncResolveConflict {
                    conflict_id,
                    resolution,
                })
                .await?
        }
        CliSyncAction::Push {
            path,
            peer,
            node_id,
        } => {
            client
                .call(&IpcRequest::SyncPushNow {
                    local_path: path,
                    target_peer: peer,
                    node_id,
                })
                .await?
        }
        CliSyncAction::SyncOnlySession { .. } => unreachable!(),
    };
    match resp {
        IpcResponse::SyncIgnores { patterns } => {
            for p in patterns {
                println!("{p}");
            }
        }
        IpcResponse::SyncRules { rules } => {
            for r in rules {
                println!(
                    "{} enabled={} paths={:?} peers={:?}",
                    r.rule_id, r.enabled, r.paths, r.peer_ids
                );
            }
        }
        IpcResponse::SyncJobs { jobs } => {
            for j in jobs {
                println!("{} {:?} → {}", j.job_id, j.status, j.target_peer);
            }
        }
        IpcResponse::SyncConflicts { conflicts } => {
            for c in conflicts {
                println!(
                    "{} local={} remote={}",
                    c.conflict_id, c.local_path, c.remote_path
                );
            }
        }
        IpcResponse::SyncJob { job } => println!("queued {}", job.job_id),
        IpcResponse::Unit => println!("ok"),
        IpcResponse::Error { code, message } => anyhow::bail!("daemon error {code}: {message}"),
        other => anyhow::bail!("unexpected {other:?}"),
    }
    Ok(())
}

fn print_hosts(hosts: &[PeerDescriptor]) {
    if hosts.is_empty() {
        tlm::eprintln_status("no hosts online");
        return;
    }

    for host in hosts {
        let transports: Vec<String> = host
            .capabilities
            .transports
            .iter()
            .map(|t| format!("{t:?}").to_lowercase())
            .collect();
        if tlm::is_enabled() {
            tlm::emit(&TelemetryEvent::HostDiscovered {
                peer_id: host.peer_id.to_string(),
                device_name: host.device_name.clone(),
                transports,
            });
        } else {
            println!(
                "{}  device={}  {}  {:?}  transports={:?}  capture={:?}  encoders={:?}",
                host.peer_id,
                host.device_id,
                host.device_name,
                host.os,
                host.capabilities.transports,
                host.capabilities.capture,
                host.capabilities.encoders,
            );
        }
    }
}

fn resolve_host_in_inventory<'a>(
    hosts: &'a [PeerDescriptor],
    host: &str,
) -> anyhow::Result<&'a PeerDescriptor> {
    if hosts.is_empty() {
        anyhow::bail!("no hosts are currently online")
    }

    if let Ok(host_id) = Uuid::parse_str(host) {
        return hosts
            .iter()
            .find(|candidate| candidate.peer_id == host_id)
            .ok_or_else(|| anyhow!("host {} is not currently online", host_id));
    }

    let matches = hosts
        .iter()
        .filter(|candidate| candidate.device_name.eq_ignore_ascii_case(host))
        .collect::<Vec<_>>();

    match matches.len() {
        0 => anyhow::bail!("no online host matched display name {}", host),
        1 => Ok(matches[0]),
        _ => anyhow::bail!(
            "display name {} matched multiple hosts: {}",
            host,
            matches
                .into_iter()
                .map(|candidate| format!("{} ({})", candidate.device_name, candidate.peer_id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

async fn fetch_hosts<S>(
    writer: &mut S,
    reader: &mut SignalingReader,
) -> anyhow::Result<Vec<PeerDescriptor>>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    send_json(writer, &ClientMessage::ListHosts).await?;

    while let Some(frame) = reader.next().await {
        match frame? {
            Message::Text(text) => {
                let message: ServerMessage = serde_json::from_str(text.as_ref())?;
                match message {
                    ServerMessage::Hosts { hosts } => return Ok(hosts),
                    ServerMessage::Welcome(_)
                    | ServerMessage::HeartbeatAck
                    | ServerMessage::Presence(_) => {}
                    ServerMessage::Error(error) => {
                        anyhow::bail!("{}: {}", error.code, error.message)
                    }
                    ServerMessage::PairingRequested(_)
                    | ServerMessage::PairingEstablished(_)
                    | ServerMessage::PairingRejected { .. }
                    | ServerMessage::SessionPlanned(_)
                    | ServerMessage::SessionRequested(_)
                    | ServerMessage::Signal(_)
                    | ServerMessage::ShareLinkCreated { .. }
                    | ServerMessage::SessionKicked { .. }
                    | ServerMessage::PairingRevoked { .. }
                    | ServerMessage::SessionConsentPending { .. }
                    | ServerMessage::SessionBundleAccepted(_)
                    | ServerMessage::SignedKillReceived(_) => {}
                }
            }
            Message::Close(_) => {
                anyhow::bail!("signaling connection closed while fetching host inventory")
            }
            _ => {}
        }
    }

    anyhow::bail!("signaling connection closed before the host inventory arrived")
}

async fn resolve_host_argument<S>(
    writer: &mut S,
    reader: &mut SignalingReader,
    host: &str,
) -> anyhow::Result<PeerDescriptor>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let hosts = fetch_hosts(writer, reader).await?;
    Ok(resolve_host_in_inventory(&hosts, host)?.clone())
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

async fn receive_native_quic_stream<S>(
    writer: &mut S,
    reader: &mut SignalingReader,
    descriptor: &PeerDescriptor,
    plan: SessionPlan,
    mute_playback: bool,
    max_stream_frames: u64,
    skip_window: bool,
    list_streams: bool,
    stream_registry: &StreamRegistry,
    clipboard_sync: CliClipboardSync,
    clipboard_formats: CliClipboardFormats,
    clipboard_poll_ms: u32,
    mic: bool,
    mic_device: Option<String>,
    mic_disable_aec: bool,
    mic_disable_ns: bool,
    mic_bitrate_bps: u32,
    mic_frame_ms: u8,
    decoder: CliDecoder,
    renderer: CliRenderer,
    tone_map: CliToneMap,
    pen: CliPenMode,
) -> anyhow::Result<()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    println!(
        "session {} -> host {} via {:?} using {:?} decoder={:?} renderer={:?} token={} token_expires={} ice_servers={}",
        plan.session_id,
        plan.target_host_id,
        plan.transport,
        plan.codec,
        decoder,
        renderer,
        plan.client_credential.token,
        plan.client_credential.expires_unix_millis,
        format_ice_servers(&plan.ice_servers)
    );

    if tlm::is_enabled() {
        tlm::emit(&TelemetryEvent::SessionPlanned {
            session_id: plan.session_id.to_string(),
            transport: format!("{:?}", plan.transport).to_lowercase(),
            codec: format!("{:?}", plan.codec).to_lowercase(),
            rtt_ms: 0,
        });
    }

    tracing::info!(
        session_id = %plan.session_id,
        host_id = %plan.target_host_id,
        transport = ?plan.transport,
        codec = ?plan.codec,
        decoder = ?decoder,
        renderer = ?renderer,
        "waiting for native QUIC ticket"
    );
    let ticket = wait_for_native_quic_ticket(reader, plan.session_id, plan.target_host_id).await?;
    tracing::info!(
        session_id = %plan.session_id,
        connect_addr = %ticket.connect_addr,
        "received native QUIC ticket"
    );
    let session = connect_to_native_quic(&ticket, &plan.client_credential).await?;
    tracing::info!(session_id = %plan.session_id, "native QUIC session established; sending ready signal");

    send_json(
        writer,
        &ClientMessage::RelaySignal(RelaySignal {
            session_id: plan.session_id,
            from_peer_id: descriptor.peer_id,
            to_peer_id: plan.target_host_id,
            signal: SessionSignal::Ready,
        }),
    )
    .await?;

    run_native_quic_viewer(
        plan.session_id,
        session,
        mute_playback,
        max_stream_frames,
        skip_window,
        list_streams,
        stream_registry,
        clipboard_sync,
        clipboard_formats,
        clipboard_poll_ms,
        mic,
        mic_device,
        mic_disable_aec,
        mic_disable_ns,
        mic_bitrate_bps,
        mic_frame_ms,
        decoder,
        renderer,
        tone_map,
        pen,
    )
    .await
}

async fn wait_for_native_quic_ticket(
    reader: &mut SignalingReader,
    session_id: Uuid,
    host_peer_id: Uuid,
) -> anyhow::Result<NativeQuicTicket> {
    let timeout = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            _ = &mut timeout => anyhow::bail!("timed out waiting for native QUIC ticket for session {}", session_id),
            frame = reader.next() => {
                let Some(frame) = frame else {
                    anyhow::bail!("signaling connection closed while waiting for native QUIC ticket");
                };
                match frame? {
                    Message::Text(text) => {
                        let message: ServerMessage = serde_json::from_str(text.as_ref())?;
                        match message {
                            ServerMessage::Signal(signal)
                                if signal.session_id == session_id
                                    && signal.from_peer_id == host_peer_id => {
                                if let SessionSignal::NativeQuicTicket { ticket_b64, .. } = signal.signal {
                                    tracing::debug!(
                                        %session_id,
                                        from_peer_id = %host_peer_id,
                                        "received native QUIC ticket relay signal"
                                    );
                                    return decode_ticket_b64(&ticket_b64);
                                }
                            }
                            ServerMessage::Error(error) => anyhow::bail!("{}: {}", error.code, error.message),
                            ServerMessage::HeartbeatAck
                            | ServerMessage::Welcome(_)
                            | ServerMessage::Presence(_)
                            | ServerMessage::Hosts { .. }
                            | ServerMessage::PairingRequested(_)
                            | ServerMessage::PairingEstablished(_)
                            | ServerMessage::PairingRejected { .. }
                            | ServerMessage::SessionPlanned(_)
                            | ServerMessage::SessionRequested(_)
                            | ServerMessage::Signal(_)
                            | ServerMessage::ShareLinkCreated { .. }
                            | ServerMessage::SessionKicked { .. }
                            | ServerMessage::PairingRevoked { .. }
                    | ServerMessage::SessionConsentPending { .. }
                    | ServerMessage::SessionBundleAccepted(_)
                    | ServerMessage::SignedKillReceived(_) => {}
                        }
                    }
                    Message::Close(_) => anyhow::bail!("signaling connection closed while waiting for native QUIC ticket"),
                    _ => {}
                }
            }
        }
    }
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

async fn run_native_quic_viewer(
    session_id: Uuid,
    session: NativeQuicClientSession,
    mute_playback: bool,
    max_stream_frames: u64,
    skip_window: bool,
    list_streams: bool,
    stream_registry: &StreamRegistry,
    clipboard_sync: CliClipboardSync,
    clipboard_formats: CliClipboardFormats,
    clipboard_poll_ms: u32,
    mic: bool,
    mic_device: Option<String>,
    mic_disable_aec: bool,
    mic_disable_ns: bool,
    mic_bitrate_bps: u32,
    mic_frame_ms: u8,
    decoder: CliDecoder,
    renderer: CliRenderer,
    tone_map: CliToneMap,
    pen: CliPenMode,
) -> anyhow::Result<()> {
    // Open the client→host control channel for clipboard + mic BEFORE
    // destructuring the session so we still have a method-bag around
    // the underlying `quinn::Connection`.
    let clip_mic_channel = match session.open_control_channel().await {
        Ok(ch) => Some(std::sync::Arc::new(tokio::sync::Mutex::new(ch))),
        Err(error) => {
            tracing::warn!(?error, "failed to open clip/mic control channel");
            None
        }
    };
    let quinn_conn = session.connection.clone();
    filesync_session::spawn_session_filesync(quinn_conn.clone(), session_id.to_string());

    let NativeQuicClientSession {
        video_config,
        audio_config,
        media_receiver,
        audio_receiver,
        input_sender,
        control_receiver,
        connection: _,
    } = session;
    tracing::info!(
        %session_id,
        width = video_config.width,
        height = video_config.height,
        fps = video_config.framerate,
        audio_sample_rate = audio_config.sample_rate,
        audio_channels = audio_config.channels,
        skip_window,
        renderer = ?renderer,
        tone_map = ?tone_map,
        pen = ?pen,
        // TODO(P2-14): wire --renderer wgpu into the run_video_window loop
        "starting native QUIC viewer"
    );

    // If --list-streams, we still connect and receive a frame to populate the registry
    let list_streams_request = list_streams;

    let (overlay_tx, overlay_rx) = mpsc::channel();
    let overlay_tx_for_control = overlay_tx.clone();
    let _overlay_controller = OverlayController::new(overlay_tx);

    // Spawn the control stream consumer (reads ControlMsg from the host)
    let registry_for_control = Arc::new(stream_registry.clone());
    let stats_collector = Arc::new(StatsCollector::new());
    let stats_for_control = stats_collector.clone();
    let control_task = tokio::spawn(receive_control_stream(
        control_receiver,
        registry_for_control,
        overlay_tx_for_control,
        stats_for_control,
    ));

    // Spawn the clipboard watcher if the user opted in.
    let mut clipboard_watcher: Option<ClipboardWatcher> = None;
    let clipboard_text_enabled = matches!(
        clipboard_formats,
        CliClipboardFormats::Text | CliClipboardFormats::Both
    );
    let clipboard_image_enabled = matches!(
        clipboard_formats,
        CliClipboardFormats::Image | CliClipboardFormats::Both
    );
    if !matches!(clipboard_sync, CliClipboardSync::Off) {
        if let Some(channel) = clip_mic_channel.clone() {
            let config = ClipboardSyncConfig {
                text_enabled: clipboard_text_enabled,
                image_enabled: clipboard_image_enabled,
                poll_interval: Duration::from_millis(clipboard_poll_ms.max(50) as u64),
            };
            let (clip_tx, mut clip_rx) = tokio_mpsc::unbounded_channel::<ControlMsg>();
            let watcher = ClipboardWatcher::new(config, clip_tx);
            clipboard_watcher = Some(watcher);
            tokio::spawn(async move {
                while let Some(msg) = clip_rx.recv().await {
                    let mut guard = channel.lock().await;
                    let _ = guard.send(&msg).await;
                }
            });
        }
    }
    if let Some(watcher) = clipboard_watcher.take() {
        tokio::spawn(watcher.run());
    }

    // Spawn the mic pipeline if --mic was set.
    let mut mic_pipeline: Option<qubox_mic::PipelineHandle> = None;
    let mut mic_capture: Option<qubox_mic::MicCapture> = None;
    if mic {
        if let Some(channel) = clip_mic_channel.clone() {
            let mic_config = MicStreamConfig {
                sample_rate_hz: 48_000,
                channels: 1,
                frame_ms: mic_frame_ms,
                bitrate_bps: mic_bitrate_bps,
                aec_enabled: !mic_disable_aec,
                ns_enabled: !mic_disable_ns,
                agc_enabled: !mic_disable_ns,
            };
            match qubox_mic::MicCapture::start(&mic_config, mic_device.as_deref()) {
                Ok(capture) => {
                    let (out_tx, mut out_rx) =
                        tokio::sync::mpsc::unbounded_channel::<qubox_mic::EncodedMicFrame>();
                    let ring = capture.ring.clone();
                    let reference_tap = qubox_mic::ReferenceAudioTap::new(960);
                    match qubox_mic::spawn_pipeline(
                        mic_config.clone(),
                        ring,
                        reference_tap.clone(),
                        out_tx,
                    ) {
                        Ok(handle) => {
                            mic_pipeline = Some(handle);
                            mic_capture = Some(capture);
                            {
                                let mut guard = channel.lock().await;
                                let _ = guard
                                    .send(&ControlMsg::MicStart {
                                        config: mic_config.clone(),
                                    })
                                    .await;
                            }
                            let conn_for_datagrams = quinn_conn.clone();
                            tokio::spawn(async move {
                                while let Some(frame) = out_rx.recv().await {
                                    let _ = conn_for_datagrams
                                        .send_datagram(bytes::Bytes::from(frame.bytes));
                                }
                            });
                        }
                        Err(error) => {
                            tracing::warn!(?error, "mic pipeline spawn failed");
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(?error, "mic capture start failed");
                }
            }
        }
    }

    // Spawn the pen capture pipeline if --pen=client-to-host was set.
    // For v0.1.0 the platform capture stubs return empty device lists
    // so no events actually flow until the libinput/WM_POINTER APIs
    // are completed in a follow-up PR.
    let _pen_capture_handle: Option<PenCaptureHandle> = if pen == CliPenMode::ClientToHost {
        match setup_pen_capture(quinn_conn.clone()) {
            Ok(handle) => {
                tracing::info!("pen capture started (stub — no events until platform APIs land)");
                Some(handle)
            }
            Err(error) => {
                tracing::warn!(?error, "pen capture setup failed; --pen ignored");
                None
            }
        }
    } else {
        None
    };

    if skip_window && !list_streams_request {
        let network_task = tokio::spawn(receive_media_stream_skip_window(
            media_receiver,
            max_stream_frames,
        ));
        let audio_task = tokio::spawn(receive_audio_stream(audio_receiver, None));
        drop(input_sender);

        tlm::eprintln_status("skipping video window; consuming media stream in background");
        let received_frames = network_task.await??;
        let _ = audio_task.await;
        tlm::eprintln_status(&format!(
            "skip-window session {session_id} received {received_frames} compressed frames",
        ));
        if tlm::is_enabled() {
            tlm::emit(&TelemetryEvent::SessionEnded {
                reason: "skip_window_complete".to_string(),
            });
        }
        return Ok(());
    }

    let (encoded_tx, encoded_rx) = mpsc::channel();
    let (decoded_tx, decoded_rx) = mpsc::channel();
    let decoder = spawn_chosen_decoder(decoder, &video_config, encoded_rx, decoded_tx)?;
    let audio_playback = if mute_playback {
        tlm::eprintln_status("client audio playback muted; incoming audio will be discarded");
        None
    } else {
        Some(RunningAudioPlayback::start(&audio_config)?)
    };
    let reg = stream_registry;
    let registry_for_network = Arc::new(reg.clone());
    let stats_for_network = stats_collector.clone();
    let network_task = tokio::spawn(receive_media_stream_registry(
        media_receiver,
        encoded_tx,
        max_stream_frames,
        registry_for_network,
        stats_for_network,
    ));
    let audio_task = tokio::spawn(receive_audio_stream(
        audio_receiver,
        audio_playback.as_ref().map(RunningAudioPlayback::handle),
    ));
    let (input_tx, input_rx) = tokio_mpsc::unbounded_channel();
    let input_task = tokio::spawn(send_input_events(input_sender, input_rx));

    if list_streams_request {
        let first_stream = loop {
            let streams = reg.list_streams();
            if !streams.is_empty() {
                break streams;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        print_stream_registry_table(&first_stream);
        network_task.abort();
        audio_task.abort();
        control_task.abort();
        decoder.shutdown(true)?;
        return Ok(());
    }

    let outcome = if reg.is_tile_mode() {
        run_tiled_view(
            &format!("qubox session {session_id}"),
            decoded_rx,
            &input_tx,
            max_stream_frames,
            &overlay_rx,
            reg,
            stats_collector.clone(),
            video_config.width,
            video_config.height,
        )?
    } else {
        run_video_window(
            &format!("qubox session {session_id}"),
            &video_config,
            decoded_rx,
            &input_tx,
            max_stream_frames,
            &overlay_rx,
            reg,
            stats_collector.clone(),
        )?
    };

    if tlm::is_enabled() {
        tlm::emit(&TelemetryEvent::FrameRendered {
            rendered: outcome.rendered_frames,
            skipped: 0,
        });
    }

    drop(input_tx);

    let received_frames = if outcome.closed_by_user && !outcome.stream_ended {
        network_task.abort();
        audio_task.abort();
        control_task.abort();
        0
    } else {
        network_task.await??
    };

    let received_audio_chunks = if outcome.closed_by_user && !outcome.stream_ended {
        0
    } else {
        audio_task.await??
    };

    if let Some(handle) = mic_pipeline.take() {
        handle.shutdown();
    }
    drop(mic_capture.take());

    let _ = input_task.await;
    let _ = control_task.await;
    decoder.shutdown(outcome.closed_by_user)?;

    println!(
        "interactive session {} rendered {} decoded frames and received {} compressed frames and {} audio chunks{}",
        session_id,
        outcome.rendered_frames,
        received_frames,
        received_audio_chunks,
        if outcome.closed_by_user && !outcome.stream_ended {
            " before the remote stream ended"
        } else {
            ""
        }
    );

    if tlm::is_enabled() {
        let reason = if outcome.closed_by_user {
            "user_quit"
        } else if outcome.stream_ended {
            "stream_ended"
        } else {
            "completed"
        };
        tlm::emit(&TelemetryEvent::SessionEnded {
            reason: reason.to_string(),
        });
    }

    Ok(())
}

async fn receive_media_stream(
    mut media_receiver: NativeQuicMediaReceiver,
    encoded_tx: Sender<Vec<u8>>,
    max_stream_frames: u64,
) -> anyhow::Result<u64> {
    let mut received_frames = 0_u64;

    while let Some(access_unit) = media_receiver.read_access_unit().await? {
        if received_frames == 0 {
            tracing::info!(
                frame_id = access_unit.frame_id,
                timestamp_micros = access_unit.timestamp_micros,
                keyframe = access_unit.keyframe,
                bytes = access_unit.bytes.len(),
                "received first native QUIC video access unit"
            );
        }

        if encoded_tx.send(access_unit.bytes).is_err() {
            break;
        }

        received_frames += 1;
        if max_stream_frames > 0 && received_frames >= max_stream_frames {
            break;
        }
    }

    Ok(received_frames)
}

/// Like `receive_media_stream` but discards the encoded data.
/// Used by the `--skip-window` path.
async fn receive_media_stream_skip_window(
    mut media_receiver: NativeQuicMediaReceiver,
    max_stream_frames: u64,
) -> anyhow::Result<u64> {
    let mut received_frames = 0_u64;

    while let Some(_access_unit) = media_receiver.read_access_unit().await? {
        received_frames += 1;
        if max_stream_frames > 0 && received_frames >= max_stream_frames {
            break;
        }
    }

    Ok(received_frames)
}

/// Reads media frames AND populates the StreamRegistry from frame metadata.
///
/// For the single-stream path, all frames map to DisplayId(0). A multi-stream
/// pipeline will expose per-stream display_ids from WireAccessUnitHeader once
/// the transport crate makes that info public (future phase).
async fn receive_media_stream_registry(
    mut media_receiver: NativeQuicMediaReceiver,
    encoded_tx: Sender<Vec<u8>>,
    max_stream_frames: u64,
    stream_registry: Arc<StreamRegistry>,
    stats: Arc<StatsCollector>,
) -> anyhow::Result<u64> {
    let mut received_frames = 0_u64;

    while let Some(access_unit) = media_receiver.read_access_unit().await? {
        if received_frames == 0 {
            tracing::info!(
                frame_id = access_unit.frame_id,
                timestamp_micros = access_unit.timestamp_micros,
                keyframe = access_unit.keyframe,
                bytes = access_unit.bytes.len(),
                "received first native QUIC video access unit via registry"
            );
        }

        let display_id = DisplayId(access_unit.display_id);
        let now = std::time::Instant::now();

        if !stream_registry.has_stream(display_id) {
            stream_registry.add_stream(StreamEntry {
                display_id,
                width: access_unit.width,
                height: access_unit.height,
                refresh_hz: 0.0,
                color_space: qubox_display::ColorSpaceId::Srgb,
                fps: 0.0,
                privacy_state: DisplayState::Active,
                first_frame_at: now,
                last_frame_at: now,
                frame_count: 0,
            });
        }

        stream_registry.update_fps(display_id, 60.0);

        stats.record_frame_decoded(access_unit.bytes.len(), now);

        if tlm::is_enabled() {
            tlm::emit(&TelemetryEvent::FrameDecoded {
                frame_id: access_unit.frame_id as u32,
                bytes: access_unit.bytes.len() as u32,
                keyframe: access_unit.keyframe,
            });
        }

        if encoded_tx.send(access_unit.bytes).is_err() {
            break;
        }

        received_frames += 1;
        if max_stream_frames > 0 && received_frames >= max_stream_frames {
            break;
        }
    }

    Ok(received_frames)
}

/// Reads ControlMsg from the host→client control uni-stream and dispatches
/// them to the StreamRegistry, overlay channel, and stats collector.
async fn receive_control_stream(
    mut control_receiver: NativeQuicControlReceiver,
    stream_registry: Arc<StreamRegistry>,
    overlay_tx: Sender<OverlayCommand>,
    stats: Arc<StatsCollector>,
) -> anyhow::Result<()> {
    let clipboard_applier = ClipboardApplier::new();
    let mut last_clipboard_seq: u64 = 0;

    while let Some(msg) = control_receiver.read_control_msg().await? {
        if tlm::is_enabled() {
            if let Ok(value) = serde_json::to_value(&msg) {
                tlm::emit(&TelemetryEvent::Control { msg: value });
            }
        }
        match msg {
            ControlMsg::DisplayStateChanged {
                display_id,
                old_state: _,
                new_state,
            } => {
                let state = match new_state {
                    0 => DisplayState::Active,
                    1 => DisplayState::Privacy,
                    _ => DisplayState::Blanked,
                };
                stream_registry.update_privacy_state(DisplayId(display_id), state);
                tracing::info!(
                    display_id,
                    ?state,
                    "privacy state changed via control stream"
                );
            }
            ControlMsg::StreamUnsubscribe { display_ids } => {
                for did in &display_ids {
                    stream_registry.remove_stream(DisplayId(*did));
                    tracing::info!(display_id = did, "stream removed via control stream");
                }
            }
            ControlMsg::StreamSubscribe { display_ids: _ } => {
                // Client→Host message; ignored on the receiver side.
            }
            ControlMsg::BlankOverlay { show, display_id } => {
                let _ = overlay_tx.send(OverlayCommand { show, display_id });
                tracing::info!(
                    show,
                    ?display_id,
                    "blank overlay command received via control stream"
                );
            }
            ControlMsg::RateFeedback(rf) => {
                stats.record(TelemetrySnapshot {
                    rtt_ms: rf.rtt_ms,
                    loss_x1000: rf.loss_x1000,
                    jitter_ms: rf.jitter_ms,
                    one_way_delay_ms: rf.one_way_delay_ms,
                    ..Default::default()
                });
            }
            ControlMsg::StreamStats {
                frames_decoded,
                frames_dropped,
                frames_recovered,
                ..
            } => {
                stats.record(TelemetrySnapshot {
                    frames_decoded,
                    frames_dropped,
                    frames_recovered,
                    ..Default::default()
                });
            }
            ControlMsg::ClipboardChanged { .. } => {
                if let Err(error) = clipboard_applier.apply(&msg, &mut last_clipboard_seq) {
                    tracing::warn!(?error, "clipboard apply failed; ignoring");
                }
            }
            ControlMsg::MicConfigAck {
                config,
                virtual_device_ok,
            } => {
                tracing::info!(
                    ?config,
                    virtual_device_ok,
                    "received mic config ack from host"
                );
            }
            ControlMsg::DisplayCapabilities {
                hdr_static_metadata: _,
                max_resolution: _,
                max_refresh_hz: _,
            } => {
                let view = msg.display_capabilities();
                tracing::info!(?view, "host advertised display capabilities");
            }
            ControlMsg::PenDeviceList { devices } => {
                tracing::info!(?devices, "host advertised pen device list (re-broadcast)");
            }
            ControlMsg::PenEvent {
                device_id,
                tool,
                contact,
            } => {
                tracing::debug!(device_id, ?tool, contact, "pen event received");
            }
            ControlMsg::Nack { .. }
            | ControlMsg::KeyframeRequest { .. }
            | ControlMsg::GamepadConnect { .. }
            | ControlMsg::GamepadDisconnect { .. }
            | ControlMsg::GamepadRumble { .. }
            | ControlMsg::MicStart { .. }
            | ControlMsg::MicStop => {
                tracing::debug!("received unhandled control message type (ignoring)");
            }
        }
    }
    Ok(())
}

/// Print the stream registry as a formatted table. In telemetry mode,
/// the table is written to **stderr** so the stdout stream remains pure
/// JSON.
fn print_stream_registry_table(entries: &[StreamEntry]) {
    let mut out = String::new();
    out.push_str(&format!("{:-<72}\n", ""));
    out.push_str(&format!(
        " {:<12} | {:<10} | {:<10} | {:<12} | {:<7} | {:<9} | {:<8}\n",
        "display_id", "size", "refresh_hz", "color_space", "fps", "privacy", "uptime"
    ));
    out.push_str(&format!("{:-<72}\n", ""));
    for entry in entries {
        let size = format!("{}x{}", entry.width, entry.height);
        let cs = format!("{:?}", entry.color_space);
        let privacy = format!("{:?}", entry.privacy_state);
        let uptime = {
            let elapsed = entry.last_frame_at.duration_since(entry.first_frame_at);
            let secs = elapsed.as_secs();
            if secs < 60 {
                format!("{}s", secs)
            } else {
                format!("{}m{:02}s", secs / 60, secs % 60)
            }
        };
        out.push_str(&format!(
            " {:<12} | {:<10} | {:<10.1} | {:<12} | {:<7.1} | {:<9} | {:<8}\n",
            entry.display_id.0, size, entry.refresh_hz, cs, entry.fps, privacy, uptime,
        ));
    }
    out.push_str(&format!("{:-<72}\n", ""));
    if tlm::is_enabled() {
        eprint!("{out}");
    } else {
        print!("{out}");
    }
}

async fn receive_audio_stream(
    mut audio_receiver: NativeQuicAudioReceiver,
    audio_playback: Option<AudioPlaybackHandle>,
) -> anyhow::Result<u64> {
    let mut received_chunks = 0_u64;

    while let Some(chunk) = audio_receiver.read_audio_chunk().await? {
        if let Some(audio_playback) = audio_playback.as_ref() {
            audio_playback.push_chunk(&chunk.bytes)?;
        }
        received_chunks += 1;
    }

    Ok(received_chunks)
}

async fn send_input_events(
    mut input_sender: NativeQuicInputSender,
    mut input_rx: tokio_mpsc::UnboundedReceiver<RemoteInputEvent>,
) -> anyhow::Result<()> {
    let mut pending_event = None;

    while let Some(event) = next_outbound_input_event(&mut pending_event, &mut input_rx).await {
        input_sender.send_input_event(&event).await?;
    }

    let _ = input_sender.finish().await;
    Ok(())
}

fn run_video_window(
    title: &str,
    video_config: &VideoStreamParams,
    frame_rx: Receiver<Vec<u32>>,
    input_tx: &tokio_mpsc::UnboundedSender<RemoteInputEvent>,
    max_stream_frames: u64,
    overlay_rx: &Receiver<OverlayCommand>,
    stream_registry: &StreamRegistry,
    stats: Arc<StatsCollector>,
) -> anyhow::Result<WindowLoopOutcome> {
    let width = video_config.width as usize;
    let height = video_config.height as usize;
    tracing::debug!(title, width, height, "opening native QUIC viewer window");
    let mut window = Window::new(title, width, height, WindowOptions::default())
        .with_context(|| format!("failed to open client window for {title}"))?;
    window.set_target_fps(60);

    let mut frame = vec![0_u32; width * height];
    let mut rendered_frames = 0_u64;
    let mut stream_ended = false;
    let mut last_mouse = None;
    let mut button_states = [false; 3];
    let mut overlay = BlankOverlayWindow::new();

    while window.is_open() {
        if window.is_key_down(Key::Escape) {
            overlay.hide();
            return Ok(WindowLoopOutcome {
                rendered_frames,
                closed_by_user: true,
                stream_ended,
            });
        }

        // Process overlay commands (show/hide blank overlay from host)
        qubox_client_cli::blank_overlay::process_overlay_commands(overlay_rx, &mut overlay);

        loop {
            match frame_rx.try_recv() {
                Ok(next_frame) => {
                    frame = next_frame;
                    rendered_frames += 1;
                    if rendered_frames == 1 {
                        tracing::info!(title, "rendered first decoded video frame");
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    stream_ended = true;
                    break;
                }
            }
        }

        // ── Keyboard shortcuts ──
        let ctrl = window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl);
        if ctrl {
            if window.is_key_pressed(Key::T, KeyRepeat::No) {
                stream_registry.set_tile_mode(!stream_registry.is_tile_mode());
                tracing::info!(
                    tile = stream_registry.is_tile_mode(),
                    "Ctrl+T: toggled tile mode"
                );
            }
            if window.is_key_pressed(Key::S, KeyRepeat::No) {
                stream_registry.cycle_selected_stream();
                tracing::info!(display_id = ?stream_registry.get_selected_stream(), "Ctrl+S: cycled to stream");
            }
            if window.is_key_pressed(Key::P, KeyRepeat::No) {
                stream_registry
                    .set_show_privacy_indicator(!stream_registry.should_show_privacy_indicator());
                tracing::info!(
                    show = stream_registry.should_show_privacy_indicator(),
                    "Ctrl+P: toggled privacy indicator"
                );
            }
        }

        if stats_overlay::hotkey_pressed(&window) {
            let now_visible = stats.toggle_visibility();
            tracing::info!(visible = now_visible, "Ctrl+Alt+S: toggled stats overlay");
        }

        pump_window_input(&mut window, input_tx, &mut last_mouse, &mut button_states);

        // ── Privacy indicator overlay (single-stream view) ──
        if stream_registry.should_show_privacy_indicator() {
            if let Some(sel_id) = stream_registry.get_selected_stream() {
                if let Some(entry) = stream_registry.get_stream(sel_id) {
                    if entry.privacy_state == DisplayState::Privacy {
                        qubox_client_cli::privacy_indicator::apply_red_overlay(&mut frame);
                    }
                }
            }
        }

        let now = std::time::Instant::now();
        stats.record(TelemetrySnapshot {
            stream_count: stream_registry.stream_count(),
            ..Default::default()
        });
        let overlay_data = stats.render_data();
        if overlay_data.visible {
            paint_overlay(&mut frame, width, height, &overlay_data);
        }
        stats.record_rendered_frame(now);

        window
            .update_with_buffer(&frame, width, height)
            .context("failed to paint decoded frame")?;

        // Paint overlay black if visible
        qubox_client_cli::blank_overlay::paint_overlay_black(&mut overlay);

        if max_stream_frames > 0 && rendered_frames >= max_stream_frames {
            overlay.hide();
            return Ok(WindowLoopOutcome {
                rendered_frames,
                closed_by_user: true,
                stream_ended,
            });
        }

        if stream_ended {
            overlay.hide();
            return Ok(WindowLoopOutcome {
                rendered_frames,
                closed_by_user: false,
                stream_ended: true,
            });
        }
    }

    overlay.hide();
    Ok(WindowLoopOutcome {
        rendered_frames,
        closed_by_user: true,
        stream_ended,
    })
}

/// Runs the tiled view loop (grid of all streams).
#[allow(unused_variables)]
fn run_tiled_view(
    title: &str,
    frame_rx: Receiver<Vec<u32>>,
    input_tx: &tokio_mpsc::UnboundedSender<RemoteInputEvent>,
    max_stream_frames: u64,
    overlay_rx: &Receiver<OverlayCommand>,
    stream_registry: &StreamRegistry,
    stats: Arc<StatsCollector>,
    source_width: u32,
    source_height: u32,
) -> anyhow::Result<WindowLoopOutcome> {
    let mut tiled_view = TiledView::new(stream_registry.clone())
        .map_err(|e| anyhow!("failed to create tiled view: {e}"))?;
    tiled_view.window.set_target_fps(60);

    let mut frame = Vec::new();
    let mut rendered_frames = 0_u64;
    let mut stream_ended = false;
    let mut last_mouse = None;
    let mut button_states = [false; 3];
    let mut overlay = BlankOverlayWindow::new();

    while tiled_view.is_open() {
        if tiled_view.window.is_key_down(Key::Escape) {
            overlay.hide();
            return Ok(WindowLoopOutcome {
                rendered_frames,
                closed_by_user: true,
                stream_ended,
            });
        }

        // Process overlay commands
        qubox_client_cli::blank_overlay::process_overlay_commands(overlay_rx, &mut overlay);

        loop {
            match frame_rx.try_recv() {
                Ok(next_frame) => {
                    frame = next_frame;
                    rendered_frames += 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    stream_ended = true;
                    break;
                }
            }
        }

        // ── Keyboard shortcuts ──
        let ctrl = tiled_view.window.is_key_down(Key::LeftCtrl)
            || tiled_view.window.is_key_down(Key::RightCtrl);
        if ctrl {
            if tiled_view.window.is_key_pressed(Key::T, KeyRepeat::No) {
                stream_registry.set_tile_mode(!stream_registry.is_tile_mode());
                tracing::info!(
                    tile = stream_registry.is_tile_mode(),
                    "Ctrl+T: toggled tile mode"
                );
            }
            if tiled_view.window.is_key_pressed(Key::S, KeyRepeat::No) {
                stream_registry.cycle_selected_stream();
                tracing::info!(display_id = ?stream_registry.get_selected_stream(), "Ctrl+S: cycled to stream");
            }
            if tiled_view.window.is_key_pressed(Key::P, KeyRepeat::No) {
                stream_registry
                    .set_show_privacy_indicator(!stream_registry.should_show_privacy_indicator());
                tracing::info!(
                    show = stream_registry.should_show_privacy_indicator(),
                    "Ctrl+P: toggled privacy indicator"
                );
            }
        }

        if stats_overlay::hotkey_pressed(&tiled_view.window) {
            let now_visible = stats.toggle_visibility();
            tracing::info!(visible = now_visible, "Ctrl+Alt+S: toggled stats overlay");
        }

        pump_window_input(
            &mut tiled_view.window,
            input_tx,
            &mut last_mouse,
            &mut button_states,
        );

        let now = std::time::Instant::now();
        stats.record(TelemetrySnapshot {
            stream_count: stream_registry.stream_count(),
            ..Default::default()
        });

        if stats.is_visible() && !frame.is_empty() {
            let overlay_data = stats.render_data();
            paint_overlay(
                &mut frame,
                source_width as usize,
                source_height as usize,
                &overlay_data,
            );
        }

        // Push decoded frame to the appropriate tile.
        // With a single ffmpeg decoder, all frames use the selected stream's ID.
        if !frame.is_empty() {
            if let Some(sel_id) = stream_registry.get_selected_stream() {
                let _ = tiled_view.render_frame(sel_id, &frame, source_width, source_height);
            } else {
                // No selected stream yet: use the first discovered stream
                let streams = stream_registry.list_streams();
                if let Some(first) = streams.first() {
                    let _ = tiled_view.render_frame(
                        first.display_id,
                        &frame,
                        source_width,
                        source_height,
                    );
                }
            }
        } else {
            let _ = tiled_view.redraw();
        }

        stats.record_rendered_frame(now);

        qubox_client_cli::blank_overlay::paint_overlay_black(&mut overlay);

        if max_stream_frames > 0 && rendered_frames >= max_stream_frames {
            overlay.hide();
            return Ok(WindowLoopOutcome {
                rendered_frames,
                closed_by_user: true,
                stream_ended,
            });
        }

        if stream_ended {
            overlay.hide();
            return Ok(WindowLoopOutcome {
                rendered_frames,
                closed_by_user: false,
                stream_ended: true,
            });
        }
    }

    overlay.hide();
    Ok(WindowLoopOutcome {
        rendered_frames,
        closed_by_user: true,
        stream_ended,
    })
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Handle returned by [`setup_pen_capture`] that owns the capture
/// threads. Dropping this handle signals the threads to stop.
struct PenCaptureHandle {
    _stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _device_threads: Vec<std::thread::JoinHandle<()>>,
    /// The tokio task that reads from the outbound datagram channel
    /// and forwards encoded pen events over the QUIC connection.
    _datagram_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for PenCaptureHandle {
    fn drop(&mut self) {
        self._stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(task) = self._datagram_task.take() {
            task.abort();
        }
    }
}

/// Set up the pen capture pipeline when `--pen=client-to-host` is
/// active. For v0.1.0 the platform stubs return empty device lists
/// so the capture is effectively a no-op; the scaffolding is correct
/// and events will flow once libinput/Windows tablet APIs land.
fn setup_pen_capture(
    conn: qubox_transport::NativeQuicConnection,
) -> anyhow::Result<PenCaptureHandle> {
    use qubox_pen::PenCapture;
    #[cfg(target_os = "linux")]
    let mut capture: Box<dyn PenCapture + Send> =
        Box::new(qubox_pen::linux::LibinputCapture::default());
    #[cfg(not(target_os = "linux"))]
    let mut capture: Box<dyn PenCapture + Send> = Box::new(qubox_pen::platform::StubCapture::new());

    let devices = capture.enumerate_devices()?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (datagram_tx, datagram_rx) = crossbeam_channel::bounded::<Vec<u8>>(64);
    let mut device_threads = Vec::new();

    for dev in &devices {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let _cap_rx = capture.start(event_tx)?;
        let stop_clone = stop.clone();
        let tx_clone = datagram_tx.clone();
        let dev_name = dev.descriptor.name.clone();
        let dev_id = dev.descriptor.device_id;

        let thread_handle = std::thread::Builder::new()
            .name(format!("bp-pen-capture-{dev_id}"))
            .spawn(move || {
                while !stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    match event_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                        Ok(event) => {
                            let buf = qubox_transport::media::encode_pen_datagram(&event);
                            let _ = tx_clone.send(buf);
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
                tracing::debug!(device = %dev_name, "pen capture thread exiting");
            })
            .context("failed to spawn pen capture thread")?;
        device_threads.push(thread_handle);
    }

    let datagram_task = {
        Some(tokio::spawn(async move {
            while let Ok(buf) = datagram_rx.recv() {
                if conn.send_datagram(bytes::Bytes::from(buf)).is_err() {
                    tracing::debug!("pen datagram send failed; connection may be closed");
                    break;
                }
            }
        }))
    };

    Ok(PenCaptureHandle {
        _stop: stop,
        _device_threads: device_threads,
        _datagram_task: datagram_task,
    })
}

fn pump_window_input(
    window: &mut Window,
    input_tx: &tokio_mpsc::UnboundedSender<RemoteInputEvent>,
    last_mouse: &mut Option<(u32, u32)>,
    button_states: &mut [bool; 3],
) {
    if !window.is_active() {
        *last_mouse = None;
        return;
    }

    if let Some((mouse_x, mouse_y)) = window.get_mouse_pos(MouseMode::Clamp) {
        let position = (mouse_x.max(0.0) as u32, mouse_y.max(0.0) as u32);
        if Some(position) != *last_mouse {
            let _ = input_tx.send(RemoteInputEvent::MouseMove {
                x: position.0,
                y: position.1,
            });
            *last_mouse = Some(position);
        }
    }

    for (index, (button, mapped_button)) in [
        (MouseButton::Left, InputMouseButton::Left),
        (MouseButton::Right, InputMouseButton::Right),
        (MouseButton::Middle, InputMouseButton::Middle),
    ]
    .into_iter()
    .enumerate()
    {
        let pressed = window.get_mouse_down(button);
        if pressed != button_states[index] {
            button_states[index] = pressed;
            let _ = input_tx.send(RemoteInputEvent::MouseButton {
                button: mapped_button,
                pressed,
            });
        }
    }

    for key in window.get_keys_pressed(KeyRepeat::No) {
        let _ = input_tx.send(RemoteInputEvent::Keyboard {
            key: format!("{key:?}"),
            pressed: true,
        });
    }

    for key in window.get_keys_released() {
        let _ = input_tx.send(RemoteInputEvent::Keyboard {
            key: format!("{key:?}"),
            pressed: false,
        });
    }
}

async fn next_outbound_input_event(
    pending_event: &mut Option<RemoteInputEvent>,
    input_rx: &mut tokio_mpsc::UnboundedReceiver<RemoteInputEvent>,
) -> Option<RemoteInputEvent> {
    let mut event = if let Some(event) = pending_event.take() {
        event
    } else {
        input_rx.recv().await?
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

fn decoder_writer_loop(mut stdin: ChildStdin, encoded_rx: Receiver<Vec<u8>>) -> anyhow::Result<()> {
    let mut written_frames = 0_u64;

    while let Ok(access_unit) = encoded_rx.recv() {
        if written_frames == 0 {
            tracing::info!(
                bytes = access_unit.len(),
                "writing first H.264 access unit into ffmpeg decoder"
            );
        }
        stdin
            .write_all(&access_unit)
            .context("failed to write H.264 access unit into ffmpeg decoder")?;
        stdin
            .flush()
            .context("failed to flush ffmpeg decoder stdin")?;
        written_frames += 1;
    }

    tracing::debug!(
        written_frames,
        "ffmpeg decoder stdin closed after access unit stream ended"
    );
    Ok(())
}

fn is_benign_decoder_pipe_end(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .map(|io_error| {
                io_error.kind() == ErrorKind::BrokenPipe || io_error.raw_os_error() == Some(109)
            })
            .unwrap_or(false)
    })
}

fn decoder_reader_loop(
    mut stdout: ChildStdout,
    width: u32,
    height: u32,
    decoded_tx: Sender<Vec<u32>>,
) -> anyhow::Result<()> {
    let frame_len = width as usize * height as usize * 4;
    let mut decoded_frames = 0_u64;

    loop {
        let mut bgra = vec![0_u8; frame_len];
        match stdout.read_exact(&mut bgra) {
            Ok(()) => {
                decoded_frames += 1;
                if decoded_frames == 1 {
                    tracing::info!(
                        width,
                        height,
                        frame_len,
                        "received first decoded BGRA frame from ffmpeg"
                    );
                }
                if decoded_tx.send(bgra_to_window_frame(&bgra)).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
                tracing::warn!(
                    decoded_frames,
                    "ffmpeg decoder stdout closed before another complete BGRA frame was available"
                );
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }

    Ok(())
}

fn decoder_stderr_loop(stderr: impl Read) -> anyhow::Result<()> {
    let reader = BufReader::new(stderr);

    for line in reader.lines() {
        let line = line.context("failed to read ffmpeg decoder stderr")?;
        if !line.trim().is_empty() {
            tracing::warn!(ffmpeg = %line, "ffmpeg decoder stderr");
        }
    }

    Ok(())
}

fn bgra_to_window_frame(bgra: &[u8]) -> Vec<u32> {
    bgra.chunks_exact(4)
        .map(|pixel| {
            let blue = u32::from(pixel[0]);
            let green = u32::from(pixel[1]);
            let red = u32::from(pixel[2]);
            (red << 16) | (green << 8) | blue
        })
        .collect()
}

fn decode_audio_chunk_samples(bytes: &[u8]) -> anyhow::Result<Vec<f32>> {
    if bytes.len() % std::mem::size_of::<f32>() != 0 {
        anyhow::bail!(
            "audio chunk length {} was not aligned to f32 samples",
            bytes.len()
        );
    }

    Ok(bytes
        .chunks_exact(std::mem::size_of::<f32>())
        .map(|sample| f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]))
        .collect())
}

fn fill_audio_output_buffer_f32(data: &mut [f32], queue: &Arc<Mutex<VecDeque<f32>>>) {
    fill_audio_output_buffer(data, queue, |sample| sample);
}

fn fill_audio_output_buffer_i16(data: &mut [i16], queue: &Arc<Mutex<VecDeque<f32>>>) {
    fill_audio_output_buffer(data, queue, f32_to_i16_sample);
}

fn fill_audio_output_buffer_u16(data: &mut [u16], queue: &Arc<Mutex<VecDeque<f32>>>) {
    fill_audio_output_buffer(data, queue, f32_to_u16_sample);
}

fn fill_audio_output_buffer<T>(
    data: &mut [T],
    queue: &Arc<Mutex<VecDeque<f32>>>,
    convert: impl Fn(f32) -> T,
) {
    if let Ok(mut queue) = queue.lock() {
        for sample in data.iter_mut() {
            *sample = convert(queue.pop_front().unwrap_or(0.0));
        }
    } else {
        for sample in data.iter_mut() {
            *sample = convert(0.0);
        }
    }
}

fn f32_to_i16_sample(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

fn f32_to_u16_sample(sample: f32) -> u16 {
    (((sample.clamp(-1.0, 1.0) + 1.0) * 0.5) * f32::from(u16::MAX)).round() as u16
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use qubox_display::ColorSpaceId;

    use super::*;

    fn test_host(name: &str, id: Uuid) -> PeerDescriptor {
        PeerDescriptor {
            device_id: Uuid::new_v4(),
            peer_id: id,
            device_name: name.to_string(),
            role: PeerRole::Host,
            os: qubox_proto::PlatformOs::Windows,
            capabilities: Default::default(),
        }
    }

    #[test]
    fn resolve_host_in_inventory_accepts_display_name_or_uuid() {
        let alpha_id = Uuid::new_v4();
        let beta_id = Uuid::new_v4();
        let hosts = vec![
            test_host("AlphaRig", alpha_id),
            test_host("BetaRig", beta_id),
        ];

        assert_eq!(
            resolve_host_in_inventory(&hosts, "AlphaRig")
                .unwrap()
                .peer_id,
            alpha_id
        );
        assert_eq!(
            resolve_host_in_inventory(&hosts, &beta_id.to_string())
                .unwrap()
                .peer_id,
            beta_id
        );
        assert!(resolve_host_in_inventory(&hosts, "MissingRig").is_err());
    }

    #[test]
    fn decode_audio_chunk_samples_round_trips_f32_bytes() {
        let bytes = vec![0, 0, 128, 63, 0, 0, 0, 191];

        assert_eq!(decode_audio_chunk_samples(&bytes).unwrap(), vec![1.0, -0.5]);
    }

    #[test]
    fn control_stream_display_state_changed_updates_registry() {
        let reg = StreamRegistry::new();
        reg.add_stream(StreamEntry {
            display_id: DisplayId(1),
            width: 1920,
            height: 1080,
            refresh_hz: 60.0,
            color_space: ColorSpaceId::Srgb,
            fps: 0.0,
            privacy_state: DisplayState::Active,
            first_frame_at: Instant::now(),
            last_frame_at: Instant::now(),
            frame_count: 0,
        });

        // Simulate ControlMsg::DisplayStateChanged dispatch (new_state = 1 = Privacy)
        let state = match 1_u8 {
            0 => DisplayState::Active,
            1 => DisplayState::Privacy,
            _ => DisplayState::Blanked,
        };
        reg.update_privacy_state(DisplayId(1), state);

        let entry = reg.get_stream(DisplayId(1)).unwrap();
        assert_eq!(entry.privacy_state, DisplayState::Privacy);
    }

    #[test]
    fn control_stream_stream_unsubscribe_removes_stream() {
        let reg = StreamRegistry::new();
        reg.add_stream(StreamEntry {
            display_id: DisplayId(1),
            width: 1920,
            height: 1080,
            refresh_hz: 60.0,
            color_space: ColorSpaceId::Srgb,
            fps: 0.0,
            privacy_state: DisplayState::Active,
            first_frame_at: Instant::now(),
            last_frame_at: Instant::now(),
            frame_count: 0,
        });
        reg.add_stream(StreamEntry {
            display_id: DisplayId(2),
            ..Default::default()
        });

        assert_eq!(reg.stream_count(), 2);

        // Simulate ControlMsg::StreamUnsubscribe dispatch
        reg.remove_stream(DisplayId(1));
        assert_eq!(reg.stream_count(), 1);
        assert!(reg.get_stream(DisplayId(1)).is_none());
        assert!(reg.get_stream(DisplayId(2)).is_some());
    }

    #[test]
    fn pen_args_default_to_off() {
        use clap::Parser;
        let args =
            Args::try_parse_from(["qubox-client-cli", "start-session", "--host", "dummy-host"])
                .expect("start-session args should parse with defaults");
        match args.command {
            Command::StartSession { pen, .. } => {
                assert_eq!(pen, CliPenMode::Off);
            }
            _ => panic!("expected StartSession subcommand"),
        }
    }

    #[test]
    fn control_stream_blank_overlay_sends_command() {
        let (tx, rx) = mpsc::channel::<OverlayCommand>();

        // Simulate ControlMsg::BlankOverlay dispatch
        let cmd = OverlayCommand {
            show: true,
            display_id: Some(0),
        };
        tx.send(cmd.clone()).unwrap();

        let received = rx.recv().unwrap();
        assert!(received.show);
        assert_eq!(received.display_id, Some(0));
    }
}
