# API Inventory

Generated 2026-07-06. Every external Rust crate, OS API, wire protocol, internal module, and ops-side service the codebase touches. Source: walked every `Cargo.toml`, every `use` statement in `apps/` + `crates/`, plus `ops/{coturn,tuf,signaling-server,vm-lab}/`.

---

## 1. External Rust crates (versions from workspace `Cargo.toml` + per-crate manifests)

### Async / runtime
| Crate | Version | Used for |
|---|---|---|
| `tokio` | `1.44` (`features = ["full"]`) | Async runtime, `tokio::net::{TcpStream, UnixStream, UnixListener, UdpSocket}`, `tokio::io::{AsyncReadExt, AsyncWriteExt}`, `tokio::sync::{mpsc, broadcast, Mutex, Notify, watch}`, `tokio::task::JoinHandle`, `tokio::time::{interval, timeout, Instant}` |
| `futures` | `0.3` | `futures::stream::{SplitSink, StreamExt}`, `Sink`, `SinkExt` for tungstenite streams |
| `crossbeam-channel` | `0.5` | `bounded`, `unbounded`, `Receiver`, `Sender`, `RecvTimeoutError` (frame pipeline, pen coalesce, decoder HW thread) |
| `pollster` | `0.3` | `block_on` for wgpu device init |

### Serialization / data
| Crate | Version | Used for |
|---|---|---|
| `serde` | `1.0` (`features = ["derive"]`) | Derive `Serialize`/`Deserialize` on every wire type |
| `serde_json` | `1.0` | JSON for signaling WebSocket frames, TUF metadata blobs |
| `bincode` | `1.3` | Daemon IPC payloads (`bincode::serialize` / `deserialize`) |
| `bytes` | `1` | `Bytes` buffer for QUIC datagrams, frame slices |
| `byteorder` | `1.5` | Network-order read/write in TURN/STUN codec (`transport/src/turn.rs`) |
| `bitflags` | `2` | Bitflag type for capability masks in `qubai-proto` |

### Wire / network / crypto
| Crate | Version | Used for |
|---|---|---|
| `quinn` | `0.11` | QUIC endpoint (`Endpoint::server`, `Endpoint::connect`), `Connection`, `SendStream`, `RecvStream`, `Bi`, `VarInt`, `TransportConfig`, `ServerConfig::with_single_cert`, `EndpointConfig` |
| `quinn-udp` | `0.5` | `Transmit`, `RecvMeta`, `AsyncUdpSocket` trait (target for `TurnClient`) |
| `rustls` | `0.23` | `ServerConfig`/`ClientConfig`, `pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, PrivateKeyDer}`, `RootCertStore` |
| `rcgen` | `0.13` | `generate_simple_self_signed` for QUIC host certs |
| `reqwest` | `0.12` (`rustls-tls`, sometimes `json`) | TUF metadata + target download; daemon's own copy (separate from `tough`'s bundled 0.11) |
| `tokio-tungstenite` | `0.24` | WebSocket client to signaling server |
| `axum` | `0.8` (`features = ["ws"]`) | HTTP + WebSocket server (`axum::serve`, `axum::http`, `axum::body`, `Extension`, `Json`, `HeaderMap`, `StatusCode`) |
| `tower` | `0.5` | dev-dep for signaling tests |
| `hmac` | `0.12` | `Hmac<Sha1>`, `Mac::update`, `finalize` for TURN creds + STUN MESSAGE-INTEGRITY |
| `sha1` | `0.10` | TURN username HMAC + STUN integrity |
| `sha2` | `0.10` | TUF target hash verification |
| `md-5` | `0.10` | STUN `MD5` for `USERNAME` derivation (`transport/src/turn.rs:14`) |
| `blake3` | `1.8` | Clipboard payload dedup hash |
| `hex` | `0.4` | Hex-encode TUF target hashes |
| `base64` | `0.22` (`engine = general_purpose`) | Encode/decode QUIC tickets and certs |
| `url` | `2` | Parse TUF repo URLs |

### Codecs / media
| Crate | Version | Used for |
|---|---|---|
| `opus` | `0.3` | `Encoder::create`, `Decoder::create`, `Channels`, `Application::Audio` |
| `cpal` | `0.17` | Audio capture (`default_host`, `device`, `StreamConfig`, `BufferSize`, `SampleFormat`, `Stream`) |
| `dasp` | `0.11` | Sample rate / channel conversion in mic pipeline |
| `nnnoiseless` | `0.5` | RNNoise wrapper: `DenoiseState::new`, `DenoiseState::process_frame` (`FRAME_SIZE = 480`) |
| `webrtc-audio-processing` | `0.3` (`features = ["bundled"]`) | AEC + NS + AGC (`webrtc_audio_processing::AudioProcessing`, `StreamConfig`, `NoSuppression`, …) |
| `webrtc-audio-processing-sys` | `0.3` | Raw FFI fallback (`mic/src/pipeline.rs`) |
| `png` | `0.18` | Encode/decode clipboard PNG payloads |
| `ffmpeg-next` | `8.1` (optional, feature `hw-decode`) | `codec::decoder::...`, `format::Pixel::YUV420P`, `frame::Video`, `software::scaling`, `init` |

