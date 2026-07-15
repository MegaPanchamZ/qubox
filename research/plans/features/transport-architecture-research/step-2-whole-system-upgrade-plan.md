# Step 2 Whole System Upgrade Plan

## Goal

Define a realistic end-to-end plan to upgrade Qubox from a prototype into a reliable multi-platform remote desktop and game-streaming system.

## Forecast

- Repository: `h:/development/qubox`
- Files to modify for this planning step:
  - `research/decisions/ADR-002-target-architecture-and-upgrade-strategy.md`
  - `research/plans/roadmap.md`
  - `research/plans/features/transport-architecture-research/tasks.md`
  - `research/plans/features/transport-architecture-research/step-2-whole-system-upgrade-plan.md`
  - `research/logs/daily/2026-05-16.md`

## Exact Types / Concepts To Define

- System upgrade phases and milestones.
- Proposed crate and module boundaries for the production runtime.
- Verification strategy per phase.
- Dependency and migration order.

## Verification

- `git status --short`

## Upgrade Strategy

### Principle: no big-bang rewrite

The correct path is a strangler-style replacement of the runtime, not a single rewrite branch that tries to replace everything at once.

The current system has enough shape to keep:

- `qubox-signaling` as control-plane foundation,
- `qubox-proto` as the shared model anchor,
- `qubox-platform` as the place for host and client platform abstractions,
- the app split between host, client, and GUI.

The parts that need aggressive replacement are:

- native media transport semantics,
- receive-side media pipeline,
- presentation runtime,
- input protocol and runtime,
- host capture and acceleration design.

## Target Repository Shape

### Existing crates to retain and evolve

- `crates/qubox-proto`
- `crates/qubox-signaling`
- `crates/qubox-platform`

### Existing crates to refactor substantially

- `crates/qubox-media`
- `crates/qubox-transport`
- `apps/host-agent`
- `apps/client-cli`
- `apps/client-gui`

### Proposed new crates or major modules

1. `crates/qubox-packets`
   - packet formats for native media and input feedback
   - frame, packet, FEC, and stats headers

2. `crates/qubox-receive`
   - reorder queue
   - jitter buffer
   - FEC recovery
   - depacketizer
   - frame assembly

3. `crates/qubox-input`
   - shared input model
   - controller model
   - keyboard and mouse code representation
   - optional touch and pen model

4. `crates/qubox-present`
   - native viewer runtime
   - viewport and scaling math
   - display mode handling
   - future decoder-present integration

These may begin as modules inside existing crates if necessary, but the architectural goal should remain clear.

## Phase Plan

### Phase 0: Stabilization and measurement

Objective:

- establish performance baselines before invasive changes.

Work:

- instrument host capture, encode, send, receive, decode, and present timings.
- add counters for dropped frames, backlog depth, and input latency.
- record host display size, stream size, and present size in logs.

Likely files:

- `apps/host-agent/src/main.rs`
- `apps/client-cli/src/runtime.rs`
- `crates/qubox-transport/src/lib.rs`

Acceptance:

- end-to-end latency can be decomposed into at least capture, encode, network, decode, and present slices.
- viewer can report effective frame rate and dropped frame count.

Verification:

- `cargo check`
- targeted local streaming smoke test

### Phase 1: Protocol and transport redesign

Objective:

- replace reliable access-unit streaming with packetized real-time media transport.

Work:

- add media packet types and versioning.
- add QUIC datagram path for video.
- keep reliable QUIC streams for auth, stats, config, and control.
- design feedback messages for loss, jitter, and decoder resync.
- define FEC block format.

Likely files:

- `crates/qubox-proto/src/lib.rs`
- `crates/qubox-transport/src/lib.rs`
- new `crates/qubox-packets`

Acceptance:

- host can emit packetized video frames over datagrams.
- client can receive packets without relying on ordered byte-stream reconstruction.

Verification:

- `cargo check`
- packet encode/decode unit tests
- local packet-loss simulation tests if added

### Phase 2: Client receive pipeline

Objective:

- build the path Moonlight already has: reorder, recover, assemble, decode handoff.

Work:

- implement sequence-aware receive queue.
- add jitter buffering and packet expiry rules.
- add Reed-Solomon FEC recovery.
- add access-unit reconstruction and missing-frame handling.
- add keyframe and recovery state handling.

Likely files:

- new `crates/qubox-receive`
- `apps/client-cli/src/runtime.rs`
- `crates/qubox-transport/src/lib.rs`

Acceptance:

- client tolerates packet reordering and moderate packet loss without catastrophic visual corruption.
- client can request recovery or identify keyframe dependency gaps.

Verification:

- `cargo test`
- targeted receive-pipeline tests with synthetic reorder/loss cases

### Phase 3: Viewer and scaling runtime replacement

Objective:

- replace `minifb` with a real viewer stack.

Work:

- move to `winit` for windowing.
- add fullscreen, borderless, and windowed modes.
- add aspect-ratio aware viewport math and letterboxing.
- prepare for GPU presentation via `wgpu`.
- separate stream resolution from presentation resolution.

