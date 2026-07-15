# P2-17: Cross-Platform Host (Linux / Windows / macOS)

Status: research complete, implementation pending.
Owner: `apps/host-agent` (capture, audio, input, encoder), with per-OS modules.
Depends on: P1-7 (multi-monitor capture; provides per-OS backends), P0-1 (HW encode; per-OS encoder selection), P0-6 (gamepad; per-OS virtual gamepad), P1-13 (daemon; cross-platform service).
Blockers: Windows + macOS build CI on the dev box is limited (no Windows / macOS host). Cross-compile Linux → Windows works (mingw-w64), but macOS must be built on a Mac.

## Goal

Make the host-agent run on Linux, Windows, and macOS from a single Rust codebase. Per-OS backends for capture, audio, input, and encoder; a shared `CaptureBackend` / `AudioBackend` / `InputBackend` / `EncoderBackend` trait abstracts the differences. Ship `.msi` and `.pkg` installers that work on first install.

## Research Summary

### Capture APIs per platform

| OS | Best capture | Fallback |
|----|--------------|----------|
| Linux | x11grab / x11rb (P1-7), or PipeWire | DRM/KMS direct (root) |
| Windows | Windows.Graphics.Capture (UWP API, Win32-callable since 1803) | DXGI Desktop Duplication (P1-7) |
| macOS | ScreenCaptureKit (12.3+) | CGDisplayStream (deprecated) |

- **Windows.Graphics.Capture** is the modern, recommended API. It can capture any display, window, or region. The user must grant the app a capability (one-time, UWP-style picker UI). Latency: 1-3 ms per frame, similar to DXGI. The `windows-capture` crate wraps it.
- **ScreenCaptureKit** on macOS 12.3+: the user must grant Screen Recording permission. Latency: 5-15 ms. The `screencapturekit` crate provides bindings.
- **x11grab / x11rb** on Linux: existing path. Latency: 1-3 ms.

For Windows, Windows.Graphics.Capture is preferred over DXGI Desktop Duplication for two reasons: (1) it doesn't require admin; (2) it can capture per-window, not just per-monitor. DXGI is the fallback for older Windows.

### Audio capture APIs per platform

- **All three**: `cpal` works on Linux (ALSA / PulseAudio / PipeWire), Windows (WASAPI), and macOS (CoreAudio). Single Rust API.
- **Per-OS tuning**: Linux may need PulseAudio null-source for privacy mode (P1-8); Windows may need WASAPI loopback; macOS uses CoreAudio aggregate devices for routing.

### Input injection APIs per platform

- **Linux**: enigo (mouse/keyboard) + uinput (gamepad, pen).
- **Windows**: enigo (uses `SendInput` for keyboard/mouse), `vigem-client` (gamepad), Wintab or Pointer Input (pen).
- **macOS**: enigo (uses `CGEventPost` for keyboard/mouse; limited), HID emulation for gamepad/pen is hard (kernel extension or DriverKit). **Defer macOS gamepad/pen to a follow-up.**

### Encoder per platform

The HW encode survey (P0-1) covers the per-OS encoders. ffmpeg (or ffmpeg-next) abstracts the differences; the runtime probe picks the best encoder for the codec + platform + GPU combination.

| OS | Best encoder family | Common options |
|----|--------------------|-----------------|
| Linux | VAAPI, NVENC, QSV | `h264_vaapi`, `h264_nvenc`, `h264_qsv` |
| Windows | NVENC, AMF, QSV | `h264_nvenc`, `h264_amf`, `h264_qsv` |
| macOS | VideoToolbox | `h264_videotoolbox`, `hevc_videotoolbox` |

### Per-OS Cargo features

