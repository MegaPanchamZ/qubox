# Sunshine and Moonlight Architecture Notes

## Purpose

This document captures how Sunshine and Moonlight work at a system level, with focus on frame transport, capture and encoding, acceleration, input, scaling, and the library stack that makes the pair reliable in practice.

## High-Level Split

Sunshine is the host.

- Session negotiation and transport control live in `src/rtsp.cpp` and `src/stream.cpp`.
- Capture, frame preparation, encoder selection, and hardware acceleration live in `src/video.cpp` plus platform-specific directories under `src/platform/`.
- Audio capture and encoding live in `src/audio.cpp`.
- Input translation and host injection live in `src/input.cpp` plus platform-specific input backends.

Moonlight Qt is the native client shell.

- UI, preferences, window modes, and user settings live under `app/gui/`, `app/settings/`, and `app/cli/`.
- Streaming session orchestration lives in `app/streaming/session.cpp`.
- Client input modes live under `app/streaming/input/`.
- Audio playback and video renderer selection live under `app/streaming/audio/` and `app/streaming/video/`.

The protocol runtime is mostly inside `moonlight-common-c`, which both Sunshine and Moonlight depend on.

- RTSP negotiation lives in `moonlight-common-c/src/RtspConnection.c`.
- Video receive queueing and FEC recovery live in `moonlight-common-c/src/RtpVideoQueue.c`.
- Audio receive queueing and FEC recovery live in `moonlight-common-c/src/RtpAudioQueue.c`.
- Video frame reassembly and decoder-facing state live in `moonlight-common-c/src/VideoDepacketizer.c`.

## How Frames Move Through the System

### Host-side frame path in Sunshine

1. Capture a display or application surface using platform capture backends.
2. Convert or scale the frame as needed.
3. Feed the frame into a hardware or software encoder.
4. Packetize encoded output into RTP-oriented packets with Sunshine-specific metadata.
5. Apply forward error correction for video and audio.
6. Send media over UDP-style channels and control over separate control transport.

Important host modules:

- `src/video.cpp`
  - Chooses and configures encoder path.
  - Handles software conversion and scaling with FFmpeg libs.
  - Integrates platform-specific acceleration such as D3D11, VAAPI, CUDA, VideoToolbox, and Vulkan.
- `src/stream.cpp`
  - Defines per-session state.
  - Owns audio and video worker threads.
  - Builds packet headers and FEC blocks.
  - Separates control traffic from media traffic.
- `src/thread_safe.h`
  - Provides the queue and event abstractions that keep capture, encode, and network work from blocking one another.

### Client-side frame path in Moonlight

1. Complete RTSP handshake and session setup.
2. Receive audio and video packets through the streaming transport.
3. Reorder packets and assemble FEC blocks.
4. Recover missing packets when enough parity data exists.
5. Reconstruct frame boundaries from packetized codec payloads.
6. Deliver complete decode units to the decoder.
7. Present frames with aspect-ratio-aware window management and pacing.

Important client modules:

- `moonlight-common-c/src/RtpVideoQueue.c`
  - Reorders video packets.
  - Tracks sequence continuity.
  - Handles Reed-Solomon recovery.
  - Reports frame/FEC status back to the host.
- `moonlight-common-c/src/VideoDepacketizer.c`
  - Waits for IDR when needed.
  - Drops corrupt or incomplete frame state.
  - Reconstructs codec configuration and picture payload order.
- `app/streaming/video/ffmpeg.cpp` and renderers under `app/streaming/video/ffmpeg-renderers/`
  - Decode and display path.
- `app/streaming/streamutils.cpp`
  - Aspect-ratio math, display sizing, refresh handling, and output geometry.

## Reliability Methodology

Sunshine and Moonlight are reliable because they do not treat video as a raw byte stream.

They instead use these system properties:

- Explicit packet sequencing and timestamps.
- Receive-side jitter buffering and reordering.
- Forward error correction rather than waiting only on retransmission.
- Session-local worker isolation, so one slow stage does not stall everything.
- Decoder resynchronization rules, especially around IDR and invalidated references.
- Feedback loops from client to host about loss or degradation.

### Video FEC and recovery

`src/stream.cpp` in Sunshine contains host-side FEC generation.

