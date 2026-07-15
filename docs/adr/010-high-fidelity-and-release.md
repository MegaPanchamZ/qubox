# ADR-010 High-Fidelity Streaming, Stylus Integration, and Release Packaging

## Status

Proposed. Branch: `feature/adr-010-hdr-pen-release`. Based on `main` at commit
`883a6dd` ("P0-3 + P0-5 — in-process ffmpeg-next decoder + wgpu renderer",
252 tests green). Builds on ADR-009 (the wgpu + HW decoder substrate landed
across `apps/client-cli/src/{decoder_hw,render_wgpu,frame_pipeline,winit_user_event}.rs`
plus the existing `start_session` re-export) and ADR-008 (clipboard/mic + the
shared datagram path with the gamepad 0x47 / mic 0x4D discriminator scheme).

## Context

ADR-009 wired up the in-process `ffmpeg-next` HW decoder and the wgpu renderer
scaffold (`apps/client-cli/src/render_wgpu.rs:73-91` `WgpuRenderer`,
`apps/client-cli/src/decoder_hw.rs:53-67` `HwDeviceType`, the cross-thread
`crossbeam-channel` between decoder and renderer), and explicitly deferred
four items to "Path 3, which becomes ADR-010":

1. Full winit `ApplicationHandler<WinitUserEvent>` integration with a single
   `EventLoop<WinitUserEvent>` driving Main Video + Blank Overlay + Tiled View.
2. Real `av_hwdevice_ctx_create` + `get_format` `extern "C"` FFI for VAAPI /
   CUDA / D3D11VA / VideoToolbox HW device binding.
3. `wgpu_glyph` migration of the P1-12 stats overlay (`apps/client-cli/src/stats_overlay.rs:20`
   already calls out the long-term wgpu plan).
4. Wire-format extensions for HDR static metadata + pen/tablet events + 4K144
   capabilities, plus the actual platform ports for macOS ScreenCaptureKit
   (`crates/qubox-media/src/lib.rs:2204-2247` is the `probe_windows_gdigrab_capture`
   substrate but macOS path is currently `cfg`-gated to a stub) and Windows
   DXGI (currently exercised only via the `gdigrab` ffmpeg option), and
   release packaging for Authenticode / `notarytool` / SPDX SBOM.

This ADR closes out the high-fidelity phase (P2-14 HDR, P2-15 Pen, P2-16
4K144), the platform ports (P2-17 macOS, P2-18 Windows DXGI confirmation),
and release packaging (P2-19). It also retires the three deferred items
above as in-flight integration.

Constraints that shape the design:

- Project rule #1: no prose comments inside function bodies — `//!` / `///`
  only. The tone of existing code at `apps/host-agent/src/main.rs:1-1600`
  and the new wgpu shaders at `render_wgpu.rs:42-67` already obey this.
- Project rule #2: every new proto field MUST have `#[serde(default)]` so
  clients built against earlier schemas keep deserializing new payloads.
  The existing pattern at `crates/qubox-proto/src/lib.rs:557-558`
  (`#[serde(default)] pub video: Option<VideoStreamPreferences>` in
  `SessionRequested`) is the template.
- Project rule #3: `winit::EventLoop` is a process singleton. Today
  `apps/client-cli/src/blank_overlay.rs` is a separate `minifb::Window`
  (per ADR-009 §4.1: "consolidates the input pump and hotkey handling").
  This ADR promotes blank-overlay into a winit subwindow driven by the
  same `ApplicationHandler<WinitUserEvent>`.
- Project rule #4: `enigo 0.6` is not `Send + Sync` (declared at
  `apps/host-agent/src/main.rs` deps). Pen injection does **not** use enigo
  (different API surface: WinTab `WT_PACKET`, `uinput::Event::*`), so this
  rule is irrelevant for §4.
- Project rule #5: `client_cli::start_session` import stub preserved.
  `apps/client-cli/src/lib.rs:14` re-exports `runtime::start_session`; the
  Tauri GUI imports it. We do not modify the re-export. A new
  `start_session_v2` enters `apps/client-cli/src/runtime.rs` and is wired
  to the new `winit_app.rs` dispatcher but the old call site keeps
  compiling untouched.
- Project rule #6: no sudo. The only allowed installation steps are
  `pkg-config` and `bindgen`. New `crates/qubox-pen` deps are all
  `cfg`-gated; `libinput` and `uinput` are optional features with a
  sensible default.
- Project rule #7: `--datagram-media` default is on. The pen wire format
  sits inside that same QUIC datagram path (see §1.1 discriminator 0x50),
  same as mic 0x4D and gamepad 0x47
  (`crates/qubox-proto/src/lib.rs:716-720`).
- Project rule #8: `tracing::*!`, `anyhow::Result`, `.context(...)`, `?`.
  The new modules follow it.

Substrate state at `883a6dd` (verified):

- `apps/host-agent/src/capture_orchestrator.rs:1-813` — multi-stream
  capture orchestrator with `CaptureOrchestrator::new` at `:90-105` and
  `subscribe` at `:150-171`. Already takes a `hover_display_tx:
  tokio::sync::mpsc::UnboundedSender<RemoteInputEvent>` (`:99`) — the
  `RemoteInputEvent::HoverDisplay` variant at
  `crates/qubox-proto/src/lib.rs:531-533` is the existing precedent
  for augmenting the enum without breaking ABI.
- `crates/qubox-media/src/lib.rs:1-2680` — full media pipeline
  (H.264 / H.265 / AV1 encoders at `:1721-1741`, libswscale path at
  `:1152-1269` `encoder_args_for`, libpipewire for Linux at
  `:2110-2155`, DXGI stub for Windows at `:935-945`, ScreenCaptureKit
  stub for macOS — both gated on compile-time platform triples).
- `crates/qubox-display/src/` — display enumeration, X11 / Wayland
  window manipulation, DPMS, vkms. The `DisplayPipeline::DisplayInfo` at
  `capture_orchestrator.rs:68-73` propagates up; we extend it with HDR
  color space advertisement per §3.5.
- `crates/qubox-proto/src/lib.rs:1-1213` — full proto surface.
  `RemoteInputEvent` at `:495-534`; `ControlMsg` at `:287-372`; mic wire
  header at `:735-741` (8 bytes); gamepad wire state at `:15-54`
  (28 bytes).
- `crates/qubox-transport/src/media/mod.rs:1-1090` — datagram
  machinery. `MEDIA_DATAGRAM_MAGIC = [0xB2, 0x16]` at `:30`, gamepad
  encode at `:846-862`, gamepad decode at `:870-888`.
- `apps/client-cli/src/decoder_hw.rs:1-478` — `RunningHwFrameDecoder` via
  ffmpeg-next in-process; software fallback via libswscale; HW device
  preference list declared at `:69-90` but the `av_hwdevice_ctx_create`
  call is still pending.
- `apps/client-cli/src/render_wgpu.rs:1-496` — `WgpuRenderer` with
  `Bgra8Unorm` swapchain, Mailbox/Fifo fallback (the `VIDEO_BLIT_WGSL`
  shader at `:42-67`), `WgpuRenderer::new` at `:98-271`,
  `WgpuRenderer::upload_frame` at `:288-341`.
- `apps/client-cli/src/stats_overlay.rs` — software overlay (P1-12);
  `:20` already documents the GPU replacement plan.
- `apps/client-cli/src/blank_overlay.rs` — privacy blank overlay; today a
  separate minifb window (will become a winit subwindow in §2.1).
- `apps/client-cli/src/tiled_view.rs` — multi-display tiled view.
- `apps/client-cli/src/winit_user_event.rs` — `WinitUserEvent` enum
  (matches the ADR-009 §4.2 design).
- `apps/host-agent/src/main.rs:1-2005` — `enigo` for keyboard/mouse
  injection (channel-routed due to `!Send`); `RemoteInputInjector::new`
  at `:434-460`, `RemoteInputInjector::apply` at `:462-502`.
- `apps/client-cli/src/gamepad_capture.rs:1-154` — gamepad capture on
  client (gilrs-based; parallels the new pen capture shape).
- `.github/workflows/ci.yml` — Rust CI matrix (ubuntu/windows/macos at
  `:23-25`), client-gui Tauri build, daemon MinGW cross-compile.
- `apps/daemon/dist/` — Linux systemd units, Windows MSI, macOS
  pkgproj + plist (existing release infra, extended in §8).
- `dist/windows-x86_64/` — pre-built Windows .exe artifacts.
- TUF auto-update lives in `apps/daemon/src/`; TUF signing keys in
  `/ops/tuf/keys/`.

