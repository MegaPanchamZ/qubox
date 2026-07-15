# ADR-007 Unified Display Capture and Virtualization Architecture

## Status

Proposed. Branch: `feature/adr-007-display-capture-privacy`. Based on `main` (Phase 1 is in PR #2, not yet landed). This branch does NOT merge `feature/daemon-and-turn-design` — the multi-display capture + privacy mode is independent of the daemon/TURN work and will be merged into `main` first. If `feature/daemon-and-turn-design` lands first, this branch rebases onto it (no conflicts expected — the daemon crate is new, and the host-agent changes in this ADR are additive: a new `capture_orchestrator.rs` module and a new `crates/qubox-display/` crate). Rebase strategy: `git rebase --onto main feature/daemon-and-turn-design feature/adr-007-display-capture-privacy` if needed.

## Context

Qubox currently captures a single display as a single ffmpeg subprocess (`x11grab` on Linux, `gdigrab` on Windows). The existing `apps/host-agent/src/main.rs::run_native_quic_session` (line 956) opens one ffmpeg pipeline, reads one stream of H.264 Annex B bytes, and sends them over one QUIC stream. The `WireAccessUnitHeader` (in `crates/qubox-transport/src/lib.rs:655`) has no `stream_id` or `display_id` field — it only carries `session_id`, `frame_id`, `timestamp_micros`, `keyframe`, `byte_len`.

Two research documents (P1-7 multi-monitor, P1-8 privacy mode) have been accepted and now require a unified architecture. The key insight: **multi-display capture and privacy-mode virtualization share the same core abstraction** — the system needs to enumerate, create, destroy, blank, and capture multiple display-like entities, each producing a separate video stream. Combining them into a single architectural pass avoids duplicated traits, conflicting coordinate systems, and a second API migration.

The existing `crates/qubox-proto/src/lib.rs` already has `VideoStreamPreferences::display_index: Option<u32>` and `CaptureRegion` — glimmers of multi-display awareness. The `ControlMsg::Nack` and `ControlMsg::KeyframeRequest` carry `stream_id: u16`. The wire format is ready for multi-stream before the capture pipeline is.

The gap analysis (`research/references/qubox-gap-analysis.md`) identifies "the media path is still a reliable ordered stream" as the core problem, but multi-monitor and privacy are the feature gaps that block the next release.

**Constraints:**
- bincode for wire format, serde + serde_json for app-level types, tracing for logs.
- No `unsafe` in trait impls (safe Rust wrappers for OS APIs only).
- The existing single-stream pipeline must remain functional (`--single-stream` flag).
- macOS ScreenCaptureKit requires macOS 12.3+; Windows DXGI 1.2 requires Windows 10+.
- The dev box is Linux (X11, headless Xephyr :99 in CI). All paths must compile on macOS/Windows even if they `bail!("not supported")` at runtime.
- Privacy mode on Windows: IddCx requires a signed kernel driver (substantial cost). First release uses the "dummy HDMI plug" model documented in P1-8.

## Decision

### 1. The `CaptureBackend` and `DisplayManager` Traits

Two related but distinct traits. The split is justified because:

- **`CaptureBackend`** is concerned with producing pixel data from displays. It is the data-plane abstraction — enumerate, open a capture session, pull frames.
- **`DisplayManager`** is concerned with the control-plane of display topology — create/destroy virtual displays, blank/privacy-state physical ones, move windows between them. It is the lifecycle abstraction.

A single backend (e.g. Linux X11 + RandR + vkms) will implement both traits on a given platform. But a future backend (e.g. a null-capture backend for testing) might only implement `CaptureBackend`. Separating them keeps the concerns decoupled.

Both traits live in a new crate `crates/qubox-display/` with the following module layout:

```
crates/qubox-display/
  Cargo.toml
  src/
    lib.rs            # detect_backend(), display_manager(), public types
    traits.rs         # CaptureBackend, CaptureSession, DisplayManager trait definitions
    types.rs          # DisplayId, DisplayInfo, ColorSpaceId, CaptureOptions, CapturedFrame, PixelFormat, error types
    x11.rs            # X11RandrBackend (cfg(target_os = "linux"))
    dxgi.rs           # DxgiBackend (cfg(target_os = "windows"))
    sc.rs             # ScreenCaptureKitBackend (cfg(target_os = "macos"))
    pipewire.rs       # PipeWirePortalBackend (cfg(target_os = "linux"))
    manager_x11.rs    # X11DisplayManager (cfg(target_os = "linux"))
    manager_dxgi.rs   # WindowsDisplayManager (cfg(target_os = "windows"))
    manager_sc.rs     # MacDisplayManager (cfg(target_os = "macos"))
    privacy_overlay.rs # BlankOverlayManager — fallback for privacy when vkms/IddCx not available
```

#### 1.1 `pub trait CaptureBackend: Send + Sync + 'static`

```rust
/// Data-plane abstraction for capturing pixel data from displays.
/// Implementations are per-platform (X11+RandR, DXGI, ScreenCaptureKit, PipeWire).
pub trait CaptureBackend: Send + Sync + 'static {
    /// Enumerate all displays visible to the OS at this moment.
    /// Returns the full list of physical + virtual displays.
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, CaptureError>;

    /// Open a capture session for the given `display`.
    /// Returns a boxed `CaptureSession` that produces frames.
    fn open_capture(
        &self,
        display: DisplayId,
        options: CaptureOptions,
    ) -> Result<Box<dyn CaptureSession>, CaptureError>;

    /// Report which color spaces, HDR metadata, scaling modes,
    /// and per-frame pixel formats this backend supports.
    fn list_capabilities(&self) -> BackendCapabilities;
}
```

#### 1.2 `pub trait CaptureSession: Send`

```rust
/// A single display's capture session. Produces frames until closed.
/// The implementor owns the platform capture handle (e.g. IDXGIOutputDuplication,
/// SCStream, x11rb get_image loop thread).
pub trait CaptureSession: Send {
    /// Block for up to `timeout` waiting for the next captured frame.
    /// Returns `Ok(Some(frame))` on new frame, `Ok(None)` on timeout or EOF,
    /// `Err(error)` on capture failure.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError>;

    /// The actual region being captured in the display's local coordinate space.
    /// This may differ from the display's full size if a sub-region was requested.
    fn capture_region(&self) -> Rect;

    /// The display ID this session was opened for.
    fn display_id(&self) -> DisplayId;

    /// The color space of the captured frames (e.g. sRGB, scRGB, Display P3, HDR10).
    fn color_space(&self) -> ColorSpaceId;

    /// The display's current refresh rate in Hz (e.g. 60.0, 144.0, 240.0).
    fn refresh_hz(&self) -> f32;

    /// Close the capture session. The implementor should release any
    /// OS handles (e.g. ReleaseFrame for DXGI, stopCapture for SCK).
    fn close(&mut self) -> Result<(), CaptureError>;
}
```

#### 1.3 `pub trait DisplayManager: Send + Sync + 'static`

```rust
/// Control-plane abstraction for display topology management:
/// virtual display creation/destruction, privacy blanking, window movement.
pub trait DisplayManager: Send + Sync + 'static {
    /// Enumerate all displays with full metadata (EDID-friendly name, color space,
    /// HDR capability, current mode timing). Mirror of CaptureBackend::enumerate_displays
    /// but richer — for management UIs rather than capture.
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, DisplayError>;

    /// Set a display's state: Active (normal operation), Privacy (blanked, game
    /// moved away), or Blanked (DPMS off / disconnected).
    fn set_display_state(
        &self,
        display: DisplayId,
        state: DisplayState,
    ) -> Result<(), DisplayError>;

    /// Move a window to a different display. Used by Privacy Mode to shift the
    /// game window from the physical display to the virtual display.
    /// `window` is an opaque handle: on X11 it is a Window ID (u32), on Windows
    /// an HWND (isize), on macOS an NSWindow*.
    fn move_window_to_display(
        &self,
        window: WindowHandle,
        target: DisplayId,
    ) -> Result<(), DisplayError>;

    /// Create a virtual display (vkms on Linux, IddCx on Windows,
    /// CGVirtualDisplay on macOS). Returns the DisplayId of the new display.
    fn create_virtual_display(
        &self,
        config: VirtualDisplayConfig,
    ) -> Result<DisplayId, DisplayError>;

    /// Destroy a virtual display. Fails with NotSupported on platforms where
    /// virtual displays cannot be programmatically destroyed (e.g. the dummy
    /// HDMI plug on Windows — the user must unplug it physically).
    fn destroy_virtual_display(
        &self,
        display: DisplayId,
    ) -> Result<(), DisplayError>;
}
```

#### 1.4 Type Definitions

All in `crates/qubox-display/src/types.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DisplayId(pub u32);

impl DisplayId {
    /// The primary monitor, as reported by the OS. On X11 this is the output
    /// marked "primary" by xrandr; on Windows the monitor with
    /// MONITORINFOF_PRIMARY; on macOS the display with
    /// CGDisplayIsMain() == true.
    pub fn primary() -> Self { Self(0) /* resolved at runtime */ }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: DisplayId,
    /// Friendly name (e.g. "DP-1", "\\.\DISPLAY1", "DELL U2723QE" from EDID).
    pub name: String,
    /// Position of this display's top-left corner in the OS virtual desktop
    /// coordinate space. Negative values are valid (display left of primary).
    pub position: Point<i32>,
    /// Native resolution in physical pixels.
    pub size: Size<u32>,
    /// Current refresh rate in Hz.
    pub refresh_hz: f32,
    /// HiDPI / Retina scale factor (1.0 = standard DPI, 2.0 = Retina).
    pub scale_factor: f32,
    /// The native color space of the display.
    pub color_space: ColorSpaceId,
    /// Whether the display and its driver support HDR output.
    pub hdr_capable: bool,
    /// Whether this display is a virtual (software-created) display
    /// vs a physical monitor.
    pub is_virtual: bool,
}

/// Canonical color space identifiers. The H.273 / QuickTime codec string is
/// included for mapping to video codec colour_primaries / transfer_characteristics.
///
/// H.273 mapping (colour_primaries / transfer_characteristics / matrix_coefficients):
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorSpaceId {
    /// sRGB / BT.709. Default.
    /// H.273: 1 / 1 / 1  (BT.709 primaries, BT.709 transfer, BT.709 matrix)
    /// QT: "SRGB"
    Srgb,
    /// scRGB (linear extended sRGB, Windows HDR path).
    /// H.273: 1 / 13 / 1  (BT.709 primaries, sRGB linear transfer, BT.709 matrix)
    /// QT: "SCRGB"
    Scrgb,
    /// Display P3 (Apple HDR path, macOS).
    /// H.273: 12 / 1 / 1  (EG 432-1 primaries, BT.709 transfer, BT.709 matrix)
    /// QT: "P3_D65"
    DisplayP3,
    /// HDR10 (BT.2020 ST.2084 SMPTE 2084).
    /// H.273: 9 / 16 / 9  (BT.2020 primaries, ST.2084 PQ transfer, BT.2020 constant luminance)
    /// QT: "BT2020_HLG" or "BT2020_PQ"
    Hdr10,
}

impl ColorSpaceId {
    pub fn h273_primaries(self) -> u8 {
        match self { Srgb | Scrgb => 1, DisplayP3 => 12, Hdr10 => 9 }
    }
    pub fn h273_transfer(self) -> u8 {
        match self { Srgb => 1, Scrgb => 13, DisplayP3 => 1, Hdr10 => 16 }
    }
    pub fn h273_matrix(self) -> u8 {
        match self { Srgb | Scrgb => 1, DisplayP3 => 1, Hdr10 => 9 }
    }
    /// QuickTime / Apple color space string for CMSampleBuffer creation.
    pub fn qt_string(self) -> &'static str {
        match self { Srgb => "SRGB", Scrgb => "SCRGB", DisplayP3 => "P3_D65", Hdr10 => "BT2020_PQ" }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureOptions {
    /// Optional sub-region of the display to capture. None = full display.
    pub region: Option<Rect>,
    /// Desired color space. None = use the display's native color space.
    pub color_space: Option<ColorSpaceId>,
    /// Target capture frame rate. The backend will attempt to produce frames
    /// at this rate but may deliver fewer (e.g. static desktop = no new frames).
    pub target_fps: u32,
    /// Whether to include the OS cursor in the captured frame.
    pub capture_cursor: bool,
}

#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub display_id: DisplayId,
    pub width: u32,
    pub height: u32,
    /// The pixel data. Always owned as an Arc<Vec<u8>> so it can be shared
    /// between the capture thread and the encoder pipeline without copying.
    pub bytes: Arc<Vec<u8>>,
    pub format: PixelFormat,
    pub captured_at: Instant,
    pub frame_index: u64,
}

/// Supported pixel formats from capture backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    /// 8-bit BGRA (byte[0]=B, byte[1]=G, byte[2]=R, byte[3]=A). Default for
    /// x11rb get_image, DXGI DuplicateOutput, ScreenCaptureKit default.
    Bgra8,
    /// 8-bit YUV 4:2:0 semi-planar. Used by some hardware capture paths and
    /// preferred by encoders (NV12 is the standard encoder input).
    Nv12,
    /// 16-bit-per-channel RGBA floating point (scRGB / HDR path).
    Rgba16F,
}

/// Capabilities reported by a CaptureBackend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendCapabilities {
    pub supported_color_spaces: Vec<ColorSpaceId>,
    pub supports_hdr_metadata: bool,
    pub supported_formats: Vec<PixelFormat>,
    pub max_capture_resolution: (u32, u32),
    pub max_fps: u32,
    pub supports_region_capture: bool,
    pub notes: Vec<String>,
}

/// Error type for CaptureBackend and CaptureSession.
#[derive(Debug, Clone, thiserror::Error)]
pub enum CaptureError {
    #[error("display {0} not found")]
    DisplayNotFound(DisplayId),
    #[error("capture not supported on this platform: {0}")]
    NotSupported(String),
    #[error("OS error: {0}")]
    Os(String),
    #[error("frame acquisition timed out")]
    Timeout,
    #[error("the display was disconnected")]
    DisplayDisconnected(DisplayId),
    #[error("backend-specific error: {0}")]
    Backend(String),
}

/// Error type for DisplayManager.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DisplayError {
    #[error("display {0} not found")]
    DisplayNotFound(DisplayId),
    #[error("operation not supported on this platform: {0}")]
    NotSupported(String),
    #[error("OS error: {0}")]
    Os(String),
    #[error("virtual display creation failed: {0}")]
    VirtualDisplayFailed(String),
    #[error("window {0:?} not found or not movable")]
    WindowNotFound(WindowHandle),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayState {
    /// Normal operation: the display shows whatever the desktop environment renders.
    Active,
    /// Privacy mode: the display is blanked (DPMS off or overlay) and the game
    /// has been moved to a virtual display.
    Privacy,
    /// The display is off — DPMS power-off, disconnected, or GPU-reset.
    /// This is NOT a normal privacy state; it is the result of a physical
    /// disconnection or driver reset that the DisplayManager observes.
    Blanked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualDisplayConfig {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f32,
    pub name: String,
    pub color_space: ColorSpaceId,
}

/// Opaque window handle for move_window_to_display.
/// Platform-specific: X11 Window (u32), Windows HWND (isize), macOS NSWindow*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WindowHandle {
    X11(u32),
    Windows(isize),
    Mac(*const std::ffi::c_void), // NSWindow* — never dereferenced, just passed to OS.
}
```

#### 1.5 Per-OS Implementations

| Backend | Platform | Enumeration | Capture | Virtual Display | Window Move |
|---------|----------|-------------|---------|-----------------|-------------|
| `X11RandrBackend` | Linux X11 | `x11rb::randr::get_screen_resources` + per-output | `xproto::get_image(ZPixmap)` per region | `drm` crate + `modprobe vkms` | `xrandr --output` or `_NET_WM_MOVERESIZE` |
| `PipeWirePortalBackend` | Linux Wayland | `pw_core_enum_params` for nodes | PipeWire stream via `pipewire` crate | `wlr-output-management` protocol if available; else `NotSupported` | wlr-foreign-toplevel-management or xdg-shell |
| `DxgiBackend` | Windows | `IDXGIFactory1::EnumAdapters1` + `EnumOutputs` | `DuplicateOutput` per monitor | `Err(NotSupported("use a dummy HDMI plug"))` for v1 | `SetWindowPos` / `MoveWindow` |
| `ScreenCaptureKitBackend` | macOS 12.3+ | `SCShareableContent::current().await`.displays | `SCStream` per display | `Err(NotSupported("CGVirtualDisplay deferred; see ADR-007"))` | `NSWindow::setFrame` via `objc2` |

**`X11RandrBackend` details** (the only backend the dev box can test):
- Depends on `x11rb` 0.13+ and the `drm` crate.
- `enumerate_displays`: calls `randr::get_screen_resources` on the root window, iterates outputs, calls `randr::get_output_info` + `randr::get_crtc_info`. Maps `RROutput` to `DisplayId`. Parses the crtc's mode dot_clock/h_total/v_total for refresh_hz.
- `open_capture`: starts a dedicated thread (`bp-capture-X`) that loops calling `xproto::get_image(format=ZPixmap, drawable=root, x=display.position.x, y=display.position.y, width, height, plane_mask=0xFFFFFFFF)`. Frames are sent over a `tokio::sync::mpsc::channel` to the `CaptureSession` wrapper.
- `move_window_to_display`: uses `_NET_WM_MOVERESIZE` client message on the target window. If the window's pid is known, focuses it first.
- `create_virtual_display`:
  1. `Command::new("modprobe").args(["vkms"]).output()`.
  2. Wait up to 1s for `/dev/dri/cardN` for vkms to appear.
  3. Use the `drm` crate to create a framebuffer and a CRTC with the requested mode.
  4. Add the display via xrandr: `Command::new("xrandr").args(["--addmode", "VKMS-1", mode_str])`.
  5. Return the new `DisplayId`.
- `destroy_virtual_display`:
  1. Remove the mode: `xrandr --delmode VKMS-1 <mode>`.
  2. Unload vkms: `modprobe -r vkms`.
  - If the display has active captures, the `CaptureSession::next_frame` returns `Err(DisplayDisconnected)`.
- `set_display_state(Privacy)`:
  1. Create virtual display if not exists.
  2. Find the game window (the window with focus, or the window with the matching PID).
  3. Move it to the virtual display via `move_window_to_display`.
  4. DPMS off: `Command::new("xset").args(["dpms", "force", "off"])`.
- `set_display_state(Active)`:
  1. DPMS on: `xset dpms force on`.
  2. Move window back to primary display.
  3. Optionally remove the virtual display.

The Linux vkms path requires `modprobe vkms` to succeed. If `modprobe` exits with an error (module not found, Secure Boot blocks unsigned modules), the `create_virtual_display` returns an error. In that case, the caller falls back to `BlankOverlayManager` below.

**`BlankOverlayManager`** — pure-software fallback for privacy mode:
- Implements `DisplayManager` by creating a fullscreen black window on the target display using `winit` + `wgpu` (or `softbuffer` for simplicity).
- The overlay window is borderless, always-on-top, captures all input events, and cannot be dismissed by the user (it paints on every frame).
- The game window is NOT moved — it continues to render to the same display. The overlay just covers it.
- This is the "casual privacy" mode Parsec uses. Documented as bypassable (Win+Tab, screenshot).

**`DxgiBackend` details** (Windows, compile-only on dev box):
- Depends on `windows` 0.58+ crate with `Win32_Graphics_Dxgi`, `Win32_Graphics_Direct3D11`, `Win32_Foundation`, `Win32_UI_Shell`.
- `enumerate_displays`: enumerate adapters via `CreateDXGIFactory1` → `EnumAdapters1` → per-adapter `EnumOutputs`. For each output, call `GetDesc` for resolution and name. For HDR detection, query `IDXGIOutput6::GetDesc1` when available (DXGI 1.2+).
- `open_capture`: create a `ID3D11Device`, `ID3D11DeviceContext`, then `IDXGIOutput1::DuplicateOutput`. Loop calling `AcquireNextFrame`, read the `IDXGIResource` texture via `Map`/`Unmap` (BGRA), then `ReleaseFrame`. Cursor is composited via `DuplicateOutput`'s metadata.
- Virtual display via IddCx: **deferred.** For v1, `create_virtual_display` returns `Err(NotSupported("Use a $10 dummy HDMI plug. See docs/dummy-hdmi.md"))`.
- `set_display_state(Privacy)`: Not supported (the dummy HDMI plug is a physical monitor; the user moves the game window manually or via the OS display settings panel). Returns `Err(NotSupported)` for now.

**`ScreenCaptureKitBackend` details** (macOS, compile-only on dev box):
- Depends on `screencapturekit-rs` and `objc2`.
- `enumerate_displays`: `SCShareableContent::current().await` → `.displays` → map `SCDisplay` to `DisplayInfo`.
- `open_capture`: create `SCStreamConfiguration` with the display's width/height, pixel format `kCVPixelFormatType_32BGRA`, color space from the display's native color space. Create `SCStream` with a filter for the display. Start capture. The delegate callback receives `CMSampleBuffer` → `CVPixelBuffer` → BGRA bytes.
- Virtual display via CGVirtualDisplay: **deferred** (entitlement-gated; requires Apple cooperation). `create_virtual_display` returns `Err(NotSupported("CGVirtualDisplay requires Apple entitlements; see ADR-007 §4"))`.

**`PipeWirePortalBackend` details** (Linux Wayland, compile-only on dev box):
- Depends on `pipewire` 0.8+ crate.
- `enumerate_displays`: connect to PipeWire, iterate nodes for video sources.
- `open_capture`: create a PipeWire stream from a node, negotiate `SpaVideoFormat` (BGRA or NV12), enter streaming loop.
- Virtual display: uses the `wlr-output-management` protocol (available on Sway, Hyprland, Wayfire). If not available, returns `NotSupported`.

#### 1.6 Selection Logic

In `crates/qubox-display/src/lib.rs`:

```rust
/// Detect the best capture backend for the current platform and runtime session.
/// The detection is stateless — call this at session start.
pub fn detect_backend() -> Result<Box<dyn CaptureBackend>, CaptureError> {
    #[cfg(all(target_os = "linux", feature = "x11"))]
    if let Ok(conn) = x11rb::connect(None) {
        if has_randr(&conn) {
            return Ok(Box::new(X11RandrBackend::new(conn)));
        }
    }
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    if env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland") && pipewire_portal_available() {
        return Ok(Box::new(PipeWirePortalBackend::new()));
    }
    #[cfg(target_os = "windows")]
    return Ok(Box::new(DxgiBackend::new()));

    #[cfg(target_os = "macos")]
    if macos_version() >= (12, 3) {
        return Ok(Box::new(ScreenCaptureKitBackend::new()));
    }

    Err(CaptureError::NotSupported(
        "no supported capture backend for this platform / session type".into()
    ))
}

pub fn display_manager() -> Result<Box<dyn DisplayManager>, DisplayError> {
    // Same dispatch as detect_backend() but returns DisplayManager implementors.
    // On Linux, also checks for vkms availability (tries `modinfo vkms`).
    // If vkms is unavailable, returns a BlankOverlayManager instead of failing.
}
```

### 2. Display Coordinates and Scaling

The existing `scale_input_coordinate` (host-agent main.rs:659) is a single-monitor affine mapping:
`target = (client_value * (host_display_extent - 1)) / (stream_extent - 1)`. This breaks with multi-monitor because:
1. Client coordinates are in a single virtual desktop space, not per-display.
2. The cursor may cross display boundaries mid-session.
3. HiDPI scaling differs per display.

#### 2.1 Coordinate Spaces

Four coordinate types defined in `crates/qubox-display/src/coords.rs`:

```rust
/// Physical pixel on a specific display. This is the raw framebuffer address.
/// On a 3840×2160 display (no scaling), a PhysicalPixel(0,0) is the top-left.
/// On a Retina display (scale=2.0), PhysicalPixel(0,0) is still the raw pixel.
pub struct PhysicalPixel(pub i32, pub i32);

/// Logical (DPI-scaled) pixel. On a Retina display at scale=2.0,
/// LogicalPixel(0,0) maps to PhysicalPixel(0,0) but LogicalPixel(1,0)
/// maps to PhysicalPixel(2,0). The client thinks in logical pixels.
pub struct LogicalPixel(pub f32, pub f32);

/// A point in the OS's virtual desktop coordinate space. This is the union of
/// all display rectangles placed by the user in the OS display settings.
/// E.g. in a 2-display horizontal layout: display 0 at (0,0,1920,1080),
/// display 1 at (1920,0,2560,1440). VirtualDesktopPoint(2400, 500) is on display 1.
pub struct VirtualDesktopPoint(pub i32, pub i32);

/// A rectangle in a display's local coordinate space. Used by CaptureOptions::region.
pub struct CaptureRegion(pub i32, pub i32, pub u32, pub u32 /* x, y, w, h */);
```

Conversion functions:

```
PhysicalPixel::from_logical(lp: LogicalPixel, scale: f32) -> PhysicalPixel
  PhysicalPixel(round(lp.0 * scale), round(lp.1 * scale))

LogicalPixel::from_physical(pp: PhysicalPixel, scale: f32) -> LogicalPixel
  LogicalPixel(pp.0 / scale, pp.1 / scale)

VirtualDesktopPoint::to_display_local(
  vdp: VirtualDesktopPoint, displays: &[DisplayInfo]
) -> (DisplayId, PhysicalPixel)
  For each display:
    if vdp.x in [d.position.x, d.position.x + d.size.width)
    && vdp.y in [d.position.y, d.position.y + d.size.height):
      return (d.id, PhysicalPixel(vdp.x - d.position.x, vdp.y - d.position.y))
  Return error "point not on any display"

VirtualDesktopPoint::from_display_local(
  display: &DisplayInfo, local: PhysicalPixel
) -> VirtualDesktopPoint
  VirtualDesktopPoint(local.0 + display.position.x, local.1 + display.position.y)
```

#### 2.2 Wire Protocol Changes

**`WireAccessUnitHeader`** (currently in `crates/qubox-transport/src/lib.rs:655`):

```
Old:
  session_id: Uuid
  frame_id: u64
  timestamp_micros: u64
  keyframe: bool
  byte_len: usize

New:
  session_id: Uuid
  stream_id: u16            // NEW: 0 = legacy single-stream, 1+ = display-specific
  display_id: u32           // NEW: the DisplayId of the captured display
  frame_id: u64
  timestamp_micros: u64
  keyframe: bool
  width: u32                // NEW: the captured frame's width (display native, before scaling)
  height: u32               // NEW: the captured frame's height
  refresh_hz: f32           // NEW: the display's native refresh rate
  color_space_id: u8        // NEW: ColorSpaceId as u8 (0=Srgb, 1=Scrgb, 2=DisplayP3, 3=Hdr10)
  hdr_static_metadata: Option<HdrStaticMetadata>  // NEW: for P2-14
  byte_len: usize

Backward compatibility:
  stream_id = 0 for single-stream sessions.
  display_id = 0 for single-stream sessions.
  Existing parsers that ignore unknown JSON fields (serde_json default behavior)
  will skip the new fields if we use #[serde(default)].
```

**`RemoteInputEvent::MouseMove`** and **`RemoteInputEvent::Touch`**:

Client sends coordinates in `VirtualDesktopPoint` space:

```
RemoteInputEvent::MouseMove {
    x: u32,         // VirtualDesktopPoint.x (cast to u32, 0..=max coordinate)
    y: u32,         // VirtualDesktopPoint.y
}
RemoteInputEvent::Touch {
    x: u32,         // VirtualDesktopPoint.x
    y: u32,
    finger_id: u8,
    action: TouchAction,
}
```

New event:

```
RemoteInputEvent::HoverDisplay {
    display_id: u32,
}
```

- The host emits `HoverDisplay` when `display_at_point()` changes (the cursor crossed a display boundary). Implementation: the host's input thread maintains `Option<DisplayId> current_display` and emits the event on transition.
- The client uses `HoverDisplay` to highlight the active display in a tiled UI and to route raw mouse events (e.g. relative mouse mode in an FPS).

#### 2.3 HiDPI / Scaling

Each `DisplayInfo.capture_scale_factor: f32` communicates the host DPI to the client.

**The client renders at the host's physical resolution by default.** This gives pixel-perfect rendering. For low-bandwidth clients, the host offers a "logical resolution" mode:
- `CaptureOptions::target_fps` still captures at the full physical resolution (to preserve detail for the encoder).
- The `VideoPipelineConfig::width` and `height` in the signaling are set to the logical resolution (e.g. 1920×1080 on a 3840×2160 Retina display at scale=2.0).
- The ffmpeg pipeline scales down (via the `scale` filter) before encoding.
- On the input side, the host scales the client's coordinates up: `VirtualDesktopPoint → display_local → PhysicalPixel * scale_factor`.

#### 2.4 Input Mapping Algorithm (Multi-Monitor)

```
Host receives: RemoteInputEvent::MouseMove { x, y }

1. Convert to VirtualDesktopPoint: vdp = (x as i32, y as i32)

2. Find the display containing vdp:
   (display_id, local_pp) = VirtualDesktopPoint::to_display_local(vdp, displays)
   If no display found: clamp to nearest display edge

3. Apply HiDPI scaling:
   physical = PhysicalPixel::from_logical(
       LogicalPixel(local_pp.0 as f32, local_pp.1 as f32),
       display.scale_factor
   )

4. Inject via enigo:
   enigo.move_mouse(physical.0, physical.1, Coordinate::Abs)
```

For relative mouse (FPS games):

```
Host receives: RemoteInputEvent::RelativeMouseMove { dx, dy }
  → enigo.move_mouse(dx, dy, Coordinate::Rel) — no multi-monitor mapping needed
```

### 3. Privacy State Transitions

#### 3.1 State Machine

Per-display state machine:

```
                          ┌─────────────────────────────┐
                          │         Active              │
                          │ (default; display shows the │
                          │  user's desktop normally)   │
                          └─────────────────────────────┘
                                     │           ▲
                           user      │           │  user
                           enables   │           │  disables
                           privacy   ▼           │  privacy
                          ┌─────────────────────────────┐
                          │         Privacy             │
                          │ - physical display is       │
                          │   blanked (DPMS off /       │
                          │   overlay on)               │
                          │ - game window moved to      │
                          │   virtual display           │
                          │ - capture session continues │
                          │   on the virtual display    │
                          └─────────────────────────────┘
                                     │           ▲
                           display   │           │  display
                           unplugged │           │  re-plugged
                           / GPU     ▼           │  (handled by OS)
                           reset               │
                          ┌─────────────────────────────┐
                          │         Blanked             │
                          │ - display is off (DPMS off, │
                          │   disconnected, GPU reset)  │
                          │ - NOT a privacy action —    │
                          │   this is a physical/topo-  │
                          │   logical change observed   │
                          │   by DisplayManager         │
                          └─────────────────────────────┘
```

**Transitions:**

| From | To | Trigger | Side Effect |
|------|----|---------|-------------|
| Active | Privacy | `set_display_state(Privacy)` called | Create virtual display (if not exists), move game window, blank physical display. |
| Active | Blanked | Display disconnected / GPU reset | The OS handles the display removal. `enumerate_displays` no longer returns this display. Capture sessions on this display get `Err(DisplayDisconnected)`. |
| Privacy | Active | `set_display_state(Active)` called | Unblank physical display, move game window back (or keep on virtual), optionally destroy virtual display. |
| Privacy | Blanked | Virtual display disconnected / GPU reset | Fall back to blank overlay. Capture session closes with `DisplayDisconnected`. The game window is on a now-gone virtual display; the OS moves it to the primary display. |
| Blanked | Active | Display re-plugged | OS re-enumerates the display. `enumerate_displays` re-discovers it. The user may need to re-create the session if the capture was dropped. |
| Blanked | Privacy | Display re-plugged + `set_display_state(Privacy)` | Same as Active → Privacy. |

#### 3.2 Per-OS `set_display_state(Privacy)` Implementation

**Linux (X11 with vkms):**
1. `create_virtual_display()` → `DisplayId` for the new VKMS output.
2. Find the foreground game window: read `_NET_ACTIVE_WINDOW` from the root window via `x11rb`. If no window is active, probe for the window with `_NET_WM_PID` matching the user-specified game PID (or the PID of the foreground process).
3. `move_window_to_display(game_window, virtual_display)`:
   - Set `_NET_WM_FULLSCREEN_MONITORS` to the VKMS output's `RROutput`.
   - Send `_NET_WM_STATE` with `_NET_WM_STATE_FULLSCREEN`.
   - Set `_NET_WM_DESKTOP` to the virtual desktop's viewport.
   - If the window doesn't respond, fall back to `xrandr --output VKMS-1 --primary` (makes VKMS the primary display; the OS automatically moves the active window).
4. Blank the physical display: `Command::new("xset").args(["dpms", "force", "off"])`.

**Failure mode**: If `modprobe vkms` fails (Secure Boot, missing module):
- Log `tracing::warn!("privacy mode: vkms unavailable, falling back to blank overlay")`.
- Instead of creating a virtual display, spawn `BlankOverlayManager` — a fullscreen black `winit` window on the physical display.
- The game window stays on the physical display, but the user sees only black.
- The user is warned via the event channel: `IpcEvent::PrivacyDegraded { reason: "vkms unavailable" }`.

**Windows (v1):**
- Not supported at the `DisplayManager` trait level. `set_display_state(Privacy)` returns `Err(DisplayError::NotSupported("Privacy mode on Windows requires a dummy HDMI plug or IddCx driver; see docs/privacy-windows.md"))`.
- The user installs a dummy HDMI plug, then manually opens Windows display settings ("Extend these displays") and moves the game window to the dummy monitor. The capture pipeline picks up the dummy monitor as a new `DisplayInfo`.
- The host-agent detects the dummy plug by looking for a display with `is_virtual=false` and `name` matching the EDID "Dummy HDMI" string (configurable). It suggests the user move the game there.

**macOS (v1):**
- `set_display_state(Privacy)` returns `Err(DisplayError::NotSupported("CGVirtualDisplay not yet implemented; workaround: use BetterDummy or DisplayDummy app"))`.
- The user runs a third-party virtual display app. The host-agent enumerates it and can capture it.

#### 3.3 Coordination with the Capture Pipeline

When a display enters `Privacy` state:
1. The `DisplayManager` calls `CaptureOrchestrator::on_display_state_change(display_id, Privacy)`.
2. The orchestrator closes the existing `CaptureSession` for the physical display.
3. The orchestrator opens a new `CaptureSession` for the virtual display via `backend.open_capture(virtual_display_id, options)`.
4. The orchestrator starts a new ffmpeg subprocess for the new session.
5. The `WireAccessUnitHeader.display_id` in the stream changes from the physical display ID to the virtual display ID.
6. The client receives `IpcEvent::DisplayStateChanged { display_id, old_state: Active, new_state: Privacy }` and updates its UI (shows "(private)" badge, switches to the virtual display's stream).

When a display re-enters `Active`:
1. Same process but reversed: close virtual display session, open physical display session, update UI.

#### 3.4 Atomicity and Error Handling

The `set_display_state(Privacy)` transition is **best-effort**. The trait method returns `Result<(), DisplayError>` but the implementation may have partially completed:
- If virtual display creation succeeds but `xset dpms force off` fails (e.g. no permission), the virtual display is still live and the game is still captured. Return `Err(DisplayError::Os("xset failed; virtual display is active but physical display is not blanked"))`.
- The caller (the orchestrator) logs the error and continues with whatever succeeded.
- The user sees a warning via the event channel.

### 4. Multi-Stream Pipeline Fan-Out

#### 4.1 Capture Orchestrator

New module `apps/host-agent/src/capture_orchestrator.rs`:

```rust
/// Owns the lifecycle of all capture sessions and their encoder pipelines.
pub struct CaptureOrchestrator {
    backend: Box<dyn CaptureBackend>,
    display_manager: Box<dyn DisplayManager>,
    sessions: Arc<Mutex<HashMap<DisplayId, CaptureSessionHandle>>>,
    encoder_tasks: Vec<JoinHandle<()>>,
    ffmpeg_subprocesses: Vec<tokio::process::Child>,
}

struct CaptureSessionHandle {
    session: Box<dyn CaptureSession>,
    ffmpeg_stdin: tokio::process::ChildStdin,
}

pub struct PerStreamConfig {
    pub codec: VideoCodec,
    pub encoder: EncoderBackend,
    pub target_fps: u32,
    pub target_bitrate_kbps: u32,
    pub scale_mode: ScaleMode,
    /// Optional downscale target. If set, the encoder resizes to this.
    pub target_resolution: Option<(u32, u32)>,
    /// Whether to capture the cursor.
    pub capture_cursor: bool,
}
```

**`start(displays: Vec<DisplayId>, config: HashMap<DisplayId, PerStreamConfig>)`:**
1. For each `display_id` in `displays`:
   - `backend.open_capture(display_id, CaptureOptions { region: None, color_space: None, target_fps: config.fps, capture_cursor: true })` → `CaptureSession`.
   - Spawn ffmpeg subprocess: `ffmpeg -f rawvideo -pix_fmt bgra -video_size WxH -framerate FPS -i pipe:0 -c:v <encoder> -b:v <bitrate> -f <codec> pipe:1`.
   - Connect the capture session's frame channel to ffmpeg's stdin.
   - Spawn tokio task: reads ffmpeg's stdout, packetizes into `EncodedVideoAccessUnit` with `stream_id = display_id.0`, sends over the appropriate QUIC stream.
2. Track all sessions + subprocesses + tasks.
3. All streams are independent — one subprocess crash does not affect others (the orchestrator logs the error and offers to restart that stream).

**`stop()`:**
1. Send SIGTERM to all ffmpeg subprocesses.
2. Wait for all tasks (with 5-second timeout).
3. Close all sessions.
4. Kill any remaining subprocesses.

**QUIC integration:**
- The existing `NativeQuicHost::accept_authenticated_connection` opens one QUIC connection per session.
- The orchestrator opens **one QUIC receive stream per display** within the session. Each stream carries `WireAccessUnitHeader` with the display's `stream_id`.
- The existing `NativeQuicMediaSender` (in `crates/qubox-transport`) needs a `stream_id` parameter per `send_access_unit` call to select which QUIC stream to write to.
- New method: `connection.open_display_stream(display_id) -> NativeQuicMediaSender`.
- The `NativeQuicMediaSender` wraps a single QUIC uni-directional stream. The connection opens one per display.

#### 4.2 One ffmpeg Subprocess Per Display

For the first cut, each display gets its own ffmpeg subprocess. This is the simplest path:

- Pros: no code change to the encoder pipeline beyond looping. Per-display configuration (codec, bitrate, resolution) is independent. A crash in one subprocess only affects that display.
- Cons: more processes (~50 MB RAM each for a 1080p60 stream). 3 displays = ~150 MB for ffmpeg processes alone. No encoder state sharing across displays.

Each ffmpeg subprocess reads raw BGRA from a pipe (stdin) and writes encoded annex B bytes to stdout:
```
ffmpeg \
  -hide_banner -loglevel warning -nostdin \
  -f rawvideo -pix_fmt bgra -video_size {W}x{H} -framerate {FPS} -i pipe:0 \
  -an \
  -vf {scale_filter} \
  -c:v {encoder_name} \
  -b:v {bitrate}k -maxrate {bitrate}k -bufsize {bitrate/2}k \
  -g {keyframe_interval} -bf 0 \
  -bsf:v {aud_bsf} -f {codec_fmt} pipe:1
```

The orchestrator task for each display:
```rust
loop {
    let frame = session.next_frame(Duration::from_millis(16)); // ~60 FPS
    match frame {
        Ok(Some(frame)) => {
            ffmpeg_stdin.write_all(&frame.bytes).await?;
            // Also read encoded output from ffmpeg stdout in a concurrent select!
        }
        Ok(None) => break, // timeout or EOF
        Err(CaptureError::DisplayDisconnected(id)) => {
            tracing::warn!(?id, "display disconnected, stopping stream");
            break;
        }
        Err(e) => {
            tracing::error!(?e, "capture error on display");
            break;
        }
    }
}
```

**Important**: The ffmpeg subprocess's stdout must be read concurrently with writing to its stdin. A deadlock can occur if we only write (ffmpeg's pipe buffer fills up, blocking the write) without reading. The solution: use `tokio::io::copy` in a separate task that reads ffmpeg's stdout into the QUIC sender, OR use `tokio::select!` to alternate between writing frames and reading encoded output. The `AsyncRead` + `AsyncWrite` interleaving is handled by `tokio::process::Child` pipes.

#### 4.3 Per-Stream Config

Each display can have its own codec, bitrate, and resolution. The client negotiates per-display preferences via the `StartSessionRequest`:

```rust
// Extension to StartSessionRequest:
pub struct MultiStreamRequest {
    pub displays: Vec<DisplayStreamPref>,
}

pub struct DisplayStreamPref {
    pub display_id: Option<u32>,   // None = "the host picks"
    pub codec: Option<VideoCodec>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub framerate: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    pub scale_mode: Option<ScaleMode>,
    pub encoder: Option<String>,
}
```

For v1, the simpler approach: the client sends `VideoStreamPreferences` as today (one set of preferences), and the host applies them to ALL displays. The `MultiStreamRequest` is a future extension. The orchestrator defaults to `PerStreamConfig` cloned from the single session config for each display.

#### 4.4 Client-Side Changes

The client (currently `client-cli` and `client-gui`) subscribes to multiple streams:

- **CLI flags** (on `client-cli start-session`):
  - `--display <id>` — pick a specific display to view (single-stream mode). Default: `0` (primary).
  - `--all-displays` — subscribe to all available displays. Overrides `--display`.
  - `--tile` — enable the tiled unified view (all displays shown in a grid in one window).
  - `--stream-window` — for `--all-displays` without `--tile`, opens N windows, one per display.

- **Client GUI** (future, P2): the stream picker lists available displays with names, resolutions, and a "(private)" indicator. The user selects which to view or enables tiled mode.

- **Backward compatibility**: `stream_id: 0` is the default for single-stream sessions. Clients that don't understand multi-stream ignore extra streams (they only subscribe to stream_id 0 on the control channel via `ControlMsg`). The host only spawns one ffmpeg subprocess if the client only subscribed to one display (or if `--single-stream` is set).

#### 4.5 Migration from Single-Stream

| Aspect | Current | After ADR-007 |
|--------|---------|---------------|
| `WireAccessUnitHeader` | No stream_id, no display_id | Has stream_id: u16, display_id: u32 |
| Capture backend | ffmpeg subprocess (x11grab/gdigrab) | `CaptureBackend` trait with per-platform impl |
| Display enumeration | `enumerate_x11_displays` via xrandr | `CaptureBackend::enumerate_displays()` unified |
| Pipeline spawn | `spawn_ffmpeg_pipeline(&plan)` — one | `CaptureOrchestrator::start()` — N per display |
| Input mapping | `scale_input_coordinate(v, stream, display)` — single | `VirtualDesktopPoint::to_display_local()` — multi-monitor |
| Privacy | Not implemented | `DisplayManager::set_display_state(DisplayState::Privacy)` |
| CLI flags | `--display <idx>`, `--list-displays` | Same + `--multi-display`, `--all-displays`, `--privacy {off,overlay,virtual}` |

**Backward compat path**: When the host is started with `--single-stream` (default), it:
1. Uses `detect_backend()` to enumerate displays.
2. Captures display 0 (primary) only, using the new `CaptureBackend::open_capture()`.
3. Spawns exactly one ffmpeg subprocess.
4. Sets `WireAccessUnitHeader.stream_id = 0`.
5. Input mapping uses the single-display path (scale to the primary display's physical size).

When `--multi-display` or `--all-displays` is passed, the orchestrator opens N sessions.

### 5. Migration / Rollout

Four phases, each independently mergeable:

#### Phase A: Crate + Traits + Stubs

- Create `crates/qubox-display/` with `lib.rs`, `traits.rs`, `types.rs`.
- Implement `X11RandrBackend` (Linux X11, full).
- Implement `DxgiBackend` (Windows, compile-only, runtime-tested on the dev box via CI cross-compile only — no Windows runner).
- Implement `ScreenCaptureKitBackend` (macOS, stub: `open_capture` returns `Err(NotSupported)`).
- Implement `PipeWirePortalBackend` (Linux Wayland, stub).
- `detect_backend()` and `display_manager()` selection logic.
- `cargo build --workspace` passes on all platforms.
- **No integration into `host-agent` yet.**

#### Phase B: Host-Agent Integration

- Add `crates/qubox-display` as a dependency of `apps/host-agent`.
- Add `apps/host-agent/src/capture_orchestrator.rs` with `CaptureOrchestrator`.
- Replace the single `spawn_ffmpeg_pipeline` call in `run_native_quic_session` with a configurable path:
  - `--single-stream` (default): same behavior as today; uses `CaptureOrchestrator` with 1 display.
  - `--multi-display` or `--all-displays`: orchestrator with N displays.
- Add per-display QUIC streams to `NativeQuicConnection`: `open_display_stream(display_id)`.
- Extend `WireAccessUnitHeader` with the new fields (`stream_id`, `display_id`, `width`, `height`, `refresh_hz`, `color_space_id`, `hdr_static_metadata`). Use `#[serde(default)]` for backward compat.
- Update `RemoteInputInjector` to use the multi-monitor coordinate system.
- Add `HoverDisplay` event emission.
- **The single-stream path is bit-for-bit identical** (same ffmpeg args, same QUIC stream shape).

#### Phase C: Privacy Mode

- Implement `X11DisplayManager` (Linux): `create_virtual_display` (vkms), `destroy_virtual_display`, `set_display_state`.
- Implement `BlankOverlayManager`: cross-platform fallback using `winit` + `softbuffer`.
- Add `--privacy {off,overlay,virtual}` and `--privacy-virtual-display` (for specifying a config) CLI flags.
- Wire `Privacy` state transitions into the `CaptureOrchestrator`.
- Add `IpcEvent::DisplayStateChanged` and `IpcEvent::PrivacyDegraded` events.
- Windows: document the dummy HDMI plug workaround in `docs/privacy-windows.md`.
- macOS: return `NotSupported` with a pointer to `BetterDummy`.

#### Phase D: Client-Side

- Add `--display`, `--all-displays`, `--tile` to `client-cli start-session`.
- Client receives N streams, filters by the selected display, or tiles them.
- Add `HoverDisplay` event handling: highlight the active display in the tiled UI.
- Stats overlay (P1-12), mic (P1-10), and clipboard (P1-9) are separate phases.

### 6. Test Plan

#### 6.1 Unit Tests

| Area | Test | What it verifies |
|------|------|------------------|
| `detect_backend()` | Platform dispatch: on Linux+X11, returns X11RandrBackend | Selection logic |
| `detect_backend()` | Platform dispatch: on Linux+Wayland, returns PipeWirePortalBackend (with mock `XDG_SESSION_TYPE`) | Selection logic |
| `VirtualDesktopPoint::to_display_local` | Point on display 0, point on display 1, point on no display | Coordinate round-trip |
| `VirtualDesktopPoint::to_display_local` | HiDPI scaling: Logical→Physical at scale 1.0, 1.5, 2.0 | Scale factor math |
| `PhysicalPixel::from_logical` / `LogicalPixel::from_physical` | Round-trip at various scales | Scale factor math |
| `ColorSpaceId::h273_*` | All 4 variants return correct H.273 values | Wire format |
| `DisplayState` transitions | Active→Privacy→Active, Active→Blanked, Privacy→Blanked etc. | State machine validity |
| `CaptureOptions` serde | Round-trip through bincode | Wire stability |
| `CapturedFrame` creation | Frame with each PixelFormat | Data integrity |
| `DisplayManager::set_display_state` error forwarding | vkms fails → returns `Err` with clear message | Error path |
| `WindowHandle` platform dispatch | Construction from raw handles | Platform abstraction |

#### 6.2 Integration Tests

| Test | Setup | Verifies |
|------|-------|----------|
| X11RandrBackend enumeration | Xephyr `:99` display (the existing e2e setup) | At least 1 display reported with correct resolution (1280×720 for the default Xephyr) |
| X11RandrBackend open_capture | Xephyr `:99`, capture display 0 | A `CapturedFrame` is received within 100 ms |
| X11RandrBackend create_virtual_display (mocked) | Mock `std::process::Command` for `modprobe` (use `Command::new` wrapper that we inject) | Returns `DisplayId` without actually loading vkms |
| X11RandrBackend create_virtual_display (failure) | Mock `modprobe` returns non-zero exit | Returns `Err` |
| CaptureOrchestrator start/stop | 2 fake `CaptureSession`s (mocked backend) | Sessions opened, ffmpeg processes spawned, clean shutdown |
| Multi-stream WireAccessUnitHeader | Encode 2 frames from 2 displays, decode at client | `stream_id` and `display_id` correctly round-trip |

#### 6.3 E2E Tests

| Test | Setup | Verifies |
|------|-------|----------|
| Single-stream backward compat | `host-agent --single-stream` on Xephyr `:99`, `client-cli start-session --display 0` | Same frame flow as today |
| Multi-display host | `host-agent --multi-display` on a multi-head setup (dev box has 1 physical display; use a second Xephyr with `-screen 1280x720` via `Xephyr :1` on a separate virtual display) | Client receives 2 streams with distinct `stream_id` values |
| Privacy mode (Linux, mocked vkms) | Host with `--privacy virtual`, vkms mocked | `set_display_state(Privacy)` called, fallback to blank overlay since vkms is mocked |
| Privacy mode (Linux, real vkms) | Dev box with `modprobe vkms` (requires sudo; manual test) | Physical display blanked, game moved to VKMS, client sees game on virtual display |

#### 6.4 Manual Tests

Discussed in the open questions — not automated, but critical to validate:

| Test | What to check |
|------|---------------|
| Tri-monitor Linux host | 3 streams, each showing the correct display's content. Verify no performance regression vs single-stream. |
| Windows DXGI 2-monitor | 2 streams, correct per-monitor content, cursor composited correctly. |
| Privacy mode + re-plug | Privacy mode active → unplug the physical display → client sees `DisplayDisconnected` → re-plug → auto-recovery? (Current design: user must reconnect.) |
| Input mapping across 2 displays | Client moves cursor from display 0 to display 1. Verify the host cursor reaches display 1's top-left correctly. |

### 7. Open Questions

1. **What happens to the input mapping when the host is in Privacy Mode and the cursor is over the virtual display (which has no physical representation)?**
   - The virtual display has a position in the virtual desktop coordinate space (at the primary display's right, for example). The cursor is mapped to that position. `enigo.move_mouse()` moves the cursor to the virtual display's region on the physical monitor (which is blanked/off). The cursor is invisible to the local user but the OS tracks its position. **This is acceptable** — the virtual display is a real OS display, just not wired to a physical panel.
   - Edge case: if the OS blanks the physical display via DPMS, `enigo.move_mouse()` may or may not wake the monitor. On Linux, `xset dpms force off` disables the monitor; any input event (including programmatic mouse movement) wakes it. **The host must re-blank** after every input injection while in Privacy mode. Either: (a) call `xset dpms force off` after every input injection (racy — the user sees a flash), or (b) accept that the physical display shows the cursor on a black background (not a privacy leak — the desktop is still on the virtual display). **Decision**: option (b) — don't fight DPMS. The physical display shows a black screen with only the cursor (which is on the virtual desktop). If the user enables privacy mode, they accept this behavior. The docs will state: "Your physical monitor may show a cursor on a black background; the actual desktop is never visible."

2. **How does the client signal to the host that it wants to switch from tiled view to single-display view mid-session?**
   - **New control message**: `ControlMsg::StreamSubscribe { stream_id: u16, action: Subscribe | Unsubscribe }`.
   - The client sends `StreamSubscribe { stream_id: display_1, action: Subscribe }` when the user selects a specific display in the UI.
   - The host responds by starting/stopping the corresponding ffmpeg subprocess (if not already running).
   - The `ControlMsg` already has stream_id routing. Add a `StreamSubscribe` variant.
   - **Backward compat**: clients that don't send `StreamSubscribe` continue to receive only stream_id 0 (primary display).

3. **What is the failure mode when the user unplugs a physical display while a session is active?**
   - The capture backend detects the disconnection on the next `next_frame()` call: `Err(CaptureError::DisplayDisconnected(display_id))`.
   - The orchestrator logs the event, closes the capture session, kills the ffmpeg subprocess.
   - The client's stream stalls (no more frames on that stream_id). The `ControlMsg::StreamStats` shows `frames_dropped` increasing as the jitter buffer empties.
   - **Client UX**: show "Display disconnected" overlay on that stream's tile. If no other streams remain, show "Session ended: all displays disconnected."
   - **Auto-reconnect**: not for v1. The user must close and reconnect (the display topology change may require a new ffmpeg pipeline with different geometry). Future: listen for udev/drm hotplug events and re-enumerate.

4. **Can the host run in headless mode with only virtual displays?**
   - Yes — if the host has no physical monitor (headless server), it can start with `--privacy virtual` and create one or more virtual displays without blanking any physical display (there is none). The game renders to the virtual display and is captured. The host's desktop environment must support a headless display server (Xorg with dummy driver or Wayland with headless backend). Linux is the headless path.
   - **For v1**: the host must have at least one physical or virtual display. Pure-headless (no physical, no virtual at startup) requires creating a virtual display before the session begins, which is supported via `CaptureOrchestrator::start()` with a virtual display.

5. **Can the client negotiate different codecs for different displays?**
   - **For v1**: all displays use the same codec (the one negotiated in `SessionRequested.codec`). The `PerStreamConfig` supports per-display codec but the signaling doesn't expose it yet.
   - **Future**: `MultiStreamRequest` with per-display `codec`, `bitrate`, etc.

6. **Does the VirtualDesktopPoint space overflow u32?**
   - Modern multi-monitor setups rarely exceed 32768×32768 virtual desktop (the Windows limit for GDI coordinates). However, `RemoteInputEvent::MouseMove` currently sends `x: u32, y: u32`. If the virtual desktop space exceeds 4 billion pixels (unlikely in practice), the coordinate wraps. **Acceptable for v1.** Document that coordinates are signed i32 internally but serialized as u32 (cast from i32; negative offsets on some display configurations are clamped to 0).

7. **How does `move_window_to_display` work on Wayland?**
   - Wayland restricts window movement (the client can't arbitrarily reposition windows owned by another process). The `wlr-foreign-toplevel-management` protocol (available on wlroots-based compositors like Sway, Hyprland) supports moving toplevels between outputs. On GNOME/KDE, no such protocol exists. **For v1**: Wayland privacy mode is NotSupported unless `wlr-foreign-toplevel-management` is available. This is documented as a Known Gap.

8. **What happens when the user presses Win+Tab (Task View) or Alt+Tab in Privacy Mode?**
   - On Linux with vkms: the virtual display is a real display. The task switcher shows the virtual display's content (the game) and the physical display's content (blank/black). The user can tab to the game window on the virtual display — it's still a legitimate target. The privacy mechanism is: the physical display is blanked, so even if the game window is selected, the user sees only black. **This is sufficient for the threat model** (casual oversight, not NSA-grade isolation).
   - On Windows: no programmatic privacy mode in v1. The user manages this manually via the dummy HDMI plug.

9. **Per-display memory budget: 3 × 1080p60 BGRA = 720 MB/s. How does the orchestrator not bottleneck?**
   - Each display's capture → pipe → ffmpeg is independent. The bottleneck is ffmpeg's encoding speed and the QUIC stream send rate, not the pipe throughput. BGRA→NV12 conversion happens inside ffmpeg via the `scale` filter. The orchestrator tasks are `tokio::task::spawn_blocking` for the ffmpeg read/write (they do blocking I/O on child pipes).
   - Short-term: use `tokio::task::spawn_blocking` for each display's capture+encode loop.
   - Long-term: ffmpeg-next (P0-3) replaces subprocesses with in-process encoding, reducing copies and memory.

## Consequences

**Positive:**
- Multi-display capture (P1-7) and privacy mode (P1-8) share the same abstraction layer, reducing future architecture work.
- The trait boundary allows per-platform optimization without changing the orchestrator.
- Backward compatibility: `--single-stream` produces exactly the same output as the current code. The `WireAccessUnitHeader` extensions use `#[serde(default)]` so old clients ignore new fields.
- Privacy mode on Linux has a concrete implementation path with a well-understood fallback (blank overlay).
- The 4-phase rollout allows incremental integration without blocking other work.

**Negative:**
- New crate `crates/qubox-display/` adds workspace build time and dependency surface.
- The `CaptureBackend` trait's `enumerate_displays` and `CaptureSession::next_frame` are blocking (synchronous). The async layer (`CaptureOrchestrator`) wraps them in `spawn_blocking`. This adds a tokio thread per display for the capture loop. Acceptable for v1 (3 threads for 3 displays).
- Privacy mode on Windows requires manual user action (dummy HDMI plug). The software fallback (blank overlay) is bypassable.
- macOS virtual displays are deferred entirely. Users on macOS must use third-party tools.

**Risks:**
- `vkms` may not be available on all Linux kernels (missing module, Secure Boot, custom kernel). The blank overlay fallback is always available.
- Display hot-plug during a session is not fully resilient (the orchestrator detects disconnection but doesn't auto-re-enumerate). This matches the behavior of most game streaming apps.
- Wayland virtual display creation depends on compositor-specific protocols. The `wlr-output-management` path only works on wlroots compositors.
- `enigo::move_mouse` on DPMS-off monitors may wake the display. Documented as an acceptable trade-off.

## References

- P1-7: Multi-Monitor Capture (research/roadmap/p1-07-multi-monitor.md)
- P1-8: Privacy Mode (research/roadmap/p1-08-privacy-mode.md)
- P2-17: Cross-Platform Host (research/roadmap/p2-17-cross-platform-host.md) — the `CaptureBackend` trait is the direct implementation
- ADR-002: Target Architecture and Upgrade Strategy (research/decisions/ADR-002-target-architecture-and-upgrade-strategy.md)
- ADR-004: Virtual Gamepad (research/decisions/ADR-004-virtual-gamepad-uinput-vigembus.md) — pattern for per-platform backend traits
- Existing `WireAccessUnitHeader` in `crates/qubox-transport/src/lib.rs:655`
- Existing `DisplayInfo`, `CaptureSourceConfig`, `HostVideoPipelineConfig` in `crates/qubox-media/src/lib.rs`
- Existing `VideoStreamPreferences::display_index` in `crates/qubox-proto/src/lib.rs`
- x11rb RandR: https://docs.rs/x11rb
- DXGI Desktop Duplication: https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api
- ScreenCaptureKit: https://developer.apple.com/documentation/screencapturekit
- vkms kernel docs: https://docs.kernel.org/gpu/vkms.html
- IddCx Indirect Display Driver: https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview
