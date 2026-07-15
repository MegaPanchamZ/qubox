# P1-10: Microphone Streaming (cpal + Opus + WebRTC APM)

Status: research complete, implementation pending.
Owner: `apps/client-cli` (mic capture) and `apps/host-agent` (mic routing), with a new `mic` module in each.
Depends on: P0-2 (datagram media path; mic uses the same transport), the host's virtual audio device (P1-8's audio privacy work or a new module).
Blockers: WebRTC APM (`webrtc-audio-processing` crate) requires libwebrtc-audio-processing-dev system package on Linux. The host needs a virtual audio input device for the game to read from.

## Goal

Stream the client's microphone audio to the host, where the host's game (or Discord, Steam Voice, in-game VC) reads it from a virtual input device. With WebRTC's AEC3, AGC, and NS so the audio is clean (no echo from the host's speaker playback, no fan noise). Latency target: <100 ms end-to-end. Format: 48 kHz mono Opus at 32-64 kbps VBR, 20 ms frames.

## Research Summary

### cpal (cross-platform audio capture)

`cpal` is the de-facto Rust crate for low-level cross-platform audio I/O.

- **Current version**: 0.15+ as of 2024-2026 (releases: https://github.com/rustaudio/cpal/releases).
- **Platforms**:
  - Linux: ALSA, PulseAudio, JACK, PipeWire (via the appropriate feature flags).
  - Windows: WASAPI (default), DirectSound, ASIO.
  - macOS: CoreAudio.
  - Android: AAudio / Oboe.
  - iOS: CoreAudio.
- **API**: `Host::new()`, `host.default_input_device()`, `device.default_input_config()`, `device.build_input_stream(config, data_callback, error_callback, None)`.
- **Sample formats**: F32, I16, U16 (depends on the device).
- **Latency control**: via the stream config's `buffer_size`; cpal returns the device's preferred buffer size. A small buffer (~5-10 ms) gives low latency; the audio thread callback must be fast (no allocations, no I/O).

```rust
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

let host = cpal::default_host();
let device = host.default_input_device().expect("no input device");
let config = device.default_input_config()?;
let sample_format = config.sample_format();

let stream = device.build_input_stream(
    &config.into(),
    move |data: &[f32], _| { /* process samples */ },
    move |err| eprintln!("err: {err}"),
    None,
)?;
stream.play()?;
```

Alternatives considered:
- `alsa-rs` (Linux only).
- `coreaudio-rs` (macOS only).
- `wasapi` (Windows only).
- `oboe` (Android only).
- `tinyaudio` (newer, simpler).

`cpal` is the right cross-platform choice.

### Opus codec

Opus is the de-facto voice codec for real-time communication (used by WebRTC, Discord, Mumble, Steam Voice, etc.). Low-latency, high quality, royalty-free.

- **Rust crate**: `opus` 0.3+ wraps libopus. Pure-Rust `opus-rs` exists but is less mature; for production use, libopus is the standard.
- **Build**: link system libopus (Linux) or vendored (cross-compile).
- **API**: `Encoder::new(sample_rate, channels, Application)`, `encode_float()`, `encode()`.
- **Settings for voice**:
  - `Application::Voip` (vs `Audio`).
  - Bitrate: 32-64 kbps mono is plenty for voice.
  - CBR for predictable bitrate, VBR for better quality at the same average bitrate.
  - FEC: enable for lossy networks (1-frame FEC, ~20 ms of extra protection).
  - Complexity: 0-10 (default 9-10); lower for low-CPU devices.

```rust
use opus::{Application, Channels, Encoder};
let mut enc = Encoder::new(48_000, Channels::Mono, Application::Voip)?;
enc.set_bitrate(opus::Bitrate::Bits(64_000))?;
let mut out = vec![0u8; 4000];
let n = enc.encode_float(&samples_f32, &mut out)?;
out.truncate(n);
```

### WebRTC Audio Processing (AEC + AGC + NS + VAD)

The `webrtc-audio-processing` crate wraps Google's libwebrtc-audio-processing (also called audio_processing or APM). It provides:

- **AEC3** (Acoustic Echo Cancellation, third generation): removes the host's game audio from the mic capture, so the remote side doesn't hear an echo. Requires a "reference" audio stream — the host's audio output. This is the killer feature for game streaming.
- **AGC2** (Automatic Gain Control, second generation): normalizes the mic level across headsets.
- **NS** (Noise Suppression): removes fan noise, keyboard clicks, room hiss.
- **VAD** (Voice Activity Detection): detects speech vs silence for bitrate adaptation.

```rust
use webrtc_audio_processing::{AudioProcessing, AudioProcessingBuilder, StreamConfig};

let mut apm = AudioProcessingBuilder::new()
    .capture_sample_rate(48_000)
    .enable_aec(true)
    .enable_agc(true)
    .enable_ns(true)
    .build()?;

let stream_config = StreamConfig { sample_rate: 48_000, num_channels: 1, num_frames: 480 };
apm.process_capture_frame(&mut samples_f32, &stream_config)?;

// Feed the reference (host playback) audio:
apm.process_render_frame(&host_audio_f32, &stream_config)?;
```

**Linux dependency**: `libwebrtc-audio-processing-dev` (Debian/Ubuntu) or equivalent. The `webrtc-audio-processing` crate uses `pkg-config` to find it.

**NNNoise** (RNNoise): an alternative noise suppressor based on a small RNN. Available in Rust as the `nnnoiseless` crate. Slightly better NS than WebRTC's NS in noisy environments; requires a model file (~250 KB). Use as a complement to (or replacement for) WebRTC NS.

### Sample rate, format, channels

- **48 kHz** is the standard for voice chat (USB audio devices, WebRTC, Opus native rate).
- **Mono** is preferred for voice (halves bandwidth; speech localization is unnecessary).
- **F32 or I16**: cpal returns the device's preferred format. Convert to F32 for the WebRTC APM, then encode as F32 to Opus.

If the mic is 44.1 kHz (some cheap headsets), resample to 48 kHz via `dasp` (sample rate conversion) or `rubato` (a higher-quality resampler).

### Wire format

20 ms Opus frames, sent over QUIC datagrams (P0-2):

```rust
#[repr(C, packed)]
pub struct MicPacketHeader {
    pub magic: u8,         // 0xB3
    pub sequence: u16,
    pub timestamp: u32,    // in 48 kHz sample units
}
```

20 ms at 48 kHz = 960 samples. Encoded Opus payload is typically 50-200 bytes. Total packet: 7 + 200 = 207 bytes. At 50 packets/sec, 10.4 KB/s. Negligible bandwidth.

### Latency budget

| Stage | Latency |
|-------|---------|
| Mic capture (cpal buffer) | 5-10 ms |
| AEC + AGC + NS (WebRTC APM) | 1-2 ms |
| Opus encode | 1-2 ms |
| Wire (QUIC datagram) | 5-50 ms |
| Opus decode | 1-2 ms |
| Virtual device playback | 5-10 ms |
| **Total** | **18-76 ms** |

Well under 100 ms.

### Host audio routing

The host's game needs to read the mic stream from a virtual input device.

- **Linux**: PulseAudio / PipeWire virtual source. `pactl load-module module-null-sink sink_name=bp_mic_sink`, then `pactl load-module module-remap-source source_name=bp_mic source_properties=... master=bp_mic_sink.monitor`. Or use PipeWire's `pw-cli create node` to create a virtual source.
- **Windows**: Virtual Audio Cable (VAC) or VB-Audio. The host-agent writes the decoded mic to the VAC input; the game reads from VAC output.
- **macOS**: aggregate device or `kAudioUnitSubType_VoiceProcessingIO`. Use `coreaudio` crate.

For the first release, support **Linux PipeWire virtual source** (the dev box is Linux). The Windows and macOS paths are a follow-up.

### Rust crate matrix (2024-2026)

- `cpal` 0.15+: cross-platform audio capture.
- `opus` 0.3+: Opus encoder/decoder.
- `webrtc-audio-processing` 0.3+: WebRTC APM (AEC3 + AGC + NS).
- `nnnoiseless` 0.4+: RNNoise deep-learning NS (optional, complement).
- `dasp` 0.11+: resampling, ring buffers.
- `bytes` + `tokio` + `bincode`: wire format.
- `libpulse-binding` or `pipewire` (Linux): virtual source creation.

### 2024-2026 status

- **AEC3** is the standard; AEC2 is deprecated.
- **NNNoise** is a popular complement to WebRTC NS, especially in noisy environments.
- **AV1 audio** is not a thing; AV1 is video. Opus remains the audio codec.
- **Bluetooth headsets**: HFP mode is needed for the mic; A2DP doesn't expose the mic. Latency on HFP is 30-50 ms (worse than wired). Document this.
- **WebRTC APM Rust bindings**: there are multiple crate forks; check the latest on crates.io.

## Implementation Plan

### Step 1: Mic capture (client-cli)

`apps/client-cli/src/mic/capture.rs` (new):
- `pub struct MicCapture { stream: cpal::Stream, ring: Arc<Mutex<AudioRingBuffer>> }`.
- `pub fn new(prefs: &MicPreferences) -> Result<Self>` — picks the default input device, builds the input stream, pushes samples to the ring buffer.
- The cpal callback writes to a lock-free ring buffer; the encoder reads from it.

### Step 2: Audio processing pipeline

`apps/client-cli/src/mic/pipeline.rs` (new):
- `pub struct MicPipeline { apm: AudioProcessing, opus: Encoder, ring: AudioRingBuffer }`.
- `pub fn run(self, tx: tokio::sync::mpsc::Sender<MicPacket>)` — a worker thread that:
  1. Reads 480 samples (10 ms at 48 kHz) from the ring buffer.
  2. Feeds the **reference** audio (the host's audio output stream) to `apm.process_render_frame()`.
  3. Feeds the mic samples to `apm.process_capture_frame()`.
  4. Encodes the cleaned samples to Opus.
  5. Sends the packet over the QUIC connection.

The reference audio is the host's audio output that's already being streamed; we re-route a copy of it into the APM on the client side.

### Step 3: Wire format

`crates/qubox-proto/src/lib.rs`:
- Add `MicPacket { header: MicPacketHeader, opus_payload: Vec<u8> }`.
- Sent over QUIC datagrams (P0-2), stream_id for the mic stream.

### Step 4: Host-side receiver and virtual device

`apps/host-agent/src/mic/host.rs` (new):
- `pub struct MicReceiver { opus: Decoder, ring: AudioRingBuffer }`.
- `pub fn run(self, rx: tokio::sync::mpsc::Receiver<MicPacket>, virtual_device: VirtualMicDevice) -> Result<()>` — receives packets, decodes Opus, writes to the virtual device.

`apps/host-agent/src/mic/linux.rs` (Linux virtual source):
- Creates a PulseAudio / PipeWire virtual source. The decoded mic samples are written to the source via the PulseAudio / PipeWire API.
- The game sees a "BP Virtual Mic" input device and reads from it.

### Step 5: Preferences

Add to `VideoStreamPreferences` (or a new `MicConfig`):
- `mic_enabled: bool` (default: false — opt-in)
- `mic_device: Option<String>` (default: default input device)
- `mic_aec_enabled: bool` (default: true)
- `mic_agc_enabled: bool` (default: true)
- `mic_ns_enabled: bool` (default: true)
- `mic_vad_enabled: bool` (default: true)
- `mic_bitrate_bps: u32` (default: 64_000)
- `mic_sample_rate_hz: u32` (default: 48_000)
- `mic_frame_ms: u8` (default: 20)

CLI flag: `--mic` to enable.

### Step 6: Tests

- Unit test: Opus encode → decode round-trip is lossless (within 1 sample).
- Unit test: WebRTC APM reduces a synthetic echo (play a sine wave into the reference, capture the same sine wave from the mic, run APM, verify the output is silent).
- Integration test: capture mic on the client, send to host, verify the host's virtual device produces the same audio.
- Latency test: total round-trip < 100 ms.

## Risks and Open Questions

- **WebRTC APM build complexity**: the `webrtc-audio-processing` crate has a complex build (CMake, C++17, a large dependency). For the first release, consider `nnnoiseless` only (simpler, just NS) and defer the full AEC3 to a follow-up. Or use a simpler echo canceller like `speexdsp` (Speex's AEC, simpler but lower quality than AEC3).
- **Reference audio routing**: getting the host's audio output into the client's APM requires the client to know the host's output audio. We already have the host's audio stream on the client (for the host→client audio). Tee a copy of it into the APM's render frame.
- **Bluetooth headset latency**: 30-50 ms; total mic latency could be 50-100 ms, near the budget. Document this.
- **Multiple mics**: the user may have multiple input devices (webcam mic, headset mic, USB mic). The user must pick the right one. The CLI flag `--mic-device` allows this; the GUI should list devices.
- **Mic on the host's speakers**: if the user has the host's speakers turned up, the mic captures the game audio. The reference feed cancels it, but a poorly-tuned AEC leaves residual echo. Document the recommendation: use headphones on the host.
- **macOS virtual audio device**: requires `kAudioUnitSubType_VoiceProcessingIO` or an aggregate device. Complex. Defer to a follow-up.
- **Windows virtual audio cable**: requires VAC or VB-Audio. Document the dependency.
- **Linux PipeWire virtual source**: relatively easy to set up; the `pipewire` crate or the `pw-cli` CLI both work.
- **Per-frame timing**: the audio thread callback must be deterministic. Use a bounded queue (lock-free ring buffer) between the capture thread and the encoder thread; if the queue overflows, drop the oldest samples (better than blocking).
- **VAD-driven bitrate**: silence → 32 kbps, speech → 64 kbps. Saves bandwidth on the wire. Need to handle the VAD→bitrate transition smoothly (no clicks).

## References

- cpal docs: https://docs.rs/cpal/latest/cpal/
- cpal on crates.io: https://crates.io/crates/cpal
- cpal releases: https://github.com/rustaudio/cpal/releases
- cpal user thread on listening to mic: https://users.rust-lang.org/t/a-crate-that-listens-to-the-microphone/89349
- opus crate: https://crates.io/crates/opus
- Opus codec explained: https://www.forasoft.com/learn/audio-for-video/articles-audio/opus-codec-explained
- Pion Opus blog: https://pion.ly/blog/pion-opus/
- webrtc-audio-processing: https://github.com/webrtc-rs/webrtc/issues/550 (related discussion)
- Steam audio codec discussion: https://steamcommunity.com/discussions/forum/10/882959527690571136/
- Stream.io on WebRTC codecs: https://getstream.io/resources/projects/webrtc/advanced/codecs/
- Joy of the unknown (circular buffers in Rust audio): https://dev.to/drsh4dow/the-joy-of-the-unknown-exploring-audio-streams-with-rust-and-circular-buffers-494d
- Perplexity research, 2026-07-02: cpal, Opus, WebRTC APM, AEC3, latency, virtual device, 2024-2026 status.
