# Step 3 Phase Implementation Guide

## Goal

Provide a structured implementation guide for each phase of the Qubox system upgrade, including concrete deliverables, dependency order, validation targets, and notes on which Sunshine and Moonlight code should be referenced while implementing.

## Forecast

- Repository: `h:/development/qubox`
- Files to modify for this planning step:
  - `research/plans/features/transport-architecture-research/step-3-phase-implementation-guide.md`
  - `research/references/implementation-reference-map.md`
  - `research/plans/features/transport-architecture-research/tasks.md`
  - `research/logs/daily/2026-05-16.md`

## Verification

- `git status --short`

## General Rules For Every Phase

1. Keep prototype and production paths separate until the replacement path is proven.
2. Validate the smallest usable slice before widening scope.
3. Do not allow UI concerns to lead transport design.
4. Do not copy protocol artifacts from Sunshine or Moonlight unless the underlying methodology is also needed.
5. Prefer one working native path over shallow support for every platform.

## Phase 0: Stabilization and Measurement

### Purpose

Create a truthful latency and quality baseline before replacing runtime pieces.

### Deliverables

- host-side timing spans for capture, encode, packetize, and send
- client-side timing spans for receive, decode, queue wait, and present
- basic counters for frame drops, queue depth, and input send rate
- a structured log format that can be compared across runs

### Qubox execution notes

- Start in `apps/host-agent/src/main.rs`, `apps/client-cli/src/runtime.rs`, and `crates/qubox-transport/src/lib.rs`.
- Add timestamp propagation with frame IDs before changing transport semantics.
- Keep instrumentation cheap and always-on in debug builds.

### Reference notes

- Study Sunshine session and stat tracking in `src/stream.cpp` and `src/stat_trackers.cpp`.
- Study Moonlight session status handling in `app/streaming/session.cpp` and bandwidth reporting in `app/streaming/bandwidth.cpp`.
- Do not recreate every stat they expose. Keep only metrics that guide architectural decisions.

### Validation

- `cargo check`
- local host and client smoke run with readable timing logs

### Exit condition

- a single session can be profiled end to end and the main latency buckets are visible.

## Phase 1: Packet and Transport Redesign

### Purpose

Replace reliable access-unit streaming with explicit packetized real-time media semantics.

### Deliverables

- media packet schema
- QUIC datagram send and receive path for video
- reliable control stream kept for auth, negotiation, and feedback
- loss and recovery feedback message types
- first FEC block format draft

### Qubox execution notes

- Start by introducing packet types and codecs without deleting the old access-unit path.
- Keep a compatibility path while packet transport is being proven.
- Avoid JSON for per-packet hot paths.
- Use `bytes`-oriented buffers and explicit binary layouts.

### Reference notes

- Study Sunshine `src/stream.cpp` for session separation and packet-building patterns.
- Study Moonlight `moonlight-common-c/src/Connection.c` and `moonlight-common-c/src/Limelight-internal.h` for how feedback and receive logic expect explicit transport state.
- Do not copy RTSP or RTP compatibility details literally. Translate the concepts into a Qubox-native format.

### Validation

- packet serialization tests
- `cargo check`
- local datagram send and receive smoke test

### Exit condition

- host can send packetized media datagrams and client can parse them deterministically.

## Phase 2: Receive Queue, Jitter Buffer, and FEC

### Purpose

Create the receive pipeline that makes media robust under reorder and loss.

### Deliverables

- sequence-aware packet queue
- reorder and expiry policy
- frame assembly stage
- Reed-Solomon recovery stage
- decoder-facing access-unit output
- recovery and keyframe state tracking

### Qubox execution notes

- Implement the queue as a standalone module or crate with deterministic tests.
- Keep codec parsing minimal at first; solve transport integrity before deep codec awareness.
- Add synthetic test cases for reorder, burst loss, duplicate packets, and delayed recovery.

### Reference notes

- Study `moonlight-common-c/src/RtpVideoQueue.c` first.
- Study `moonlight-common-c/src/VideoDepacketizer.c` second.
- Study Sunshine `src/stream.cpp` only for host-side parity generation patterns.
- Do not mirror Moonlight's exact data structures unless they fit Rust cleanly.

### Validation

- `cargo test`
- queue and recovery tests with synthetic inputs
- end-to-end local run with controlled packet impairment if available

### Exit condition

- client continues rendering under moderate reorder and moderate packet loss.

## Phase 3: Viewer, Scaling, and Presentation

### Purpose

Replace the simplistic viewer stack and fix scaling correctness.

### Deliverables

- `winit` viewer shell
- correct stream-to-viewport mapping
- aspect-ratio preserving presentation
- letterboxing math
- fullscreen and borderless modes
- monitor and DPI handling

### Qubox execution notes

- Remove dependence on `minifb` for the production path.
- Separate window size from stream size and from visible video region.
- Preserve input coordinate mapping through viewport transforms.

### Reference notes

- Study Moonlight `app/streaming/streamutils.cpp` first.
- Study `app/streaming/video/ffmpeg-renderers/pacer/pacer.cpp` for timing ideas.
- Study Sunshine `src/video.cpp` and `src/display_device.cpp` for host-side display assumptions.
- Do not copy Qt UI patterns.