## Decision

### 1. Wire format changes

All extensions are backward-compatible: every new field carries
`#[serde(default)]` (project rule #2), every new `RemoteInputEvent`
variant is an append at the tail of the enum at
`crates/qubox-proto/src/lib.rs:495-534`, every new `ControlMsg`
variant is an append at the tail of the enum at `:287-372`, and the new
packed structs match the `WireMicHeader` / `WireGamepadState` templates
for the QUIC datagram path.

#### 1.1 New `RemoteInputEvent::Pen` variant

Added at line ~535 (after `HoverDisplay`):

```rust
/// Pen / tablet event captured on the client. Routes over the
/// same reliable control uni-stream as keyboard/mouse, NOT through
/// the high-frequency datagram path; pen events tolerate loss but
/// the connection/disconnection lifecycle requires reliability. The
/// high-frequency per-sample stream rides `WirePenEvent` over the
/// QUIC datagram channel (§1.2).
Pen {
    /// Logical pen tool. `#[serde(default)]` for backward compat.
    #[serde(default)]
    tool: PenTool,
    /// 0.0 ..= 1.0; Pen only meaningful (else 0).
    #[serde(default)]
    pressure: f32,
    /// Degrees, -90 ..= 90; Pen only.
    #[serde(default)]
    tilt_x: f32,
    /// Degrees, -90 ..= 90; Pen only.
    #[serde(default)]
    tilt_y: f32,
    /// Degrees, 0 ..= 360.
    #[serde(default)]
    rotation: f32,
    /// Tool-specific button bitmask (Eraser tip, barrel buttons, etc.).
    #[serde(default)]
    button_state: u32,
    /// Pixels, screen-space, relative to the captured display.
    x: u16,
    y: u16,
    /// Hover distance in millimeters; 0 = contact; u16::MAX = "out of range".
    #[serde(default)]
    hover_distance: u16,
},
```

`PenTool` enum (after `InputMouseButton` at
`crates/qubox-proto/src/lib.rs:485-491`):

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PenTool {
    Pen, Eraser, Brush, Airbrush, Finger, Mouse, Lens,
}
```

#### 1.2 New `WirePenEvent` packed struct

Mirrors `WireMicHeader` at
`crates/qubox-proto/src/lib.rs:735-741`. Lives in
`crates/qubox-proto/src/pen.rs` (new module) and is re-exported
through `lib.rs`. 36 bytes total:

| Offset | Bytes | Field | Notes |
|--------|-------|-------|-------|
| 0..2   | 2     | `magic = [0xB2, 0x16]`  | Same as `MEDIA_DATAGRAM_MAGIC` at `crates/qubox-transport/src/media/mod.rs:30`. |
| 2      | 1     | `discriminator = 0x50` ('P') | New; sits alongside `0x47` (gamepad) and `0x4D` (mic). |
| 3      | 1     | `flags` | bit 0 = `FLAG_LAST_IN_BURST`, bit 1 = `FLAG_HAS_HOVER`, bit 2 = `FLAG_BARREL_PRESSED`. |
| 4..6   | 2     | `device_id: u16` | Index into the device list advertised at session start (see §4). |
| 6..8   | 2     | `tool_id: u16` | `PenTool` enum cast. |
| 8..12  | 4     | `x: f32` | Screen-space pixels (24-bit float approximation). |
| 12..16 | 4     | `y: f32` | |
| 16..20 | 4     | `pressure: f32` | 0..=1. |
| 20..24 | 4     | `tilt_x: f32` | Degrees. |
| 24..28 | 4     | `tilt_y: f32` | Degrees. |
| 28..32 | 4     | `rotation: f32` | Degrees. |
| 32..36 | 4     | `timestamp_us: u32` | Wraps every ~71 min. |

`hover_distance` and `button_state` are encoded into `flags` (bits
1-3) for transport size parity with the mic / gamepad datagram headers
(`WireMicHeader` is 8 bytes; `WireGamepadState` is 28 bytes; pen at 36
bytes sits in the same middle tier and stays under the 1200-byte
`CHUNK_PAYLOAD_MAX` cap at `crates/qubox-transport/src/media/mod.rs:95`
after any datagram header prefix).

Discriminator choice rationale: `0x50` is ASCII `'P'`, distinct from
`'M'` (mic, `0x4D` at `crates/qubox-proto/src/lib.rs:720`) and
`'G'` (gamepad, `0x47`). The single `buf[2]` dispatch byte at
`crates/qubox-transport/src/media/mod.rs:846-862` becomes a
three-arm match.

#### 1.3 New `ControlMsg::DisplayCapabilities`

After `MicConfigAck` (which is at `crates/qubox-proto/src/lib.rs:364-371`):

```rust
/// Host → Client: advertise what the host can deliver so the client
/// can pick a compatible `VideoStreamPreferences` instead of probing
/// blindly. Always sent once per session, immediately after
/// `SessionEstablished`. `#[serde(default)]` on all fields.
DisplayCapabilities {
    /// `None` means the host output is SDR (BT.709 / sRGB). `Some`
    /// means the host display(s) can deliver HDR static metadata.
    #[serde(default)]
    hdr_static_metadata: Option<HdrStaticMetadata>,
    /// Max pixel resolution supported by the capture pipeline (after
    /// any client-side downscaling request this maps to 1:1).
    max_resolution: [u16; 2],
    /// Max refresh rate the pipeline can sustain, in Hz.
    max_refresh_hz: u32,
},
```

#### 1.4 `VideoStreamPreferences` extensions

`VideoStreamPreferences` lives at
`crates/qubox-proto/src/lib.rs:223-243`. All existing fields stay
unchanged. Append:

```rust
/// HDR transfer / primaries / matrix. `None` (the default) means SDR;
/// the host chooses BT.709 / sRGB defaults. `#[serde(default)]` on
/// every Option<u8>-equivalent field.
#[serde(default)]
pub color_space: Option<ColorSpace>,
/// 8 or 10. 8 is the implicit default; `#[serde(default = "default_eight")]`
/// returns 8.
#[serde(default = "default_eight")]
pub bit_depth: u8,
/// Cap the maximum framerate the host delivers, in frames per second.
/// Used for 4K144 preset negotiation: a 4K-capable client requests
/// 144 Hz; a 1080p240-only client requests 240 Hz on 1080p.
#[serde(default)]
pub max_framerate: Option<u32>,
/// The framerate the host should drive the encoder at. Distinct from
/// `max_framerate`: `max` is the cap, `target` is the goal. Default
/// is `None` (host picks 60).
#[serde(default)]
pub target_framerate: Option<u32>,
```

`fn default_eight() -> u8 { 8 }` next to the existing
`fn default_true() -> bool` at `:443-445`.

#### 1.5 New proto types: `HdrStaticMetadata` + `ColorSpace`

Added after `MicStreamConfig` (~L423):

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ColorSpace {
    Bt709, Bt2020, Bt2100, ScRgb,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct HdrStaticMetadata {
    /// CIE 1931 chromaticity primaries. See CTA-861-G §6.4.
    pub primaries: u8,
    /// Transfer characteristics. 16 = PQ (BT.2100), 18 = HLG.
    pub transfer: u8,
    /// Matrix coefficients. 9 = BT.2020 non-constant-luminance, 10 = BT.2020 constant-luminance.
    pub matrix: u8,
    /// Max Content Light Level in 1 cd/m² units. 0 if unknown.
    pub max_cll: u16,
    /// Max Frame-Average Light Level in 1 cd/m² units. 0 if unknown.
    pub max_fall: u16,
    /// Mastering display chromaticity + luminance, encoded as a
    /// raw CTA-861-G SEI blob (24 bytes). Empty if unknown.
    #[serde(default)]
    pub mastering_display_metadata: Vec<u8>,
}
```

#### 1.6 Backward-compat audit

- `RemoteInputEvent` is `#[serde(tag = "kind", rename_all = "snake_case")]` at
  `crates/qubox-proto/src/lib.rs:493-494`. Appending a new variant
  is a non-breaking change for Serde's untagged enum deserialization
  (older clients ignore unknown variants and log a warning via `tracing::warn!`).
- `ControlMsg` is also `#[serde(tag = "kind", rename_all = "snake_case")]`
  at `:286`. Same property.
- `VideoStreamPreferences` is a plain `#[derive(Serialize, Deserialize)]`
  struct at `:223`. With `#[serde(default)]` on every new field, an older
  client can deserialize a newer payload (fields fill in with defaults).
  And a newer client deserializing an older payload gets `None` / 8 for
  the defaults. The existing field set has no `#[serde(default)]` —
  a deliberate choice for required fields — which means we MUST add
  `#[serde(default)]` to the new ones in isolation (each is appended,
  never modifies an existing field).
