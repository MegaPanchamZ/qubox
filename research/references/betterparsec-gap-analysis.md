# Qubox Gap Analysis

## Current Qubox Shape

Qubox is structurally clean but behaviorally thin.

Core responsibilities are split across:

- `crates/qubox-proto`
- `crates/qubox-signaling`
- `crates/qubox-media`
- `crates/qubox-transport`
- `apps/host-agent`
- `apps/client-cli`
- `apps/client-gui`

That is a good long-term shape, but the current runtime still behaves like a prototype.

## How It Works Today

### Capture and frame production

`crates/qubox-media/src/lib.rs`

- Plans FFmpeg capture pipelines.
- Spawns FFmpeg as a subprocess.
- Reads H.264 Annex B access units from stdout.
- Performs basic access-unit framing and keyframe detection.

This is a useful bootstrap, but it is not equivalent to a production host pipeline.

### Transport

`crates/qubox-transport/src/lib.rs`

- Uses QUIC with authenticated session setup.
- Opens a reliable auth stream, a media stream, an audio stream, and a control stream.
- Writes access units with JSON headers followed by raw bytes.

This means the media path is still modeled as a reliable ordered byte stream carrying full access units.

### Client rendering

`apps/client-cli/src/runtime.rs`

- Launches FFmpeg as an external decoder.
- Reads raw BGRA frames from FFmpeg stdout.
- Converts BGRA to `u32` pixels.
- Paints frames in a `minifb` window.

This is simple and effective for a proof of life, but it is missing most of the runtime sophistication that Moonlight has.

### Input

`apps/client-cli/src/runtime.rs` and `apps/host-agent/src/main.rs`

- Client captures only mouse move, mouse button, and keyboard press or release.
- Events are sent individually.
- Host injects mouse and keyboard via `enigo`.
- Input coordinate mapping is a simple scale from stream size to host display size.

## Why the App Feels Laggy

### 1. Media is still delivered as a reliable ordered stream

This is the single biggest structural issue.

Reliable ordered delivery is excellent for control traffic and setup, but it is not the ideal primary model for real-time video. When packets are delayed or lost, later media waits behind earlier media.

Result:

- visible input-to-photon delay spikes,
- poor degradation under loss,
- poor responsiveness on imperfect networks.

### 2. No jitter buffer or receive-side packet model

There is no equivalent to Moonlight's video or audio RTP queues.

Result:

- no controlled reordering,
- no packet-loss recovery window,
- no receive pacing based on stream timing.

### 3. Decode and present path is naive

`apps/client-cli/src/runtime.rs` shows:

- FFmpeg subprocess decode,
- frame copies through process pipes,
- conversion to window pixels every frame,
- presentation in `minifb` without richer display control.

Result:

- avoidable process and copy overhead,
- no proper hardware decode path,
- no advanced window mode or monitor handling,
- weak scaling behavior.

### 4. Input is too small a protocol

`crates/qubox-proto/src/lib.rs` only models:

- mouse move,
- mouse button,
- keyboard.

Missing entirely:

- scroll,
- gamepads,
- analog triggers and sticks,
- rumble,
- gyro and accelerometer,
- touch,
- pen,
- absolute mouse versus relative mouse modes,
- modifier and hotkey semantics beyond key strings.

Result:

- bad remote desktop ergonomics,
- bad game ergonomics,
- no serious controller path,
- no advanced host feedback.

### 5. Scaling is simplistic

Current host input scaling only maps stream coordinates to display coordinates.

What is missing:

- explicit aspect-ratio aware viewport math,
- letterbox handling,
- DPI awareness beyond initial injection setup,
- fullscreen and borderless presentation rules,
- refresh-rate aware presentation.

Result:

- input drift near edges,
- poor cursor feel,
- poor behavior when stream and output geometry differ,
- weak user-perceived polish.

## Capability Gaps Compared with Sunshine and Moonlight

### Media and transport gaps

- No FEC.
- No datagram-oriented real-time media layer.
- No receive-side jitter queue.
- No frame-loss feedback to host.
- No adaptive bitrate or FEC tuning.
- No serious pacing model.

### Input gaps

- No gamepad support.
- No rumble.
- No motion sensors.
- No touch or pen.
- No scroll wheel.
- No absolute-mouse remote desktop mode.
- No background input or cursor-capture model.

### Presentation gaps

- No mature windowing toolkit in the native viewer path.
- No decoder backend selection.
- No GPU-present path.
- No monitor or refresh-rate handling.
- No aspect-ratio preserving viewport model.

## Library Stack Reality Today

### Qubox currently uses

- `quinn` through `qubox-transport`
- `tokio`
- `tokio-tungstenite`
- `serde` and `serde_json`
- `cpal`
- `enigo`
- `minifb`
- external `ffmpeg` process invocation in the media and viewer paths

This is enough for a technical prototype but not enough for a polished, low-latency, multi-platform remote desktop or game-streaming product.

## Most Important Conclusions

1. The crate structure is not the core problem.
2. The transport and input semantics are the core problem.
3. Qubox needs a real media packet model and real receive pipeline.
4. Qubox needs a richer client runtime, not just a better encoder.
5. Qubox needs a full input and controller architecture, not incremental keyboard and mouse patches.
