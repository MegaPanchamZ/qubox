# ADR-008 Integrated Microphone and Clipboard Synchronization

## Status

Proposed. Branch: `feature/adr-008-clipboard-mic-sync`. Based on `main` (Phase 0 + Phase 1 PRs merged through P1-12). P1-9 (clipboard) and P1-10 (mic) research documents exist at `research/roadmap/p1-09-clipboard.md` and `research/roadmap/p1-10-mic.md`; this ADR is the consolidated design that picks crate boundaries, threading model, and wire layout.

The P0-2 datagram media path (`crates/qubox-transport/src/media/mod.rs:38-89` for the 14-byte wire header, `:511-633` for `MediaDatagramSender`, `:840-902` for the gamepad discriminator pattern) and the daemon's Unix-socket IPC + redb state (`apps/daemon/src/ipc.rs:69-174`, `apps/daemon/src/state.rs:1-50`) are the substrate. Both features reuse existing pieces rather than introducing new infrastructure.

This ADR adds:

- One new variant to `ControlMsg` (clipboard payload).
- One new `ControlMsg` variant family for mic lifecycle (subscribe / unsubscribe / config).
- One new packed wire header for the mic datagram path (mirrors `MediaDatagramHeader`).
- One new `IpcEvent` variant (session-state changed, so the clipboard/mic allowlists can be enforced by the daemon even if a buggy app tries to send without permission).
- Two new modules: `crates/qubox-clipboard/` and `crates/qubox-mic/` (described in §3).
- Three new dependencies in `apps/client-cli/Cargo.toml` and one each in `apps/host-agent/Cargo.toml` (see §11).

No `unsafe` in any new code. The codebase's "no inline `//` comments inside fn bodies" rule is honored; only `///` and `//!` doc comments appear in the new APIs.

## Context

Qubox currently streams video, audio (host→client), and input (client→host) over a native QUIC connection. Two feature gaps remain for a "you're sitting at the remote machine" experience:

1. **Clipboard sync (P1-9).** A user copy-pasting on the host can't paste on the client and vice versa. Roadmap research picked `arboard` 3.4+ for cross-platform clipboard access (Linux X11 + Wayland, Windows, macOS), with a 250 ms polling loop and a `blake3` content hash to detect changes. P1-9 specifies text + PNG image (no HTML, no file drop lists in v1).

2. **Microphone streaming (P1-10).** The client's mic must reach the host's game/voice app so Discord / Steam Voice / in-game VC can hear the user. The pipeline is `cpal` capture (48 kHz mono) → `webrtc-audio-processing` (AEC3 + AGC2 + NS, with the host's speaker feed as the AEC reference) → `opus` encode (20 ms frames, 32-64 kbps VBR) → QUIC datagram. On the host side, a virtual input device (PipeWire virtual source on Linux, WASAPI loopback on Windows) makes the mic visible to apps.

Constraints that shape this ADR:

- The host already captures host audio and ships it to the client (`apps/host-agent/src/main.rs:1359-1394` `forward_audio_chunks`, `apps/host-agent/src/main.rs:1378-1410` `forward_audio_input_f32`). The client's playback path (`apps/client-cli/src/main.rs:275-348` `RunningAudioPlayback`) is a `cpal` output stream. **The mic's AEC reference is already on the client** (it was just played to the speakers). No new cross-stream routing is needed.
- The control plane is a single reliable QUIC uni-stream per direction (`crates/qubox-transport/src/lib.rs:587-626` `NativeQuicControlReceiver` / `NativeQuicHostControlSender`). Clipboard fits naturally on this stream.
- The data plane uses QUIC datagrams (`crates/qubox-transport/src/media/mod.rs:30` `MEDIA_DATAGRAM_MAGIC = [0xB2, 0x16]`, plus a 1-byte discriminator at offset 2; e.g. `:846-862` `encode_gamepad_datagram` uses `0x47`). Mic datagrams reuse this with a new discriminator.
- The daemon is the persistent identity + policy boundary. Sensitive data (clipboard) must not be routed through it as a relay, but the daemon *should* be the gatekeeper that enforces "clipboard/mic only during an active session" (the isolation requirement from the task brief).
- Wire-format changes are constrained by the existing `#[serde(tag = "op")]` discriminator on `ControlMsg` (`crates/qubox-proto/src/lib.rs:285-287`); new variants are additive (no v1 client breaks on a v2 server because the receiver silently ignores unknown `op` values — see `:1254-1260` of `apps/client-cli/src/main.rs`).

## Decision

### 1. Wire protocol changes

All additions are **additive and `#[serde(default)]`-friendly**, so a v1 client/server that does not know the new variants simply ignores them (the existing receiver does exactly this for `ControlMsg::Nack`, `Gamepad*`, etc.; see `apps/client-cli/src/main.rs:1254-1260`).

#### 1.1 New `ControlMsg` variants (`crates/qubox-proto/src/lib.rs`)

Three new variants added to the enum at `:287-338`:

```rust
/// Host↔Client (bidirectional): clipboard payload. Both directions use
/// the same variant; the direction is implicit in the stream that
/// carried it (host→client control uni-stream or client→host control
/// uni-stream).
///
/// `seq` is a monotonic counter per direction. Receivers apply the
/// payload only if `seq > last_seen_seq_for_kind(kind)`, giving
/// last-write-wins semantics and avoiding flip-flop when both sides
/// copy in quick succession.
ClipboardChanged {
    /// Monotonic per-direction counter (wraps at u64::MAX).
    seq: u64,
    /// What changed. Text and PNG image only in v1 (HTML deferred).
    payload: ClipboardPayload,
},

/// Client→Host: opt-in request to start streaming the microphone. The
/// host replies with `MicConfigAck`. Idempotent: a second `MicStart`
/// while a mic stream is already active is a no-op + a fresh `MicConfigAck`.
MicStart {
    /// Negotiated audio parameters (sample rate, channels, frame size).
    /// All `#[serde(default)]` so an older client can omit fields.
    config: MicStreamConfig,
},

