# P1-12: Stats Overlay (egui + wgpu)

Status: research complete, implementation pending.
Owner: `apps/client-cli` (overlay rendering), with a new `stats_overlay` module.
Depends on: P0-5 (frame pacing; provides some stats), P0-3 (ffmpeg-next decoder stats), the QUIC `ConnectionStats` (already exposed by quinn).
Blockers: none. egui 0.30+ is mature and integrates cleanly with wgpu 22+ and winit 0.29.

## Goal

Add a real-time stats overlay that shows: FPS, bitrate, capture-to-display latency, packet loss, encoder info, decoder info, frame pacing stats, and network stats. Toggle with a hotkey (default Ctrl+Alt+S). Render budget: <0.5 ms/frame. Update frequency: 10-30 Hz (text doesn't need 144 Hz). The overlay is composited on top of the streaming video in the same wgpu swapchain.

## Research Summary

### egui (0.30+ as of 2024-2026)

`egui` is the de-facto Rust immediate-mode GUI library. It's used by Bevy's editor, Rerun, and many other Rust projects. 0.30 is the current major line.

- **Immediate-mode**: every frame, the UI code is called with the latest state; egui computes the layout and the draw commands. Easier than retained-mode (like Qt) for transient UIs.
- **Backend integration**: `egui-wgpu` (renders to a wgpu render pass) + `egui-winit` (handles winit events). Together they make a wgpu+winit+egui app ~200 lines of glue code.
- **Text rendering**: vendored `ab_glyph` (font rasterizer). Bundled default font (DejaVu Sans). Custom fonts are easy to load via `FontDefinitions`.
- **Accessibility**: 0.30+ adds screen reader support via the `accesskit` integration (optional).

### Two-pass rendering (video + overlay on the same swapchain)

The standard pattern: render the streaming video in pass 1, then render the egui overlay in pass 2. Both passes use the same swapchain texture as the color attachment. Pass 2 uses `LoadOp::Load` (don't clear) so the video frame is preserved; egui's translucent pixels are blended on top.

```rust
// Pass 1: video
let mut rp = encoder.begin_render_pass(&RenderPassDescriptor {
    color_attachments: &[Some(RenderPassColorAttachment {
        view: &view,
        ops: Operations { load: LoadOp::Clear(BLACK), store: true },
        ...
    })],
    ...
});
// draw video frame
drop(rp);

// Pass 2: egui overlay
egui_renderer.update_buffers(&device, &queue, &paint_jobs, &screen_desc);
egui_renderer.update_texture(&device, &queue, &full_output.textures_delta);

let mut rp = encoder.begin_render_pass(&RenderPassDescriptor {
    color_attachments: &[Some(RenderPassColorAttachment {
        view: &view,
        ops: Operations { load: LoadOp::Load, store: true },  // <-- key
        ...
    })],
    ...
});
egui_renderer.render(&mut rp, &paint_jobs, &screen_desc);
drop(rp);
```

A separate wgpu surface (a transparent overlay window) is more complex (layered windows, OS-level compositing) and not used by Parsec/Moonlight. The two-pass approach is the standard for game streaming clients.

### Frame budget

egui's render cost depends on the UI complexity. For our stats overlay (a few lines of text in a small panel):
- Tessellation: ~0.1-0.2 ms.
- Texture update (no animation, no new textures): ~0.05 ms.
- Draw: ~0.1-0.2 ms.
- **Total: <0.5 ms per frame**, well within budget.

We update the stats at 10-30 Hz (text doesn't change faster than the eye can read), but render the overlay at the display's refresh rate (60-144 Hz) so the UI is responsive to toggles.

### Data sources (`StreamingStats`)

```rust
pub struct StreamingStats {
    // Network
    pub rtt_ms: f32,            // from quinn ConnectionStats
    pub bandwidth_mbps: f32,    // from rate controller
    pub lost_datagrams: u64,    // from quinn stats (lost)
    pub sent_datagrams: u64,    // from quinn stats (sent)
    pub congestion_window: u32, // from quinn stats

    // Video
    pub bitrate_bps: u32,       // from host's RateFeedback
    pub fps: f32,               // from host's RateFeedback
    pub width: u32, pub height: u32,
    pub codec: VideoCodec,

    // Decoder
    pub decoded_frames: u64,    // from ffmpeg-next
    pub dropped_frames: u64,    // from ffmpeg-next
    pub decode_ms: f32,         // per-frame decode time

    // Frame pacing
    pub present_lag_ms: f32,    // from P0-5 stats
    pub frames_dropped: u64,    // from P0-5 stats
    pub present_mode: String,   // "Mailbox" / "Fifo"

    // Display
    pub display_hz: f32,
    pub display_resolution: (u32, u32),
    pub hdr: bool,
}
```

The `StreamingStats` is a snapshot; the data is sampled at 1-10 Hz and stored. The overlay reads the latest snapshot.

### Visualization (Parsec/Moonlight design)

- **Top-left corner**: a translucent dark panel with the key stats (FPS, bitrate, latency, loss, codec, resolution).
- **Top-right corner**: encoder info (host, target bitrate, current bitrate, codec).
- **Bottom-left corner**: network stats (RTT, loss %, bandwidth, congestion window).
- **Bottom-right corner**: a sparkline of the last 60 seconds of latency (decode + present lag).
- **Toggle**: Ctrl+Alt+S (or whatever the user configures).
- **Opacity**: adjustable (default 80%).

The layout is a `egui::Window::new("Streaming Stats").resizable(false).fixed_pos(Pos2::new(16.0, 16.0))`.

### Hotkey

Capture `Ctrl+Alt+S` in the winit event loop. Toggle `overlay_visible: bool`. **Do not** forward the keystroke to the host (the user doesn't want the host to receive "Ctrl+Alt+S" as a remote key event).

```rust
if let WindowEvent::KeyboardInput { event: KeyEvent { physical_key: PhysicalKey::Code(KeyCode::KeyS), state: ElementState::Pressed, .. }, .. } = &event {
    let mods = elwt.window(&window).unwrap().modifiers();
    if mods.state().ctrl() && mods.state().alt() {
        overlay.visible = !overlay.visible;
    }
    return;  // don't forward to the host
}
```

### Update frequency

- **Stats data**: sampled at 1 Hz from the network/decoder.
- **Display update**: every frame (60-144 Hz) with the latest stats snapshot. The numbers change slowly enough that this is fine.
- **Refresh interval**: the egui run is called every frame. To save CPU, we can skip egui when `overlay.visible = false`, but wgpu still needs a clear pass to keep the swapchain from going stale.

### Per-platform fonts

- **Default**: egui's bundled font (DejaVu Sans) works everywhere.
- **Monospace** (recommended for stats): JetBrains Mono, Roboto Mono, or system monospace. Load via `FontDefinitions` and `include_bytes!`.
- **CJK** (Chinese/Japanese/Korean characters): if the host or client names contain CJK, ensure the font covers them. egui's default font covers Latin only; for CJK, load `NotoSansCJK-Regular.ttf` (~30 MB, or use NotoSansSC for simplified Chinese only, ~5 MB).

### Rust crate matrix (2024-2026)

- `egui` 0.30+ (the core UI)
- `egui-wgpu` 0.30+ (wgpu renderer)
- `egui-winit` 0.30+ (winit event integration)
- `ab_glyph` 0.2+ (font rasterizer; vendored with egui)
- `epaint` 0.30+ (vendored with egui)
- `quinn` 0.10 (already in workspace, for ConnectionStats)
- `ffmpeg-next` 0.6+ (P0-3, for decoder stats)

### 2024-2026 status

- **egui 0.30+** is the current major. Big features: accessibility (screen reader), better text shaping, multi-viewport support.
- **egui-wgpu** is the recommended backend for native apps. egui_glow (OpenGL) is for OpenGL ES only.
- **wgpu 22+** is stable; the wgpu integration with egui is solid.
- **AccessKit** is the screen-reader integration; opt-in via a feature flag.

## Implementation Plan

### Step 1: Stats aggregation

`apps/client-cli/src/stats/mod.rs`:
- `pub struct StreamingStats { ... }` (the struct above).
- `pub struct StatsAggregator { network: NetworkStats, decoder: DecoderStats, pacing: PacingStats, display: DisplayStats }`.
- Sampled at 1 Hz from each source; the overlay reads the latest snapshot.

### Step 2: egui integration

`apps/client-cli/src/stats_overlay/egui_state.rs`:
- `pub struct OverlayState { visible: bool, opacity: f32, font: FontArc, position: (f32, f32), show_advanced: bool, stats: StreamingStats }`.
- `pub fn new(window: &Window, device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Self`.
- `pub fn handle_event(&mut self, event: &WindowEvent) -> bool` — returns true if the event was consumed by the overlay (so the host doesn't see it).
- `pub fn render(&mut self, encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, device: &wgpu::Device, queue: &wgpu::Queue, screen_desc: ScreenDescriptor) -> Result<()>`.

### Step 3: Hotkey

In `apps/client-cli/src/main.rs`:
- `if ctrl && alt && key == KeyS && state == Pressed { overlay.toggle(); return; }`.
- Don't process the key as a remote input event.

### Step 4: Render pass integration

In `apps/client-cli/src/main.rs` `run_video_window`:
- After the video render pass, call `egui_renderer.render()` in a second pass with `LoadOp::Load`.
- The egui renderer's `update_buffers` and `update_texture` are called before the render.

### Step 5: Stats sampling

- A tokio task that polls each stats source every 1 second and updates `StreamingStats`.
- For quinn: `connection.stats()` returns `ConnectionStats` (RTT, sent, lost, congestion window).
- For the decoder (P0-3): `decoder.stats()` returns decoded frames, dropped frames, decode time.
- For the frame pacer (P0-5): `pacer.stats()` returns present_lag, drops, present mode.
- For the display: `wgpu::Surface::get_capabilities` for the surface format, OS API for refresh rate.

### Step 6: CLI flag

- `--stats-overlay` to enable.
- `--stats-overlay-hotkey <key>` to customize the toggle.

### Step 7: Tests

- Unit test: `StreamingStats` serde round-trip.
- Unit test: egui render produces paint jobs without crashing.
- Manual: streaming session, toggle the overlay, verify all stats are accurate.

## Risks and Open Questions

- **Render cost on low-end GPUs**: <0.5 ms is fine on modern integrated GPUs; on a Raspberry Pi 4 with a software-rendered wgpu, egui's tessellation could be 2-3 ms. Test on low-end.
- **Per-frame overlay render**: the user may have a 240 Hz display. Rendering egui at 240 Hz is wasteful when the stats update at 1 Hz. Decouple: render the overlay at 30 Hz, but ensure the swapchain is still updated at the display's refresh rate.
- **Custom font size**: egui's `pixels_per_point` may not match the OS DPI on all platforms. Test on Windows with non-100% DPI scaling.
- **CJK fonts**: not in the bundled font. Ship a separate font file (e.g. NotoSansSC) and load it on demand. The user's setup dictates which CJK font is needed.
- **Stat accuracy**: the stats are sampled at 1 Hz. A 1-second spike in latency is visible in the overlay only after a delay. For real-time accuracy, sample at 10 Hz and EMA.
- **Privacy**: the overlay shows the host's IP (in the network stats) and the bandwidth. Some users may not want this visible in screenshots. Add a `--stats-overlay-privacy` mode that omits the IP.
- **Multiple windows** (P1-7 multi-monitor): each window has its own overlay; the user can toggle them independently or globally. The hotkey toggles all.
- **Network stats from quinn**: `ConnectionStats` exposes `path.rtt`, `udp_tx.datagrams`, `udp_rx.datagrams`, `frame_tx.lost`, etc. Check the quinn 0.10 docs for the exact field names.
- **Decoder stats from ffmpeg-next**: `AVCodecContext` has frame counts in the `decoded_frames` field (via ffmpeg 6.0+). For older versions, count manually in the decode loop.

## References

- egui GitHub: https://github.com/emilk/egui
- egui-wgpu docs: https://docs.rs/egui-wgpu
- egui-winit docs: https://github.com/emilk/egui/discussions/3067
- egui implementations for wgpu and winit: https://www.reddit.com/r/VegasPro/comments/e0cdh0/is_twopass_rendering_worth_it_for_youtube/
- wgpu example with egui: https://github.com/matthewjberger/wgpu-example
- wgpu render pipelines: https://whoisryosuke.com/blog/2022/render-pipelines-in-wgpu-and-rust/
- StackOverflow: one render pass on top of another: https://stackoverflow.com/questions/63783388/using-one-render-pass-on-top-of-another
- Perplexity research, 2026-07-02: egui 0.30+ integration, two-pass rendering, hotkey, 2024-2026 status.
