# Rust Streaming Ecosystem Survey

## Purpose

This document records practical Rust ecosystem options for a production-grade multi-platform remote desktop and game-streaming system, plus cases where FFI or external native dependencies are still the better choice.

## Summary

Rust is mature enough to own the control plane, much of the transport, a large share of input and audio work, and some parts of capture.

Rust is not yet strong enough to replace every platform media API or every hardware video stack with pure Rust alone.

The pragmatic model is:

- Rust owns orchestration, session state, packetization, receive queues, pacing, input semantics, and control protocols.
- Native APIs or FFI-backed libraries still handle a lot of hardware video and some platform capture details.

## Transport and Session Layers

### Mature options

- `quinn`
  - Best Rust-native option for QUIC transport.
  - Good fit for control traffic and a custom datagram-based media protocol.
- `webrtc-rs`
  - Best option when browser interoperability matters.
  - More complex, but useful for a web client or TURN-compatible deployment path.
- `tokio`
  - Mature async runtime for control plane, session orchestration, timers, pacing, and pipeline workers.

### Recommendation

Use `quinn` for native-to-native transport.

- Reliable QUIC streams for session setup, control, stats, and input feedback.
- QUIC datagrams for video and probably low-latency audio packets.

Use `webrtc-rs` only when browser clients or strict WebRTC interoperability become required.

## Packetization and FEC

### Useful Rust options

- `bytes`
  - Good for explicit packet layout and buffer reuse.
- `tokio-util` codec patterns
  - Good for framed control protocols, not by itself enough for real-time media.
- `reed_solomon_rs`
  - Candidate for Reed-Solomon parity on media blocks.
- `raptor`
  - Candidate for more advanced erasure-code experiments.

### Recommendation

Do not ship media as reliable JSON-framed access units.

Build a packet protocol that includes:

- stream type,
- frame number,
- packet sequence,
- intra-frame packet index,
- timestamp,
- keyframe/reference metadata,
- parity block metadata when FEC is enabled.

## Screen Capture

### Mature or promising Rust options

- `scap`
  - Strong cross-platform capture candidate.
  - Uses native APIs under the hood.
- `xcap`
  - Good cross-platform screen/window capture option.
- platform-specific direct APIs when necessary
  - PipeWire on Linux Wayland
  - Graphics Capture / Desktop Duplication on Windows
  - ScreenCaptureKit on macOS

### Recommendation

Rust can own the abstraction, but some platform paths will still be thin wrappers over native APIs.

Best long-term direction:

- Linux Wayland: PipeWire-based capture
- Linux X11 fallback: X11 capture only as fallback
- Windows: Graphics Capture or Desktop Duplication backend
- macOS: ScreenCaptureKit

Avoid treating external FFmpeg capture processes as the final architecture. They are useful for bring-up and diagnostics, but weak for latency, control, and robustness.

## Video Encode and Decode

### Practical options

- FFmpeg through library bindings or FFI-backed crates
- platform-native APIs:
  - VideoToolbox on macOS
  - Media Foundation and D3D11VA or DXVA/D3D12-based paths on Windows
  - VAAPI, NVENC, AMF, QSV, Vulkan Video, or V4L2-related paths on Linux depending on hardware
- `playa-ffmpeg`
  - Useful as a safer Rust-facing FFmpeg integration point

### Recommendation

Do not try to force pure-Rust hardware video everywhere.

Rust should own:

- encoder selection,
- fallback policy,
- capability probing,
- frame pacing,
- packetization boundaries,
- metrics,
- backpressure.

Native codec APIs or FFmpeg-backed integrations should still own the actual hardware encode and decode path.

## Audio Capture and Playback

### Mature Rust options

- `cpal`
  - Good cross-platform device IO abstraction.
- `rodio`
  - Good for high-level playback, less central for a custom low-latency streaming client.
- `rust-opus`
  - Pragmatic choice for Opus.

### Recommendation

Use:

- `cpal` for capture and playback device plumbing,
- Opus via a mature crate or FFI-backed binding for compressed audio,
- custom packetization and buffering for low-latency audio.

Audio should not remain an afterthought. A production system needs its own jitter queue, loss handling, and sync strategy.

## Input Injection

### Mature Rust options

- `enigo`
  - Good baseline for keyboard and mouse injection.
- `mouse-keyboard-input`
  - Worth evaluating for some lower-level patterns.

### Reality

Keyboard and mouse injection are tractable in Rust.

Controller virtualization, touch, pen, and advanced host feedback still often require platform-specific work or native dependencies.

Examples:

- Windows virtual gamepads often still involve ViGEm-related paths or equivalent native stacks.
- Linux controller injection may require uinput-based integration or native helper layers.

### Recommendation

Keep a Rust input abstraction, but expect platform backends:

- mouse and keyboard,
- scroll,
- touch and pen,
- gamepad and haptics,
- motion sensors,
- cursor capture and absolute/relative modes.

## Gamepads

### Mature Rust options

- `gilrs`
  - Best current general-purpose gamepad input library in Rust.

### Recommendation

Use `gilrs` on the client side for discovery, state, and hotplug handling.

Do not stop there. A production remote-streaming protocol also needs:

- explicit controller IDs,
- multi-controller mode,
- analog values,
- rumble return path,
- optional touchpad and motion data on supported devices.

## Windowing, Rendering, and Scaling

### Better options than `minifb`

- `winit`
  - Better windowing, monitor, fullscreen, and DPI foundations.
- `wgpu`
  - Strong cross-platform GPU rendering layer.
- `raw-window-handle`
  - Useful glue around native handles.
- `fast_image_resize`
  - Good CPU-side resizing helper when needed.

### Recommendation

Replace `minifb` in the serious client runtime.

Preferred direction:

- `winit` for window and monitor management,
- `wgpu` for presentation and scaling,
- a GPU texture upload or decode-present pipeline,
- explicit aspect-ratio aware viewport math.

## Web and Desktop Shells

### Current Qubox GUI direction

- Tauri is acceptable for shell and settings UI.
- Tauri is not the core streaming runtime.

### Recommendation

Keep the Tauri GUI for control surfaces and settings.

Move the serious streaming window, input capture, and presentation path into a proper native runtime layer that can be embedded or launched by the GUI.

## Areas Still Best Served by Native Dependencies or FFI

- Hardware encode and decode backends.
- Some capture APIs.
- Virtual controller backends.
- Some advanced touch/pen integration.
- Browser-grade WebRTC interop in the most feature-complete form.

## Shortlist for Qubox

### Strong candidates to adopt or evaluate soon

- `quinn`
- `tokio`
- `bytes`
- `reed_solomon_rs`
- `cpal`
- `rust-opus`
- `gilrs`
- `winit`
- `wgpu`
- `fast_image_resize`
- `scap` or `xcap`

### Candidates to keep behind optional backends

- `webrtc-rs`
- FFmpeg bindings or safe FFmpeg wrappers
- platform-native capture and codec integrations

## Bottom Line

Rust is strong enough today to build the control plane and much of the runtime architecture for a serious remote desktop or game-streaming product.

Rust alone is not yet the entire answer for production media acceleration and some platform IO. The right strategy is a Rust-owned architecture with carefully chosen native integrations at the edges.