### GPU / windowing / UI
| Crate | Version | Used for |
|---|---|---|
| `wgpu` | `23` (`default-features = false, features = ["wgsl"]`) | `Instance`, `Adapter`, `Device`, `Queue`, `Surface`, `Texture`, `BindGroup*`, `RenderPipeline*`, `PresentMode::Mailbox/Fifo`, `TextureFormat`, `Sampler`, `Features::EXTERNAL_MEMORY` (declared, not used), `MemoryHints`, `PowerPreference`, `Backends` |
| `wgpu_glyph` | `0.23` | `GlyphBrush`, `Section`, `Text` for HUD text |
| `glyph_brush` | `0.7` | Underlying glyph layout |
| `winit` | `0.29` | Event loop, `Window`, `EventLoop`, `ControlFlow::WaitUntil`, `winit::event_loop::ActiveEventLoop` |
| `raw-window-handle` | `0.6` | Surface interop handle from winit → wgpu |
| `minifb` | `0.27` | CPU framebuffer (`Window`, `WindowOptions`, `Key`, `KeyRepeat`, `update_with_buffer`) — used for stats overlay CPU path, blank-overlay window, and the legacy minifb renderer |
| `softbuffer` | `0.4` | Software framebuffer (legacy path; `softbuffer::Surface`, `Buffer`) |
| `tauri` | (transitive, workspace member only) | Tauri 2.x shell for `qubai-client-gui` (`tauri::async_runtime`) |

### OS / platform
| Crate | Version | Used for |
|---|---|---|
| `x11rb` | `0.13` (`features = ["randr"]`) | `x11rb::connect`, `Connection`, `rust_connection::RustConnection`, `protocol::{xproto, randr}` — X11 capture + RandR display enumeration |
| `uinput` | `0.1` | `uinput::device::Builder`, `Device::create`, `Event`, `VirtualDevice` — Linux virtual gamepad + pen injector |
| `libinput` (re-exported as `input`) | `0.7` | `Libinput::new_from_udev`, event handling for pen capture (Linux only, feature-gated) |
| `pipewire` | `0.10` (optional in mic) | PipeWire virtual mic source API (declared, never actually called — `pipewire_available()` hardcoded `false` at `mic/src/platform/mod.rs:132-134`) |
| `libspa` | `0.10` (optional in mic) | Same — declared, not used |
| `enigo` | `0.6` | Host input synthesis: `Enigo::new`, `Settings`, `move_mouse`, `button`, `key`, `main_display`, `set_dpi_awareness`; `Direction::{Press, Release}`, `Coordinate::Abs`, `Key`, `Mouse` |
| `gilrs` | `0.11` | Client gamepad capture: `Gilrs::new`, `Event`, `Axis`, `Button`, `GamepadId` |
| `arboard` | `3.6` | Cross-platform clipboard (`Clipboard::new`, `get_text`, `set_text`, `get_image`, `set_image`) — Linux/macOS/Windows per-platform sub-modules in `crates/qubai-clipboard/src/platform/` |
| `systemd` | `0.10` (optional, default-on) | `sd_notify`, `JournalStream`, watchdog pings in `daemon/src/notify.rs` |
| `nix` | `0.29` (`features = ["socket", "user", "signal"]`) | `nix::sys::socket::{getsockopt, sockopt}` (SO_PEERCRED), `nix::unistd::{Pid, Uid}`, `nix::sys::signal::{kill, Signal}`, `nix::net::UnixDatagram` (notify), `nix::io::FromRawFd` (socket activation), `nix::fs::PermissionsExt` (TUF key perms) |
| `windows` | `0.58` | Win32 API: `Win32_Graphics_Dxgi::*`, `Win32_Foundation::*`, `Win32_System_Com::*`, `Win32_System_Pipes::*`, `Win32_Security::*`, `Win32_Security_Authorization::*`, `Win32_UI_TabletPC::*`, `Win32_UI_WindowsAndMessaging::*`, `Win32_UI_Input_Pointer::*` (selected via features per crate) |
| `windows-service` | `0.8` | `service_dispatcher::start`, `service_control_handler`, `ServiceStatus`, `ServiceControlAccept` (daemon SCM integration) |