```toml
[target.'cfg(target_os = "linux")'.dependencies]
x11rb = { version = "0.13", optional = true }
pipewire = { version = "0.6", optional = true }
cpal = "0.15"
evdev = "0.12"
enigo = "0.6"

[target.'cfg(target_os = "windows")'.dependencies]
windows = { version = "0.58", features = ["Win32_Graphics_Dxgi", "Win32_Graphics_Direct3D11", "Win32_UI_Input_KeyboardAndMouse", "Win32_System_Services"] }
windows-capture = "2"   # wraps Windows.Graphics.Capture
vigem-client = "0.3"
cpal = "0.15"
enigo = "0.6"

[target.'cfg(target_os = "macos")'.dependencies]
screencapturekit = "0.3"
objc2 = "0.5"
cpal = "0.15"
enigo = "0.6"
```

### Code structure

```rust
// apps/host-agent/src/capture/mod.rs
pub trait CaptureBackend {
    fn enumerate() -> Result<Vec<DisplayInfo>>;
    fn start(display: &DisplayInfo, prefs: &VideoStreamPreferences) -> Result<CaptureStream>;
    fn stop(&mut self);
}

#[cfg(target_os = "linux")] mod linux;
#[cfg(target_os = "windows")] mod windows;
#[cfg(target_os = "macos")] mod macos;

#[cfg(target_os = "linux")] pub use linux::PlatformCapture;
#[cfg(target_os = "windows")] pub use windows::PlatformCapture;
#[cfg(target_os = "macos")] pub use macos::PlatformCapture;
```

The same pattern for `audio/`, `input/`, and `encoder/`.

### Platform-specific behaviors

- **Permissions**:
  - macOS Screen Recording: per-app permission; system prompt on first use. The user can revoke in System Settings → Privacy & Security → Screen Recording.
  - Windows: ViGEmBus install requires admin (one-time). UAC prompt.
  - Linux: `/dev/uinput` requires group membership (udev rule).
- **DPI scaling**: Windows has per-monitor DPI scaling. The captured frame is in physical pixels; client must scale.
- **High refresh rate**: capture at the host's refresh rate (e.g. 240 Hz) if the encoder can keep up.
- **Multi-display** (P1-7): per-display capture via the per-OS `DisplayInfo` enumeration.
- **HDR** (P2-14): per-OS HDR capture; macOS 14+ and Windows 10 1709+ supported.
- **Audio routing** (P1-8 privacy mode): per-OS virtual audio device.

### Distribution

- **Linux**: `.deb` (Debian / Ubuntu), `.rpm` (Fedora / RHEL), AppImage (universal). Build via cargo-deb / cargo-rpm.
- **Windows**: `.msi` (WiX) or `.exe` (NSIS). Cross-compile from Linux via `x86_64-pc-windows-gnu` + mingw-w64. Sign the binary (P2-19).
- **macOS**: `.pkg` (Packages) or signed `.dmg`. Build on a Mac (cannot cross-compile from Linux). Sign + notarize (P2-19).

### Testing

- **Linux**: dev box (Xephyr 1024×768). CI: Docker + Xvfb.
- **Windows**: real hardware or VM. CI: GitHub Actions `windows-latest` runner (build + unit tests; HW encode requires a real GPU).
- **macOS**: real Mac (no VM for HW encoding). CI: GitHub Actions `macos-latest` runner (build + unit tests; HW encode requires a real Mac).

### Common cross-platform bugs

- **Endianness**: x86 is little-endian; the wire format must be explicit. Use `bytemuck::Pod` for safe casts.
- **Path separators**: use `std::path::PathBuf`, not string concatenation.
- **Line endings**: LF on Linux/macOS, CRLF on Windows. Use `serde` for the config files; not raw text.
- **Color space**: BGRA on Windows DXGI / WGC, RGBA on macOS SCK, BGRA on x11grab. Convert to a single internal format (RGBA) early.
- **Time zones**: store timestamps as UTC nanoseconds; convert for display.
- **UTF-16 vs UTF-8**: Windows APIs return UTF-16 device names; convert to UTF-8 for the wire.
- **File locking**: Windows files default to exclusive lock; Linux uses advisory locks. Plan for both.
- **Antivirus**: Windows may quarantine unsigned binaries. Sign them (P2-19).
- **macOS Gatekeeper**: unsigned binaries are blocked. Sign + notarize.

