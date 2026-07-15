# P2-15: Pen / Tablet Streaming

Status: research complete, implementation pending.
Owner: `crates/qubox-proto` (wire format), `apps/client-cli` (capture), `apps/host-agent` (inject).
Depends on: P0-6 (gamepad pattern; same per-thread ownership, similar virtual device), the existing `RemoteInputEvent` (extended with `PenEvent`).
Blockers: Windows requires a virtual tablet driver (Wintab or Pointer Input injection); macOS has limited pen injection; Linux is straightforward via uinput.

## Goal

Stream the client's pen / tablet input (Wacom, N-trig, Apple Pencil, S Pen) to the host with pressure, tilt, hover, and tool type preserved. The host's drawing app (Krita, Photoshop, etc.) reads the virtual tablet as a real device. Use cases: digital artists streaming their workflow, remote drawing sessions, design reviews.

## Research Summary

### Linux: libinput + evdev + uinput

libinput is the standard Linux input library. Tablet support includes:
- **Pressure** (`ABS_PRESSURE`): 0-65535.
- **Tilt** (`ABS_TILT_X`, `ABS_TILT_Y`): -90 to 90 degrees.
- **Distance** (`ABS_DISTANCE`): hover distance; 0 = touching, larger = further away.
- **Tool type** (`BTN_TOOL_PEN`, `BTN_TOOL_RUBBER`, `BTN_TOOL_BRUSH`, `BTN_TOOL_AIRBRUSH`).
- **Side button** (`BTN_STYLUS`, `BTN_STYLUS2`).
- **Invert** / **rotation**: not common.

Rust crates:
- **`input`** (Smithay): user-friendly libinput bindings. https://github.com/Smithay/input.rs
- **`evdev`**: raw access to `/dev/input/event*`; can read tablet events directly.
- **`uinput`** (via the `evdev::uinput` module): create a virtual tablet device on the host.

Linux pen injection via uinput: declare `ABS_PRESSURE`, `ABS_TILT_X`, `ABS_TILT_Y`, `ABS_DISTANCE`, `BTN_TOOL_PEN`, `BTN_STYLUS`, `BTN_STYLUS2`, `BTN_TOUCH` (touch contact), `EV_KEY` and `EV_ABS` events. Krita / GIMP / MyPaint read from `/dev/input/event*` and see the virtual tablet.

### Windows: WinTab vs Pointer Input

Two APIs for tablet input on Windows:

- **WinTab** (legacy, since 1990s): the de-facto standard for drawing apps (Photoshop, Clip Studio Paint, Krita). Rich feature set: pressure, tilt, rotation, hover, tool type. Vendor SDK: https://www.wacom.com/en-us/support/developer-support.
- **Pointer Input** (Windows 8+): modern API; `WM_POINTERUPDATE` etc. with `POINTER_INFO` carrying pressure, tilt, contact rect. Cleaner API but less feature-rich than WinTab (e.g. rotation, distance may not be available).

Many pro art apps still support WinTab specifically for compatibility. For our purposes:
- **Capture (client)**: use Pointer Input (`WM_POINTER*`) — modern, easy to access via the `windows` crate.
- **Injection (host)**: use WinTab if the host app is WinTab-only; use Pointer Input otherwise. The host can detect which API the app uses.

Rust crate: `windows` / `windows-sys` for both. There's no high-level `wintab` crate; FFI to the Wintab DLL is needed for the host injection path.

### macOS: NSEvent tablet events

`NSEvent` provides tablet events:
- `NSEvent.pressure`: 0.0 to 1.0.
- `NSEvent.tiltX`, `NSEvent.tiltY`: -1.0 to 1.0 (normalized; multiply by 90 for degrees).
- `NSEvent.deviceID`: the tablet device.
- `NSEventType.tabletProximity`, `.tabletPoint`.

For third-party tablets (Wacom, etc.), NSEvent works. For Apple Pencil, it's iPad-only; the Mac side doesn't see Apple Pencil events unless the iPad is connected via Sidecar (which is a screen-share, not input share).

Rust crate: `objc2` + Cocoa bindings; or the `cocoa` crate (older).

For host injection: `CGEventCreateTabletEvent` exists but is limited (no tilt, no rotation). The robust path is to use a kernel extension (deprecated on macOS 11+) or a third-party driver (e.g. TabletDriver). **Mac pen injection is the hardest platform**; defer to a follow-up.

### Wire format

Compact per-pen event, sent on every change (typically 100-1000 Hz):