### Application
| Crate | Version | Used for |
|---|---|---|
| `clap` | `4.5` (`features = ["derive", "env"]`) | Every CLI binary's `Parser`/`ValueEnum`/`Subcommand` |
| `anyhow` | `1.0` | `anyhow::Result`, `anyhow!`, `Context` everywhere |
| `thiserror` | `1` (transport) / `2` (others) | Error enums (`#[derive(Error)]`) |
| `tracing` | `0.1` | `info!`, `warn!`, `debug!`, `trace!`, `error!`, `span!` |
| `tracing-subscriber` | `0.3` (`features = ["env-filter", "fmt"]`) | `EnvFilter`, `fmt::Subscriber`, init |
| `uuid` | `1.10` (`features = ["serde", "v4"]`) | `Uuid::new_v4`, `uuid::Uuid` everywhere |
| `directories` | `5` | `ProjectDirs::from` for XDG-style state paths in the daemon |
| `widestring` | `1` | `U16CStr` for Windows SDDL strings over `interprocess` |
| `semver` | `1` | TUF target version comparison |
| `chrono` | `0.4` (`serde`) | dev-dep — TUF test repo timestamps |
| `olpc-cjson` | `0.1` | dev-dep — TUF test fixtures |
| `ring` | `0.17` | dev-dep — TUF test signing |
| `tempfile` | `3` | dev-dep — daemon tests |
| `serial_test` | `3` | dev-dep — `qubai-media` integration tests |
| `interprocess` | `2.4` (`features = ["tokio"]`) | Windows named-pipe IPC (`interprocess::local_socket::tokio::{Stream, Listener}`) |
| `redb` | `4.1` | Embedded KV store for daemon state |
| `tough` | `0.17` (`default-features = false, features = ["http"]`) | TUF client (`tough::Repository`, `tough::Metadata`, `tough::TargetPath`) |
| `async-trait` | `0.1` | `#[async_trait]` on `CaptureBackend`, `CaptureSession`, `DisplayManager`, `PenCapture`, `PenInjector` |
| `tokio-tungstenite` | `0.24` | (see wire) |

---

## 2. OS / system APIs

