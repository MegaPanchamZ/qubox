# qubox-display

Unified display capture and virtualization API for Qubox.

## Purpose

This crate provides the platform abstraction layer for enumerating, capturing,
and managing displays across Windows, macOS, and Linux. It is the foundation
for multi-monitor capture (P1-7) and privacy mode (P1-8).

The design is documented in [ADR-007](../../research/decisions/ADR-007-unified-display-capture-and-virtualization.md).

## Traits

- **`CaptureBackend`** — Data-plane abstraction. Enumerate displays, open
  capture sessions, list backend capabilities. The `enumerate_displays()`
  method returns all physical and virtual displays visible to the OS, with
  full metadata (name, resolution, position, refresh rate, color space, HDR).

- **`CaptureSession`** — A single display's capture lifecycle. Produce frames
  via `next_frame(timeout)`, close the session, query the capture region and
  color space. Frames are delivered as `CapturedFrame` with BGRA8, NV12, or
  RGBA16F pixel data wrapped in `Arc<Vec<u8>>`.

- **`DisplayManager`** — Control-plane abstraction. Create/destroy virtual
  displays, set display state (Active, Privacy, Blanked), move windows
  between displays. Privacy mode transitions are orchestrated by the
  host-agent's `CaptureOrchestrator` (Phase B).

## Per-OS Backend Status

| Backend | Platform | Phase A | Phase B | Phase C |
|---------|----------|---------|---------|---------|
| X11RandrBackend | Linux (X11) | Full | Full | Full |
| DxgiBackend | Windows | Stub | Full | Deferred |
| ScreenCaptureKitBackend | macOS | Stub | Full | Deferred |
| PipeWirePortalBackend | Linux (Wayland) | Stub | Stub | Stub |

- **X11RandrBackend**: First-class backend. Uses `x11rb` with RandR for
  enumeration and `xproto::get_image(ZPixmap)` for capture.
- **DxgiBackend**: Compile-only stub in Phase A. Full DXGI Output Duplication
  implementation deferred to Phase B. Privacy mode requires IddCx or a dummy
  HDMI plug (deferred to Phase C).
- **ScreenCaptureKitBackend**: Compile-only stub in Phase A. Full SCK
  implementation deferred to Phase B. Virtual displays require
  CGVirtualDisplay entitlement (deferred).
- **PipeWirePortalBackend**: Compile-only stub in Phase A on Wayland. Full
  PipeWire stream implementation deferred to Phase B.

## Privacy Mode State Machine

```
Active → Privacy: Create virtual display, move game window, blank physical display
Privacy → Active: Unblank physical display, move window back
Active → Blanked: Display disconnected / GPU reset (observed by OS)
Privacy → Blanked: Virtual display disconnected, fallback to blank overlay
```

The `set_display_state(Privacy)` transition is best-effort: partial failures
(logged via `tracing::warn!`) do not block the overall transition.

## Coordinate Spaces

Four coordinate types bridge client input mapping to physical pixels:

- `PhysicalPixel` — raw framebuffer address
- `LogicalPixel` — DPI-scaled pixel (client coordinate space)
- `VirtualDesktopPoint` — OS virtual desktop union of all displays
- `CaptureRegion` — rectangle in a display's local coordinate space

See `src/coordinates.rs` for conversion functions and `display_at_point()`
for finding which display contains a virtual desktop point.

## Multi-Stream Pipeline Fan-Out (Phase B)

The `CaptureOrchestrator` (in the host-agent) manages one ffmpeg subprocess
per display. Each subprocess reads raw BGRA frames from a pipe and writes
encoded H.264 Annex B to stdout. The orchestrator owns the lifecycle of all
sessions and pipelines.

## Backward Compatibility

The existing single-stream path remains the default (`--single-stream`).
Multi-stream mode (`--multi-display`, `--all-displays`) is an opt-in flag
added in Phase B. The `WireAccessUnitHeader.stream_id = 0` identifies the
legacy single-stream path.

## License

Same as the Qubox project.