```rust
#[repr(C, packed)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct WirePenEvent {
    pub kind: u8,        // PenEventKind: Down=0, Move=1, Up=2, Hover=3, OutOfRange=4
    pub tool: u8,        // PenTool: Pen=0, Eraser=1, Brush=2, Airbrush=3
    pub x: f32,          // tablet X (0..1 or absolute; depends on the tablet)
    pub y: f32,          // tablet Y
    pub pressure: f32,   // 0.0 to 1.0
    pub tilt_x: f32,     // -1.0 to 1.0
    pub tilt_y: f32,     // -1.0 to 1.0
    pub distance: f32,   // hover distance (0.0 to 1.0, where 0.0 = touching)
    pub buttons: u8,     // side button mask
    pub sequence: u32,   // monotonic per-direction counter
}
```

36 bytes per event. At 1000 Hz active drawing, 36 KB/s. Send over QUIC datagrams (P0-2) or the existing reliable stream.

### Coordinate mapping

The client's tablet and the host's display may have different sizes / aspect ratios. Map the tablet's coordinate range to the host's display range:

```rust
fn map_coords(tablet_x: f32, tablet_y: f32, tablet_size: (f32, f32), host_size: (f32, f32)) -> (f32, f32) {
    // Normalize to 0..1, then scale to host.
    let nx = tablet_x / tablet_size.0;
    let ny = tablet_y / tablet_size.1;
    (nx * host_size.0, ny * host_size.1)
}
```

The mapping should be configurable (e.g. the client can choose to map the tablet to a specific display on the host, or to a specific window).

Pressure, tilt, distance, and buttons pass through unchanged.

### Host injection

- **Linux**: uinput with the right capabilities (see Linux section above). Per-thread ownership (same as enigo / gamepad). The host's drawing app reads the virtual tablet from `/dev/input/event*`.
- **Windows**: WinTab injection for WinTab-aware apps. Pointer Input injection (`InjectTouchInput`) for Pointer-aware apps. Detect which API the host app uses.
- **macOS**: deferred (limited support).

### Latency budget

| Stage | Latency |
|-------|---------|
| Capture (libinput/WinTab/NSEvent) | 1-3 ms |
| Wire (QUIC datagram) | 5-50 ms |
| Inject (uinput/WinTab) | 1-3 ms |
| **Total** | **7-56 ms** |

