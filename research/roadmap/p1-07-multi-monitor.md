# P1-7: Multi-Monitor Capture (X11/RandR, DXGI Desktop Duplication, ScreenCaptureKit)

Status: research complete, implementation pending.
Owner: `apps/host-agent` (capture pipeline), with a new `capture` module.
Depends on: P0-1 (HW encode; per-stream encoders), the existing `WireAccessUnitHeader.stream_id` (already supports multi-stream).
Blockers: macOS ScreenCaptureKit requires macOS 12.3+; Windows DXGI 1.2 requires Windows 10+ for HDR; Linux X11 still dominant (Wayland is a separate path).

## Goal

Add per-display capture so each physical monitor is streamed as a separate video stream, with the user able to select which to view (or view all in a tiled layout). Replace the current single-stream x11grab capture on Linux with a per-display loop, and on Windows/macOS use the platform-native capture API. The existing wire format already supports `stream_id`; the change is in the capture and encoder pipelines.

## Research Summary

### Linux/X11 with x11rb (current + multi-monitor)

The current capture uses ffmpeg's `x11grab` with `-video_size 5760x1080 -i :0.0` (full tri-monitor). For per-display capture, switch to x11rb and enumerate via **RandR**:

- `randr::get_screen_resources` returns the list of CRTCs and outputs.
- For each output: `randr::get_output_info`, `randr::get_crtc_info` to get position, size, and mode.
- Mode timing: `mode.dot_clock / (mode.h_total * mode.v_total)` gives the refresh rate.
- Per-display capture: `xproto::get_image(format=ZPixmap, drawable=root, x, y, width, height, !0)` reads the framebuffer at the display's region. The bytes are BGRA.

x11rb 0.13+ is the current stable line. Latency for `get_image` is **1-3 ms per frame** at 1080p locally, similar to x11grab.

Rust crate: `x11rb` (the protocol binding), `x11rb_protocol` (the protocol types). For DRM/KMS direct capture (lower latency at 0.5-2 ms but requires root and is per-CRTC), use the `drm` crate.

### Windows: DXGI Desktop Duplication API

The native Windows capture API. Per-output:

1. `IDXGIFactory1::CreateFactory` (via `CreateDXGIFactory1`).
2. `factory.EnumAdapters1(idx)` → `IDXGIAdapter1`.
3. `adapter.EnumOutputs(idx)` → `IDXGIOutput`.
4. `IDXGIOutput1::DuplicateOutput(&device)` → `IDXGIOutputDuplication`.
5. `duplication.AcquireNextFrame(timeout_ms, &frame_info, &desktop_resource)` → `IDXGIResource` (a `ID3D11Texture2D`).
6. `Map` the texture, read BGRA bytes, `Unmap`.
7. `ReleaseFrame`.

One `IDXGIOutputDuplication` per monitor. The capture loop runs all of them in parallel (one thread per monitor or a tokio task with `select!`).

**Latency: 1-3 ms per frame** (GPU-accelerated, the desktop is already a GPU texture).

**HDR**: DXGI 1.2+ supports scRGB and HDR10 formats. Check the output's `DXGI_OUTPUT_DESC1` (1.2+) for color space. To capture HDR, set the texture format to `DXGI_FORMAT_R16G16B16A16_FLOAT` (scRGB) or `DXGI_FORMAT_R10G10B10A2_UNORM` (HDR10). The captured surface may be converted to SDR by the OS; verify with a real HDR display.

**Cursor**: `duplication.GetCursorPositionInfo()` and `GetPointerShapeInfo()` give the mouse pointer; draw it onto the captured frame before encoding.

Rust crate: `windows` 0.58+ has the DXGI + D3D11 bindings. Use `windows::Win32::Graphics::Dxgi::Common::*`, `Direct3D11::*`.

### macOS: ScreenCaptureKit (12.3+)

The native macOS capture API. Per-display:

1. `SCShareableContent::current().await` (async).
2. Iterate `.displays` to get `SCDisplay` objects.
3. For each display, create `SCStreamConfiguration` with `width`, `height`, `pixelFormat` (BGRA or 10-bit for HDR), `colorSpaceName` (sRGB / extended sRGB / displayP3).
4. Create `SCStream(filter, configuration, delegate)`.
5. `stream.startCapture()`.
6. The delegate's `stream(_:didOutputSampleBuffer:of:)` callback receives `CMSampleBuffer` per frame; convert to `CVPixelBuffer` and read BGRA bytes.

**Latency: 5-15 ms** (higher than DXGI/x11grab; macOS adds an extra frame of compositing).

**HDR**: macOS 14+ supports HDR capture via `kCGColorSpaceExtendedSRGB` or `kCGColorSpaceDisplayP3` and 10/16-bit pixel formats. Verify with a real HDR display.

Rust binding: `screencapturekit-rs` (community crate) + `objc2` for Objective-C interop. The API is rapidly evolving; pin to a specific version.