- `WirePenEvent` is a packed `#[repr(C, packed)]` struct; the magic byte
  discriminator lets older QUIC datagram dispatchers in the wild treat
  it as "unknown variant" and drop it, exactly the behavior they already
  exhibit on 0x47 gamepad / 0x4D mic datagrams.

### 2. Deferred items from P0-3 / P0-5 (final integration)

#### 2.1 Full winit `ApplicationHandler<WinitUserEvent>`

Today the renderer instantiates an `EventLoop` somewhere in its
constructor flow (per ADR-009 §4.5), and `blank_overlay.rs` lives as
a separate `minifb::Window`. ADR-009 §4.5 declared the design; we
now write it.

A new file `apps/client-cli/src/winit_app.rs` (separate from
`winit_user_event.rs` which holds the enum) defines:

```rust
//! Process-wide winit application glue. Owns the EventLoop singleton,
//! routes WinitUserEvent to whichever sub-app is active, and exposes
//! `pub fn run_video_app(...)` as the single entry point used by
//! every renderer entry point.

pub trait AppState: 'static + Send {
    fn resumed(&mut self, event_loop: &ActiveEventLoop);
    fn redraw(&mut self, window: &Window);
    fn window_event(&mut self, window: &Window, event: WindowEvent);
    fn user_event(&mut self, event: WinitUserEvent, event_loop: &ActiveEventLoop);
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // default no-op
    }
}

pub fn run_video_app<App: AppState>(mut app: App) -> anyhow::Result<()> {
    let event_loop = EventLoop::<WinitUserEvent>::with_user_event()
        .build()?;
    let mut app = app;
    event_loop.run_app(&mut app)?;
    Ok(())
}
```

`WinitUserEvent` (already defined in `apps/client-cli/src/winit_user_event.rs`)
is reused; this ADR extends it with the new variants needed for §3, §4, §5:

```rust
#[derive(Debug, Clone)]
pub enum WinitUserEvent {
    FrameReady,
    Exit,
    Overlay(OverlayCommand),
    ToggleStats,
    ToggleTile,
    CycleStream,
    TogglePrivacy,
    // New in ADR-010:
    /// Sent when the host advertises new capabilities (P2-14).
    CapabilitiesChanged { hdr: bool, max_hz: u32, max_resolution: [u16; 2] },
    /// Sent when a pen device arrives or disappears (P2-15 §4 hotplug).
    PenDeviceList(Vec<PenDeviceInfo>),
    /// Sent when the user toggles tone mapping (`Ctrl+Alt+T`).
    CycleToneMap,
    /// Sent when the user adjusts the pen pressure curve (`Ctrl+Alt+P`).
    PenPressureCurve(f32),
}
```