/// Client→Host: stop streaming the microphone.
MicStop,

/// Host→Client: acknowledge the latest `MicStart` with the actual
/// parameters the host will use (may differ if the client requested
/// something the host can't satisfy — e.g. 48 kHz vs 44.1 kHz).
MicConfigAck {
    config: MicStreamConfig,
    /// True if the host successfully created the virtual input device.
    /// False means mic capture continues but the host app cannot hear
    /// it (e.g. PipeWire not available). Client surfaces a warning.
    virtual_device_ok: bool,
},
```

The new `MicStreamConfig` struct (also in `crates/qubox-proto/src/lib.rs`):

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MicStreamConfig {
    /// Sample rate in Hz. 48_000 is the default; 16_000 also supported.
    pub sample_rate_hz: u32,
    /// Always 1 (mono) in v1.
    pub channels: u8,
    /// Frame size in milliseconds: 10, 20, or 60. Default 20.
    pub frame_ms: u8,
    /// Opus bitrate in bits per second. 32_000..=128_000. Default 64_000.
    pub bitrate_bps: u32,
    /// Whether the client should run AEC3, NS, AGC2 before encoding.
    /// All default `true`; the host can disable via `MicConfigAck`.
    #[serde(default = "default_true")]
    pub aec_enabled: bool,
    #[serde(default = "default_true")]
    pub ns_enabled: bool,
    #[serde(default = "default_true")]
    pub agc_enabled: bool,
}
```

Every field uses `#[serde(default)]` (provided via the field-level default functions on the bools; numeric fields default to 0 and are validated downstream). This is the same backward-compat discipline the existing `VideoStreamPreferences` already follows (`crates/qubox-proto/src/lib.rs:223-243`).

#### 1.2 New `ClipboardPayload` enum (`crates/qubox-proto/src/lib.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClipboardPayload {
    /// UTF-8 text. PNG-free path; smallest payload.
    Text { utf8: String },
    /// PNG-encoded image. `width` and `height` are pre-encoding
    /// pixel dimensions; the receiver passes the PNG bytes to
    /// `arboard::Clipboard::set_image` (which decodes back to RGBA).
    ImagePng {
        width: u32,
        height: u32,
        png: Vec<u8>,
    },
    /// Empty clipboard (user selected "clear"). Sent on every
    /// transition from non-empty to empty so the receiver
    /// unconditionally drops its own cached content.
    Clear,
}
```

The `Clear` variant is a small but important detail: it lets the receiver wipe a previously-synced image without the sender having to re-send the same image bytes. The blake3 hash comparison in §2.2 will produce the same hash for two empty clipboards, so a transition into the empty state needs an explicit signal.

`Hash` is **not** part of the wire enum. The sender computes a `blake3` hash of `utf8` or `png` and compares to its `last_hash`; if equal, the message is dropped before serialization. The hash is local-state only, never sent. This keeps the wire format free of redundant information and matches the P1-9 design (`research/roadmap/p1-09-clipboard.md:60-80`).

#### 1.3 New `IpcEvent` variant (`crates/qubox-proto/src/lib.rs:540-562`)

For session-isolation of clipboard and mic (the "only sync during active session" requirement):

```rust
/// Emitted by the daemon when the active session state changes.
/// Subscribers (host-agent, client-cli) use this to gate clipboard
/// sync and microphone streaming. Sensitive data must not flow when
/// `active == false`.
SessionStateChanged {
    /// True while a host↔client media session is established.
    active: bool,
    /// Optional session id (None when `active == false`).
    session_id: Option<Uuid>,
    /// Why the state changed (for UI / audit log).
    reason: SessionStateReason,
},

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStateReason {
    SessionEstablished,
    SessionEnded,
    DaemonShuttingDown,
    PairingRevoked,
}
```

The daemon publishes this when it starts/stops a host or client subprocess (in the same `event_tx.send(...)` calls that already publish `HostStateChanged` / `ClientStateChanged` at `apps/daemon/src/ipc.rs:643-649, 664-670, 718-724, 739-745`). Subscribers — the host-agent and client-cli — listen via `SubscribeEvents` (`apps/daemon/src/ipc.rs:864-893`) and gate their clipboard / mic threads on `active == true`. The "no session, no leak" property is therefore enforced by the daemon, not by app-level cooperation.

#### 1.4 New mic datagram wire header (`crates/qubox-proto/src/lib.rs` + `crates/qubox-transport/src/media/mod.rs`)

The mic stream rides the existing QUIC datagram path with a new discriminator byte (the gamepad code already shows this pattern: `media/mod.rs:846-862` uses magic `[0xB2, 0x16]` + `0x47`):

```rust
/// 8-byte mic datagram header. Packed for zero-copy deserialization.
/// Layout (big-endian for multi-byte fields):
///   [0..2]  magic = MEDIA_DATAGRAM_MAGIC = [0xB2, 0x16]
///   [2]     discriminator = MIC_DATAGRAM_DISCRIMINATOR = 0x4D ('M')
///   [3]     flags (bit 0 = last packet in burst, future bits reserved)
///   [4..6]  sequence (u16, per-stream, wraps)
///   [6..8]  reserved (zero in v1)
/// Followed by the Opus payload (typically 50-200 bytes; max ~400).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct WireMicHeader {
    pub magic: [u8; 2],
    pub discriminator: u8,
    pub flags: u8,
    pub sequence: [u8; 2],
    pub _reserved: [u8; 2],
}
```

Plus constants in `crates/qubox-transport/src/media/mod.rs`:

```rust
/// Mic datagram discriminator byte. Placed at offset 2 immediately
/// after the 2-byte `MEDIA_DATAGRAM_MAGIC`. Distinct from gamepad
/// (0x47) so a single shared dispatch byte checks both kinds.
pub const MIC_DATAGRAM_DISCRIMINATOR: u8 = 0x4D;