- Sunshine computes data and parity shards.
- Moonlight uses `rswrapper` and `RtpVideoQueue.c` to recover missing packets.
- Recovery happens before the decoder path whenever enough parity packets arrive.

This is critical for interactive streaming because it avoids the worst latency spikes caused by waiting on retransmission of already-late media.

### Frame integrity rules

`VideoDepacketizer.c` contains the most important client rules.

- If decode state is corrupted, Moonlight drops frame state instead of feeding bad state forward.
- It tracks when an IDR frame is mandatory.
- It limits consecutive frame drops and requests recovery when needed.
- It validates that SPS/PPS or VPS/SPS/PPS ordering exists before certain frame types.

This is a big reason the system degrades gracefully instead of cascading into seconds of visual corruption.

## Capture, Recording, and Frame Preparation

Sunshine is not a dumb recorder that shells out to FFmpeg for everything. It uses FFmpeg libraries and platform APIs directly.

Key characteristics:

- Capture is platform-specific.
- Encoder path is platform-specific.
- Color conversion and frame reshaping are done with FFmpeg image and scaling helpers when needed.
- Aspect ratio and padding are treated as first-class concerns.

`src/video.cpp` shows this clearly.

- It allocates hardware frames when available.
- It uses `sws_scale_frame()` and related FFmpeg utilities for conversion and scaling.
- It handles aspect-ratio preservation and black padding.
- It prepares frames for GPU encoders rather than assuming one fixed memory layout.

Moonlight similarly does not just show decoded pixels in a naive window.

- It maintains stream geometry and display geometry separately.
- It offers fullscreen, borderless, and windowed behavior.
- It preserves aspect ratio and letterboxes when appropriate.

## Input, Scaling, and Controllers

### Sunshine host-side input model

Sunshine has a much richer input model than Qubox.

The input stack includes:

- Mouse absolute and relative motion.
- Keyboard with modifier and shortcut handling.
- Scroll support.
- Multi-controller support.
- Touch and pen support.
- Controller motion, touchpad, LEDs, triggers, and rumble on supported stacks.

Files worth studying:

- `src/input.cpp`
- `src/input.h`
- `src/platform/windows/input.cpp`
- `src/platform/linux/input/inputtino_gamepad.cpp`
- `src/platform/linux/input/inputtino_touch.cpp`
- `src/platform/linux/input/inputtino_pen.cpp`

Sunshine also models client and host coordinate spaces explicitly via `touch_port_t`, scaling factors, and display-device data.

### Moonlight client-side input model

Moonlight has multiple mouse and touch modes because one input model does not fit gaming and remote desktop equally well.

It supports:

- Relative mouse for games.
- Absolute mouse for remote desktop.
- Touchscreen direct and trackpad-style input.
- Multi-controller support.
- Background or merged controller handling.
- Window modes and cursor capture rules.
- Keyboard capture and shortcut routing.

Files worth studying:

- `app/streaming/input/mouse.cpp`
- `app/streaming/input/keyboard.cpp`
- `app/streaming/input/gamepad.cpp`
- `app/streaming/input/abstouch.cpp`
- `app/streaming/input/reltouch.cpp`
- `app/streaming/streamutils.cpp`

## Libraries and External Dependencies

### Sunshine

Main dependency directions visible from `CMakeLists.txt`, `src/`, and submodules:

- Boost
- OpenSSL
- FFmpeg libraries
- moonlight-common-c
- ENet
- platform-specific GPU and display APIs
- ViGEm on Windows for virtual controllers
- inputtino on Linux for input injection
- Wayland, X11, VAAPI, libdrm, VDPAU, and Vulkan-related libs on Linux

### Moonlight Qt

Visible from `app/app.pro`:

- Qt Quick / Qt Quick Controls
- OpenSSL
- SDL2 and SDL2_ttf
- FFmpeg libraries
- Opus
- libplacebo
- platform-specific APIs such as DXGI, D3D11, Metal, VideoToolbox, AVFoundation, CoreMedia
- moonlight-common-c

## Key Lessons for Qubox

The most important lesson is that system quality comes from runtime control and recovery, not from simply using a low-latency transport.

Sunshine and Moonlight are good because they have:

- real packetization,
- real receive queues,
- real recovery logic,
- real scaling logic,
- real controller and input support,
- real platform abstraction.

Qubox currently has only the earliest slice of this stack.