### Linux (via x11rb + libc + libspa + uinput)
- **X11 core**: `x11rb::connect`, `XOpenDisplay` equivalent, `XDefaultRootWindow`, `XCreateWindow`, `XChangeWindowAttributes`, `XSelectInput` (through x11rb's `protocol::xproto::*`).
- **RandR**: `x11rb::protocol::randr::*` — `GetScreenResources`, `GetCrtcInfo`, `GetOutputInfo`, `SetCrtcConfig`, `GetMonitors` for display enumeration.
- **DPMS**: referenced in code (X11 manager `set_display_state(Blanked)`) but not yet wired — comment "Phase C: replaces Phase A stubs with real vkms + xrandr + DPMS logic".
- **vkms**: Linux kernel module loaded via `modprobe vkms` (the privacy path) — not yet wired through Rust.
- **uinput**: `uinput::open`, `Device::create`, `Device::write`, `Device::establish`, event injection for gamepad + pen. Syscalls: `open(/dev/uinput)`, `write` ioctls (`UI_SET_EVBIT`, `UI_SET_KEYBIT`, `UI_SET_ABSBIT`), `UI_DEV_CREATE`/`UI_DEV_DESTROY`.
- **libinput**: `input::Libinput`, `udev` enumeration (`/dev/input/event*`), event reading.
- **PipeWire / libspa**: declared in `Cargo.toml` (`pipewire = "0.10"`, `libspa = "0.10"`) but **never actually called** in `crates/qubai-mic/src/platform/mod.rs:104-135` (`pipewire_available()` returns `false`).
- **systemd**: `sd_notify(STATE_READY=1)` via `systemd` crate's wrapper; `WATCHDOG_USEC` watchdog.
- **`/proc`/`/sys`**: pidfile via `nix::unistd::Pid`, `kill(pid, signal)` via `nix::sys::signal`.
- **SCM credentials** (`SO_PEERCRED`): `nix::sys::socket::getsockopt` (`Ucred`) for IPC auth (`daemon/src/ipc.rs`).
- **systemd socket activation**: `LISTEN_FDS`, `LISTEN_PID` env vars; `nix::io::FromRawFd` to wrap an inherited `UnixListener` (`daemon/src/socket_activation.rs`).

### Windows
- **SCM** (Windows Service Control Manager): `windows-service` crate → `ServiceDispatcher::start`, `ServiceControlHandler`, `ServiceStatusHandle`, `ServiceControlAccept::all()`.
- **Named pipes**: `interprocess::local_socket::tokio::Listener` + `Stream`. ACLs set via `widestring::U16CStr`.
- **DXGI**: declared in `crates/qubai-display/src/dxgi/mod.rs` via `windows = "0.58"` features `Win32_Graphics_Dxgi` / `Win32_Foundation` / `Win32_System_Com`. **Methods all return `NotSupported`** — actual capture uses ffmpeg `gdigrab`.
- **WM_POINTER**: declared in `crates/qubai-pen/src/windows.rs` via `Win32_UI_Input_Pointer`. **Stub only** — methods return `FeatureDisabled`.
- **WinTab**: declared via `Win32_UI_TabletPC`. **Stub only**.
- **IddCx**: mentioned in ADR; not imported.

### macOS
- **ScreenCaptureKit**: declared in `crates/qubai-display/src/screencapturekit/mod.rs` via `#[cfg(target_os = "macos")]` + `feature = "screencapturekit"`. **Stub only** — methods return `NotSupported`.
- **CGVirtualDisplay**: referenced in comments only; not imported.
- **Clipboard on macOS**: real impl in `crates/qubai-clipboard/src/platform/macos.rs` via `arboard::Clipboard` (arboard's mac backend uses `NSPasteboard`).

### Process / subprocess
- **ffmpeg**: invoked as a subprocess via `tokio::process::Command` (and `std::process::Command` for sync paths). Argument builders: `-hide_banner -loglevel warning -nostdin -f {x11grab|gdigrab|avfoundation|pipewire} -framerate -video_size -i {input} -an -vf scale=WxH -c:v {encoder} -b:v -maxrate -bufsize -g -bf 0 -bsf:v h264_metadata=aud=insert -f h264 pipe:1` (Linux X11 path, `host-agent/src/capture_orchestrator.rs:236-280`). Per-encoder tuning: `-preset`, `-tune ull`, `-rc cbr`, `-forced-idr 1`, `-vaapi_device`, `-low_power 1`, `-rc_mode CBR`, `-realtime 1`, `-allow_sw 0`.
- **stdio capture**: `tokio::process::ChildStdout` + `tokio::io::AsyncReadExt`; framer in `crates/qubai-media/src/lib.rs`.

---

## 3. Wire / protocol APIs

| Protocol | Wire | Crate / module |
|---|---|---|
| **QUIC (RFC 9000)** | UDP, 0-RTT, datagrams, bidi streams | `quinn::Endpoint` + `quinn::Connection` (`crates/qubai-transport/src/lib.rs`) |
| **QUIC ALPN** | `qubai-native-quic/0` | `pub const NATIVE_QUIC_ALPN: &str` in `transport/src/lib.rs:28` |
| **QUIC server name** | `qubai-native` | `DEFAULT_SERVER_NAME` in `transport/src/lib.rs:29` |
| **QUIC ticket format** | `NativeQuicTicket { session_id, connect_addr, server_name, alpn, cert_der_b64, expires_unix_millis }` | `transport/src/lib.rs:42-50` |
| **TLS 1.3** | over QUIC | `rustls` (`ServerConfig::with_single_cert`) |
| **STUN (RFC 5389)** | UDP, magic cookie `0x2112_A442` | `crates/qubai-transport/src/turn.rs:26` |
| **TURN (RFC 8656)** | over UDP (planned: DTLS only) | `crates/qubai-transport/src/turn.rs` (1528 LOC hand-rolled), `apps/qubai-signaling-server/src/turn.rs` (HMAC short-term creds) |
| **coturn config** | `turnserver.conf` — `listening-port=3478`, `tls-listening-port=5349`, `static-auth-secret`, `realm`, `no-tlsv1`, `no-tlsv1_1`, `no-cli`, `fingerprint`, `mobility` (RFC 6062 TCP), `stun-only=0` | `ops/coturn/turnserver.conf` |
| **WebSocket** | RFC 6455 | `tokio-tungstenite` (signaling client), `axum::ws` (signaling server) |
| **Signaling protocol** | JSON over WS: `ClientMessage::{Hello, PairingDecision, Heartbeat, RelaySignal}`, `ServerMessage::{Welcome, Hosts, PairingRequested, PairingEstablished, PairingRejected, SessionPlanned, SessionRequested, Signal, Presence, HeartbeatAck, Error}` | `crates/qubai-proto/src/lib.rs` |
| **JSON-RPC-ish** | n/a — bespoke message types |
| **TUF (The Update Framework)** | 5-role metadata: root, snapshot, targets, timestamp, mirror (last optional) | `ops/tuf/{root,snapshot,targets,timestamp}.json` |
| **TUF signing** | ed25519 (handled by `tough` + `ring`) | `crates/qubai-daemon/src/tuf.rs` |
| **DAEMON IPC** | custom framed binary on Unix socket (Linux/macOS) or named pipe (Windows) | `apps/qubai-daemon/src/ipc.rs:5-34` — magic `0xB0_1A_1C_BE`, version `0x0001`, 20-byte header, bincode payload, kinds `1=Request, 2=Response, 3=Event`, max payload 1 MiB |
| **Audio codec** | Opus over QUIC datagram | `crates/qubai-mic/src/pipeline.rs` |
| **Audio sample format** | PCM F32 48 kHz 2ch over QUIC | `AudioStreamParams { codec: PcmF32, sample_rate: 48_000, channels: 2 }` |
| **Video codec** | H.264 Annex-B over QUIC uni-stream (and over QUIC datagram via `--no-datagram-media`) | `crates/qubai-proto::VideoCodec`, `transport/src/media/mod.rs` |
| **Pen wire format** | 36-byte `WirePenEvent` (per ADR-010) + discriminator byte `0x50` | `crates/qubai-proto/src/pen.rs`, `transport/src/media/mod.rs` |
| **Clipboard wire format** | `ControlMsg::ClipboardChanged { payload, content_type }` on the control channel; payload is blake3-hashed + sequence-numbered | `crates/qubai-clipboard/src/hash.rs`, `crates/qubai-proto/src/lib.rs` |
| **Privacy wire format** | `ControlMsg::BlankOverlay { show, display_id }` on the control channel | `apps/qubai-host-agent/src/privacy.rs` |
| **Gamepad wire format** | `RemoteInputEvent::Gamepad(WireGamepadState)` with Xbox360 surface | `crates/qubai-proto/src/lib.rs` |
| **Telemetry wire format** | `TelemetrySnapshot` over `qubai-telemetry` JSON line stream | `apps/qubai-client-cli/src/telemetry.rs` |

---

## 4. Internal crate APIs (what each workspace crate exports)

### `qubai-proto` (`crates/qubai-proto/src/lib.rs`, `pen.rs`)
- **Enums**: `VideoCodec { H264 }`, `AudioCodec { PcmF32 }`, `PenTool { Pen, Eraser, … }`, `PlatformOs { Linux, Windows, Macos, Other }`, `PeerRole { Host, Client }`, `TransportKind { NativeQuic, WebRTC }`, `CaptureKind { X11, PipeWire, GdiGrab, AvFoundation, ScreenCaptureKit }`, `InputMouseButton { Left, Right, Middle, … }`
- **Messages**: `ClientMessage`, `ServerMessage`, `ControlMsg` (large enum: `BlankOverlay`, `ClipboardChanged`, `MicConfigAck`, …), `RemoteInputEvent` (MouseMove, MouseButton, Keyboard, Gamepad, Pen, HoverDisplay, RelativeMouseMove, MouseWheel)
- **Stream params**: `VideoStreamParams`, `AudioStreamParams`, `MicStreamConfig`, `PenDeviceDescriptor`, `WirePenEvent`
- **Session**: `SessionCredential`, `SessionRequested`, `SessionPlanned`, `SessionSignal::NativeQuicTicket`
- **Capabilities**: `DisplayCapabilities`, `PeerCapabilities`, `PeerDescriptor`
- **Bitflags**: `bitflags! { … }` for capability masks

### `qubai-identity` (`crates/qubai-identity/src/lib.rs`)
- `load_or_create_identity(path, name) -> (Identity, PathBuf)`
- `Identity { peer_id, device_id, display_name, keypair }`
- `peer_id_for(PeerRole)` → UUID
- Keypair persisted as JSON

### `qubai-platform` (`crates/qubai-platform/src/lib.rs`)
- `describe_peer(role, name, device_id, peer_id) -> PeerDescriptor`
- Cross-platform info for the peer descriptor

### `qubai-clipboard` (`crates/qubai-clipboard/src/lib.rs`, `platform/`)
- `ClipboardWatcher::new(config, outbound_tx)`, `start()`, `stop()`
- `ClipboardApplier::new(inbound_rx, config)`, `run()`
- `read_snapshot_with_formats(formats) -> ClipboardSnapshot`
- `hash_payload(&[u8]) -> blake3::Hash`
- `seq_advances(prev, next) -> bool`
- Per-platform files: `linux.rs`, `macos.rs`, `windows.rs` — all wrap `arboard::Clipboard` (constructed per-call, `!Send + !Sync`)

### `qubai-display` (`crates/qubai-display/src/lib.rs`)
- Traits: `CaptureBackend`, `CaptureSession`, `DisplayManager` (all `#[async_trait]`)
- Types: `DisplayId`, `DisplayInfo`, `DisplayState { Normal, Privacy, Blanked, Virtual }`, `CaptureOptions`, `CapturedFrame`, `PixelFormat { Bgra8, Rgba16F, Nv12 }`, `ColorSpaceId`, `BackendCapabilities { supports_hdr, supports_scrgb, supports_virtual_display, max_refresh_hz, supported_formats }`, `VirtualDisplayConfig`, `Point`, `Size`, `Rect`
- Errors: `CaptureError::NotSupported`, `DisplayError::NotSupported`
- Backends: `X11RandrBackend` (real), `DxgiBackend` (stub), `ScreenCaptureKitBackend` (stub), `PipeWirePortalBackend` (stub)
- X11 sub-API: `X11RandrContext::new()`, `X11RandrDisplayManager::with_fallback(ctx, fallback)`, `X11RandrBackend::new()`

### `qubai-pen` (`crates/qubai-pen/src/lib.rs`, `traits.rs`, `linux.rs`, `windows.rs`, `platform.rs`, `coalesce.rs`, `error.rs`)
- Traits: `PenCapture { enumerate_devices, start }`, `PenInjector { inject, device_name }`
- Errors: `PenCaptureError::FeatureDisabled`, `PenInjectError::FeatureDisabled`
- Dispatch: `current_capture() -> Box<dyn PenCapture>`, `current_injector() -> Box<dyn PenInjector>`, `CurrentPlatformPen` enum
- Linux: `linux::UinputInjector::new(name)` (real), `linux::enumerate_via_libinput()` (stub-when-no-feature)
- Windows: stub impl
- Coalesce: `Coalescer::new(interval_us)` for rate-limiting 240 Hz → 1 kHz
- Constant: `PEN_DATAGRAM_DISCRIMINATOR = 0x50`

### `qubai-mic` (`crates/qubai-mic/src/lib.rs`, `pipeline.rs`, `reference.rs`, `ring.rs`, `platform/`)
- `MicPipeline::new(config)` — `cpal` capture → `opus` encode → `webrtc-audio-processing` (AEC/NS/AGC) → `nnnoiseless` fallback → chunk sender
- `VirtualMicDevice::try_create(name, config) -> Self` (currently `device_created: false` on all OSes)
- `push_samples(&[f32])` (no-op without device)
- AEC reference: `reference::loopback_capture()` returns a separate `cpal::Stream` of the host's output for echo cancellation
- `RingBuffer` (lock-free SPSC) for cross-thread PCM handoff

### `qubai-media` (`crates/qubai-media/src/lib.rs`, `encoder_probe.rs`, `preset.rs`)
- `FfmpegPipelinePlan { program, args, output: EncodedOutput, notes }`
- `spawn_ffmpeg_pipeline(plan) -> RunningMediaPipeline`
- `read_h264_access_units(stdout, framer, scratch) -> MediaPipelineRead`
- `H264AnnexBStreamFramer::new(fps) -> Result<Self>` — splits by 00 00 01 NAL markers
- `H264EncoderBackend { Nvenc, Vaapi, Qsv, Amf, VideoToolbox, Libx264 }` + `ffmpeg_name() -> &str`
- `HostVideoPipelineConfig { capture, encoder, width, height, framerate, bitrate_kbps }`
- `CaptureSourceConfig { LinuxX11 { display, region }, LinuxPipeWire { node }, WindowsGdigrab { input }, … }`
- `probe_default_host_pipeline() -> MediaBackendReport { platform, encoders, … }`
- `probe_linux_capture_backends() -> Vec<CaptureKind>`
- `best_h264_encoder_for_platform(platform) -> H264EncoderBackend`
- `inspect_h264_annex_b_nal_units(&[u8]) -> Vec<NalUnitSummary>`
- `EncodedVideoAccessUnit { frame_id, timestamp_micros, keyframe, nal_units, bytes }`

### `qubai-transport` (`crates/qubai-transport/src/lib.rs`, `media/mod.rs`, `turn.rs`)
- `NativeQuicHost::bind(addr, advertised_ip, session_id, client_credential) -> Result<Self>`
- `NativeQuicHost::accept_authenticated_connection() -> NativeQuicHostConnection`
- `NativeQuicTicket { … }` + `encode_ticket_b64(&ticket) -> Result<String>` + `decode_ticket_b64`
- `connect_to_native_quic(&ticket, &client_credential) -> Result<NativeQuicClientSession>`
- `NativeQuicHostConnection::open_input_receiver(video_config)` / `open_audio_sender(audio_config)` / `open_media_sender()` / `open_control_sender()`
- Media module: `decode_pen_datagram(&[u8]) -> Result<PenEvent>`, `ControlChannel` (open/close/send), stream registry
- TURN: `TurnClient::new(config)`, `TurnClient::allocate()`, `ChannelBind`, `CreatePermission`, `Send`, `Data` (`transport/src/turn.rs`); STUN `Method::{Binding, Allocate, Refresh, Send, CreatePermission, ChannelBind}` (lines 48-53)
- `TurnConfig` (turn server config the client consumes)
- `DEFAULT_SERVER_NAME`, `NATIVE_QUIC_ALPN`
- Build transport config: `build_transport_config()` — keepalive, congestion (BBR or Cubic), max streams, datagram receive buffer

### `qubai-signaling` (`crates/qubai-signaling/src/lib.rs`)
- Axum handlers: `GET /ws` (WebSocket upgrade), `POST /turn-credentials` (issues short-term creds)
- Shared `AppState { peers: HashMap<PeerId, PeerDescriptor>, sessions: …, turn: TurnState }`
- `peers_presence_broadcast` event emitter

### `qubai-signaling-server` (binary, `apps/qubai-signaling-server/src/main.rs`, `turn.rs`)
- `axum::serve` binds `0.0.0.0:7000`
- TURN credential issuance: env-driven (`QUBOX_TURN_{SECRET,URLS,TTL_SECS,SECRET_PREVIOUS}`)
- Service file: `ops/signaling-server/qubox-signaling.service`
- Scripts: `install-service.sh`, `run-signaling-server.sh`

### `qubai-host-agent` (binary, `apps/qubai-host-agent/src/main.rs` + `capture_orchestrator.rs` + `privacy.rs` + `gamepad.rs` + `input_mapping.rs` + `rate_control.rs` + `rate_feedback.rs`)
- CLI args (clap `Parser`, `ValueEnum`): `server`, `name`, `identity-path`, `auto-approve-pairing`, `probe-media`, `plan-host-h264`, `smoke-test`, `plan-linux-pipewire-h264`, `run-linux-pipewire-h264`, `pipewire-node`, `linux-capture { Auto | Pipewire | X11 }`, `x11-display`, `windows-capture-input`, `h264-encoder { Nvenc | Vaapi | Qsv | Amf | VideoToolbox | Libx264 }`, `disable-audio`, `media-{width,height,fps,bitrate_kbps}`, `max-media-frames`, `native-quic-bind`, `native-quic-advertise-ip`, `stream-mode { single-stream | multi-display | all-displays }`, `display`, `privacy-mode { none | vkms | blank-overlay }`, `enable-privacy-on-session-start`, `vkms-output-name`, `clipboard-sync { off | host-to-client | client-to-host | both }`, `clipboard-formats { text | image | both }`, `clipboard-poll-ms`, `mic-virtual-source-name`, `advertise-hdr`, `pen-virtual-device-name`
- `CaptureOrchestrator::start_{single_stream, multi_display, all_displays, subscribe, unsubscribe}`, `stop`, `wait_for_all`, `enumerate_displays`
- `RemoteInputInjector` (enigo-based) — scales stream coords → host coords, enigo `move_mouse`/`button`/`key`
- `setup_clip_mic_handler(connection, runtime)` — bidirectional clipboard + virtual mic lifecycle
- `BlankOverlayManager::new/set_control_channel/show/hide/is_visible`
- `GccRateController::update(rtt, loss, owd_ms) -> bitrate_bps` — EWMA OWD, gradient→OveruseState, multiplicative decrease, fast-start, panic mode

### `qubai-client-cli` (binary, `apps/qubai-client-cli/src/{main,runtime,frame_pacing,frame_pipeline,decoder_hw,render_wgpu,winit_app,winit_user_event,tiled_view,stats_overlay,blank_overlay,privacy_indicator,gamepad_capture,stream_registry,telemetry,lib}.rs`)
- Renderer switch: `--renderer { wgpu | minifb }`; wgpu path is preferred (`PresentMode::Mailbox` → fallback `Fifo`)
- Decoder switch: `--decoder { subprocess | hw }` (HW requires `--features hw-decode`)
- HUD toggle: `Ctrl+Alt+S`
- Modules expose: `start_session(config, args)`, `start_streaming_session`, `start_subprocess_decoder`, `RunningHwFrameDecoder::spawn(config, encoded_rx, decoded_tx)`, `FramePacer::new(fps, vsync)`, `WgpuRenderer::new(surface, device, queue, config)`, `GlyphRenderer::new`, `TiledViewManager::new`, `BlankOverlayWindow::new/show/hide`, `StatsOverlay::new/toggle/render`, `TelemetryEmitter::install/emit`

### `qubai-client-gui` (binary, `apps/qubai-client-gui/src-tauri/src/lib.rs`)
- Tauri shell — imports `qubai_client_cli::start_session`, `qubai_daemon` (ipc), `qubai_signaling`, `qubai_identity`
- Persists identity, opens a Tauri window, redirects to CLI binary

### `qubai-daemon` (binary, `apps/qubai-daemon/src/{lib,main,service,service_scm,notify,pidfile,socket_activation,state,tuf,subprocess,ipc}.rs`)
- CLI args: `--socket-path`, `--state-db-path`, `--log-level`, `--update-repo`
- `Daemon::run(DaemonConfig)`
- `IpcServer::bind(path)`, `IpcRequest::*`, `IpcResponse::*`, `IpcEvent::*`, `IpcClient::call(req)`
- `StateDb::open(path)`, `state.set(key, value)`, `state.get(key)`, tables: `config`, `paired_keys`, `last_seen_peers`, `pending_updates`
- `UpdateChecker::check_rollback`, `check_for_update`, `apply_update`, `record_update_status`
- `pidfile::write(path)`, `pidfile::read(path) -> Option<Pid>`, `pidfile::remove(path)`
- `notify::sd_notify_ready()`, `notify::watchdog_ping()`, `spawn_watchdog()`
- `subprocess::SubprocessManager::spawn_child(name, args)` — tracks child PIDs for clean shutdown
- Service entry: `service_dispatcher::start("qubai", ffi_service_main)` (Windows), `Daemon::run` (Linux via systemd `Type=notify`)

---

## 5. External service / repo APIs (ops/)

### coturn (`ops/coturn/`)
- `turnserver.conf`: `listening-port=3478`, `tls-listening-port=5349`, `realm=bp`, `static-auth-secret`, `user-quota=12`, `total-quota=1200`, `no-tlsv1`, `no-tlsv1_1`, `no-cli`, `log-file=stdout`, `stun-only=0`, `no-daemon`, `fingerprint` (RFC 5389), `mobility` (RFC 6062).
- `Dockerfile`: builds coturn from source.
- `docker-compose.yml`: runs coturn + (likely) the signaling server.

### TUF (`ops/tuf/`)
- Roles: `root.json`, `snapshot.json`, `targets.json`, `timestamp.json`.
- `init-tuf.sh`: bootstrap root keys (ed25519), sign root.json.
- `publish-target.sh`: publish a new release artefact, sign snapshot+timestamp, bump version.
- Consumed by `qubai-daemon/src/tuf.rs` via `tough::Repository::load` (HTTP fetch from `QUBOX_UPDATE_REPO`).

### VM lab (`ops/vm-lab/`)
- `virtualbox-jammy-cloud.ps1`: PowerShell script to provision an Ubuntu 22.04 (jammy) cloud image on VirtualBox for headless E2E.

### AWS (`ops/aws/`)
- (No files yet — directory exists for future CloudFormation / deploy scripts.)

### Signaling service (`ops/signaling-server/`)
- `qubox-signaling.service`: systemd unit (`ExecStart`, `Restart=on-failure`, `Environment=QUBOX_TURN_SECRET=…`).
- `install-service.sh`, `run-signaling-server.sh`: helper scripts.

---

## 6. Subprocess CLIs (binary invocations)

| CLI | Used in | Args (representative) |
|---|---|---|
| `ffmpeg` | `apps/qubai-host-agent/src/capture_orchestrator.rs:236-280`, `main.rs:594-707`, `runtime.rs` | `-hide_banner -loglevel warning -nostdin -f x11grab -framerate 60 -video_size 1920x1080 -draw_mouse 1 -i :99+0,0 -an -vf scale=1920x1080 -c:v h264_nvenc -b:v 20000k -maxrate 20000k -bufsize 10000k -g 120 -bf 0 -bsf:v h264_metadata=aud=insert -preset p1 -tune ull -rc cbr -forced-idr 1 -f h264 pipe:1` (per-display, runtime bitrate changes via subprocess restart). VAAPI variant: `-vaapi_device /dev/dri/renderD128 -low_power 1 -rc_mode CBR`. QSV: `-preset veryfast -look_ahead 0`. AMF: `-quality speed -usage ultralowlatency`. VideoToolbox: `-realtime 1 -allow_sw 0`. x264 fallback: `-preset ultrafast -tune zerolatency`. |
| `ffmpeg` (client decode path) | `apps/qubai-client-cli/src/runtime.rs` | `-f h264 -i pipe:0 -an -fflags nobuffer -flags low_delay -probesize 32 -analyzeduration 0 -f rawvideo -pix_fmt bgra pipe:1` (subprocess decoder path). HW variant: `-c:v h264_vaapi`/`h264_cuvid`/`h264_qsv`/`h264_videotoolbox`/`h264_d3d11va`. |
| `Xephyr` (dev only) | CI scripts + manual | `Xephyr :99 -screen 1920x1080x24 -ac -nolisten tcp &` (target for E2E tests; gated on `DISPLAY=:99` in `capture_orchestrator.rs:533, 665`, `tests/multi_display_e2e.rs`, `tests/privacy_e2e.rs`) |

---

## 7. Build / packaging APIs

| Tool | Hook | Where |
|---|---|---|
| `cargo build/test/clippy/fmt` | standard | workspace |
| `cargo deb` | daemon's `[package.metadata.deb]` + `[[package.metadata.deb.systemd-unit]]` blocks for `qubai.service` + `qubai.socket` | `apps/qubai-daemon/Cargo.toml:82-106` |
| `cargo rpm` | daemon's `[package.metadata.rpm]` + `service-file` for `qubai.service` + `qubai.socket`; `pre/post-install.sh`, `pre/post-uninstall.sh` | `apps/qubai-daemon/Cargo.toml:108-126` |
| `cargo install` | `cargo install cargo-deb && cargo deb -p qubai` | comments in daemon Cargo.toml |
| `tough` (CLI) | used indirectly via `tough::Repository` from daemon | `crates/qubai-daemon/src/tuf.rs` |
| `ring` | dev-dep for TUF test signing | `apps/qubai-daemon/Cargo.toml:73` |
| `gh` (CI) | workflow files expected in `.github/workflows/` | dir exists; contents not in tree |

---

## 8. Quick-reference index

- **Wire entry points**: `apps/qubai-host-agent/src/main.rs` (host CLI), `apps/qubai-client-cli/src/main.rs` (client CLI), `apps/qubai-signaling-server/src/main.rs` (HTTP+WS), `apps/qubai-daemon/src/main.rs` (daemon CLI), `apps/qubai-client-gui/src-tauri/src/lib.rs` (Tauri shell).
- **All `use` of an external crate**: searchable via `rg --no-config 'use <crate>' --type rust apps crates`.
- **Per-crate dep graph**: `crates/<crate>/Cargo.toml` `[dependencies]` block.