/// Total mic datagram wire header size in bytes.
pub const MIC_WIRE_HEADER_SIZE: usize = 8;
```

The mic datagram is **not** chunked (unlike media). A 20 ms Opus frame fits comfortably under the QUIC datagram MTU (~1200 bytes), so a single datagram per frame is sufficient. This simplifies the receiver: it does not need the `JitterBuffer` + `FrameChunker` machinery from `media/mod.rs:103-210`; a small ring buffer that holds 2-3 frames of PCM samples for glitch-recovery under loss is enough. A lost datagram is masked by Opus's built-in PLC (Packet Loss Concealment), so we do not retransmit.

**Why not reuse `MediaDatagramHeader`?** That header's fields (`stream_id`, `frame_id`, `chunk_id`, `chunk_count`) are tied to the chunked media pipeline. The mic stream is a flat per-frame datagram; reusing the same header would set most fields to zero and burn bytes. A separate 8-byte header is cheaper on the wire and clearer in code.

The new variant on the existing `AudioDirection` semantics is implicit in the discriminator byte — `MEDIA_DATAGRAM_MAGIC + MIC_DATAGRAM_DISCRIMINATOR` means "client→host mic", distinct from any host→client media datagram (which is always video) and from the gamepad discriminator (0x47).

#### 1.5 Sequence number sizing

| Counter | Width | Where | Why |
|---|---|---|---|
| `ClipboardChanged.seq` | u64 | wire | Two directions × 250 ms polling = 16/s; u64 gives ~10^18 years of headroom. Last-write-wins is per-direction. |
| `WireMicHeader.sequence` | u16 | wire | 50 packets/sec × 65_535 = ~22 minutes wrap. The host detects wrap and resets; Opus PLC handles the gap. |
| `MediaDatagramHeader.frame_id` | u32 | wire (unchanged) | Already in place. |

### 2. Module structure

Two new crates keep the existing crate boundaries clean. Both are workspace members; both are pure-Rust with `cfg`-gated platform backends (no `unsafe`).

```
crates/
  qubox-clipboard/
    Cargo.toml           # arboard 3.4+, blake3, serde
    src/
      lib.rs             # ClipboardWatcher, ClipboardApplier, ClipboardPayload re-export
      hash.rs            # blake3 content hashing helpers (text + PNG)
      platform/
        mod.rs
        linux.rs         # cfg(target_os = "linux") — arboard with wayland-data-control
        windows.rs       # cfg(target_os = "windows")
        macos.rs         # cfg(target_os = "macos")
  qubox-mic/
    Cargo.toml           # cpal, opus, webrtc-audio-processing, dasp, tracing
    src/
      lib.rs             # MicCapture, MicPipeline, MicReceiver, public types
      capture.rs         # cpal input stream + ring buffer
      pipeline.rs        # webrtc-audio-processing (AEC3 + AGC2 + NS) + opus encode
      ring.rs            # lock-free SPSC ring buffer (dasp or custom)
      receiver.rs        # WireMicHeader parse + opus decode
      reference.rs       # ReferenceAudioTap: ring buffer the playback thread writes into
      platform/
        mod.rs
        linux.rs         # PipeWire virtual source
        windows.rs       # WASAPI loopback
        macos.rs         # BlackHole / aggregate device (deferred; see §11)
