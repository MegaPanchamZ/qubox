# P0-5: Frame Pacing (Vblank-Synchronized Present)

Status: **complete** (commits `5a3b5e3`, `6cacfc6`; PR https://github.com/MegaPanchamZ/qubox/pull/1). `FramePacer` (4/4 unit tests) is wired into the winit `run_video_window` via `ControlFlow::WaitUntil(now + target_interval)`. First frame is immediate; catch-up after a stall resets the deadline. Pacer stats (`presented` / `skipped` / `actual_fps` / `interval_jitter_ms`) are logged on loop exit. The wgpu Mailbox swapchain upgrade is a follow-up; the softbuffer path is unchanged.
Owner: `client-cli` (rendering layer), with a new `frame_pacing` module.
Depends on: winit 0.29 + wgpu 22+ (already in workspace), P0-2 (datagram media path), P0-3 (HW decode).
Blockers: none. All primitives are in the workspace; the work is integration.

## Goal

Replace the current minifb-style busy-loop and winit's `request_redraw`-in-a-tight-loop with a **vblank-synchronized frame pacing** loop using wgpu's `PresentMode::Mailbox` (with `Fifo` fallback). Drop late frames, present the freshest decoded frame at the next vblank, and reduce capture-to-display latency by 5-15 ms over the current behavior. Handle the first-frame-immediate case so the user sees a frame within ~50 ms of stream start.

## Research Summary

### wgpu PresentMode (the latency-vs-tear trade-off)

| PresentMode       | Queue length | Tearing? | Latency       | Game streaming fit?         |
|-------------------|--------------|----------|---------------|------------------------------|
| `Fifo`            | 1-2          | No       | 1-2 vblank    | OK fallback (always works)   |
| `FifoRelaxed`     | 1-2          | Maybe    | ~1 vblank     | Niche                        |
| `Mailbox`         | 1            | No       | ~0 vblank     | **Best — standard for Parsec/Moonlight** |
| `Immediate`       | 0            | Yes      | 0             | Tearing visible — no         |

**Choice: `Mailbox` if the backend supports it, else `Fifo`.** Mailbox keeps a single-slot swapchain: when the producer (our render thread) calls `present()`, the new frame replaces any pending-but-not-yet-scanned-out frame. This is the exact "drop late frames" semantics we want. The trade-off is that Mailbox is not guaranteed on every backend — `Fifo` is the only universally-supported mode in wgpu (per the docs). On D3D12, Vulkan, and Metal, Mailbox is always available. On GLES (Android), only Fifo is reliable.

### Per-platform vblank APIs (for refresh-rate detection)

The platform APIs are the source-of-truth for vblank timing; winit + wgpu use them internally but don't expose the raw timing to us. Use them for refresh-rate detection (so we know the vblank interval for the `ControlFlow::WaitUntil` deadline).

- **Windows (DXGI 1.3+)**: `IDXGIOutput::GetDesc` returns `RefreshRate` as `DXGI_RATIONAL`. The DXGI frame-latency waitable object (`DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT`) blocks until the swapchain is ready for another frame. `WaitForVBlank` is the older DXGI 1.0 API; works but lower precision.
- **Linux X11 (DRI3/Present)**: `DRI3_Present` extension provides `PresentPixmap` with vblank-aligned presentation. `drmWaitVBlank` (libdrm) is the fallback. EDID parsing gives the precise refresh rate.
- **Linux Wayland**: `wl_output.mode` event carries `refresh` (mHz). `wl_output.frame` is the per-vblank callback. `wp_presentation_time` is the high-precision timing protocol (kernel-feedback).
- **macOS**: `CVDisplayLinkGetNominalOutputVideoRefreshPeriod` returns a `CVTime`; `CVDisplayLinkSetOutputHandler` gives a per-vblank callback.

For wgpu on winit 0.29, the canonical pattern is:
- Use `ControlFlow::WaitUntil(deadline)` where `deadline = now + vblank_interval`.
- `vblank_interval = 1_000_000_000 / refresh_rate_hz` (ns).
- The render thread wakes at the deadline and presents the latest frame.

### winit 0.29 + wgpu 22 frame-pacing loop

```rust
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::event_loop::ControlFlow;
use winit::window::Window;

#[derive(Clone)]
struct DecodedFrame { pts: Instant, /* texture handle */ }

struct FramePacer {
    latest: Arc<Mutex<Option<DecodedFrame>>>,
    next_deadline: Instant,
    refresh: Duration,           // 16.67ms at 60Hz, 6.94ms at 144Hz
    first_frame: bool,
    in_stream: bool,             // true once we've received a frame
    refresh_hz: f64,
}

impl FramePacer {
    fn push_frame(&self, frame: DecodedFrame) {
        let mut slot = self.latest.lock().unwrap();
        *slot = Some(frame); // overwrite = drop late frame
    }

    fn take_latest(&self) -> Option<DecodedFrame> {
        self.latest.lock().unwrap().take()
    }
}

fn present_mode_for(caps: &wgpu::SurfaceCapabilities) -> wgpu::PresentMode {
    if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
        wgpu::PresentMode::Mailbox
    } else if caps.present_modes.contains(&wgpu::PresentMode::FifoRelaxed) {
        wgpu::PresentMode::FifoRelaxed
    } else {
        wgpu::PresentMode::Fifo
    }
}

let mut pacer = FramePacer {
    latest: Arc::new(Mutex::new(None)),
    next_deadline: Instant::now(),
    refresh: Duration::from_secs_f64(1.0 / 60.0),
    first_frame: true,
    in_stream: false,
    refresh_hz: 60.0,
};

event_loop.run(move |event, elwt| match event {
    Event::AboutToWait => {
        if pacer.in_stream {
            elwt.set_control_flow(ControlFlow::WaitUntil(pacer.next_deadline));
        } else {
            elwt.set_control_flow(ControlFlow::Wait);
        }
    }
    Event::WindowEvent { event: WindowEvent::RedrawRequested, .. } => {
        if let Some(frame) = pacer.take_latest() {
            // upload to texture, encode command buffer, present
            if pacer.first_frame {
                pacer.first_frame = false;
            }
        }
        pacer.next_deadline = Instant::now() + pacer.refresh;
    }
    _ => {}
}).unwrap();
```

The `wgpu::SurfaceConfiguration` should set `present_mode = Mailbox` and `desired_maximum_frame_latency = 1`.

### Drop late frames

The `Mutex<Option<DecodedFrame>>` slot is the canonical pattern:
- **Producer** (decoder thread): `slot = Some(new_frame)` overwrites whatever was there.
- **Consumer** (render thread at vblank): `slot.take()` returns the most recent frame; if `None`, present the previous frame's texture (don't show a black frame).
- **No queueing**: queue length is always 0 or 1. This is the "latest-only" semantics that Parsec, Moonlight, and Steam Remote Play all use.