### Rust crate matrix (2024-2026)

- `x11rb` 0.13+ (Linux X11).
- `pipewire` 0.6+ (Linux Wayland).
- `cpal` 0.15+ (cross-platform audio).
- `evdev` 0.12+ (Linux uinput + raw input).
- `enigo` 0.6+ (cross-platform input).
- `windows` 0.58+ (Windows).
- `windows-capture` 2+ (Windows.Graphics.Capture wrapper).
- `vigem-client` 0.3+ (Windows virtual gamepad).
- `screencapturekit` 0.3+ (macOS).
- `objc2` 0.5+ (macOS Objective-C interop).
- `ffmpeg-next` 6+ (P0-3).

### 2024-2026 status

- **Windows.Graphics.Capture** is the recommended Windows capture API since 2019; it's stable and widely used.
- **ScreenCaptureKit** is mature on macOS 12.3+; Apple deprecated CGDisplayStream.
- **Linux Wayland** capture is improving but still platform-specific; X11 is the universal fallback.
- **Steam Deck** (Linux) is the dominant Linux gaming platform; validate on Arch / SteamOS.
- **Asahi Linux** on Apple Silicon is making progress for HW encode on M1/M2; not production-ready in 2024-2026 but worth tracking.
- **Cross-platform Rust game libraries** (ggez, macroquad, Fyrox) demonstrate that the pattern works.

## Implementation Plan

### Step 1: Refactor capture to a trait

`apps/host-agent/src/capture/mod.rs`:
- `pub trait CaptureBackend { ... }` as above.
- Per-OS modules.
- The session startup calls `PlatformCapture::enumerate()` and starts a capture per display.

### Step 2: Linux capture

`apps/host-agent/src/capture/linux.rs`:
- Existing x11rb path (P1-7) for the default.
- PipeWire for Wayland (deferred; X11 first).
- The udev rule for uinput (P0-6 gamepad, P2-15 pen).

### Step 3: Windows capture

`apps/host-agent/src/capture/windows.rs`:
- Use `windows-capture` crate (Windows.Graphics.Capture wrapper).
- The user picks a display or window via the system picker.
- Output: `ID3D11Texture2D` per frame; map to BGRA and copy to CPU.
- For HDR, the format is `R16G16B16A16_FLOAT`; convert scRGB → P010 (P2-14).
- DXGI Desktop Duplication as a fallback for Windows 10 1803 and earlier.

### Step 4: macOS capture

`apps/host-agent/src/capture/macos.rs`:
- Use `screencapturekit` crate.
- `SCShareableContent::current().await` to enumerate displays.
- `SCStreamConfiguration` with `width/height` in physical pixels, `pixelFormat = BGRA`, `colorSpaceName = sRGB` (or `extendedSRGB` for HDR on macOS 14+).
- The user must grant Screen Recording permission; if not granted, return a clear error.

### Step 5: Audio (cpal)

`apps/host-agent/src/audio/mod.rs`:
- `pub trait AudioBackend { ... }`.
- cpal handles the per-OS details.
- For privacy mode (P1-8), per-OS virtual audio device (PulseAudio null-sink, WASAPI loopback, CoreAudio aggregate).

### Step 6: Input (enigo + per-OS)

`apps/host-agent/src/input/mod.rs`:
- `pub trait InputBackend { ... }`.
- enigo for keyboard/mouse.
- uinput / ViGEmBus / Wintab for gamepad/pen.
- The existing `RemoteInputInjector` already supports per-OS via `cfg(target_os)`.

### Step 7: Encoder

`apps/host-agent/src/encoder/mod.rs`:
- `pub trait EncoderBackend { ... }`.
- ffmpeg (subprocess for v1, ffmpeg-next for P0-3) is the abstraction.
- The runtime probe in P0-1 picks the best encoder per codec + OS.

### Step 8: Build configuration

