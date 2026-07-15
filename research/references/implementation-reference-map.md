# Implementation Reference Map

## Purpose

This file is the quick-reference map to consult while implementing the Qubox upgrade plan. It identifies which Sunshine and Moonlight source areas are the best reference for each subsystem, and what to copy in principle versus what to avoid copying literally.

## How To Use This File

- Read the phase guide first.
- Use this map to jump into the right Sunshine or Moonlight code before implementing a phase.
- Copy methodology and data flow, not protocol quirks or legacy compatibility behavior unless required.

## Phase 0: Measurement and Baseline

### Qubox focus

- `apps/host-agent/src/main.rs`
- `apps/client-cli/src/runtime.rs`
- `crates/qubox-transport/src/lib.rs`

### Reference areas

- Sunshine:
  - `src/stream.cpp`
  - `src/stat_trackers.cpp`
  - `src/stat_trackers.h`
- Moonlight:
  - `app/streaming/session.cpp`
  - `app/streaming/bandwidth.cpp`
  - `moonlight-common-c/src/Connection.c`

### What to learn

- per-session timing and state reporting,
- bandwidth and status reporting,
- separation of transport state from UI.

### What not to copy directly

- legacy protocol-specific counters that only make sense for GameStream compatibility.

## Phase 1: Packetized Media Transport

### Qubox focus

- `crates/qubox-proto/src/lib.rs`
- `crates/qubox-transport/src/lib.rs`
- future `crates/qubox-packets`

### Reference areas

- Sunshine:
  - `src/stream.cpp`
  - `src/rtsp.cpp`
- Moonlight:
  - `moonlight-common-c/src/RtspConnection.c`
  - `moonlight-common-c/src/Connection.c`
  - `moonlight-common-c/src/Limelight-internal.h`

### What to learn

- media and control separation,
- packet metadata structure,
- recovery and feedback signaling,
- session-level transport negotiation.

### What not to copy directly

- RTP and RTSP details that only exist for GameStream interoperability,
- encryption or control-packet layouts that are artifacts of the Nvidia/Sunshine compatibility layer.

## Phase 2: Receive Pipeline, Reorder, and FEC

### Qubox focus

- future `crates/qubox-receive`
- `apps/client-cli/src/runtime.rs`

### Reference areas

- Moonlight:
  - `moonlight-common-c/src/RtpVideoQueue.c`
  - `moonlight-common-c/src/RtpAudioQueue.c`
  - `moonlight-common-c/src/VideoDepacketizer.c`
- Sunshine:
  - `src/stream.cpp` for host-side FEC generation

### What to learn

- reorder queue semantics,
- missing-packet accounting,
- FEC block assembly and recovery,
- frame corruption boundaries,
- IDR or recovery-frame requirements.

### What not to copy directly

- exact RTP assumptions,
- old-generation compatibility branches,
- GameStream-specific packet numbering rules.

## Phase 3: Viewer, Scaling, and Presentation

### Qubox focus

- future `crates/qubox-present`
- `apps/client-cli/src/runtime.rs`
- `apps/client-gui/src-tauri/src/lib.rs`

### Reference areas

- Moonlight:
  - `app/streaming/streamutils.cpp`
  - `app/streaming/video/ffmpeg.cpp`
  - `app/streaming/video/ffmpeg-renderers/pacer/pacer.cpp`
  - `app/streaming/video/overlaymanager.cpp`
- Sunshine:
  - `src/video.cpp`
  - `src/display_device.cpp`

### What to learn

- stream geometry versus window geometry,
- aspect-ratio preservation,
- pacing and display timing,
- monitor-aware and DPI-aware runtime behavior.

### What not to copy directly

- Qt-specific UI patterns,
- Sunshine host display configuration logic that belongs to the host rather than the client viewer.

## Phase 4: Input and Controllers

### Qubox focus

- future `crates/qubox-input`
- `crates/qubox-proto/src/lib.rs`
- `apps/client-cli/src/runtime.rs`
- `apps/host-agent/src/main.rs`