### Queue length (Mailbox is queue length 1)

A longer queue (e.g. 2 frames) absorbs small jitter but adds 1 vblank of latency. Parsec and Moonlight both use queue length 1 because the jitter buffer (P0-2) already absorbs network jitter; the frame pacer's job is present timing, not jitter smoothing. Stay with queue length 1.

### Variable refresh rate (VRR / FreeSync / G-Sync)

VRR displays change refresh rate dynamically (1-240 Hz typically). Mailbox works correctly on VRR; the swapchain just adjusts its scanout timing. The render thread's `next_deadline = now + refresh` should use the *current* refresh rate, not a fixed one. Linux/Wayland: `wl_output.mode` events signal rate changes; Wayland's `wp_presentation_time` carries the actual present time. Windows: DXGI's `RefreshRate` updates when the display changes. macOS: `CVDisplayLink` callbacks fire on rate change.

If the display's refresh rate increases, the deadline shortens (good — the user gets more frames). If it decreases, the deadline lengthens (the render thread waits longer; not a problem because we have the most recent frame).

### Refresh-rate detection (vblank interval)

For the first release, hard-code the user's selected rate (`--max-fps 60` / `120` / `144`). Add platform-specific detection in a follow-up:
- **Linux/DRM**: `drmModeGetCrtc` → `mode.vrefresh` for the active CRTC. Library: `drm` crate.
- **Windows/DXGI**: `IDXGIOutput::GetDesc().RefreshRate` (a `DXGI_RATIONAL`). Library: `windows` crate's `dxgi` bindings.
- **Wayland**: `wl_output::Mode { refresh }` (in mHz). winit doesn't expose this directly; the user must select the rate.
- **macOS**: `CVDisplayLinkGetNominalOutputVideoRefreshPeriod` via `core-video` crate. Returns a `CVTime`; `seconds = time.value / time.timeScale`.

### First-frame immediate

The very first decoded frame should be presented as soon as it's decoded, not paced. After the first frame, switch to paced mode. Implementation: the decoder thread signals via `event_loop.event_loop_proxy().send_event(WinitUserEvent::FirstFrameReady)`; the render thread presents and starts the deadline loop.

### Latency budget

Capture-to-display for game streaming is <60 ms target. Frame pacing adds at most 1 vblank interval:
- 60 Hz: 16.7 ms
- 120 Hz: 8.3 ms
- 144 Hz: 6.9 ms
- 240 Hz: 4.2 ms