### Validation

- `cargo check`
- manual resize, monitor move, fullscreen, and aspect-ratio tests

### Exit condition

- the viewer behaves correctly under resize and input coordinates remain stable across window modes.

## Phase 4: Input and Controller Rewrite

### Purpose

Create an input subsystem that can handle both remote desktop and gaming use cases.

### Deliverables

- richer protocol-level input types
- keycode or scancode representation plus modifiers
- scroll events
- relative and absolute mouse modes
- controller state model
- first rumble feedback path
- host input backend abstraction

### Qubox execution notes

- Stop sending stringified key names.
- Do not hard-wire all host-side injection into `host-agent` forever.
- Introduce a shared input crate or module early so host and client stay aligned.
- Implement one controller path end to end before expanding to advanced controller features.

### Reference notes

- Study Sunshine `src/input.cpp` and `src/input.h` for host input translation shape.
- Study platform-specific controller and touch modules under Sunshine `src/platform/`.
- Study Moonlight `app/streaming/input/mouse.cpp`, `keyboard.cpp`, and `gamepad.cpp` for client semantics.
- Copy the idea of multiple mouse modes. Do not copy all shortcut conventions unless they fit Qubox UX.

### Validation

- `cargo check`
- local keyboard, mouse, scroll, and controller smoke tests

### Exit condition

- remote desktop mode and game mode both have correct core input semantics on at least one platform pair.

## Phase 5: Host Capture and Acceleration

### Purpose

Reduce latency and increase platform credibility by moving toward native capture and serious acceleration backends.

### Deliverables

- native capture backend abstraction
- one non-subprocess capture path on a primary platform
- encoder ranking and fallback policy
- capability probes with actionable output

### Qubox execution notes

- Keep FFmpeg subprocess path as fallback until native backends are proven.
- Prioritize the platform that will become the main development target.
- Avoid trying to perfect every capture API in one phase.

### Reference notes

- Study Sunshine `src/video.cpp` for conversion, scaling, and backend selection behavior.
- Study Sunshine `src/platform/**` only for platform-specific capture direction and integration strategy.
- Study Moonlight decoder backend diversity only to understand how much backend variation a client may need later.

### Validation

- `cargo check`
- probe command validation
- per-platform smoke tests on the first chosen backend

### Exit condition

- the host can run at least one real native capture path with measurable improvement over the subprocess fallback.

## Phase 6: Audio and Sync

### Purpose

Make audio robust enough for production use and keep it synchronized with video.

### Deliverables

- compressed audio transport path
- audio receive queue and jitter handling
- sync policy between audio and presented video
- mute and focus policies

### Qubox execution notes

- Keep audio architecture separate from video, but expose shared timing data.
- Prefer simple, stable sync rules before advanced adaptive policies.

### Reference notes

- Study Sunshine `src/audio.cpp` and its coupling points in `src/stream.cpp`.
- Study Moonlight `moonlight-common-c/src/RtpAudioQueue.c` and playback modules under `app/streaming/audio/`.
- Copy the discipline around buffering and recovery, not the wire format.

### Validation

- `cargo test`
- manual sync validation

### Exit condition

- audio remains stable and sync stays acceptable under normal network variability.

## Phase 7: GUI Integration and Productization

### Purpose

Make the improved runtime usable from the product surface.

### Deliverables

- GUI-launched native runtime path
- settings for transport, input, scaling, and backend selection
- session stats surface
- improved known-host metadata

### Qubox execution notes

- Keep real-time rendering and input out of the webview itself.
- Let the GUI orchestrate the runtime, not replace it.

### Reference notes

- Study Moonlight settings and streaming session orchestration.
- Study Sunshine configuration and tray patterns only where they help with runtime control and diagnostics.

### Validation

- frontend build
- Tauri build
- manual session launch flow

### Exit condition

- GUI is a viable control surface for the upgraded runtime.

## Phase 8: Hardening

### Purpose

Make the system survivable under real user conditions instead of just demo conditions.

### Deliverables

- soak tests
- network impairment tests
- crash and fallback handling around media backends
- compatibility notes and platform fallbacks

### Qubox execution notes

- this phase should start earlier in miniature, but only becomes the primary focus after the core runtime exists.
- preserve logs and metrics that help diagnose user failures.

### Reference notes

- Study Sunshine stats and error handling.
- Study Moonlight session termination and user-facing status behavior.

### Validation

- scenario tests
- manual cross-platform validation matrix

### Exit condition

- the system fails predictably and recovers gracefully in the most common bad conditions.

## Personal Implementation Notes

### Note 1

Do not conflate reference code with target code. Sunshine and Moonlight show what mature behavior looks like, but Qubox should stay Rust-first and crate-oriented.

### Note 2

The most dangerous trap is polishing the outer shell while the receive path is still architecturally wrong.

### Note 3

If time is constrained, the minimum sequence that matters is:

1. packetized transport,
2. receive and recovery pipeline,
3. real viewer and scaling,
4. serious input.

### Note 4

Do not defer gamepad architecture until the end if the product target includes games. The protocol and input model should leave room for it early, even if full implementation arrives in Phase 4.