Within budget for drawing (200 Hz stylus is a typical refresh; the user won't notice 56 ms latency for a single stroke).

### Anti-cheat

Virtual tablet injection may trigger anti-cheat. Drawing apps (Krita, Photoshop) don't use anti-cheat, so this is mostly a non-issue. Drawing-focused workflows have a low risk of anti-cheat flags. Document the risk for any other use case (e.g. games with stylus input).

### Rust crate matrix (2024-2026)

- **`input`** (Smithay): libinput bindings. https://github.com/Smithay/input.rs
- **`evdev`**: raw input + uinput.
- **`uinput`** crate (or `evdev::uinput`).
- **`windows`** / **`windows-sys`**: Win32, WinTab (via FFI to wintab32.dll), Pointer Input.
- **`objc2`** / **`cocoa`**: macOS NSEvent.
- **`bytemuck`**: for the wire format.

### 2024-2026 status

- **libinput** has good tablet support; KDE and GNOME integrate it for stylus-aware apps.
- **WinTab** is still the standard for Windows pro art apps; Pointer Input is the modern alternative.
- **macOS** NSEvent has good tablet support for third-party tablets; Apple Pencil is iPad-only.
- **uinput on Linux** is the standard for virtual input devices; the kernel module is built-in.

## Implementation Plan

### Step 1: Wire format

`crates/qubox-proto/src/lib.rs`:
- Add `WirePenEvent` (36 bytes packed).
- Add to `RemoteInputEvent` as a new variant: `RemoteInputEvent::Pen(WirePenEvent)`.
- `ControlMsg` for hotplug (analogous to gamepad): `PenConnect`, `PenDisconnect`.

### Step 2: Client capture (Linux)

`apps/client-cli/src/input/pen_linux.rs` (new, behind `cfg(target_os = "linux")`):
- `pub struct PenCapture { libinput: input::Libinput }`.
- `pub fn enumerate() -> Result<Vec<PenInfo>>` — list pen-capable devices.
- `pub fn run(self, tx: tokio::sync::mpsc::Sender<WirePenEvent>)` — reads events from libinput, builds `WirePenEvent`, sends.

### Step 3: Client capture (Windows)

`apps/client-cli/src/input/pen_windows.rs` (new, behind `cfg(target_os = "windows")`):
- `pub struct PenCapture { /* Pointer Input */ }`.
- Subscribe to `WM_POINTERUPDATE` events; extract pressure, tilt, etc. from `POINTER_INFO`.
- Send over QUIC.

### Step 4: Client capture (macOS)

`apps/client-cli/src/input/pen_macos.rs` (new, behind `cfg(target_os = "macos")`):
- `pub struct PenCapture { /* NSEvent */ }`.
- Subscribe to `NSEventType.tabletPoint` events; extract pressure, tilt.

### Step 5: Host injection (Linux)

`apps/host-agent/src/input/pen_linux.rs` (new, behind `cfg(target_os = "linux")`):
- `pub struct VirtualPen { device: evdev::uinput::VirtualDevice, id: u8 }`.
- `pub fn new(id: u8, kind: PenTool) -> Result<Self>` — creates a uinput device with the right capabilities.
- `pub fn apply(&mut self, event: &WirePenEvent) -> Result<()>` — writes the events to uinput.

### Step 6: Host injection (Windows)

`apps/host-agent/src/input/pen_windows.rs` (new, behind `cfg(target_os = "windows")`):
- For WinTab apps: FFI to wintab32.dll; create a virtual context, send packets.
- For Pointer apps: `InjectTouchInput` with `POINTER_INPUT_TYPE::POINTER_INFO` carrying pressure, tilt.
- Detect the host app's preference via the WinTab API's `WTInfo` function.

### Step 7: Host injection (macOS — stub)

`apps/host-agent/src/input/pen_macos.rs`: stub that returns "not implemented".

### Step 8: Configuration

- `pen_enabled: bool` (default: false).
- `pen_device: Option<String>` (default: first pen-capable device).
- `pen_target_display: Option<u8>` (default: primary display; allows mapping the pen to a specific host display).
- `pen_aspect_ratio: PenAspect { Fit, Fill, Stretch }` (default: Fit).

### Step 9: Tests

- Unit test: `WirePenEvent` is `Copy` and 36 bytes.
- Integration test on Linux: a real or virtual pen, capture events, send to host, verify the host's `/dev/input/event*` shows the events.
- Latency test: total round-trip < 60 ms.

## Risks and Open Questions

- **macOS pen injection**: limited. CGEventCreateTabletEvent is restricted; no tilt/rotation. Defer to a follow-up; document the limitation.
- **WinTab FFI**: hand-written FFI to wintab32.dll is significant work. The `windows` crate doesn't have high-level WinTab bindings.
- **Multiple pen tools**: the same tablet can have pen, eraser, airbrush modes. The client captures the tool type; the host's virtual device is configured for the specific tool. Multiple virtual devices per pen (one per tool) may be needed.
- **Coordinate mapping**: the client tablet may be 16:10, the host display 16:9, or vice versa. Configurable mapping.
- **Pointer Input on Windows**: limited hover; tilt is supported but rotation is not. Drawing apps that need rotation fall back to WinTab.
- **Anti-cheat**: low risk for drawing apps; document for any other use.
- **High pen event rate**: 1000 Hz = 36 KB/s. Some pens (Wacom Intuos Pro) support 240 Hz or 360 Hz; some (Wacom Cintiq) up to 1000 Hz. Need to throttle or batch if the wire can't keep up.
- **uinput permissions**: `/dev/uinput` is root-only by default (same as gamepad). Same udev rule.
- **Linux kernel module signing** (Secure Boot): same as gamepad.

## References

- libinput tablet support: https://wayland.freedesktop.org/libinput/doc/latest/tablet-support.html
- libinput: https://www.mankier.com/1/libinput
- input.rs (Smithay): https://github.com/Smithay/input.rs
- input.rs docs: https://smithay.github.io/input.rs/
- input crate: https://crates.io/crates/input
- libinput man page: https://man.archlinux.org/man/libinput.4
- Wacom Developer SDK: https://www.wacom.com/en-us/support/developer-support
- WinTab vs Pointer Input: https://learn.microsoft.com/en-us/windows/dev-environment/rust/rust-for-windows
- Windows Pointer Input: WM_POINTERUPDATE, POINTER_INFO
- Apple NSEvent tablet: macOS developer docs
- evdev: https://docs.rs/evdev
- Perplexity research, 2026-07-02: libinput, WinTab, NSEvent, uinput tablet, 2024-2026 status.
