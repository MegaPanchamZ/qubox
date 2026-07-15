# Production Recommendation for Qubox

## Goal

Turn Qubox from a prototype into a reliable multi-platform system with low latency, sane scaling, strong input support, and a credible roadmap toward controller-heavy and desktop-heavy use cases.

## Design Principles

1. Separate control, media, input, and platform concerns.
2. Treat video and audio as packetized real-time media, not as generic byte streams.
3. Keep native transport optimized for low latency, but do not force one transport profile onto every future client type.
4. Build the client receive path before chasing encoder sophistication.
5. Make input architecture explicit, not incidental.

## Recommended Target Architecture

### Control plane

Keep and evolve the existing signaling direction.

- WebSocket or HTTP-based signaling remains acceptable for session planning, identity, pairing, and setup.
- Reliable QUIC streams should carry post-connect control traffic, stats, input feedback, and configuration changes.

### Media plane

Move to a packetized media design.

- Native profile:
  - QUIC datagrams for video packets.
  - QUIC datagrams or carefully tuned low-latency packet streams for audio.
  - Reliable QUIC streams only for control and metadata.
- Browser profile, if added later:
  - WebRTC.

The native packet format should include:

- session ID,
- stream kind,
- frame number,
- packet sequence number,
- timestamp,
- packet count metadata,
- FEC metadata,
- codec-specific flags such as keyframe or reference invalidation signals.

### Receive pipeline

Implement a real receive stack on the client.

Required modules:

- packet receive queue,
- jitter and reorder queue,
- FEC recovery stage,
- depacketizer,
- frame assembly,
- decoder handoff,
- present queue.

This is where Moonlight is far stronger than Qubox today.

### Input plane

Expand input into a proper subsystem.

Protocol additions should include:

- scroll,
- relative mouse,
- absolute mouse,
- keyboard with scan or key codes plus modifiers,
- controller buttons and analog values,
- rumble feedback,
- optional motion and touch data,
- touchscreen and pen events where supported.

Do not model keyboard input as stringified UI keys long term.

### Presentation plane

Replace the current viewer stack.

- use `winit` for windowing and monitor control,
- use `wgpu` or a serious native presentation layer for frame display,
- support fullscreen, borderless, and windowed modes,
- maintain aspect ratio and explicit letterboxing math,
- separate stream resolution from window resolution.

## Immediate Priorities by Impact

### Priority 1: receive and transport reliability

Implement first:

- packetized video transport,
- receive-side jitter queue,
- frame assembly,
- Reed-Solomon FEC,
- basic feedback loop for frame loss.

Without this, all other work still sits on a weak foundation.

### Priority 2: replace the viewer runtime

Implement next:

- `winit`-based windowing,
- aspect-ratio aware viewport math,
- fullscreen and borderless modes,
- better decode integration,
- monitor and DPI awareness.

This addresses a large share of the perceived lag and poor scaling.

### Priority 3: input architecture rewrite

Implement next:

- relative and absolute mouse modes,
- scroll,
- keycode-based keyboard transport,
- gamepad via `gilrs`,
- host-side input backend abstraction.

This addresses the other major share of the perceived badness.

### Priority 4: host capture and codec hardening

After the transport and client runtime are upgraded, evolve host capture and acceleration backends.

- prefer native platform capture APIs,
- keep FFmpeg only where it remains a pragmatic bridge,
- standardize capability probing and fallback policy,
- add encoder backend selection and health metrics.

## Concrete Stack Recommendation

### Recommended near-term stack

- Transport: `quinn`
- Runtime: `tokio`
- Packet buffers: `bytes`
- FEC: `reed_solomon_rs`
- Audio IO: `cpal`
- Audio codec: Opus binding or wrapper
- Gamepad: `gilrs`
- Windowing: `winit`
- Presentation: `wgpu`
- CPU resize fallback: `fast_image_resize`
- Capture abstraction: `scap` or `xcap`, plus native backends where needed

### Recommended role of FFmpeg

Use FFmpeg strategically, not as the whole architecture.

Good uses:

- diagnostics,
- fallback encoding and decoding,
- some cross-platform codec integrations,
- utility conversion paths.

Bad use as final architecture:

- main capture pipeline,
- main viewer presentation path,
- core real-time flow control.

## Multi-Platform Guidance

### Windows

- capture: Graphics Capture or Desktop Duplication
- encode: NVENC, QSV, AMF, Media Foundation fallback
- decode/present: D3D-backed path
- input: keyboard, mouse, controller virtualization, DPI-aware coordinate mapping

### Linux

- Wayland-first capture via PipeWire
- X11 fallback only when necessary
- encode: VAAPI, NVENC, AMF, QSV depending on hardware
- input: uinput or equivalent backends, controller and touch support where feasible

### macOS

- capture: ScreenCaptureKit
- encode/decode: VideoToolbox
- input: entitlement and accessibility-aware keyboard and mouse injection

## What Not To Do

- Do not keep adding features on top of `minifb` and stringified input events.
- Do not keep treating complete access units over reliable streams as the long-term media design.
- Do not spend months tuning encode settings before building the real receive path.
- Do not make the Tauri shell the place where real-time streaming logic lives.

## Conclusion

Qubox can become a strong system, but only if it stops behaving like a thin demo pipeline and starts behaving like a real streaming runtime.

The order matters:

1. packetized transport and receive reliability,
2. viewer and scaling runtime,
3. full input subsystem,
4. platform capture and acceleration hardening.

That sequence will fix the actual product pain faster than continued incremental tweaking of the current prototype path.