`pub fn run_video_app(...)` is exposed via the new
`start_session_v2` declared in `apps/client-cli/src/runtime.rs:1-...`;
the existing `start_session` re-export at
`apps/client-cli/src/lib.rs:14` stays (project rule #5). The Tauri GUI
subprocess path (`apps/client-gui/src-tauri/`) keeps calling
`client_cli::start_session` unchanged.

Implementation steps land on individual commits for bisecting, per
ADR-009 §6.1 (the phase-by-phase checklist pattern):

| # | Step | Files | Notes |
|---|------|-------|-------|
| A | Add `WinitUserEvent` variants | `winit_user_event.rs` | Additive enum extension |
| B | Define `AppState` trait + `run_video_app` | new `winit_app.rs` | One `EventLoop::run_app` |
| C | Migrate Main Video renderer to `AppState::redraw` | `render_wgpu.rs`, `run_video_window.rs` | Honors project rule #3 |
| D | Migrate Blank Overlay to a winit subwindow | `blank_overlay.rs` | Removes the orphan `minifb::Window` |
| E | Migrate Tiled View to a winit subwindow | `tiled_view.rs` | Single `ApplicationHandler` for all three |
| F | Add capabilities / pen / tone-map user events | `winit_user_event.rs` | Per §3, §4, §5 |

#### 2.2 Real `av_hwdevice_ctx_create` + `get_format` FFI

ADR-009 §3.5 designed the callback; today
`apps/client-cli/src/decoder_hw.rs:69-90` only declares a
`HwDeviceType` enum with a `preferred_order()` static. This ADR wires
the FFI.

Three additions in `apps/client-cli/src/decoder_hw.rs`:

```rust
//! P2-14 / P2-18: real HW device binding via `av_hwdevice_ctx_create`.

extern "C" fn hw_get_format(
    ctx: *mut AVCodecContext,
    fmt: *const AVPixelFormat,
) -> AVPixelFormat {
    // Walk fmt[] until we find the pixfmt whose AVHWDeviceType matches
    // ctx->opaque. Attach shared AVHWFramesContext from the existing
    // codec ctx. Fallback to attach_sw_format() on miss.
}

extern "C" fn attach_sw_format(
    ctx: *mut AVCodecContext,
    fmt: *const AVPixelFormat,
) -> AVPixelFormat {
    // SW fallback: pick AV_PIX_FMT_YUV420P if codec supports it,
    // else the first entry of fmt. Used by `running_h264_smoke` and
    // any session where the GPU adapter probe failed.
}

fn try_create_hw_device(
    device_type: HwDeviceType,
) -> Result<*mut AVBufferRef, ffmpeg_next::Error> {
    // Call ffmpeg_next::hwdevice::find_format_for_hwdevice() to map
    // HwDeviceType → AVHWDeviceType, then av_hwdevice_ctx_create()
    // with the platform's default device ordinal:
    //   Linux   → "/dev/dri/renderD128"
    //   Windows → 0 (default adapter)
    //   macOS   → 0 (VideoToolbox default)
}
```

`RunningHwFrameDecoder::spawn` (currently at
`apps/client-cli/src/decoder_hw.rs:135-192`) is extended to:

1. Walk `cfg.preferred` and call `try_create_hw_device`. First success wins.
2. Allocate `AVHWFramesContext` (`initial_pool_size = max_num_ref_frames + 2`)
   matching §3.5 step 5 of ADR-009.
3. Set `ctx->get_format = hw_get_format` via `avcodec_set_hw_config`.
4. Stash the chosen device type in `ctx->opaque`.

Per-frame HW → CPU copyback
(`av_hwframe_transfer_data`) is the explicit decision: zero-copy direct
GPU surface handoff (VAAPI ↔ wgpu Vulkan) is **deferred to v2** because
it requires `wgpu::Texture::shared_handle_with` plumbing that ffmpeg
0.11 does not expose for VAAPI; the copyback is ~0.5 ms at 1080p on a
discrete GPU per ADR-009 §3.6 and is the documented Phase 2 target.

SW fallback path
(`attach_sw_format`) preserves the ADR-009 §3.6 `sws_scale` path so
`--decoder=sw` keeps working through the same code.

#### 2.3 `wgpu_glyph` migration of the stats overlay

Today `apps/client-cli/src/stats_overlay.rs` mutates a CPU BGRA buffer
in place. We replace the body of `paint_overlay` with a GPU draw,
keeps the existing data model (snapshot + Sections), and adds the
glyph atlas lazily on first call:

```rust
//! GPU text rendering for the stats overlay (P1-12 → P2-14).
//! See ADR-009 §4.6.

fn ensure_atlas(renderer: &WgpuRenderer) -> &mut wgpu_glyph::GlyphBrush<(String, Color)> {
    // First call: build SectionVec atlas from the bundled TTF asset.
    // Subsequent calls reuse it.
}
```

`wgpu_glyph 0.23` is already in the workspace at `Cargo.toml:69`
(per `wgpu_glyph = "0.23"` workspace declaration). The existing
glyph_brush 0.7 transitive dep stays.

`render_wgpu.rs` owns the atlas; `stats_overlay.rs` constructs the
`SectionVec` from the snapshot, hands it to `wgpu_glyph::GlyphBrush::queue`,
and `WgpuRenderer::render()` (at `apps/client-cli/src/render_wgpu.rs:378-440`)
encodes the glyph draw between the video blit and the swapchain present
pass. The hotkey path (`Ctrl+Alt+S`) flips a `render_wgpu::StatsVisible(bool)`
bit and continues to drive `WinitUserEvent::ToggleStats` (already declared
in `apps/client-cli/src/winit_user_event.rs`); the minifb fallback path
keeps the original CPU render.

### 3. P2-14 HDR pipeline

#### 3.1 Host capture — Windows (DXGI)

`crates/qubox-media/src/lib.rs:935-945` is the Windows
gdigrab-based capture stub. This ADR confirms the DXGI path already
exercised in `crates/qubox-display/src/win/`
(`IDXGIOutputDuplication`) and extends it to read
`DXGI_FORMAT_R16G16B16A16_FLOAT` (scRGB). The ffmpeg side keeps
gdi capture as the non-DXGI fallback (the `--capture-backend`
flag from §12 risk register).

Capture flow for HDR:

1. `IDXGIOutput6::QueryOutputCapabilities` discovers scRGB support;
   failing that, fall back to `B8G8R8A8_UNORM` and report the host as
   SDR via `ControlMsg::DisplayCapabilities { hdr_static_metadata: None }`.
2. Build a staging `ID3D11Texture2D` with
   `D3D11_TEXTURE2D_DESC { Format = DXGI_FORMAT_R16G16B16A16_FLOAT, ... }`
   and `Map` it to read back as `f16` pixels. The Map / Unmap is one
   copy; for zero-copy eventually hand the staging texture to ffmpeg's
   `ID3D11VA` device context (v2 follow-up).
3. Convert from linear scRGB `[0, 1+]` PQ-encoded to a YUV 4:2:0 10-bit
   frame via libswscale + a BT.2020 / ST 2084 (PQ) transfer encode.
4. Feed the YUV frame into ffmpeg's libx265 ("Main10") or libaom
   ("AV1 profile 0, bit-depth 10, monochrome 0") with the HDR10 SEI
   metadata per §3.3.

#### 3.2 Host capture — Linux

Today `crates/qubox-media/src/lib.rs:2110-2155` is the
`probe_pipewire_capture` entry and `:2157-2202` is the X11 path. The
Wayland path (`libspa` + `wlr-screencopy-unstable-v1` for compositor
negotiation) returns HDR-capable formats only when
`wl_output_color_space == COLOR_SPACE_BT2020_PQ`; `wlr-screencopy`
returns explicit `wl_output_format` chosen from the negotiated
compositor capabilities. The `vkms` test fixture in
`crates/qubox-display/src/` is extended with a virtual HDR
display path for CI validation.

Linux HDR capture also needs an explicit HDR `<->` SDR cue: hosts
without an HDR desktop produce YUV420P10 in BT.2020 HLG (which the
client can tone-map back to its own display). The `<>` are normalized
via PQ transfer for `bit_depth=10 && color_space=BT2100` and via
gamma-encoded HLG for `HLG` color primaries.

#### 3.3 Host capture — macOS (ScreenCaptureKit)

`crates/qubox-media/src/platform/macos.rs` does not yet
exist as a top-level module; it lives as a `#[cfg(target_os = "macos")]`
section inside `lib.rs`. This ADR promotes it to its own file
paralleling `linux.rs` and `windows.rs`, then implements it.

`SCStreamConfiguration { pixelFormat = kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange, colorPrimaries = kCVImageBufferColorPrimaries_ITU_R_2020, transferFunction = kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ, yCbCrMatrix = kCVImageBufferYCbCrMatrix_ITU_R_2020 }`
gets the HDR10 path on macOS 13+. macOS 12 falls back to
`kCVPixelFormatType_32BGRA` (sRGB).

`SCWindow` for app windows + `SCDisplay` for full-screen captures
map to the existing `DisplayInfo` type at
`crates/qubox-media/src/lib.rs:201-213`.

TCC `ScreenCapture` entitlement is required; on macOS the binary
must hold the entitlement and the user must approve the dialog at
first launch. Run-time detection: `SCShareableContent::get()`
returns an empty `displays` array → entitlement not granted →
emits a clear `tracing::error!` and exits the session with
`ErrorMessage::new("ScreenCaptureNotGranted", "Screen Recording permission required")`.

#### 3.4 Encoder configuration

**x265 Main10**:

```
x265-params: "hdr-opt=1:repeat-headers=1:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1)MAXCLL=1000,400:max-cll=1000,400:max-fall=400"
```

Driven from `encoder_args_for` at
`crates/qubox-media/src/lib.rs:1152-1269` via a new branch
on `VideoStreamPreferences::bit_depth == 10`. The 24-byte mastering
display blob from `HdrStaticMetadata::mastering_display_metadata`
maps directly to `master-display=...`.

**libaom-av1 10-bit**:

```
-aom-params: "bit-depth=10:profile=0:tier=0:enable-highbitdepth=1"
```

**svt-av1** with HDR10+ dynamic metadata (only when configured as the
host's `VideoEncoderKind::Av1` choice AND the host can deliver HDR10+):

```
--hdr10plus
```

Emitting static `master_display_metadata` when HDR10+ dynamic metadata
is unavailable; preserving `max_cll` / `max_fall` in the bitstream
SEI header either way.

#### 3.5 Transport + `EncodedVideoAccessUnit` extension

`EncodedVideoAccessUnit` lives at
`crates/qubox-media/src/lib.rs:444-463`. Append two fields:

```rust
/// Color space of this access unit. `#[serde(default)]` so older
/// clients default to BT.709.
#[serde(default)]
pub color_space: Option<ColorSpace>,
/// Bit depth (8 or 10). Defaults to 8.
#[serde(default = "default_eight")]
pub bit_depth: u8,
```

The wire form of the annexb / OBU bytes is unchanged; color metadata
travels as JSON over the control stream, separate from the encoded
byte payload, so a single signed bitstream never has its SEI metadata
lossy-copied.

#### 3.6 Client tone mapping (WGSL)

Two shaders ship, selectable via `--tone-map {hable,bt2390,srgb-passthrough}`
(the default is BT.2390). Both live as constants next to
`VIDEO_BLIT_WGSL` at `apps/client-cli/src/render_wgpu.rs:42-67`.

```wgsl
// Hable filmic curve (1 + tonemap; suitable for SDR display rendering HDR).
fn hable(lin: f32) -> f32 {
    let a = 0.15; let b = 0.50; let c = 0.10;
    let d = 0.20; let e = 0.02; let f = 0.30;
    return ((lin * (a * lin + c * b) + d * e) / (lin * (a * lin + b) + d * f)) - e / f;
}

// BT.2390 perceptual quantizer (PQ) inverse for SDR fallback.
fn bt2390_pq_inv(lin: f32) -> f32 {
    let m1 = 0.1593017578125; let m2 = 78.84375;
    let c1 = 0.8359375; let c2 = 18.8515625; let c3 = 18.6875;
    let y = max(lin, 0.0).powf(1.0 / m2);
    let n = (c1 + c2 * y) / (1.0 + c3 * y);
    return n.powf(1.0 / m1);
}

@fragment
fn fs_hdr(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let raw = textureSample(tex, samp, ...);
    let lin = raw.rgb;     // linear scRGB (RGBA16Float)
    let mapped = select(bt2390_pq_inv(hable(lin.r)),  // BT.2390 default
                        bt2390_pq_inv(lin.r),
                        false) * vec3(1.0);
    return vec4(mapped, raw.a);
}
```

`SelectToneMap` is implemented as a member on `WgpuRenderer`
that swaps the fragment shader module and re-creates the render
pipeline; pipeline re-creation is cheap (one `device.create_render_pipeline`).

Validation against test patterns lives in
`apps/host-agent/tests/hdr_e2e.rs::tone_map_invariant` (§11.2 below).

#### 3.7 Probe flow

1. Host enumerates displays + capture capabilities (post §2.2 HW
   device probe).
2. Host sends `ControlMsg::DisplayCapabilities` to client immediately
   after `SessionEstablished`.
3. Client compares its own display capabilities (read via
   `wgpu::SurfaceCapabilities` + EDID/Win32) against the host's
   advertisement; if both ends support HDR and the user enabled
   `--hdr`, the client requests `color_space=Bt2100`,
   `bit_depth=10` via `VideoStreamPreferences`.
4. Host picks encoder + bit depth based on §3.4; if no HDR encoder
   is available, host replies with the capability downgrade (still
   sending `DisplayCapabilities { hdr_static_metadata: None, ... }`
   so the client knows to drop to BT.709 + 8 bit).

### 4. P2-15 Pen / Tablet

#### 4.1 Wire format recap

`RemoteInputEvent::Pen` (§1.1) carries connection state and
high-level events over the **reliable** control stream.
`WirePenEvent` (§1.2) carries per-sample high-frequency events over
the QUIC datagram path with discriminator `0x50` ('P').

#### 4.2 QUIC datagram discriminator dispatch

The single shared `buf[2]` dispatch table at
`crates/qubox-transport/src/media/mod.rs:846-862` becomes:

```rust
match buf[2] {
    MIC_DATAGRAM_DISCRIMINATOR => decode_mic_datagram(&buf[..]),
    b'G' => decode_gamepad_datagram(&buf[..]),
    PEN_DATAGRAM_DISCRIMINATOR => decode_pen_datagram(&buf[..]),
    _ => Err(MediaDatagramError::UnknownDiscriminator { byte: buf[2] }),
}
```

`PEN_DATAGRAM_DISCRIMINATOR = 0x50` is declared in
`crates/qubox-proto/src/pen.rs` next to the
`MIC_DATAGRAM_DISCRIMINATOR` declaration at `lib.rs:720`.

#### 4.3 Client capture

**Windows** — `WM_POINTER` via `RegisterPointerInputTarget(HWND, true)`
in `apps/client-cli/src/win/pointer_capture.rs` (new). Decodes
`POINTER_INFO` + `POINTER_PEN_INFO`:

```rust
//! P2-15 Windows pen capture via WM_POINTER.
//! See ADR-010 §4.3.

pub fn spawn_pointer_capture(event_tx: UnboundedSender<RemoteInputEvent>) -> Result<JoinHandle<()>>;
```

Each pointer event is converted to `WirePenEvent` and pushed to a
`crossbeam_channel::bounded(64)` (240+ Hz coalescing → 1 kHz max). The
sender pumps into the QUIC datagram dispatch loop. The win32 features
used: `RegisterPointerInputTarget`, `GetPointerPenInfo`, input message
window (`HWND_MESSAGE`) for receiving `WM_POINTER*` without a visible
window.

**Linux** — `libinput` event device:

```rust
//! P2-15 Linux pen capture via libinput.

pub fn spawn_libinput_capture(event_tx: UnboundedSender<RemoteInputEvent>) -> Result<JoinHandle<()>>;
```

In `crates/qubox-pen/src/linux_capture.rs`. Subscribes to
`LIBINPUT_EVENT_TABLET_TOOL_*` (Pen, Eraser, Brush, Airbrush, Finger,
Mouse, Lens — the libinput tool taxonomy), `LIBINPUT_EVENT_TABLET_PAD_*`
(pad buttons). Each event becomes `WirePenEvent` and goes onto the
same `bounded(64)` channel. Requires `input` group membership; fails
with a clear error otherwise (see §13 risk 1).

**macOS** — deferred to v2. HID API requires TCC privacy permission
that is fragile in CLI tools; the alternative is the private
`IOHIDEvent` system, which Apple explicitly forbids in App Store apps
and is brittle for direct CLI use. Document the deferral in README.

#### 4.4 Host injection

**Linux** — `apps/host-agent/src/uinput_pen.rs` (new module).
Creates a virtual `uinput` device at session start with
`EV_KEY` (BTN_TOOL_PEN, BTN_TOOL_RUBBER, BTN_STYLUS, BTN_STYLUS2),
`EV_ABS` (ABS_X, ABS_Y, ABS_PRESSURE, ABS_TILT_X, ABS_TILT_Y,
ABS_DISTANCE, ABS_MISC / rotation). Teardown on session end releases
the device. Requires `uinput` group or root (see §13 risk 2); fail
with a clear error at session start.

**Windows** — `apps/host-agent/src/wintab_pen.rs` (stub for now).
Will use WinTab `WT_PACKET` for pen + `InjectTouchInput` as fallback
for touch-based devices (see §13 risk 3); a Phase 3 follow-up once
P2-15 lands. The stub declares the trait and a no-op impl so callers
compile.

**macOS** — deferred to v2 (HID injection requires TCC permission
mirroring capture).

#### 4.5 Hotplug + device list

At session start, the client enumerates tablet devices and emits
`RemoteInputEvent::Pen { tool: PenTool::Pen, ... }` plus a
`ControlMsg::PenDeviceList` (or repurpose `WirePenEvent::DeviceList`
over the data plane) so the host knows what to inject on which
device. Hotplug is supported: client re-emits on device arrival /
departure.

#### 4.6 Coalescing

240+ Hz source rate gets reduced to ≤ 1 kHz max via
`fn coalesce(event: WirePenEvent)` in
`crates/qubox-pen/src/coalesce.rs`. Drop-on-backpressure:
when the bounded channel is full, the coalescer drops the older
sample and logs at `tracing::warn!`. This matches the gamepad
path's loss-tolerant behavior.

### 5. P2-16 4K144 validation & presets

#### 5.1 Bitrate ladder + presets

| Preset | CBR | Encoder preset |
|--------|-----|----------------|
| 1080p60 | 12 Mbps | x265 `medium` / AV1 `speed=6` / svt `preset=10` |
| 1080p144 | 20 Mbps | x265 `medium` / AV1 `speed=6` / svt `preset=10` |
| 1080p240 | 30 Mbps | x265 `medium` / AV1 `speed=6` / svt `preset=10` |
| 1440p144 | 35 Mbps | x265 `medium` / AV1 `speed=6` / svt `preset=10` |
| 4K60 | 40 Mbps | x265 `medium` / AV1 `speed=6` / svt `preset=10` |
| 4K120 | 60 Mbps | x265 `medium` / AV1 `speed=6` / svt `preset=10` |
| 4K144 | 80 Mbps | x265 `slow` / AV1 `speed=4` / svt `preset=8` |

The mapping lives in `crates/qubox-media/src/presets.rs` as a
`pub fn bitrate_for_preset(p: PresetId, codec: VideoCodec) -> u32`
and a `pub fn encoder_args_for_preset(...)` mirroring the
existing `encoder_args_for` at
`crates/qubox-media/src/lib.rs:1152-1269`.

#### 5.2 GOP structure

- 240 frames at 60 fps → 4 s GOP.
- 480 frames at 120 fps → 4 s GOP.
- 576 frames at 144 fps → 4 s GOP.

`reference_frames = 4` for all presets; matches H.264 Level 5.1
(maxDpbSize) and avoids B-frame cascade stalls.

#### 5.3 Validation harness

`crates/qubox-media/tests/preset_validation.rs` — iterates all
seven presets × three codecs × three durations (10 s, 30 s, 60 s),
records encode latency, decode latency, frame count to
`research/perf/preset-results.json`.

#### 5.4 CI integration

`.github/workflows/ci.yml` gains a `nightly-perf` job triggered at
`cron: '0 6 * * *'` (06:00 UTC) on `ubuntu-latest + self-hosted GPU`
matrix. Results are uploaded as GitHub Actions artifacts and
trend-tracked via a single `trends.csv` per preset.

#### 5.5 Capability advertisement

The `DisplayInfo` type at
`crates/qubox-media/src/lib.rs:201-213` is extended (via
`crates/qubox-display/src/`) with `max_resolution_x: u32`,
`max_resolution_y: u32`, `max_refresh_hz: u32`. These populate
the `ControlMsg::DisplayCapabilities` (added in §1.3).

### 6. P2-17 macOS ScreenCaptureKit port

Today macOS is a `#[cfg(target_os = "macos")]` stub inside
`crates/qubox-media/src/lib.rs`. This ADR:

1. Promotes it to `crates/qubox-media/src/platform/macos.rs`
   (mirroring `linux.rs` and `windows.rs`).
2. Implements capture via `SCShareableContent` + `SCStream` with
   `SCStreamConfiguration` per §3.3.
3. Maps `SCWindow` and `SCDisplay` to the existing `DisplayInfo`
   type at `lib.rs:201-213`.
4. Surfaces TCC `ScreenCapture` requirement with a clear error
   message + recovery instructions (§3.3).
5. CI test on `macos-latest`: `cargo build -p qubox-host-agent
   --target aarch64-apple-darwin` plus a 5-second integration test
   that captures ≥ 30 frames.

`crates/qubox-media/src/platform/macos.rs` initial contents:

```rust
//! macOS host-side capture via ScreenCaptureKit (P2-17).
//! Requires TCC `ScreenCapture` entitlement.

pub fn enumerate_displays_sck() -> Result<Vec<DisplayInfo>, CaptureError>;
pub fn capture_stream_sck(...) -> Result<RunningMediaPipeline, MediaRuntimeError>;
```

### 7. P2-18 Windows DXGI port confirmation

`crates/qubox-media/src/platform/windows.rs` already exists
as a gdigrab-based stub. This ADR confirms that the DXGI Desktop
Duplication path through `crates/qubox-display/src/win/`
works on `x86_64-pc-windows-msvc` + Windows 10 21H2+ and is the
production path; `gdigrab` is the fallback.

HDR capture (scRGB) per §3.1 is exercised via
`IDXGIOutput6::QueryOutputCapabilities` + the staging
`ID3D11Texture2D` of `DXGI_FORMAT_R16G16B16A16_FLOAT`. ffmpeg
side consumes the staging texture read-back via `libavutil/hwcontext_d3d11va`.

CI test on `windows-latest`: `cargo build -p qubox-host-agent
--target x86_64-pc-windows-msvc` plus the same 5-second integration
test.

### 8. P2-19 Release packaging & signing

#### 8.1 Windows Authenticode

`signtool sign /sha1 <thumbprint> /tr http://timestamp.digicert.com /td sha256 /fd sha256 <binary>`

Certificate stored in `secrets.AZURE_KEY_VAULT_SIGNING_CERT`; CI step
uses `azure/signtool` action:

```yaml
- name: Sign Windows artifacts
  uses: azure/signtool@v2
  with:
    azure-key-vault-signing-cert: ${{ secrets.AZURE_KEY_VAULT_SIGNING_CERT }}
    azure-key-vault-signing-cert-secret: ${{ secrets.AZURE_KEY_VAULT_SIGNING_CERT_SECRET }}
    azure-key-vault-tenant-id: ${{ secrets.AZURE_TENANT_ID }}
    azure-key-vault-client-id: ${{ secrets.AZURE_CLIENT_ID }}
    azure-key-vault-client-secret: ${{ secrets.AZURE_CLIENT_SECRET }}
```

Applied to all `.exe` and `.msi` artifacts.

#### 8.2 macOS notarization

```bash
xcrun notarytool submit <pkg> \
    --keychain-profile notary-profile \
    --wait
xcrun stapler staple <pkg>
```

`Developer ID Application` certificate in `secrets.MACOS_DEVELOPER_ID_CERT`;
notary profile in `secrets.MACOS_NOTARY_KEYCHAIN_PROFILE`.
Hardened-runtime flags:

```bash
codesign --options runtime --deep --force \
    --sign "Developer ID Application: $ORG" \
    target/release/qubox-host-agent
```

Applied to all `.pkg` and `.app` bundles.

#### 8.3 Linux GPG

```bash
gpg --detach-sign --armor <binary>
sha256sum target/release/qubox-host-agent > SHA256SUMS
```

Both files published to `dist/linux-x86_64/SHA256SUMS.asc` next to the binary.

#### 8.4 SPDX SBOM

`cargo install cargo-cyclonedx --locked` runs at CI:

```bash
cargo cyclonedx --override-filename-suffix .cdx.json
```

Output published to `dist/sbom/<version>.spdx.json`; signed with
the same keypair as 8.3 (Linux GPG). `cargo-deny` is added to the
`lint-test` job at `.github/workflows/ci.yml:27-...` to fail on
unknown licenses.

#### 8.5 GitHub Actions release workflow

New `.github/workflows/release.yml` triggered on `v*` tags; uses
reusable workflow pattern for per-platform builds. Each platform
job:

1. Builds artifact.
2. Signs artifact (§8.1 / §8.2 / §8.3).
3. Generates SBOM (§8.4).
4. Uploads to GitHub Release with checksums + signatures.

### 9. Module structure (incremental)

Additions only; no existing module is renamed or moved.

**New crates**

- `crates/qubox-pen/` — parallels `qubox-clipboard`
  (`crates/qubox-clipboard/`) and `qubox-mic`
  (`crates/qubox-mic/`).
  - `Cargo.toml` — see §12.
  - `src/lib.rs` — public re-export surface.
  - `src/linux_capture.rs` — libinput capture + injection glue.
  - `src/windows_capture.rs` — WM_POINTER capture (§4.3).
  - `src/windows_inject.rs` — WinTab + `InjectTouchInput` injection.
  - `src/coalesce.rs` — 240+ Hz → 1 kHz coalescer.
  - `src/error.rs` — `PenCaptureError`, `PenInjectError`.
  - `tests/pen_e2e.rs` — round-trip wire + Linux-only uinput
    integration test (§11).
- `crates/qubox-media/src/platform/macos.rs` — ScreenCaptureKit
  (§6).
- `crates/qubox-media/src/platform/windows.rs` — DXGI
  confirmation (§7), with HDR staging texture read-back.
- `crates/qubox-media/src/presets.rs` — 4K144 bitrate ladder
  (§5.1).

**New modules in `apps/client-cli/src/`**

- `wgpu_glyph_overlay.rs` — replaces the software body of
  `stats_overlay.rs` while `stats_overlay.rs` keeps the existing
  `paint_overlay(buffer, w, h, snapshot)` API for the minifb
  fallback (§2.3).
- `winit_app.rs` — `AppState` trait + `run_video_app` (§2.1).
- `winit_user_event.rs` — already exists; new variants in §2.1.
- `win/pointer_capture.rs` (Windows-gated) — §4.3.

**New modules in `apps/host-agent/src/`**

- `uinput_pen.rs` — Linux pen injection (§4.4).
- `wintab_pen.rs` — Windows stub (§4.4).
- `hdr.rs` — `DisplayCapabilities` construction + HDR static
  metadata probe (§3.7).
- `platform/dxgi.rs` (new) — DXGI capture entry for §7.
- `platform/screencapturekit.rs` (new) — macOS capture entry
  for §6.

**`crates/qubox-display/src/`** — extended with HDR-aware color
space advertisement (`DisplayInfo::color_space`).

### 10. Threading model

- **winit app**: single main thread owns the
  `EventLoop<WinitUserEvent>`; each window is a `WindowHandle`;
  `WinitUserEvent` is the cross-thread command channel (FrameReady,
  CapabilitiesChanged, PenDeviceList, …). All windows (Main Video +
  Blank Overlay + Tiled View) live in one `ApplicationHandler`.
- **Pen capture threads**: dedicated `std::thread` per input
  device (Wayland poll, WM_POINTER pump). Events coalesced + pushed
  to `crossbeam_channel::bounded(64)` for the QUIC datagram sender.
- **HDR tone mapping**: pure GPU work in the fragment shader of
  `WgpuRenderer::render`; no CPU thread.
- **DXGI capture**: dedicated `std::thread` (real-time, must not
  be blocked by tokio); ADR-009 §5 explains why crossbeam_channel
  is used over tokio mpsc across this hop. Same reasoning applies
  to ScreenCaptureKit's CMSampleBuffer callbacks on the AVFoundation
  main thread.
- **Tone-map swap on the GPU**: no thread needed; pipeline
  re-create is synchronous and bounded to ~1 ms.
- **Pen injection**: runs inline in the `RemoteInputInjector::apply`
  loop already at `apps/host-agent/src/main.rs:462-502`. The Linux
  `uinput` device is created at session start and held in
  `RemoteInputInjector` for the session lifetime.

### 11. Test strategy

#### 11.1 Unit tests

- `crates/qubox-pen/src/coalesce.rs::tests::coalesces_240_to_1khz` —
  feeds 240 Hz synthetic source events, asserts the resulting
  stream never exceeds 1 000 events/s and preserves the last tip
  position.
- `crates/qubox-pen/src/lib.rs::tests::wire_pen_event_size` —
  asserts `WirePenEvent::SIZE == 36`.
- `crates/qubox-pen/src/lib.rs::tests::wire_pen_event_round_trip` —
  encodes + decodes a sample event; compares all fields.
- `crates/qubox-media/src/presets.rs::tests::bitrate_for_each_preset` —
  locks the bitrate matrix from §5.1 against regression.
- `crates/qubox-proto/src/pen.rs::tests::display_capabilities_backward_compat` —
  older `DisplayCapabilities` payload deserializes into a struct
  with `hdr_static_metadata: None, max_refresh_hz: 60, max_resolution: [1920, 1080]`.
- `crates/qubox-proto/src/pen.rs::tests::hdr_static_metadata_serde` —
  round-trips `HdrStaticMetadata` including the 24-byte mastering
  display blob.

#### 11.2 Integration tests

- `crates/qubox-pen/tests/pen_e2e.rs` — synthetic pen event
  through wire → host inject. Linux-only; gated on `cfg(target_os
  = "linux")` and `#[ignore]` unless `feature = "uinput-runtime"`.
- `apps/host-agent/tests/hdr_e2e.rs::hdr_passthrough_preserves_metadata` —
  encode a 10-bit YUV → decode on client → assert
  `bit_depth == 10 && color_space == Bt2100`. Gated on GPU
  detection; `#[ignore]` on runners without an adapter.
- `apps/host-agent/tests/hdr_e2e.rs::tone_map_invariant` — known
  PQ test pattern (e.g. ColorChecker) → tone-map → SDR sRGB
  reference; psnr ≥ 38 dB.
- `crates/qubox-media/tests/preset_validation.rs` — encodes
  every preset, logs perf to `research/perf/preset-results.json`.
- `apps/host-agent/tests/dxgi_capture_smk.rs` (Windows-only) —
  captures a 1080p B8G8R8A8 frame from a virtual DXGI output,
  asserts round-trip.
- `apps/host-agent/tests/screencapturekit_capture_smk.rs`
  (macOS-only) — captures a 1080p BGRA frame from a virtual
  ScreenCaptureKit source, asserts ≥ 30 frames in 5 s.

#### 11.3 Manual checklist

- [ ] HDR passthrough on a 4K HDR display (vs SDR fallback)
- [ ] Pen pressure curve linear (≤ 5 % deviation across 0.0 ..= 1.0)
- [ ] 4K144 stream sustained over 60 s without frame drops
- [ ] Signed Windows installer installs without SmartScreen warning
- [ ] Notarized macOS .pkg opens without Gatekeeper warning
- [ ] SBOM validates against SPDX 2.3 schema
- [ ] `wgpu_glyph` stats overlay renders identical text to the
      minifb CPU path (visual A/B)
- [ ] `Ctrl+Alt+S` still toggles the stats overlay (no regression)
- [ ] 10-bit YUV encoded + decoded RGB10 sample matches
- [ ] Pen tablet disconnect mid-session reconnects automatically

### 12. Dependency manifest

#### 12.1 Workspace `Cargo.toml` additions

```toml
[workspace.dependencies]
# existing wgpu_glyph & glyph_brush already at :69-70; lock to avoid drift:
wgpu_glyph = "0.23"
glyph_brush = "0.7"
# Pen / tablet + signing:
bitflags = "2"
constant_time_eq = "0.3"    # signature comparison
serde_repr = "0.1"

[target.'cfg(target_os = "linux")'.dependencies]
libinput = { version = "0.7", optional = true }
uinput = { workspace = true }
pipewire = { workspace = true }   # already

[target.'cfg(target_os = "windows")'.dependencies]
windows = { version = "0.58", features = ["Win32_UI_TabletPC", "Win32_UI_WindowsAndMessaging"] }
windows-sys = { version = "0.59", features = [
    "Win32_Graphics_Dxgi",
    "Win32_Graphics_Direct3D11",
    "Win32_UI_Input_Pointer",
] }

# macOS deps are framework-provided; nothing to add.
```

#### 12.2 `crates/qubox-pen/Cargo.toml`

```toml
[package]
name = "qubox-pen"
version.workspace = true
edition.workspace = true

[dependencies]
serde = { workspace = true }
tracing = { workspace = true }
crossbeam-channel = { workspace = true }

[target.'cfg(target_os = "linux")'.dependencies]
libinput = { workspace = true, optional = true }
uinput = { workspace = true, optional = true }

[target.'cfg(target_os = "windows")'.dependencies]
windows = { workspace = true }
windows-sys = { workspace = true }

[features]
default = ["libinput", "uinput"]
```

The `default = ["libinput", "uinput"]` matches the project rule #6
(no sudo) — both are library-only deps that resolve via `pkg-config`
on the host; `cargo` does not invoke `modprobe` or write to
`/dev/uinput` at build time.

#### 12.3 `apps/client-cli/Cargo.toml` additions

```toml
[target.'cfg(target_os = "windows")'.dependencies]
windows-sys = { workspace = true }

[dependencies]
bitflags = { workspace = true }
serde_repr = { workspace = true }
```

#### 12.4 `apps/host-agent/Cargo.toml` additions

```toml
[dependencies]
bitflags = { workspace = true }
serde_repr = { workspace = true }

[target.'cfg(target_os = "linux")'.dependencies]
uinput = { workspace = true }

[target.'cfg(target_os = "windows")'.dependencies]
windows = { workspace = true, features = ["Win32_UI_TabletPC"] }
```

#### 12.5 Lockfile policy

Pinned to the same major / minor versions already declared in the
workspace. `cargo` workspace inheritance keeps a single source of
truth; CI verifies with `cargo check --workspace --exclude client-gui`
before merge.

### 13. Risk register

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `libinput` requires seat access (group `input`) — not available in containers | High | Medium | Pen capture is `cfg`-gated; client-side reports "no tablet access" and skips silently. The Linux container CI path already runs without `input` and the existing CI matrix at `.github/workflows/ci.yml:60-69` exposes `Xephyr :99` instead, so the impact is on developer machines. |
| `uinput` needs root or `uinput` group | High | Medium | Fail at session start with a clear `tracing::error!` + `ErrorMessage::new("UinputPermissionDenied", "host-agent needs uinput group membership")`; document the requirement in `README.md`. |
| WinTab has poor Unicode support and no multitouch | Medium | Medium | Add `InjectTouchInput` as the fallback for touch-based devices; both supported per §4.4. |
| macOS ScreenCaptureKit requires TCC `ScreenCapture` entitlement | High | High | Detect at startup (§3.3); provide a clear error message + recovery instructions; CI gate on the entitlement being declared in `apps/client-gui/src-tauri/Info.plist` (and the host-agent's `Info.plist` equivalent for non-Tauri macOS builds). |
| HDR tone-mapping shader has subtle color shifts | Medium | High | Ship both Hable and BT.2390; expose `--tone-map` flag; default to BT.2390; validate against test patterns (§11.2 `tone_map_invariant`). |
| DXGI desktop duplication fails when DWM is reconfigured | Low | High | Fallback to GDI `gdigrab`; expose `--capture-backend {dxgi,gdi}` flag. |
| AV1 10-bit encoder throughput is low on non-server hardware | High | Medium | Allow fallback to H.265 Main10; document codec preference order in `--help`. |
| Code signing certificate leaks | Low | Critical | Use Azure Key Vault; never log certificate material; rotate on any incident. CI logs are scrubbed via `azure/signtool@v2`. |
| GitHub Actions runners lack GPU for HDR validation | High | Medium | Mark GPU-conditional tests as `#[ignore]`; nightly workflow on self-hosted GPU runners (mirroring `.github/workflows/ci.yml:124-134` for the `cross-build-windows` job pattern). |
| SPDX SBOM generation breaks on transitive deps with non-standard licenses | Medium | Medium | Pin versions; use `cargo-deny` in CI to fail on unknown licenses (§8.4). |
| Tauri `ApplicationHandler` migration breaks `client_cli::start_session` import | Medium | High | Keep both code paths; `start_session` keeps the `lib.rs:14` re-export untouched; new `start_session_v2` for the subprocess path (§2.1 / project rule #5). |
| 4K144 exceeds client GPU decode bandwidth on integrated graphics | Medium | High | Adaptive fallback: client reports GPU class (read from `wgpu::AdapterInfo`); host lowers preset if known-incompatible, via `ControlMsg::DisplayCapabilities::max_refresh_hz` (§1.3). |
| Pen 0x50 discriminator collision with future media datagram extension | Low | High | Reserve a 16-byte discriminator prefix space (0x50..0x5F) for stylus-family; document in `crates/qubox-proto/src/pen.rs`. |
| WinTab injection gives wrong coordinates in multi-monitor | Medium | High | Use `LogicalToAbsolute` mapping with `DisplayConfigGetDeviceInfo` + the actual monitor layout; mirror `scale_input_coordinate` at `apps/host-agent/src/main.rs:1710-1720`. |
| macOS notarization profile rotation breaks CI | Medium | Medium | Pin to a 5-year-secrets-backed notary profile; alarm before expiration. |

### 14. Open questions

- **Tauri `ApplicationHandler` migration: split or in-place?** Recommend
  in-place; share the existing `lib.rs:14` `start_session` re-export
  as the dispatcher entry and add `start_session_v2` for the new
  path. Document the divergence; mirror the `WGpuRenderer::new` /
  `render_minifb::new` split for the renderer choice.
- **HDR static metadata source: EDID from display, or OS report?**
  EDID is more accurate but harder (needs `Windows.Devices.Display`
  on Windows, `IOKit` on macOS, `wlr-output-management` on Wayland).
  OS report via `Windows.Devices.Display` / `NSScreen` is easier.
  Document the trade-off; default to OS report and offer `--hdr-edid`
  for the precise path.
- **WinTab vs `InjectTouchInput` for pen injection: which is canonical?**
  WinTab for pen fidelity, `InjectTouchInput` for touch fallback; both
  supported per §4.4. The pen-only path defaults to WinTab.
- **SBOM signing: TUF keys or separate code-signing keys?**
  TUF keys for the package; code-signing keys for the binary. This
  matches the existing `/ops/tuf/keys/` separation. Document both.
- **Linux `uinput` device persistence across sessions: create + destroy
  per session, or reuse?** Per-session create + destroy (cleaner
  teardown, no leaked devices if the host-agent crashes mid-session).
- **Pen wire format with `WirePenEvent`: include `button_state` as
  a `u32` flag bitmask or as a `u8`?** `u32` for forward expansion
  (analogous to `WireGamepadState` which carries `buttons: u16` at
  `crates/qubox-proto/src/lib.rs:15-33`); document in
  `crates/qubox-pen/src/lib.rs`.
- **HDR transport: ship static metadata inline with the access unit
  or via a separate control stream?** Currently static metadata travels
  via `ControlMsg::DisplayCapabilities` once at session start; per-frame
  `max_cll` / `max_fall` updates would require a per-stream update —
  defer to v2.

## Appendix A — file:line references consumed by this ADR

Substrate line numbers cited in this document (verified against `main`
at `883a6dd`):

- `apps/host-agent/src/capture_orchestrator.rs:1-813` — orchestrator
- `apps/host-agent/src/capture_orchestrator.rs:68-73` — `DisplayPipeline`
- `apps/host-agent/src/capture_orchestrator.rs:77-86` — `CaptureOrchestrator`
- `apps/host-agent/src/capture_orchestrator.rs:90-105` — `new()`
- `apps/host-agent/src/capture_orchestrator.rs:150-171` — `subscribe()`
- `apps/host-agent/src/capture_orchestrator.rs:480-499` — `wait_for_all()`
- `apps/host-agent/src/main.rs:60-163` — `struct Args`
- `apps/host-agent/src/main.rs:300-431` — `fn main()`
- `apps/host-agent/src/main.rs:434-460` — `RemoteInputInjector::new()`
- `apps/host-agent/src/main.rs:462-502` — `RemoteInputInjector::apply()`
- `apps/host-agent/src/main.rs:1710-1720` — `scale_input_coordinate()`
- `apps/client-cli/src/main.rs:1-2005` — full client CLI
- `apps/client-cli/src/lib.rs:1-19` — pub module declarations
- `apps/client-cli/src/lib.rs:14` — `start_session` re-export (project rule #5)
- `apps/client-cli/src/decoder_hw.rs:53-67` — `HwDeviceType` enum
- `apps/client-cli/src/decoder_hw.rs:69-90` — `HwDeviceType::preferred_order`
- `apps/client-cli/src/decoder_hw.rs:94-103` — `HwDecoderConfig`
- `apps/client-cli/src/decoder_hw.rs:130-192` — `RunningHwFrameDecoder`
- `apps/client-cli/src/decoder_hw.rs:374-381` — `decoded_channel`
- `apps/client-cli/src/render_wgpu.rs:42-67` — `VIDEO_BLIT_WGSL`
- `apps/client-cli/src/render_wgpu.rs:73-91` — `WgpuRenderer` struct
- `apps/client-cli/src/render_wgpu.rs:98-271` — `WgpuRenderer::new`
- `apps/client-cli/src/render_wgpu.rs:288-341` — `upload_frame`
- `apps/client-cli/src/render_wgpu.rs:378-440` — `render`
- `apps/client-cli/src/frame_pipeline.rs` — `DecodedFrame` carrier
- `apps/client-cli/src/stats_overlay.rs:20` — wgpu migration plan
- `apps/client-cli/src/blank_overlay.rs` — separate minifb window (pre-winit)
- `apps/client-cli/src/gamepad_capture.rs:1-154` — gamepad capture shape
- `apps/client-cli/src/runtime.rs` — `start_session` (project rule #5 surface)
- `crates/qubox-proto/src/lib.rs:15-54` — `WireGamepadState`
- `crates/qubox-proto/src/lib.rs:90-96` — `VideoCodec`
- `crates/qubox-proto/src/lib.rs:201-213` — `DisplayInfo`
- `crates/qubox-proto/src/lib.rs:223-243` — `VideoStreamPreferences`
- `crates/qubox-proto/src/lib.rs:287-372` — `ControlMsg`
- `crates/qubox-proto/src/lib.rs:443-445` — `default_true`
- `crates/qubox-proto/src/lib.rs:485-491` — `InputMouseButton`
- `crates/qubox-proto/src/lib.rs:493-534` — `RemoteInputEvent`
- `crates/qubox-proto/src/lib.rs:557-558` — `#[serde(default)]` template
- `crates/qubox-proto/src/lib.rs:716-732` — `MIC_DATAGRAM_DISCRIMINATOR`
  + discriminator coexistence with gamepad (`'G'` 0x47 + `'M'` 0x4D)
- `crates/qubox-proto/src/lib.rs:735-741` — `WireMicHeader`
- `crates/qubox-proto/src/lib.rs:1136-1180` — `wire_mic_header_*` tests
- `crates/qubox-transport/src/media/mod.rs:30` — `MEDIA_DATAGRAM_MAGIC`
- `crates/qubox-transport/src/media/mod.rs:38-89` — `MediaDatagramHeader`
- `crates/qubox-transport/src/media/mod.rs:68-89` — `from_bytes`
  dispatch entry
- `crates/qubox-transport/src/media/mod.rs:95` — `CHUNK_PAYLOAD_MAX`
- `crates/qubox-transport/src/media/mod.rs:846-862` — `encode_gamepad_datagram`
- `crates/qubox-transport/src/media/mod.rs:870-888` — `decode_gamepad_datagram`
- `crates/qubox-media/src/lib.rs:201-213` — `DisplayInfo`
- `crates/qubox-media/src/lib.rs:444-463` — `EncodedVideoAccessUnit`
- `crates/qubox-media/src/lib.rs:1152-1269` — `encoder_args_for`
- `crates/qubox-media/src/lib.rs:1721-1730` — H.265 candidates
- `crates/qubox-media/src/lib.rs:1732-1741` — AV1 candidates
- `crates/qubox-media/src/lib.rs:2110-2155` — `probe_pipewire_capture`
- `crates/qubox-media/src/lib.rs:2157-2202` — `probe_x11_capture`
- `crates/qubox-media/src/lib.rs:2204-2247` — `probe_windows_gdigrab_capture`
- `crates/qubox-display/src/` — display + DPMS + vkms
- `.github/workflows/ci.yml:18-30` — matrix
- `.github/workflows/ci.yml:27-...` — `lint-test`
- `.github/workflows/ci.yml:124-134` — `cross-build-windows` pattern
- `apps/daemon/dist/` — Linux systemd + Windows MSI + macOS pkgproj
- `dist/windows-x86_64/` — pre-built Windows .exe artifacts
- `apps/daemon/src/tuf.rs` — TUF auto-update
- `/ops/tuf/keys/` — TUF signing keys
- `Cargo.toml:45-71` — existing workspace deps
- `Cargo.toml:69-70` — `wgpu_glyph = "0.23"` and `glyph_brush = "0.7"`

## Appendix B — terminology

- **HW path** — `RunningHwFrameDecoder` with a non-empty `cfg.preferred`
  + `av_hwdevice_ctx_create` succeeded.
- **SW path** — `RunningHwFrameDecoder` with `cfg.preferred=[]`;
  `attach_sw_format` returns codec-native (typically
  `AV_PIX_FMT_YUV420P`); frames go through `libswscale`.
- **HDR path** — `bit_depth=10 && color_space=Bt2100 + PQ` bitstream
  on the wire; client runs §3.6 tone mapping shader.
- **SDR path** — `bit_depth=8 && color_space=Bt709 + sRGB`; no tone
  mapping.
- **`--tone-map`** — single CLI flag selecting Hable, BT.2390, or
  sRGB-passthrough shader for the wgpu fragment stage. Default BT.2390.
- **`--capture-backend`** — selects DXGI vs GDI on Windows or
  ScreenCaptureKit vs ffmpeg-gdigrab on macOS.
- **Pen wire (`WirePenEvent`)** — 36-byte packed datagram carrying
  per-sample pressure / tilt / coords on the QUIC datagram channel
  with discriminator `0x50`.
- **Pen control (`RemoteInputEvent::Pen`)** — the reliable variant
  carrying connection lifecycle events + low-frequency tool
  changes.
- **Tablet device list** — `ControlMsg`-equivalent advertised at
  session start enumerating the `PenTool`s available on the client.