A 144 Hz display adds 6.9 ms of frame-pacing latency — half what 60 Hz adds. **VRR + 144 Hz is the lowest-latency combo**; that's why competitive streamers use 240 Hz monitors.

### Recent (2024-2026) notes

- **wgpu 22+ has stable `PresentMode::Mailbox`** on D3D12, Vulkan, and Metal. wgpu 23 (in development as of 2026) adds `desired_maximum_frame_latency` config for the Vulkan backend.
- **DXGI frame-latency waitable** is the gold standard for Windows; wgpu's DX12 backend uses it automatically when `desired_maximum_frame_latency > 0`.
- **Wayland presentation time feedback** (`wp_presentation_time`) lets the client measure actual present time, useful for the stats overlay (P1-12).
- **DirectX 12 Present Duration** (`IDXGISwapChain2::SetMaximumFrameLatency`) is exposed via wgpu's `desired_maximum_frame_latency`.
- **Linux Vulkan fence** (`VkFence` + `vkWaitForFences`) lets the render thread wait for the previous present to complete before queuing the next. This is what vkd3d-proton and DXVK use.

## Implementation Plan

### Step 1: Detect refresh rate

`apps/client-cli/src/frame_pacing/refresh.rs`:
- `pub fn detect_refresh_hz() -> f64` — returns the active display's refresh rate.
- Linux: try `drmModeGetCrtc` first, fall back to a hard-coded list (60, 120, 144, 240) and let the user pick.
- Windows: `IDXGIOutput::GetDesc().RefreshRate` (the `windows` crate's `dxgi` feature).
- macOS: `CVDisplayLinkGetNominalOutputVideoRefreshPeriod` (the `core-video` crate).
- Add the per-platform deps to `apps/client-cli/Cargo.toml` under the existing `frame-pacing` feature flag.

### Step 2: Frame slot and pacer

`apps/client-cli/src/frame_pacing/pacer.rs`:
- `pub struct FramePacer { slot: Arc<Mutex<Option<DecodedFrame>>>, ... }`.
- `pub fn push(&self, frame: DecodedFrame)`: writes to the slot, dropping the old.
- `pub fn take(&self) -> Option<DecodedFrame>`: consumes the slot.
- `pub fn schedule_next(&mut self, now: Instant)`: `next_deadline = now + vblank_interval`.

`apps/client-cli/src/frame_pacing/winit_loop.rs`:
- The winit event-loop integration: `ControlFlow::WaitUntil`, `WindowEvent::RedrawRequested`, `WinitUserEvent::FirstFrameReady`.

### Step 3: wgpu surface config

`apps/client-cli/src/decoder/render.rs` (new):
- `pub fn configure_surface(device: &wgpu::Device, surface: &wgpu::Surface, width: u32, height: u32, refresh_hz: f64) -> SurfaceConfiguration`.
- `let present_mode = present_mode_for(&surface.get_capabilities(&adapter));`.
- `let desired_maximum_frame_latency = 1;` (Mailbox is queue length 1).
- Pass to `surface.configure(device, &config)`.

### Step 4: Integrate with the winit loop

`apps/client-cli/src/main.rs` — `run_video_window`:
- Replace the existing `request_redraw`-in-a-loop logic with the paced loop.
- The decoder thread calls `pacer.push(frame)` after each decoded frame.
- The render thread consumes from `pacer.take()` at vblank.
- The first frame triggers `WinitUserEvent::FirstFrameReady` via the `EventLoopProxy`.

### Step 5: wgpu texture upload

The decoded frame (RGBA bytes from ffmpeg-next or subprocess) is uploaded to a `wgpu::Texture` with `queue.write_texture`. This is a synchronous GPU upload that takes ~0.5 ms for 1080p RGBA. The texture is recreated only on resolution change; same-resolution frames update the existing texture.

```rust
queue.write_texture(
    wgpu::ImageCopyTexture { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
    &frame.rgba_bytes,
    wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(width * 4), rows_per_image: Some(height) },
    wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
);
```

A render pass blits the texture to the swapchain image. ~0.3 ms for 1080p on a modern GPU.

### Step 6: First-frame immediate

`apps/client-cli/src/main.rs`:
- The decoder thread sends `WinitUserEvent::FirstFrameReady` after the first decoded frame.
- The render thread presents the first frame immediately (no `WaitUntil`); subsequent frames are paced.
- Track `frames_presented: u32`; switch to paced mode when `frames_presented > 0` and the deadline is set.

### Step 7: Stats surface

`apps/client-cli/src/frame_pacing/stats.rs`:
- `pub struct FramePacingStats { refresh_hz: f64, present_mode: wgpu::PresentMode, frames_presented: u32, frames_dropped: u32, present_lag_ms_avg: f64, present_lag_ms_p99: f64 }`.
- `present_lag_ms = now - frame.pts` measured at present time.
- Exposed via the stats overlay (P1-12).

### Step 8: Tests

- Unit test: `FramePacer` correctly drops a late frame when a new one arrives before the deadline.
- Integration test: run a synthetic 60 fps source (a thread that pushes a fake frame every 16.67 ms) and verify `frames_presented ≈ 60`, `frames_dropped ≈ 0` over 10 seconds on a 60 Hz display.
- Latency test: present_lag_ms_avg should be < 8 ms on a 144 Hz display, < 16 ms on a 60 Hz display, with a 60 fps source.
- Robustness test: 5 fps source on a 60 Hz display — frames_presented should be 5/s, frames_dropped should be 0 (the deadline loop waits for the next frame).

## Risks and Open Questions

- **Mailbox availability**: wgpu's GLES backend doesn't support Mailbox. On Linux with X11 + GLES (e.g. older Intel drivers), we fall back to Fifo. Fifo adds 1-2 vblank of latency (~16-33 ms); for 60 Hz this is a 16-33 ms tax that Parsec doesn't pay (Parsec's compositor is its own native window, not GLES). Mitigation: use wgpu's Vulkan backend on Linux; on macOS, Metal always supports Mailbox.
- **Compositor latency on Linux/Wayland**: the Wayland compositor is in the path; even with Mailbox at the swapchain level, the compositor may add 1 frame of latency (especially on GNOME, which buffers frames). KDE Plasma and Sway are compositor-latency-minimal. Document the per-DE behavior in the user manual.
- **DXGI frame-latency waitable**: wgpu's D3D12 backend uses it automatically when `desired_maximum_frame_latency > 0`. We're not opting in explicitly; verify the wgpu version we ship enables it.
- **VRR flicker on FPS swings**: if the source FPS varies wildly (e.g. 30 → 144 → 30), VRR displays can show a brief flicker on the rate transition. Parsec caps the source FPS to the display's max to avoid this. We should do the same — the encoder's `-r` is capped at the display's refresh rate.
- **Surface lost** (Android, mobile, alt-tab): wgpu's surface can be lost. We need to re-create the surface and re-upload the texture. winit's `WindowEvent::Resized` is the canonical signal.
- **Multi-display**: wgpu's `Surface` is bound to one window on one display. Multi-monitor (P1-7) is multiple windows, each with its own pacer.
- **softbuffer vs wgpu**: the current `client-cli` uses `softbuffer` (CPU blit). The frame-pacing work requires `wgpu` (GPU blit). This is a migration, not an addition. Decision: replace softbuffer with wgpu in this P0-5 work. softbuffer stays as the absolute fallback if wgpu init fails.
- **First-frame latency on stream start**: the first frame still has to traverse the entire pipeline (encoder → wire → decoder → upload → present). Expect 200-500 ms for the first frame to appear. This is the "warm-up" cost; the user expects it.
- **GLES-only platforms** (Android via WebTransport in P2-18): Mailbox is not available. Use Fifo. The latency cost (~16-33 ms) is acceptable on a 5G/4G network where the network latency is already 30-50 ms.

## References

- wgpu PresentMode docs: https://docs.rs/wgpu/latest/wgpu/enum.PresentMode.html
- wgpu SurfaceConfiguration: https://wgpu.rs/doc/wgpu/type.SurfaceConfiguration.html
- winit ControlFlow: https://docs.rs/winit/latest/winit/event_loop/enum.ControlFlow.html
- Vulkan PresentModeKHR spec: https://docs.vulkan.org/refpages/latest/refpages/source/VkPresentModeKHR.html
- DirectX 12 frame latency waitable object (Microsoft docs).
- Wayland wl_output frame callback: https://wayland.freedesktop.org/docs/html/apa.html#protocol-spec-wl_output
- Wayland wp_presentation_time protocol: https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/stable/presentation-time/presentation-time.xml
- Apple CVDisplayLink: https://developer.apple.com/documentation/corevideo/cvdisplaylink
- Reddit r/vulkan: real advantages of Fifo over Mailbox: https://www.reddit.com/r/vulkan/comments/1fnh0v1/real_advantages_of_fifo_over_mailbox/
- wgpu issue #2711: present mode discussion.
- glutin issue #1336: winit + vsync.
- Perplexity research, 2026-07-02: wgpu PresentMode, vblank APIs, frame-pacing loop.