```

**Why new crates, not new modules in `qubox-transport` or `apps/*-agent`?** Both features need code on **both** the host and the client, and the platform backends (arboard, PipeWire, WASAPI) deserve their own compile unit so the rest of the workspace doesn't pay for the `webrtc-audio-processing` build (CMake + libwebrtc, slow) or for `arboard` on a binary that doesn't use the clipboard. Crate boundaries are the cheapest way to express "this only matters for clients / hosts that opt in." P0-6 gamepad followed the same pattern (`apps/host-agent/src/gamepad.rs` is host-only, with platform backends; see ADR-004).

The proto changes live in the **existing** `crates/qubox-proto` (it's the canonical wire type location) and `crates/qubox-transport` (the discriminator and the receiver type).

### 3. Threading model

Three threads, each with a strictly defined role and no shared mutable state (only `Arc<Mutex<…>>` for the ring buffers, and one `tokio::sync::mpsc` for the work queue).

#### 3.1 Client side — `apps/client-cli/src/clipboard/`

Reuses the existing `tokio` runtime on the client. No new thread is spawned *for the clipboard itself*; the polling loop runs as a `tokio::task::spawn` coroutine that yields every iteration with `tokio::time::sleep(Duration::from_millis(250)).await`. This is appropriate because:

- The polling work is `arboard::get_text()` / `get_image()` + blake3 hash + comparison — microseconds, not milliseconds.
- The result is pushed into a `tokio::sync::mpsc::UnboundedSender<ControlMsg>` that the existing `send_input_events` task (or a sibling) drains into the `NativeQuicHostControlSender` (`crates/qubox-transport/src/lib.rs:606-626`).

`arboard::Clipboard` is `!Send + !Sync`. We avoid the issue by **creating a fresh `Clipboard` instance inside each poll iteration** and dropping it at end of scope — same workaround P1-9 documented (`research/roadmap/p1-09-clipboard.md:22-24`). This is fine because the constructor is cheap and the polling interval (250 ms) makes the cost negligible.

The applier is even simpler: it lives inside `receive_control_stream` (`apps/client-cli/src/main.rs:1187-1264`) as a new match arm for `ControlMsg::ClipboardChanged { … }`. The applier thread is the same one that reads the control stream; it constructs a `Clipboard` per incoming message and drops it.

#### 3.2 Client side — mic, `apps/client-cli/src/mic/`

Three threads, plus a fourth already-existing one we route into:

| Thread | Already exists? | Role | Blocked on |
|---|---|---|---|
| `cpal` input callback | No | Pushes mic PCM samples into a lock-free SPSC ring buffer (`qubox_mic::ring`). Cannot allocate, cannot lock — runs in a real-time audio thread. | Never (RT). |
| `MicPipeline` worker | No | OS thread (`std::thread::spawn`). Pops 20 ms chunks from the ring, pulls the same number of samples from the `ReferenceAudioTap` ring (the speaker-feed the playback thread wrote), runs `apm.process_render_frame(...)` + `apm.process_capture_frame(...)`, encodes to Opus, pushes the resulting `WireMicHeader` + payload to a `tokio::sync::mpsc::UnboundedSender`. | `ring` (with timeout), `reference_tap` (with timeout). |
| `cpal` output callback (speaker playback) | Yes (`apps/client-cli/src/main.rs:275-348`) | Writes F32 PCM to the speakers. **New responsibility**: in the same callback, also push a copy into the `ReferenceAudioTap` ring (non-blocking; if the ring is full, drop the oldest reference sample — the AEC will recover over a few frames). | Never (RT). |
| `tokio` task — `send_mic_packets` | No | Reads from the mpsc the pipeline worker writes into and calls `connection.send_datagram(...)` for each. | The mpsc. |

Why a dedicated OS thread for the pipeline (not a `tokio::task`)? Because `webrtc-audio-processing`'s `process_capture_frame` is blocking and CPU-bound (it does FFTs, filtering, and gain control). If we ran it on the multi-threaded `tokio` runtime, we'd compete with network I/O and other tasks; the audio thread must have predictable latency. The pipeline thread runs a tight loop with no `await` — it only touches the lock-free rings and the unbounded mpsc — so a normal `std::thread` is the right tool.

The `cpal` callback is real-time: it must not allocate, not lock, not print, and not call into libstd I/O. Both rings use a fixed-capacity SPSC layout (single-producer single-consumer; cpal is the only producer, the pipeline is the only consumer). On overflow, the producer (cpal) overwrites the oldest sample — better than blocking the audio thread.

#### 3.3 Host side — `apps/host-agent/src/clipboard/`

Identical pattern to the client: a `tokio::task` polls every 250 ms with a fresh `arboard::Clipboard`, hashes, and pushes into a `tokio::sync::mpsc::UnboundedSender<ControlMsg>` that feeds the existing `NativeQuicClientControlSender` (the host's control-stream sender; counterpart of the receiver at `crates/qubox-transport/src/lib.rs:587-604`).

The applier is an arm in the host's control-stream consumer — wherever that lives (P0-2 §"Control channel" + ADR-006 daemon-delegation; the `client-cli` equivalent is `apps/client-cli/src/main.rs:1187-1264` `receive_control_stream`).

#### 3.4 Host side — `apps/host-agent/src/mic/`

Two threads, one borrowed.

| Thread | Already exists? | Role |
|---|---|---|
| `tokio` task — `recv_mic_packets` | No | Reads datagrams from the QUIC connection, parses the `WireMicHeader`, decodes Opus to F32 PCM, pushes samples to a `MicSink`. |
| `MicSink` (PipeWire virtual source writer) | No | OS thread. Pulls F32 PCM from the sink ring buffer and writes to the PipeWire source's buffer. |

The host does not need a pipeline thread (AEC is on the client, near the mic, which is correct — see §4). The host only decodes + writes to a virtual device.

### 4. AEC integration

The acoustic echo canceller is the reason the mic pipeline lives on the client, not the host. The reference signal — the host's audio output that the speakers played — is already in PCM form on the client (it was decoded by the existing `receive_audio_stream` and is about to be played by the `RunningAudioPlayback` cpal output stream at `apps/client-cli/src/main.rs:275-348`). The AEC needs the reference *before* it reaches the speakers, but in practice the 5-10 ms speaker latency is small enough that the AEC's adaptive filter converges without explicit alignment.

**Routing plan**:

1. The existing `AudioPlaybackHandle` (`apps/client-cli/src/main.rs:173-177`, a `Clone` of `Arc<Mutex<VecDeque<f32>>>`) gets a sibling: a `ReferenceAudioTap` (also `Clone` of an `Arc<Mutex<VecDeque<f32>>>`), but with a tighter capacity (20 ms × 48 kHz = 960 samples) so it doesn't accumulate drift.
2. In the cpal output callback (any of the three sample-format branches at `apps/client-cli/src/main.rs:300-327`), after `fill_audio_output_buffer_*` fills the output buffer with the samples-about-to-play, the callback also pushes the same samples into the `ReferenceAudioTap`'s queue. The push is a `try_lock` (non-blocking) — if it fails, we drop the reference frame, and the AEC will adapt over the next few frames.
3. The mic pipeline worker thread, on each 20 ms tick, pops 960 reference samples from the tap and feeds them via `apm.process_render_frame(&ref_f32, &stream_config)`.

**Why not move the AEC to the host?** Two reasons:

- The reference signal is only available on the client (it's the host's audio the client just decoded and is playing). Shipping the reference to the host would mean sending the audio stream twice.
- AEC adapts to the *playback* path characteristics (DAC, amplifier, speaker, room, mic). Running it next to the mic is closer to "the source of the leak" and produces better echo suppression.

**WebRTC APM lifecycle**: The `AudioProcessing` instance is created in the `MicPipeline` worker (not in the cpal callback, because constructing it allocates). The builder is `AudioProcessingBuilder::new().capture_sample_rate(48_000).enable_aec(true).enable_agc(true).enable_ns(true).build()`. We hold the instance on the worker thread for its entire lifetime; no synchronization is needed because the worker is the only user.

**Why `webrtc-audio-processing` and not `nnnoiseless`?** Both, in fact. `webrtc-audio-processing` provides AEC3 + AGC2 + NS in one library. `nnnoiseless` is RNNoise (an RNN-based NS) and is genuinely better at non-stationary noise (keyboard, fan bursts). The `MicPipeline` runs WebRTC NS first, then RNNoise, in series. This adds ~2 ms of CPU per 20 ms frame and noticeably cleaner output in noisy environments. RNNoise is a `Cargo` dependency only — no system library needed, unlike WebRTC APM which needs `libwebrtc-audio-processing-dev` (already present on the dev box; verified `pkg-config --exists libwebrtc-audio-processing-1` returns success in §11).

**Reference alignment**: the playback-to-tap pipeline adds at most a cpal buffer of latency (10-20 ms). The AEC's adaptive filter converges in <100 ms even with a 20 ms misalignment, so we do not implement an explicit resync. We do emit a "warmup" log line on the first 100 frames so operators can verify the AEC is engaged.

### 5. Daemon IPC integration (clipboard policy + session-isolation)

The daemon's role for P1-9 + P1-10 is **policy enforcement**, not data relay. Clipboard payloads and mic datagrams flow peer-to-peer over the QUIC connection; they never traverse the daemon's Unix socket. The daemon enforces two policies:

1. **Session-isolation** (the brief's "only sync during active session"): the daemon emits `IpcEvent::SessionStateChanged { active: true/false, … }` whenever a host or client subprocess starts or stops (in the same `event_tx.send(...)` calls that already publish `HostStateChanged` / `ClientStateChanged` — `apps/daemon/src/ipc.rs:643-649, 664-670, 718-724, 739-745`). The host-agent and client-cli subscribe to events (existing path: `IpcRequest::SubscribeEvents` at `apps/daemon/src/ipc.rs:864-893`) and gate their clipboard / mic tasks on `active == true`. When `active` flips to false, the clipboard applier clears the local clipboard and the mic pipeline drops the next Opus frame. This is a defense-in-depth check; the QUIC connection is itself torn down on session end, so the *transport-level* isolation is already in place.

2. **Pairing-isolation** (deferred, listed in §11): even within an active session, the clipboard must not be readable by a paired-but-revoked client. The `PairingRevoked` reason on `SessionStateReason` plus a check against `state.list_pairings()` is the mechanism. This is *not* in v1 (the existing `IpcRequest::RevokePairing` at `apps/daemon/src/ipc.rs:77-79` already takes effect; we just need to make clipboard / mic *observe* it). Documented in §11.

**Why not also route clipboard payloads through the daemon?** The daemon is intentionally minimal — it owns identity, pairing, TUF updates, and process supervision. Routing clipboard bytes (potentially multi-MB PNG images) through its Unix socket would couple clipboard latency to daemon IPC latency and bloat the daemon's binary. Peer-to-peer is correct.

### 6. Error handling and graceful degradation

Both features follow the same "never crash the session" philosophy that the existing transport uses (`crates/qubox-transport/src/media/mod.rs:606-633` `MediaDatagramSendError` is a soft error — send buffer full means drop the frame, not abort the connection).

| Failure | Detection | Behavior |
|---|---|---|
| `arboard` fails to open a clipboard handle (Wayland, locked clipboard) | `arboard::Clipboard::new()` returns `Err` | Log a warning, skip the iteration, retry on the next 250 ms tick. After 10 consecutive failures, log an error and stop the watcher task. |
| Clipboard content > 1 MiB (image) | Size check before encoding | Drop the payload, log a warning, do not send. Receiver's `last_seq` is *not* advanced, so the next smaller payload is not lost to a stale `last_seq`. |
| `cpal` returns no input device | `host.default_input_device()` is `None` | `MicStart` is never sent. Client surfaces a "no microphone" message in the overlay. |
| `cpal` input stream error | The cpal `err_fn` callback fires | Log the error; the `MicPipeline` worker detects the ring has been silent for >500 ms and stops sending frames. Host's PLC masks the gap. |
| `webrtc-audio-processing` build missing at runtime | `apm.build()` returns `Err` | Fall back to `nnnoiseless` only (NS, no AEC). Set `aec_enabled = false` in the `MicConfigAck` so the client knows. |
| Opus encode failure | `Encoder::encode_float` returns `Err` | Drop the frame, log a warning, continue. |
| `Connection::send_datagram` returns `SendDatagramError::Full` | Direct return | Skip the frame (RT priority — do not block). Log once per second to avoid log flood. |
| Mic datagram lost in transit | Receiver detects a gap in `WireMicHeader.sequence` | Opus decoder uses PLC for the missing frame; ring buffer absorbs the resync. |
| Host's virtual audio device creation fails (PipeWire not running, WASAPI in use) | `VirtualMicDevice::new()` returns `Err` | Reply with `MicConfigAck { virtual_device_ok: false, … }`; the client logs a warning and stops sending mic data. |
| Quic connection drops | `read_datagram` returns `Err` or EOF | The existing session teardown in `apps/host-agent/src/main.rs:run_native_quic_session` and `apps/client-cli/src/main.rs:run_native_quic_viewer` handles this. The clipboard watcher and mic pipeline are dropped along with the connection. |

`--no-clipboard-sync` and `--no-mic` CLI flags are the user-visible escape hatches; both default to **off** (opt-in) for the first release.

### 7. Testing strategy

The testing pyramid follows the existing pattern (heavy unit tests on the proto types, integration tests gated on `DISPLAY=:99` for capture paths, hardware paths skipped on CI).

#### 7.1 Unit tests (`crates/qubox-proto/src/lib.rs`)

Adding to the existing `#[cfg(test)] mod tests` (`:592-831`):

- `control_msg_clipboard_changed_round_trips_through_json` — text + image + clear variants.
- `control_msg_mic_start_round_trips_through_json` — with and without explicit `aec_enabled` field (backward compat).
- `control_msg_mic_config_ack_round_trips_through_json` — `virtual_device_ok: false` case.
- `wire_mic_header_is_eight_bytes_and_round_trips` — confirms `#[repr(C, packed)]` size.
- `ipc_event_session_state_changed_round_trips_through_json` — paired with the `display_state_changed` test at `:675-683`.

#### 7.2 Unit tests (`crates/qubox-clipboard/src/`)

- `blake3_text_hash_is_stable` — same UTF-8 → same hash; different UTF-8 → different.
- `blake3_png_hash_is_stable` — round-trip an RGBA buffer through `png` encode + decode; the decoded PNG bytes must hash equal to the encoded ones (this is the test that catches the "we re-encoded with a different PNG filter" bug).
- `clipboard_payload_clear_and_empty_text_are_distinct` — the applier must handle both transitions.
- `seq_comparator_advances_only_on_strictly_greater` — `last_seq = 5`, incoming `seq = 5` → reject; `seq = 6` → accept.

#### 7.3 Unit tests (`crates/qubox-mic/src/`)

- `opus_encode_decode_round_trip_is_lossless_within_one_sample` — confirms the codec choice (50 frames of 440 Hz sine).
- `webrtc_apm_cancels_synthetic_echo` — feed a 1 kHz sine as the render frame, the same sine (attenuated by 6 dB) as the capture frame; after `process_capture_frame`, the output's RMS must be at least 20 dB below the input's. Skipped if `webrtc-audio-processing` is not built.
- `reference_tap_drops_oldest_on_overflow` — push 2x capacity; oldest half is gone.
- `ring_buffer_is_spsc_safe` — concurrent push and pop from two threads; no sample is observed twice and none is lost (modulo the documented overflow drops).

#### 7.4 Integration tests

- `crates/qubox-mic/tests/mic_e2e.rs` (new, gated on Linux + a working input device): spawn a host and a client, push a sine wave into a fake cpal input via the cpal test backend, decode on the host, assert the decoded samples match the input within 50 ms end-to-end.
- `apps/host-agent/tests/clipboard_e2e.rs` (new, gated on `DISPLAY=:99`): the existing `multi_display_e2e.rs` pattern. Write a known string into a `Clipboard` instance on the host, the test consumer reads the same string from the client side.
- Add a `mic_e2e` to the orchestrator test in `apps/host-agent/src/capture_orchestrator.rs:541-633` (skipped unless an input device is present).

#### 7.5 Manual / latency checklist (documented in the PR description, not CI)

- [ ] Round-trip mic latency under headphones < 100 ms (measured with a loopback cable: speaker → mic).
- [ ] Round-trip mic latency with speakers on (no headphones) < 250 ms, residual echo < −30 dBFS.
- [ ] Clipboard text propagation end-to-end < 350 ms (250 ms poll + ~50 ms RTT + apply).
- [ ] 4K PNG (~10 MB) clipboard sync < 1.5 s end-to-end.
- [ ] `cargo check --workspace --exclude client-gui` clean.
- [ ] `cargo clippy --workspace --exclude client-gui --all-targets -- -D warnings` clean.
- [ ] 24-hour soak: mic on, clipboard sync on, no memory growth > 50 MB.

### 8. Upgrade path and migration

#### 8.1 Backward compatibility

- **Old client, new server**: the new `ControlMsg` variants and `IpcEvent` variants are unknown to the old client. The existing `receive_control_stream` match (`apps/client-cli/src/main.rs:1187-1264`) treats them like `Nack` and `Gamepad*` — logged as "unhandled" and ignored. Mic + clipboard simply don't work; everything else continues.
- **New client, old server**: the client's `MicStart` arrives at the old server, which ignores it. The client's `ClipboardChanged` arrives and is ignored. Same as above.
- **Mic datagram on an old receiver**: the new discriminator `0x4D` is unknown to the existing `MediaDatagramReceiver` dispatch (`crates/qubox-transport/src/media/mod.rs:644-689`), which only knows media + gamepad. Unknown discriminator → `from_bytes` returns `BadMagic` or the `header.chunk_id` parse path fails; the datagram is silently dropped. This is acceptable — old clients don't have a mic to stream.

#### 8.2 Field-level compatibility (the `#[serde(default)]` rule)

Every new struct field added in this ADR has `#[serde(default)]` (via `default_*` functions for bools; numeric fields default to 0 and are validated downstream, matching the `VideoStreamPreferences` style at `:223-243`). The `default_true` helper used on `aec_enabled`, `ns_enabled`, `agc_enabled` returns `true` so the absence of the field means "use the safe default." A v1 client that omits these fields gets full processing; a v1 server that doesn't know about them still parses the message successfully and applies the defaults.

#### 8.3 Persistence (redb)

No new redb tables. The settings table (`apps/daemon/src/state.rs:36`) holds a few new string keys: `clipboard.sync_enabled`, `clipboard.direction`, `clipboard.formats`, `mic.enabled`, `mic.device`, `mic.aec_enabled`, `mic.agc_enabled`, `mic.ns_enabled`, `mic.bitrate_bps`. These ride the existing `set_setting` / `get_setting` API (`apps/daemon/src/state.rs:202-210`). No schema migration is needed; old settings are simply absent and the defaults take over.

#### 8.4 Documentation updates

- `research/roadmap/p1-09-clipboard.md` and `research/roadmap/p1-10-mic.md` become "implemented" status (today: "research complete, implementation pending").
- `README.md` — add a "Clipboard & Microphone" section listing the new CLI flags and the PipeWire / VAC dependencies.
- `docs/operations.md` (if it exists; otherwise create) — host setup steps: install `libwebrtc-audio-processing-1` + `pipewire` (Linux), install VB-Audio Cable (Windows).

### 9. Risk register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `webrtc-audio-processing` build fails on a contributor's machine | Medium | High (mic path blocked) | Optional `default-features = false, features = ["ns-only"]` path that uses `nnnoiseless` only; document the trade-off (no AEC, no AGC). The `MicStart` config can disable AEC at runtime without rebuilding. |
| PipeWire virtual source creation fails (no `pipewire` daemon running) | Medium | Medium | `MicConfigAck { virtual_device_ok: false, … }`; client surfaces a warning and stops. The user can fall back to PulseAudio's `module-remap-source`. |
| macOS aggregate device creation is gated by TCC | High | Medium | v1 supports Linux + Windows only for the virtual device. macOS is documented as "follow-up" in §11. The mic capture and encoding still work — only the host-side playback into an app doesn't, which on macOS means "Discord hears the stream" requires a third-party virtual device anyway. |
| Wayland `wl_data_device` privacy | Already true (we run in the host's compositor on the host side, in the client's on the client side — both are authorized) | Low | None needed. The arboard documentation calls this out. |
| Bluetooth HFP latency makes AEC misbehave | Low | Low | Documented in the PR. The AEC's adaptive filter tracks 30-50 ms extra delay without issue. |
| Clipboard sync leaks credentials copied on either side | Inherent to the feature | High | Off by default. No "filter" — document the risk, point at the CLI flags. The session-isolation gate (§5) at least prevents the leak from persisting after the session. |
| `cpal` callback is starved by the pipeline worker | Low | High (audio glitch) | The rings are SPSC with no locks. The worker runs on its own OS thread. Worst case: pipeline falls behind, ring overflows, oldest samples are dropped — which is the same as Opus PLC for the receiver. The cpal callback itself is never blocked. |

### 10. Open questions deferred to v2

These are explicitly out of scope for v1; calling them out so they don't get smuggled in:

1. **HTML clipboard** — requires per-platform `text/html` MIME handling. P1-9 §"Risks" defers this.
2. **File drop lists** — `CF_HDROP` on Windows. v1 sends paths but not contents; v2 can do secure-transfer with the host's file server.
3. **macOS virtual audio device** — aggregate device + `kAudioUnitSubType_VoiceProcessingIO`. v1 logs "not yet supported" and exits the mic with a clear error.
4. **Multiple simultaneous mics** — pick-one semantics in v1; the GUI can list devices.
5. **VAD-driven bitrate adaptation** — silence → 32 kbps, speech → 64 kbps. P1-9 §"Risks" mentions this. Smooth bitrate transitions are tricky (clicks). Defer to v2.
6. **Pairing-revocation while a session is active** — revoke should trigger `SessionStateChanged { active: false, reason: PairingRevoked }` and tear down. The mechanism exists; we just don't wire the clipboard / mic listeners to react to it in v1. Track in a follow-up.
7. **Bidirectional mic / push-to-talk** — out of scope; the mic is always-on in v1.

### 11. Dependency manifest

`Cargo.toml` workspace additions:

```toml
[workspace.dependencies]
arboard = "3.4"
blake3 = "1.5"
png = "0.17"
opus = "0.3"
webrtc-audio-processing = { version = "0.3", optional = true }
nnnoiseless = "0.4"
dasp = "0.11"
pipewire = { version = "0.8", optional = true }
libspa = { version = "0.8", optional = true }
```

`crates/qubox-clipboard/Cargo.toml`:

```toml
[dependencies]
arboard = { workspace = true }
blake3 = { workspace = true }
png = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
qubox-proto = { path = "../qubox-proto" }
```

`crates/qubox-mic/Cargo.toml`:

```toml
[dependencies]
cpal = { workspace = true }
opus = { workspace = true }
webrtc-audio-processing = { workspace = true, optional = true }
nnnoiseless = { workspace = true }
dasp = { workspace = true }
serde = { workspace = true }
tracing = { workspace = true }
bytes = "1"

[target.'cfg(target_os = "linux")'.dependencies]
pipewire = { workspace = true, optional = true }
libspa = { workspace = true, optional = true }

[features]
default = ["webrtc-apm", "ns-rnnoise", "pipewire-virtual-source"]
webrtc-apm = ["dep:webrtc-audio-processing"]
ns-rnnoise = []
pipewire-virtual-source = ["dep:pipewire", "dep:libspa"]
```

The `webrtc-audio-processing` crate uses `pkg-config` to find `libwebrtc-audio-processing-1`. Verified on the dev box: `pkg-config --exists libwebrtc-audio-processing-1` succeeds. `libpipewire-0.3` and `libspa-0.2` are also present (verified via `pkg-config --exists libpipewire-0.3` and `libspa-0.2`); no system package install is needed for the first implementation. Per the no-sudo constraint, no `apt install` is required during build.

`apps/client-cli/Cargo.toml` additions:

```toml
qubox-clipboard = { path = "../../crates/qubox-clipboard" }
qubox-mic = { path = "../../crates/qubox-mic" }
```

`apps/host-agent/Cargo.toml` additions:

```toml
qubox-clipboard = { path = "../../crates/qubox-clipboard" }
qubox-mic = { path = "../../crates/qubox-mic" }
```

The `client-gui/src-tauri/src/lib.rs` import stub constraint is unaffected: the GUI binary already imports `client_cli::start_session` (`apps/client-gui/src-tauri/src/lib.rs:5-8`); the new modules are *additions* to `client-cli`, so the existing import keeps working without modification. No changes to the GUI's lib.rs are needed for the stub.

### 12. CLI surface

`apps/client-cli/src/main.rs` Args struct additions (extending the existing struct at `:47-119`):

```
--clipboard-sync {off,host-to-client,client-to-host,both}  # default: off
--clipboard-formats {text,image,both}                       # default: text
--clipboard-poll-ms <u32>                                   # default: 250
--mic                                                       # off by default
--mic-device <name>                                         # default: cpal default input
--mic-disable-aec                                           # rare; for testing
--mic-disable-ns
--mic-bitrate-bps <u32>                                     # default: 64_000
--mic-frame-ms {10,20,60}                                   # default: 20
```

`apps/host-agent/src/main.rs` Args additions (mostly mirrors):

```
--clipboard-sync {off,host-to-client,client-to-host,both}  # default: off
--clipboard-formats {text,image,both}
--mic-virtual-source-name <name>                            # default: "BP Virtual Mic"
```

A short user-facing string in `--help` points at the `nnnoiseless`-only fallback when `webrtc-audio-processing` is unavailable, so users on minimal Linux installs understand why AEC is missing.

## Consequences

- **Build cost**: the first `cargo build` after this lands takes longer (~2-3 minutes added) because `webrtc-audio-processing` invokes CMake. Subsequent builds are incremental. The new crates are gated behind `optional = true` in the workspace deps so a `--no-default-features` build still works (e.g. for the `client-gui` build that doesn't ship the mic UI yet).
- **Process model**: each side gains one OS thread for the mic pipeline (client) and one for the PipeWire source writer (host). The clipboard polling lives on the existing tokio runtime. No new child processes.
- **Latency budget**: clipboard propagation < 350 ms end-to-end (P1-9 target is <300 ms; +50 ms margin for the new `Clear` path and hash comparison). Mic round-trip < 100 ms head-only, < 250 ms with speakers (well under the P1-10 target).
- **Security posture**: clipboard and mic are off by default; session-isolated by the daemon; opt-in per session via the CLI. The "no plaintext clipboard to disk" property is preserved (clipboard bytes never enter the redb store; the daemon doesn't relay them).
- **Forward compat**: this ADR introduces three new `ControlMsg` variants, three new proto enums (`MicStreamConfig`, `ClipboardPayload`, `SessionStateReason`), one new packed wire struct (`WireMicHeader`), and one new `IpcEvent` variant. All are additive; no v1 client breaks.
- **Follow-up load**: §10 lists seven follow-ups, of which the macOS virtual device and the pairing-revocation wiring are the most user-visible. They are deliberately out of scope here to keep this PR reviewable.