Likely files:

- new `crates/qubox-present`
- `apps/client-cli/src/runtime.rs`
- `apps/client-gui/src-tauri/src/lib.rs`

Acceptance:

- viewer handles resize, monitor changes, DPI, and aspect ratio correctly.
- input coordinates stay correct within the actual visible video viewport.

Verification:

- `cargo check`
- manual window-mode and resize validation

### Phase 4: Input subsystem rewrite

Objective:

- make input serious enough for both remote desktop and games.

Work:

- replace string-key keyboard events with scan or keycode representation plus modifiers.
- add scroll.
- add explicit relative and absolute mouse modes.
- add controller model: buttons, sticks, triggers, controller IDs.
- add rumble feedback path.
- add host-side input backend abstraction instead of keeping all logic in `host-agent`.

Likely files:

- `crates/qubox-proto/src/lib.rs`
- new `crates/qubox-input`
- `apps/client-cli/src/runtime.rs`
- `apps/host-agent/src/main.rs`

Acceptance:

- remote desktop mode works with absolute mouse and scroll.
- game mode works with relative mouse.
- at least one controller path works end to end.

Verification:

- `cargo check`
- end-to-end local input smoke tests

### Phase 5: Host capture and acceleration backends

Objective:

- reduce latency and increase platform credibility by moving beyond subprocess-first FFmpeg capture.

Work:

- add native capture backend abstraction.
- prefer PipeWire on Linux Wayland.
- add Windows capture backend beyond gdigrab.
- design macOS capture backend entry point.
- standardize encoder capability probing and fallback ranking.

Likely files:

- `crates/qubox-media/src/lib.rs`
- `crates/qubox-platform/src/lib.rs`
- new platform-specific modules under `crates/qubox-media` or `crates/qubox-platform`

Acceptance:

- host can choose a native capture path on at least one major platform.
- FFmpeg subprocess path remains only as fallback or diagnostic path.

Verification:

- `cargo check`
- host probe commands
- platform-specific smoke capture validation

### Phase 6: Audio and A/V sync hardening

Objective:

- make audio production-grade rather than merely present.

Work:

- add audio packetization and receive jitter handling.
- add Opus or another practical compressed audio path.
- improve sync strategy between audio clock and presented video frames.
- add mute/focus/background policies for client runtime.

Likely files:

- `crates/qubox-transport/src/lib.rs`
- new or expanded audio modules in `crates/qubox-receive`
- `apps/client-cli/src/runtime.rs`
- `apps/host-agent/src/main.rs`

Acceptance:

- audio remains stable under moderate network variability.
- A/V sync stays within acceptable range during normal operation.

Verification:

- `cargo test`
- manual sync validation

### Phase 7: GUI integration and productization

Objective:

- turn the improved runtime into a usable product surface.

Work:

- promote Tauri GUI from scaffold to controller of the native runtime.
- add session stats and diagnostics view.
- add capture, encoder, input-mode, and display preferences.
- add host discovery and richer paired-host metadata.

Acceptance:

- GUI launches and controls the production viewer/runtime path.

Verification:

- `cargo check`
- frontend build and manual session launch validation

### Phase 8: Production hardening

Objective:

- make the system survivable under real user conditions.

Work:

- add soak tests and network-impairment tests.
- add crash resilience around decoder and capture backends.
- add telemetry or structured performance traces.
- add compatibility matrix and fallback behavior per platform.

Acceptance:

- transport and viewer degrade gracefully under loss, jitter, resize, and focus changes.

Verification:

- `cargo test`
- scenario tests and manual matrix validation

## Critical Sequencing Rules

1. Do not start with controller polish before fixing media semantics.
2. Do not spend large effort on capture acceleration before the client receive path is real.
3. Do not keep adding features to `minifb`.
4. Keep prototype and production paths side by side only as long as they reduce migration risk.
5. Prefer shipping one solid native path first rather than three weak platform paths at once.

## First 90-Day Execution Plan

### Sprint A

- add metrics and timing instrumentation
- draft packet format and datagram transport path
- introduce new packet crate or equivalent module

### Sprint B

- build receive reorder queue
- build frame assembly and partial loss handling
- add first FEC path

### Sprint C

- replace `minifb` viewer with `winit` viewer shell
- add viewport math and correct input coordinate mapping

### Sprint D

- expand input protocol with scroll, mouse modes, and keycode-based keyboard
- add first controller path via `gilrs`

## Recommended Definition of Done for "System Upgrade" MVP

The system should not be considered upgraded until it can do all of the following on at least one primary platform pair:

- sustain a session with packetized media transport,
- tolerate reorder and moderate packet loss,
- present frames in a real resizable or fullscreen window with correct aspect ratio,
- provide correct absolute and relative mouse behavior,
- provide scroll and keyboard modifier correctness,
- provide at least one working controller path,
- expose enough metrics to diagnose latency and drops.