### Reference areas

- Sunshine:
  - `src/input.cpp`
  - `src/input.h`
  - `src/platform/windows/input.cpp`
  - `src/platform/linux/input/inputtino_gamepad.cpp`
  - `src/platform/linux/input/inputtino_touch.cpp`
  - `src/platform/linux/input/inputtino_pen.cpp`
- Moonlight:
  - `app/streaming/input/mouse.cpp`
  - `app/streaming/input/keyboard.cpp`
  - `app/streaming/input/gamepad.cpp`
  - `app/streaming/input/abstouch.cpp`
  - `app/streaming/input/reltouch.cpp`

### What to learn

- absolute versus relative mouse modes,
- keyboard capture semantics,
- multi-controller handling,
- gamepad state aggregation,
- rumble and advanced controller features,
- touch and pen coordinate handling.

### What not to copy directly

- Sunshine's specific controller emulation choices,
- Moonlight's UI shortcut combos unless a Qubox UI design explicitly needs them.

## Phase 5: Host Capture and Hardware Acceleration

### Qubox focus

- `crates/qubox-media/src/lib.rs`
- `crates/qubox-platform/src/lib.rs`
- host-side platform modules to be introduced

### Reference areas

- Sunshine:
  - `src/video.cpp`
  - `src/display_device.cpp`
  - `src/platform/**`
  - `CMakeLists.txt`
- Moonlight:
  - `app/app.pro`
  - client decoder modules for understanding backend diversity

### What to learn

- capability probing,
- encoder backend selection,
- colorspace and scaling preparation,
- native API versus FFmpeg library tradeoffs.

### What not to copy directly

- Sunshine's entire dependency graph,
- platform-specific build complexity that does not map to Rust packaging.

## Phase 6: Audio and Sync

### Qubox focus

- `apps/host-agent/src/main.rs`
- `apps/client-cli/src/runtime.rs`
- `crates/qubox-transport/src/lib.rs`
- receive-side audio modules to be introduced

### Reference areas

- Sunshine:
  - `src/audio.cpp`
  - `src/stream.cpp`
- Moonlight:
  - `moonlight-common-c/src/RtpAudioQueue.c`
  - `app/streaming/audio/audio.cpp`
  - `app/streaming/audio/renderers/sdlaud.cpp`

### What to learn

- audio packet and FEC logic,
- playback queue design,
- latency-sensitive audio buffering,
- sync rules between media clocks.

### What not to copy directly

- GameStream-specific audio header formats if Qubox uses a different packet protocol.

## Phase 7: GUI Integration and Productization

### Qubox focus

- `apps/client-gui/src/`
- `apps/client-gui/src-tauri/src/lib.rs`

### Reference areas

- Moonlight:
  - `app/gui/**`
  - `app/settings/**`
  - `app/streaming/session.cpp`
- Sunshine:
  - `src/confighttp.cpp`
  - `src/system_tray.cpp`

### What to learn

- separation of runtime from product UI,
- settings organization,
- runtime diagnostics surfaced to the UI.

### What not to copy directly

- Qt application structure,
- Sunshine's web configuration architecture unless Qubox explicitly wants a host web console.

## Phase 8: Hardening

### Qubox focus

- all touched crates and apps,
- scenario tests and instrumentation,
- packaging and diagnostics.

### Reference areas

- Sunshine:
  - `src/stat_trackers.*`
  - `tests/`
- Moonlight:
  - `app/deploy/linux/com.moonlight_stream.Moonlight.appdata.xml`
  - session and status handling in `app/streaming/session.cpp`

### What to learn

- degrade gracefully,
- expose useful performance information,
- keep platform-specific regressions visible.

## Short Note To Self

The correct use of Sunshine and Moonlight is as an implementation school, not as a literal template. Qubox should preserve its cleaner crate shape and modernize the media and input path using the lessons from those repos.