### Wire format (already supports multi-stream)

The existing `WireAccessUnitHeader` (or its successor) carries `stream_id: u16`. Each display maps to one `stream_id`. The client's `MediaDatagramSender` (P0-2) includes the stream_id in the chunk header. The client GUI enumerates streams and lets the user pick.

The header should also carry:
- `width: u32`, `height: u32` (the display's native size)
- `refresh_hz: f32`
- `color_space_id: u8` (sRGB=0, scRGB=1, DisplayP3=2)
- `hdr_static_metadata: Option<HdrStaticMetadata>` (for P2-14)

### FFmpeg integration: one subprocess per display

For the first cut, **one ffmpeg subprocess per display**. Each subprocess has its own x11grab input for the display's region and produces a separate encoded stream. The host-agent spawns N subprocesses, reads N pipes, and sends N streams over QUIC.

Pros: simple, no code change to the encoder pipeline beyond looping.
Cons: more processes; can't share encoder state across displays.

For P0-3 (ffmpeg-next), one in-process encoder per display; no subprocess overhead.

### Latency (capture only, before encode)

| Backend            | Latency    | HDR | Cursor | Notes |
|--------------------|------------|-----|--------|-------|
| x11grab (X11)      | 1-3 ms     | No  | Yes (separate) | Works on most distros |
| DRM/KMS (Linux)    | 0.5-2 ms   | Yes | No  | Needs root, per-CRTC |
| DXGI Duplication (Win) | 1-3 ms | Yes (1.2+) | Yes (built-in) | Gold standard for Windows |
| GDI BitBlt (Win)   | 5-8 ms     | No  | Yes | Old, slow, fallback only |
| ScreenCaptureKit (Mac) | 5-15 ms | Yes (14+) | Yes | Native, recommended |
| CGDisplayStream (Mac) | 10-20 ms | No  | Yes | Deprecated but works |

### 2024-2026 status

- **ScreenCaptureKit** is mature; macOS 14+ adds HDR and a more flexible filter API. Apple deprecated `CGDisplayStream` for new code; SCK is the recommended path.
- **DXGI Desktop Duplication 1.2** is stable on Windows 10/11. scRGB support is consistent on Intel/AMD/NVIDIA drivers. HDR10 capture is possible but drivers vary in color accuracy.
- **Linux**: Wayland is the future but X11 is still 70%+ of the install base as of 2024-2026. Capture on Wayland requires `wlr-screencopy` (Sway, Hyprland), `xdg-desktop-portal` (GNOME, KDE), or DRM/KMS. For the first release, support X11 via x11rb; add Wayland via `xcap` (cross-platform Rust capture library) or a direct portal implementation.
- **scRGB** (Windows 11 22H2+): extended sRGB in linear float. Better HDR fidelity than HDR10 in many cases. Capture with `DXGI_FORMAT_R16G16B16A16_FLOAT`.

### Rust crate matrix (2024-2026)

- `x11rb` 0.13+: Linux X11, including RandR.
- `drm` 0.12+ (or `drm-rs`): Linux DRM/KMS for low-latency direct capture.
- `windows` 0.58+: Windows DXGI, D3D11, GDI.
- `screencapturekit-rs` (latest on crates.io): macOS ScreenCaptureKit.
- `objc2` 0.5+: macOS Objective-C interop.
- `xcap` 0.0.x: cross-platform capture library (X11/Wayland/Mac/Win) — useful as a reference or as a fallback for X11/Wayland.

## Implementation Plan

### Step 1: DisplayInfo type

`apps/host-agent/src/capture/mod.rs`:
- `pub struct DisplayInfo { id: u64, name: String, x: i32, y: i32, width: u32, height: u32, refresh_hz: f32, scale: f32, hdr: bool }`.
- `pub trait CaptureBackend { fn enumerate() -> Result<Vec<DisplayInfo>>; fn start(display: &DisplayInfo, prefs: &VideoStreamPreferences) -> Result<CaptureStream>; }`.
- `pub struct CaptureStream { pub rx: tokio::sync::mpsc::Receiver<CapturedFrame> }` where `CapturedFrame` is `{ bgra: Vec<u8>, width, height, pts: Instant, cursor_xy: Option<(i32, i32)>, cursor_visible: bool }`.

### Step 2: Linux/X11 backend

`apps/host-agent/src/capture/x11.rs` (new, behind `cfg(target_os = "linux")`):
- `pub struct X11Backend { conn: x11rb::RustConnection, root: u32 }`.
- `enumerate` uses RandR as shown in the research.
- `start` spawns a thread that calls `get_image` in a loop and pushes `CapturedFrame`s into the channel.
- Add `x11rb = "0.13"` to `host-agent/Cargo.toml`.

### Step 3: Windows backend

`apps/host-agent/src/capture/dxgi.rs` (new, behind `cfg(target_os = "windows")`):
- `pub struct DxgiBackend { factory: IDXGIFactory1 }`.
- `enumerate` uses `EnumAdapters1` + `EnumOutputs` + `GetDesc`.
- `start` creates a D3D11 device, calls `DuplicateOutput` per display, runs `AcquireNextFrame` in a loop, reads the texture, draws the cursor, pushes the frame.
- Add `windows = { version = "0.58", features = ["Win32_Graphics_Dxgi", "Win32_Graphics_Direct3D11", "Win32_Graphics_Direct3D", "Win32_Foundation"] }` to `host-agent/Cargo.toml`.

### Step 4: macOS backend

`apps/host-agent/src/capture/macos.rs` (new, behind `cfg(target_os = "macos")`):
- `pub struct SckBackend { /* SCStream per display */ }`.
- `enumerate` uses `SCShareableContent::current`.
- `start` creates `SCStreamConfiguration` + `SCStream`, sets a delegate, starts the capture.
- Add `screencapturekit-rs` and `objc2` to `host-agent/Cargo.toml`.

### Step 5: Multi-stream encoder

`apps/host-agent/src/encoder/pipeline.rs`:
- Replace the single `EncoderPipeline` with `pub struct MultiStreamEncoder { streams: HashMap<u16 /* stream_id */, EncoderPipeline> }`.
- `start_session` enumerates displays, starts a capture per display, starts an encoder per display, sends the encoded AUs with `WireAccessUnitHeader.stream_id = display_id`.

### Step 6: Wire format update

`crates/qubox-proto/src/lib.rs`:
- `WireAccessUnitHeader` gains `width: u32, height: u32, refresh_hz: f32, color_space_id: u8, hdr_static_metadata: Option<HdrStaticMetadata>`.
- Backward-compatible: existing parsers ignore unknown fields (use `#[serde(default)]`).

### Step 7: Tests

- Unit test: `DisplayInfo` serde round-trip.
- Integration test on Xephyr: enumerate 1 display (Xephyr is 1 monitor), start capture, verify a frame is received.
- Manual: tri-monitor Linux host (the dev box), verify 3 streams, verify each frame is the correct display's region.
- Manual: Windows host, verify DXGI capture works on 2+ monitors.

## Risks and Open Questions

- **Per-display memory pressure**: 3 streams × 1080p60 × 4 MB (BGRA) = 720 MB/s of pixel throughput. The ffmpeg subprocess per display uses ~50 MB RAM each. ffmpeg-next is more efficient. Plan for 2-3 GB RAM for a tri-monitor host.
- **Per-display bitrate**: each stream is independently rate-controlled (P0-4). Total bandwidth = sum of per-stream bitrates. For 3×1080p60 at 4 Mbps each = 12 Mbps; the user must have a 15+ Mbps upload link.
- **Color management**: sRGB on the display may not match the captured BGRA's color space. Wayland/X11 have no canonical color management; macOS/Windows do. For the first release, assume sRGB.
- **Wayland capture**: defer to a follow-up using `xcap` or a direct portal implementation. The current X11 path covers the dev box.
- **HDR capture on Linux**: requires DRM/KMS direct capture, which needs root and bypasses the compositor. Defer to P2-14.
- **Cursor drawing**: the OS may move the cursor between `AcquireNextFrame` and our read; we draw the cursor at the position from the previous frame. Most games don't notice; some cursor-heavy apps (Photoshop) will. Document the limitation.
- **DPI scaling**: Windows has per-monitor DPI scaling; the captured frame is in physical pixels. The wire format should carry the display's scale factor so the client can render at the correct DPI.
- **Per-display audio**: each display may have a separate audio source. For the first release, use the system's default audio (P1-10).
- **macOS Screen Recording permission**: required; the user must grant in System Settings → Privacy & Security → Screen Recording. Same as on Windows (where the OS prompts on first capture).

## References

- x11rb docs: https://docs.rs/x11rb
- x11rb protocol: https://doc.servo.org/x11rb_protocol/index.html
- x11rb tutorial: https://github.com/psychon/x11rb/blob/master/x11rb/examples/tutorial.rs
- DXGI Desktop Duplication API: https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api
- DXGI Output Duplication sample: https://github.com/microsoft/Windows-classic-samples/blob/main/Samples/DXGIDesktopDuplication/cpp/DesktopDuplication.cpp
- Pavel Gurenko's DXGI enumeration: https://www.pavelgurenko.com/2013/12/dxgi-outputs-enumeration-and-fast.html
- ScreenCaptureKit (Apple docs): https://developer.apple.com/documentation/screencapturekit
- xcap (cross-platform capture): https://lib.rs/crates/xcap
- screencapturekit-rs (community): https://crates.io/crates/screencapturekit-rs
- Perplexity research, 2026-07-02: x11rb, DXGI, SCK, latency, HDR, 2024-2026 status.