`Cargo.toml`:
- `target.'cfg(target_os = "linux")'.dependencies = [...]` etc.
- `features = ["default = ["linux-x11"]"`.
- Cross-compile via `.cargo/config.toml` (mingw-w64 for Windows; native for macOS).

### Step 9: CI

- Linux: existing (Docker + Xvfb).
- Windows: GitHub Actions `windows-latest`, build + unit tests, no HW encode.
- macOS: GitHub Actions `macos-latest`, build + unit tests, no HW encode.

### Step 10: Packaging

- Linux: `cargo deb` and `cargo rpm`. AppImage via `cargo appimage`.
- Windows: WiX or NSIS for the `.msi` / `.exe`. Cross-compile via mingw-w64. Sign with `signtool` (P2-19).
- macOS: `cargo bundle` for the `.app`, then `pkgbuild` for `.pkg` and `hdiutil` for `.dmg`. Sign + notarize (P2-19).

### Step 11: Tests

- Cross-platform: unit tests run on all three CI runners.
- Integration tests on Linux (dev box) and manual on Windows / macOS.
- HW encode / decode is tested manually on real hardware.

## Risks and Open Questions

- **Cross-compile macOS from Linux**: not possible. Mac binaries must be built on a Mac. Plan for a Mac CI runner.
- **macOS Gamepad / Pen**: deferred; document the limitation.
- **Code signing** (P2-19): the macOS binary must be signed + notarized; the Windows binary must be signed. Plan for the certificate cost and the CI integration.
- **Antivirus** on Windows: unsigned binaries may be quarantined. Signing + an EV certificate helps.
- **macOS Gatekeeper**: unsigned binaries prompt the user to "Open Anyway". Notarization removes the prompt.
- **Steam Deck specific paths**: validate on SteamOS (Arch-based). The defaults are usually fine but verify the udev rules.
- **Wayland** capture on Linux: the long-term path. X11 is the universal fallback for now.
- **Permissions on macOS**: if the user revokes Screen Recording permission, the host must detect and prompt.
- **Permissions on Windows**: if the user uninstalls ViGEmBus, gamepad input fails. The host must detect and report.
- **High refresh rate monitors** (240 Hz): capture is per-OS; verify the capture path doesn't clamp to 60.
- **Per-monitor DPI** on Windows: the captured frame is in physical pixels; the client must scale. The wire format should carry the DPI.
- **GPU selection** on hybrid systems (Intel iGPU + NVIDIA dGPU): the capture and encode must use the same GPU; the host must enumerate and pick.
- **Time zone in logs**: store UTC, display in the local TZ.

## References

- Windows.Graphics.Capture API: https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture
- Windows capture from Win32: https://blogs.windows.com/windowsdeveloper/2019/09/16/new-ways-to-do-screen-capture/
- Win32 Capture Sample: https://github.com/robmikh/Win32CaptureSample
- windows-capture crate: https://crates.io/crates/windows-capture
- ScreenCaptureKit (Apple): https://developer.apple.com/documentation/screencapturekit/
- WWDC 2022 ScreenCaptureKit: https://developer.apple.com/videos/play/wwdc2022/10156/
- WWDC 2023 ScreenCaptureKit: https://developer.apple.com/videos/play/wwdc2023/10136/
- screencapturekit crate: https://crates.io/crates/screencapturekit
- screencapturekit-go (Go reference): https://github.com/tfsoares/screencapturekit-go
- Recording to disk with SCK: https://nonstrict.eu/blog/2023/recording-to-disk-with-screencapturekit
- Windows.Graphics.Capture Event Log: https://winevt-kb.readthedocs.io/en/latest/sources/eventlog-providers/Provider-Microsoft-Windows-Graphics-Capture-Server.html
- Rust cross-platform patterns: ggez, macroquad, Fyrox engines
- Perplexity research, 2026-07-02: Windows.Graphics.Capture, ScreenCaptureKit, X11, cpal, ffmpeg, 2024-2026 status.
