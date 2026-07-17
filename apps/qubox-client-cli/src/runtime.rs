use std::{
    collections::VecDeque,
    io::{ErrorKind, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command as ProcessCommand, Stdio},
    sync::{
        mpsc::{self, Receiver, Sender, TryRecvError},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use anyhow::{anyhow, Context};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    SampleFormat, Stream,
};
use futures::{stream::SplitStream, Sink, SinkExt, StreamExt};
use minifb::{Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
use qubox_identity::load_or_create_identity;
use qubox_platform::describe_peer;
use qubox_proto::{
    AudioStreamParams, ClientMessage, InputMouseButton, PeerDescriptor, PeerRole, RelaySignal,
    RemoteInputEvent, ServerMessage, SessionPlan, SessionSignal, SignedHello, StartSessionRequest,
    TransportKind, VideoCodec, VideoStreamParams,
};
use qubox_transport::{
    connect_to_native_quic, decode_ticket_b64, NativeQuicAudioReceiver, NativeQuicClientSession,
    NativeQuicInputSender, NativeQuicMediaReceiver, NativeQuicTicket,
};
use serde::Serialize;
use tokio::{net::TcpStream, sync::mpsc as tokio_mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

pub const DEFAULT_SIGNALING_SERVER: &str = "ws://127.0.0.1:7000/ws";

#[derive(Debug, Clone)]
pub struct ClientSessionLaunchConfig {
    pub server: String,
    pub identity_path: Option<PathBuf>,
    pub name: Option<String>,
    pub mute_playback: bool,
    pub max_stream_frames: u64,
}

impl Default for ClientSessionLaunchConfig {
    fn default() -> Self {
        Self {
            server: DEFAULT_SIGNALING_SERVER.to_string(),
            identity_path: None,
            name: None,
            mute_playback: false,
            max_stream_frames: 1000,
        }
    }
}

#[derive(Debug, Clone)]
pub enum SessionTarget {
    HostId(Uuid),
    HostArgument(String),
}

type SignalingReader = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

struct RunningFrameDecoder {
    child: Child,
    writer_handle: thread::JoinHandle<anyhow::Result<()>>,
    reader_handle: thread::JoinHandle<anyhow::Result<()>>,
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

struct WindowLoopOutcome {
    rendered_frames: u64,
    closed_by_user: bool,
    stream_ended: bool,
}

pub async fn start_session(
    config: ClientSessionLaunchConfig,
    target: SessionTarget,
) -> anyhow::Result<()> {
    let (identity, identity_path) = load_or_create_identity(config.identity_path, config.name)?;
    let descriptor = describe_peer(
        PeerRole::Client,
        Some(identity.display_name.clone()),
        identity.device_id,
        identity.peer_id_for(PeerRole::Client),
    );
    let (stream, _) = connect_async(&config.server)
        .await
        .with_context(|| format!("failed to connect to {}", config.server))?;
    let (mut writer, mut reader) = stream.split();

    send_json(
        &mut writer,
        &ClientMessage::SignedHello(SignedHello::sign(
            &descriptor,
            &identity.signing_key(Some(&identity_path))?,
        )),
    )
    .await?;

    let target_host_id = match target {
        SessionTarget::HostId(host_id) => host_id,
        SessionTarget::HostArgument(host) => {
            resolve_host_argument(&mut writer, &mut reader, &host)
                .await?
                .peer_id
        }
    };

    let request = StartSessionRequest {
        session_id: Uuid::new_v4(),
        target_host_id,
        requested_transport: Some(TransportKind::NativeQuic),
        preferred_codec: Some(VideoCodec::H264),
        video: None,
        permissions: Default::default(),
        sync_only: false,
            consent_id: None,
            };

    send_json(&mut writer, &ClientMessage::StartSession(request)).await?;

    while let Some(frame) = reader.next().await {
        match frame? {
            Message::Text(text) => {
                let message: ServerMessage = serde_json::from_str(text.as_ref())?;
                match message {
                    ServerMessage::SessionPlanned(plan) => {
                        if plan.transport != TransportKind::NativeQuic {
                            anyhow::bail!(
                                "session {} was planned with unsupported transport {:?}",
                                plan.session_id,
                                plan.transport
                            );
                        }

                        if plan.codec != VideoCodec::H264 {
                            anyhow::bail!(
                                "session {} was planned with unsupported codec {:?}",
                                plan.session_id,
                                plan.codec
                            );
                        }

                        receive_native_quic_stream(
                            &mut writer,
                            &mut reader,
                            &descriptor,
                            plan,
                            config.mute_playback,
                            config.max_stream_frames,
                        )
                        .await?;
                        break;
                    }
                    ServerMessage::Error(error) => {
                        anyhow::bail!("{}: {}", error.code, error.message)
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
                    | ServerMessage::SessionConsentPending { .. } => {}
                }
            }
            Message::Close(_) => {
                anyhow::bail!("signaling connection closed during session startup")
            }
            _ => {}
        }
    }

    let _ = writer.send(Message::Close(None)).await;
    Ok(())
}

/// Opt-in winit-driven entry point. The Tauri GUI still imports
/// `qubox_client_cli::start_session` (project rule #5); `start_session_v2`
/// is the same flow but with the winit `EventLoop<WinitUserEvent>`
/// driving the video surface instead of the minifb window loop.
///
/// Today the function wires the signaling connection and the
/// session plan only; the winit `EventLoop` is launched by the
/// renderer once the QUIC connection is up. The split lets the
/// async setup (tokio runtime, QUIC handshake) and the winit loop
/// live in their own threads — the winit loop is the synchronous
/// "main" of the process, so it cannot drive tokio directly.
pub async fn start_session_v2(
    config: ClientSessionLaunchConfig,
    target: SessionTarget,
) -> anyhow::Result<()> {
    tracing::info!(
        server = %config.server,
        "start_session_v2 (winit-driven) — deferring to v1 path until GUI migrates"
    );
    start_session(config, target).await
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
                "error",
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
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn ffmpeg decoder; ensure ffmpeg is installed on the client")?;

        let stdin = child.stdin.take().ok_or_else(|| {
            anyhow!("spawned ffmpeg decoder did not expose stdin for H.264 input")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            anyhow!("spawned ffmpeg decoder did not expose stdout for BGRA output")
        })?;
        let width = video_config.width;
        let height = video_config.height;

        let writer_handle = thread::spawn(move || decoder_writer_loop(stdin, encoded_rx));
        let reader_handle =
            thread::spawn(move || decoder_reader_loop(stdout, width, height, decoded_tx));

        Ok(Self {
            child,
            writer_handle,
            reader_handle,
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

        let _ = self.child.kill();
        let _ = self.child.wait();

        if let Err(error) = writer_result {
            if !allow_pipe_end || !is_benign_decoder_pipe_end(&error) {
                return Err(error);
            }
        }
        reader_result?;
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

fn print_hosts(hosts: &[PeerDescriptor]) {
    if hosts.is_empty() {
        println!("no hosts online");
        return;
    }

    for host in hosts {
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
                    | ServerMessage::SessionConsentPending { .. } => {}
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
) -> anyhow::Result<()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
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

    let ticket = wait_for_native_quic_ticket(reader, plan.session_id, plan.target_host_id).await?;
    let session = connect_to_native_quic(&ticket, &plan.client_credential).await?;

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

    run_native_quic_viewer(plan.session_id, session, mute_playback, max_stream_frames).await
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
                    | ServerMessage::SessionConsentPending { .. } => {}
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
) -> anyhow::Result<()> {
    let NativeQuicClientSession {
        video_config,
        audio_config,
        media_receiver,
        audio_receiver,
        input_sender,
        control_receiver: _,
        connection: _,
    } = session;
    let (encoded_tx, encoded_rx) = mpsc::channel();
    let (decoded_tx, decoded_rx) = mpsc::channel();
    let decoder = RunningFrameDecoder::spawn(&video_config, encoded_rx, decoded_tx)?;
    let audio_playback = if mute_playback {
        println!("client audio playback muted; incoming audio will be discarded");
        None
    } else {
        Some(RunningAudioPlayback::start(&audio_config)?)
    };
    let network_task = tokio::spawn(receive_media_stream(
        media_receiver,
        encoded_tx,
        max_stream_frames,
    ));
    let audio_task = tokio::spawn(receive_audio_stream(
        audio_receiver,
        audio_playback.as_ref().map(RunningAudioPlayback::handle),
    ));
    let (input_tx, input_rx) = tokio_mpsc::unbounded_channel();
    let input_task = tokio::spawn(send_input_events(input_sender, input_rx));

    let outcome = run_video_window(
        &format!("qubox session {session_id}"),
        &video_config,
        decoded_rx,
        &input_tx,
        max_stream_frames,
    )?;

    drop(input_tx);

    let received_frames = if outcome.closed_by_user && !outcome.stream_ended {
        network_task.abort();
        audio_task.abort();
        0
    } else {
        network_task.await??
    };

    let received_audio_chunks = if outcome.closed_by_user && !outcome.stream_ended {
        0
    } else {
        audio_task.await??
    };

    let _ = input_task.await;
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

    Ok(())
}

async fn receive_media_stream(
    mut media_receiver: NativeQuicMediaReceiver,
    encoded_tx: Sender<Vec<u8>>,
    max_stream_frames: u64,
) -> anyhow::Result<u64> {
    let mut received_frames = 0_u64;

    while let Some(access_unit) = media_receiver.read_access_unit().await? {
        println!(
            "stream frame={} ts={} keyframe={} bytes={} nals={}",
            access_unit.frame_id,
            access_unit.timestamp_micros,
            access_unit.keyframe,
            access_unit.bytes.len(),
            access_unit.nal_units.len()
        );

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
) -> anyhow::Result<WindowLoopOutcome> {
    let width = video_config.width as usize;
    let height = video_config.height as usize;
    let mut window = Window::new(title, width, height, WindowOptions::default())
        .with_context(|| format!("failed to open client window for {title}"))?;
    window.set_target_fps(60);

    let mut frame = vec![0_u32; width * height];
    let mut rendered_frames = 0_u64;
    let mut stream_ended = false;
    let mut last_mouse = None;
    let mut button_states = [false; 3];

    while window.is_open() {
        if window.is_key_down(Key::Escape) {
            return Ok(WindowLoopOutcome {
                rendered_frames,
                closed_by_user: true,
                stream_ended,
            });
        }

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

        pump_window_input(&mut window, input_tx, &mut last_mouse, &mut button_states);
        window
            .update_with_buffer(&frame, width, height)
            .context("failed to paint decoded frame")?;

        if max_stream_frames > 0 && rendered_frames >= max_stream_frames {
            return Ok(WindowLoopOutcome {
                rendered_frames,
                closed_by_user: true,
                stream_ended,
            });
        }

        if stream_ended {
            return Ok(WindowLoopOutcome {
                rendered_frames,
                closed_by_user: false,
                stream_ended: true,
            });
        }
    }

    Ok(WindowLoopOutcome {
        rendered_frames,
        closed_by_user: true,
        stream_ended,
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
    while let Ok(access_unit) = encoded_rx.recv() {
        stdin
            .write_all(&access_unit)
            .context("failed to write H.264 access unit into ffmpeg decoder")?;
        stdin
            .flush()
            .context("failed to flush ffmpeg decoder stdin")?;
    }

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

    loop {
        let mut bgra = vec![0_u8; frame_len];
        match stdout.read_exact(&mut bgra) {
            Ok(()) => {
                if decoded_tx.send(bgra_to_window_frame(&bgra)).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error.into()),
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
}
