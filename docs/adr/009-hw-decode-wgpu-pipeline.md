# ADR-009 In-Process Hardware Decoding and Zero-Copy Graphics Pipeline

## Status

Proposed. Branch: `feature/adr-009-hw-decode-wgpu`. Based on `main` at commit `4f45658` (Phase E Tauri GUI production launcher merged). Builds on ADR-003 (`research/decisions/ADR-003-ffmpeg-next-decoder.md`) which established `ffmpeg-next` as the in-process decoder substrate; this ADR closes the loop by delivering the actual HW-acceleration wiring and the wgpu-based zero-copy presentation path that ADR-003 deferred. Substrate ADRs:

- ADR-003 — chose `ffmpeg-next` and the `decoder_hw.rs` scaffold (still present at `apps/client-cli/src/decoder_hw.rs:1-85` returning `Err` until this ADR lands).
- ADR-005 — established the daemon + relay architecture that the client sits behind.
- ADR-008 — Phase A wiring for clipboard/mic that we must not regress.

The current P0-3 scaffold in `apps/client-cli/src/decoder_hw.rs:41-85` is `#[cfg(feature = "hw-decode")]` gated and **always returns `Err`** (`:71-77`), forcing every caller into the ffmpeg subprocess path (`:233-322` of `main.rs`). The P0-3 build script (`apps/client-cli/build.rs:29-55`) already detects `libclang` for `bindgen` and emits a `cargo:warning=` when the toolchain is missing. This ADR is the production cutover that turns the scaffold into the real implementation, pairs it with `wgpu`, and retires the subprocess decoder from desktop builds.

P0-5 frame pacing (`apps/client-cli/src/frame_pacing.rs:1-193`) and the P1-12 stats overlay (`apps/client-cli/src/stats_overlay.rs:1-...`) both currently mutate the BGRA frame buffer in CPU memory and feed minifb. Both paths migrate to wgpu.

Honors all eight project rules (see `research/decisions/ADR-008-clipboard-mic-sync.md:18` for the rule list). No `unsafe` in any new code. No `//` comments inside function bodies — only `//!` and `///`.

## Context

The current desktop client decode path is documented in ADR-003 §"Context". With the December 2025 / January 2026 substrate additions the precise call graph is:

```
apps/client-cli/src/main.rs:1241-1242   let decoder = RunningFrameDecoder::spawn(&video_config, encoded_rx, decoded_tx)?;
apps/client-cli/src/main.rs:233-322      RunningFrameDecoder::spawn(...)         # spawn ffmpeg subprocess + 3 std::thread I/O loops
apps/client-cli/src/main.rs:2025-2055    decoder_reader_loop(stdout, w, h, ...)   # reads BGRA from ffmpeg stdout
apps/client-cli/src/main.rs:2070-2079    bgra_to_window_frame(bgra) -> Vec<u32>   # BGRA → u32 ARGB chunk-exact map
apps/client-cli/src/main.rs:1288-1305    run_video_window(...) OR run_tiled_view(...)  # drains decoded_rx via try_recv
apps/client-cli/src/main.rs:1732-1734    window.update_with_buffer(&frame, w, h)  # CPU-side paint to minifb
```

Three structural costs on the desktop hot path:

1. **Process-boundary latency.** 1080p BGRA at 60 Hz moves ≈ 0.5 GB/s through two pipe buffers (stdin commands, stdout BGRA) inside the kernel. A round-trip wake of the ffmpeg child process shows up as a 4–8 ms jitter spike on the very first frame of every GOP.
2. **CPU copy on the present step.** `bgra_to_window_frame` (`:2070-2079`) copies `W*H*4` bytes CPU-side, then `minifb::Window::update_with_buffer` (`:1732-1734`) copies them again into the softbuffer. The stats overlay (`stats_overlay.rs:1-...`) and privacy indicator (`privacy_indicator.rs`) further mutate the BGRA frame CPU-side each tick.
3. **Zero GPU usage on the client.** Every pixel passes through the CPU memory hierarchy before hitting the display. HW-accelerated decode surfaces (VAAPI on Linux, D3D11VA on Windows, VideoToolbox on macOS) are unreachable from the subprocess path without per-codec flag juggling (`-hwaccel vaapi`, `-c:v h264_qsv`, etc.) which is exactly the brittleness flagged in ADR-003 §"Context" item 2.

ADR-003 declared the destination (in-process `ffmpeg-next` decoder). The remaining work is the actual HW wiring plus the zero-copy GPU presentation pipeline that justifies owning the decoder in the first place. That is the substantive scope of this ADR.

Build-time substrate already verified at `4f45658`:

- `libclang-18-dev` (already on the box).
- `libavcodec-dev` / `libavformat-dev` / `libavutil-dev` / `libswscale-dev` (already on the box).
- `ffmpeg-next = { version = "8.1", optional = true }` declared in `apps/client-cli/Cargo.toml:37` and the `hw-decode = ["dep:ffmpeg-next"]` feature at `:39-40`.
- `winit = "0.29"`, `minifb = "0.27"`, `softbuffer = "0.4"`, `raw-window-handle = "0.6"` already in the workspace dependency set (`Cargo.toml:36-45`). The scaffold imports `winit::event_loop::EventLoopProxy` at `decoder_hw.rs:49` even though **no `winit::EventLoop` is currently instantiated anywhere** (`grep` against `apps/client-cli/src/` shows only doc-comment references in `frame_pacing.rs:27,109` and `stats_overlay.rs:20`).
- 236 tests green; clean `cargo check --workspace --exclude client-gui`.

Constraints that shape the design:

- Project rule #3: **only one `winit::EventLoop` per process**. The blank overlay window currently lives in `apps/client-cli/src/blank_overlay.rs:50-100` as a separate minifb `Window` outside any winit loop. The migration must collapse both windows into one winit `ApplicationHandler` rather than introducing a second loop.
- Project rule #5: `client_cli::start_session` re-export at `apps/client-cli/src/lib.rs:9-11` must keep compiling. `decoder_hw.rs` is feature-gated (`#![cfg(feature = "hw-decode")]` at `decoder_hw.rs:41`), so the public lib surface is unchanged whether or not the HW feature is on.
- Project rule #6: no sudo. `bindgen` (via `ffmpeg-next`) and `pkg-config` are the only system-touching install steps; both are already accommodated by the existing `build.rs`.
- Project rule #7: `--datagram-media` default is on (out of scope here, but no wire-format changes are introduced so this is unaffected).
- Stats overlay text rendering needs a GPU glyph atlas when we move it off-CPU. `stats_overlay.rs:20` already calls this out: "The long-term plan is to replace minifb with a winit + wgpu surface ..." — this ADR is the long-term plan.

## Decision

### 1. Wire format changes

**No wire-format changes.** This is a client-side rendering cutover. Inspecting the substrate:

- `crates/qubox-proto/src/lib.rs:470-476` defines `VideoStreamParams { codec, width, height, framerate }`. All four fields are already present and sufficient.
- `crates/qubox-proto/src/lib.rs:90-96` defines `VideoCodec { H264, H265, Av1 }`. The ffmpeg-next codec names match exactly (`h264`, `hevc`, `av1`) — see the existing dual helpers at `:108-123` (`ffmpeg_demux_format` and `ffmpeg_mux_format`).
- `crates/qubox-transport/src/media/mod.rs:38-89` (14-byte wire header) is untouched. The encoded annex-b bytes that pass through `read_access_unit` are exactly what `avcodec_send_packet` accepts.

Project rule #2 (every new proto field MUST have `#[serde(default)]`) is moot here: zero new fields. Every existing host in the wild remains compatible; every existing client remains compatible. New clients that build `--features hw-decode` on a box with no GPU drivers transparently fall back to the in-process software decode path, also wire-compatible.

### 2. Module structure

The current `apps/client-cli/src/main.rs` is 2 253 lines with `run_video_window` at `:1632-1764` (132 lines) and `run_tiled_view` at `:1768-1903` (135 lines) doing both the dispatcher work and the renderer-specific work. The minifb paint path lives inline in those bodies (`:1732-1734`, `:1860-1872`). This ADR splits the file along the renderer boundary.

#### Final layout

```
apps/client-cli/src/
  main.rs                       (slimmer — CLI parse + command dispatch + glue)
  decoder_hw.rs                 (rewrite — real impl, see §3; cargo feature hw-decode)
  render_wgpu.rs                (new  — wgpu device/surface/queue/pipelines; see §4)
  render_minifb.rs              (new  — current minifb paint path, lifted verbatim;
                                     kept as the no-GPU fallback; see §3 §6)
  run_video_window.rs           (new  — dispatcher: picks renderer via --renderer flag,
                                     shared input pump, overlay, hotkeys, stats)
  run_tiled_view.rs             (new  — same split for tiled view; identical dispatcher)
  frame_pipeline.rs             (new  — shared DecodedFrame type (raw bytes +
                                     width/height/instant) crossing the decoder →
                                     renderer boundary; same channel layout)
  blank_overlay.rs              (rewrite — winit subwindow in same ApplicationHandler;
                                     no minifb; honors project rule #3)
  stats_overlay.rs              (modify — wgpu_glyph text paths for overlay;
                                     retain CPU path for minifb renderer)
  privacy_indicator.rs          (modify — wgpu shader on tile, CPU bit-blit on minifb)
  tiled_view.rs                 (modify — converts from minifb TiledView to wgpu TiledView)
  runtime.rs                    (unchanged — its RunningFrameDecoder / RunningAudioPlayback
                                     continue to exist; the old fork is now code-paths for
                                     the subprocess fallback only)
```

#### Per-crate `mod` declarations (apps/client-cli/src/lib.rs)

Add the four new modules:

```rust
pub mod blank_overlay;
pub mod decoder_hw;            // already gated via #![cfg(...)] inside the file
pub mod frame_pipeline;
pub mod privacy_indicator;
pub mod render_minifb;
pub mod render_wgpu;
pub mod runtime;
pub mod run_tiled_view;
pub mod run_video_window;
pub mod stats_overlay;
pub mod stream_registry;
pub mod telemetry;
pub mod tiled_view;
```

(The existing `lib.rs:1-11` re-exports `start_session` and friends; that re-export is preserved untouched per project rule #5.)

#### Keep minifb as a fallback? — Yes, gated

Rationale: wgpu has zero useful behavior on a headless box and noisy behavior on drivers that crash during adapter enumeration. The `--renderer=minifb` flag is preserved behind `#[cfg(not(target_os = "..."))]` is wrong; instead we keep it as a runtime selection. CI uses `--renderer=minifb --decoder=subprocess`, dev boxes use `--renderer=wgpu --decoder=hw`, and a user whose GPU driver wedged during a session can recover with `--renderer=minifb --decoder=sw` without rebooting.

The minifb path in `render_minifb.rs` is **not** considered deprecated; it is the safety net.

### 3. Hardware decoder architecture

#### 3.1 Public surface

The current scaffold at `decoder_hw.rs:55-85` is replaced by:

```rust
//! P0-3 hardware-accelerated in-process decoder (production).

pub struct RunningHwFrameDecoder {
    decoder_thread: JoinHandle<anyhow::Result<()>>,
    cancel: Arc<AtomicBool>,
}

pub struct HwDecoderConfig {
    pub video: VideoStreamParams,
    /// Preferred backend order — first one whose device opens wins.
    pub preferred: &'static [AVHWDeviceType],
    /// Size of the `crossbeam_channel::bounded` between decoder and renderer.
    pub decoded_queue_depth: usize,
    /// When true, transfer RGBA via `av_hwframe_transfer_data`; when false,
    /// request BGRA from the sw frame via `libswscale`.
    pub output_bgra: bool,
}

impl RunningHwFrameDecoder {
    pub fn spawn(
        cfg: HwDecoderConfig,
        encoded_rx: Receiver<DecodedFrame::Encoded>, // see §3.2
        decoded_tx: Sender<DecodedFrame>,             // cross-thread
        stats: Arc<StatsCollector>,
    ) -> anyhow::Result<Self>;

    /// Cooperative shutdown. Sets the cancel flag, drops `encoded_rx`,
    /// and joins the decoder thread (with a 250 ms grace before
    /// `kill`-equivalent on the underlying codec).
    pub fn shutdown(self) -> anyhow::Result<()>;
}
```

Why the signature changes vs. the scaffold: the scaffold at `decoder_hw.rs:68` takes a `tokio_mpsc::UnboundedSender<Vec<u32>>` and an `EventLoopProxy<WinitUserEvent>`. We discard both:

- **`tokio_mpsc::UnboundedSender<Vec<u32>>`** → replaced with a `crossbeam_channel::bounded(N)` (`N=2`) carrying `DecodedFrame`. The renderer is the only consumer and the channel is drained on the main thread inside the winit `RedrawRequested` handler, where tokio isn't running (the binary is `#[tokio::main]` but the event loop on a winit application is the *real* main loop). See §5 for the threading model.
- **`EventLoopProxy<WinitUserEvent>`** → replaced with cross-thread waker that calls `proxy.send_event(WinitUserEvent::WakeUp)` from the decoder thread. `WinitUserEvent` is a small `enum` introduced in §4.3. This is the only reason the type survives the rewrite.

#### 3.2 Shared transport type — `frame_pipeline.rs`

```rust
//! Cross-thread frame carrier between `RunningHwFrameDecoder`
//! and either `render_wgpu` or `render_minifb`.

#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub bytes_per_row: u32,
    /// One of `wgpu::TextureFormat`-compatible pixel layouts.
    pub pixel_format: PixelFormat,
    /// `Cow<'static, [u8]>` for HW-decoded frames that were already
    /// GPU-resident (zero-copy future); `Vec<u8>` for SW transfers.
    pub data: PixelData,
    pub captured_at: Instant,
}

pub enum PixelFormat {
    Bgra8Unorm,
    Rgba8Unorm,
    /// Used by `encode_to_software` future — Phase 3 HDR work.
    Rgba16Float,
}

pub enum PixelData {
    Owned(Vec<u8>),
    GpuHandle(/* opaque */),
}
```

For the desktop cutover we only instantiate `PixelData::Owned(Vec<u8>)`. The `GpuHandle` variant documents the zero-copy direction without forcing the work today.

#### 3.3 Platform HW device selection

Replaces the scaffold's enumeration (in `decoder_hw.rs:21-23`) with a real probe. Implemented in a private `fn pick_hw_device_type(cfg: &HwDecoderConfig) -> Option<AVHWDeviceType>` that walks `cfg.preferred` and tries `av_hwdevice_ctx_create` for each.

| Platform | `cfg.preferred` (default) | Failure fallback |
|----------|---------------------------|------------------|
| Linux (X11 + Wayland) | `[VAAPI, CUDA, VDPAU]` | SW |
| Linux (headless, no `/dev/dri/renderD128`) | `[CUDA, VAAPI]` then SW | SW |
| Windows 10/11 | `[D3D11VA, CUDA, DXVA2]` | SW |
| macOS 12+ | `[VIDEOTOOLBOX]` | SW |

The probe also opportunistically re-orders based on `av_hwdevice_iter_registered()` to skip types not compiled into the system's libavcodec. `nvidia` drivers are not always present on Linux CI boxes; the preference list already lets the SW path catch them.

#### 3.4 Initialization sequence

Inside `RunningHwFrameDecoder::spawn` on the decoder thread:

1. `let codec = avcodec_find_decoder_by_name(codec_name(cfg.video.codec))` — picks the explicit HW-aware decoder (`h264`, `hevc`, `libdav1d`) rather than letting ffmpeg auto-negotiate.
2. Walk `avcodec_get_hw_config(codec, idx)` until one matches the chosen `AVHWDeviceType`; abort on failure.
3. Allocate `AVCodecContext`; set `get_format` to our callback (see §3.5).
4. **Eagerly** create the `AVHWDeviceContext` via `av_hwdevice_ctx_create(...)` with the platform's default device (`/dev/dri/renderD128` on Linux, `0` on Windows, `0` on macOS). Eager creation surfaces "no GPU driver" at session-start time instead of during the first decoded frame.
5. Allocate `AVHWFramesContext` with `initial_pool_size = max_num_ref_frames + 2` (typical: 8 for H.264, 16 for B-heavy HEVC).
6. `avcodec_open2` with `flags |= AV_CODEC_FLAG_LOW_DELAY` and a single `tune=zerolatency` `AVDictionary` entry (mirrors the subprocess CLI `flags=low_delay`).
7. Loop: `recv` on `encoded_rx` → `av_packet_make_writable` → `avcodec_send_packet` → drain `avcodec_receive_frame` until `EAGAIN` → transfer/convert → send on `decoded_tx` → recycle the temporary `sw_frame`.
8. On `cfg.cancel` set, exit the loop, `avcodec_free_context`, join.

#### 3.5 `get_format` callback

Mirrors the design already documented at `decoder_hw.rs:24-32` but with the scaffold's stubs filled in:

```rust
unsafe extern "C" fn hw_get_format(
    ctx: *mut AVCodecContext,
    fmt: *const AVPixelFormat,
) -> AVPixelFormat {
    // Walk `fmt` until we find the pixel format whose
    // `AVPixFmtDescriptor.componenteplane + hw_device_ctx.hw_type`
    // matches the precomputed preferred type. Then attach the
    // shared `AVHWFramesContext` from the codec context's
    // opaque user data.
    AV_PIX_FMT_VAAPI  // (or chosen)
}

fn attach_sw_format(
    ctx: *mut AVCodecContext,
    fmt: *const AVPixelFormat,
) -> AVPixelFormat {
    // SW fallback: pick AV_PIX_FMT_YUV420P if codec supports it,
    // else the first entry in `fmt` that libavcodec gives us.
    AV_PIX_FMT_YUV420P
}
```

The HW-vs-SW choice is made once at `spawn()` and stashed in `ctx->opaque`; the callback just looks it up.

#### 3.6 Frame transfer

After every `avcodec_receive_frame`:

- If `frame->format == VAAPI|D3D11|VIDEOTOOLBOX` (any HW pixfmt): allocate a fresh `sw_frame` of `AV_PIX_FMT_BGRA`, call `av_hwframe_transfer_data(sw_frame, hw_frame, 0)`. This is the documented "GPU → DMA → system memory" path; ~0.5 ms at 1080p on a discrete GPU.
- If `frame->format == YUV420P` (SW path): allocate a fresh `sw_frame` of `AV_PIX_FMT_BGRA`, call `sws_scale(ctx, frame->data, frame->linesize, 0, h, sw_frame->data, sw_frame->linesize)` using a process-wide `SwsContext` cached behind a `OnceLock`. This is ~2–4 ms on a modern CPU for 1080p60 — the existing subprocess path lands in the same ballpark but with the extra pipe jitter on top.
- BGRA byte ordering — `av_hwframe_transfer_data` with `AV_PIX_FMT_BGRA` returns `B,G,R,A` per pixel. `bgra_to_window_frame` at `main.rs:2070-2079` already does the `B,G,R,A → 0x00RRGGBB` conversion for minifb; the wgpu renderer instead uploads `Bgra8Unorm` textures and lets the GPU handle byte order.

#### 3.7 Backpressure / shutdown

- `encoded_rx: Receiver<DecodedFrame::Encoded>` carries raw `Vec<u8>` annex-b (matches the existing call site at `main.rs:1241`).
- `decoded_tx: Sender<DecodedFrame>` carries `crossbeam_channel::bounded(2)`.
- If the renderer thread is blocked inside a previous `queue.write_texture` (e.g. host paused), `decoded_tx.send(...)` blocks the decoder until the GPU catches up. **No frame drops** — better to spin on the decoder than to lose display coherence for a remote user moving a mouse.
- `shutdown()` flips `Arc<AtomicBool>` cancel, drops `encoded_rx` (causes the inner `recv` to error), then joins the thread with a 250 ms grace. After 250 ms the thread is detached; the `AVCodecContext` is freed on whichever thread held it last. (This mirrors the spirit of `RunningFrameDecoder::shutdown` at `main.rs:294-321` which also detaches-but-waits.)

### 4. wgpu render pipeline

#### 4.1 Replace minifb with winit

Replacing minifb with winit is the only way to get a real `wgpu::Surface`. The project has been preparing for this:

- `winit.workspace = true` is already declared (`Cargo.toml:45`) but is currently unused (the only references are doc comments at `frame_pacing.rs:27,109` and `stats_overlay.rs:20`).
- `raw-window-handle.workspace = true` is already declared (`Cargo.toml:43`).
- `blank_overlay.rs:60-82` already builds a `minifb::Window` lazily. Migrating it to a winit subwindow is mechanical and solves project rule #3 at the same time: a single `winit::EventLoop` runs the video window + blank overlay + (eventually) any hotkey tray icon.

#### 4.2 Single `ApplicationHandler`

`WinitUserEvent` enum (added to a new `apps/client-cli/src/winit_app.rs`, see §4.5) lists all the events the decoder, control plane, and overlay can dispatch back to the main thread:

```rust
#[derive(Debug)]
pub enum WinitUserEvent {
    /// Decoder has a new frame on the `crossbeam-channel`.
    FrameReady,
    /// Request graceful exit (sent on decode-error escape).
    Exit,
    /// Overlay show/hide toggle (from the control stream).
    Overlay(OverlayCommand),
    /// Stats overlay visibility flip (Ctrl+Alt+S).
    ToggleStats,
    /// Stream registry tile-mode flip (Ctrl+T).
    ToggleTile,
    /// Stream registry stream-cycle (Ctrl+S).
    CycleStream,
    /// Stream registry privacy-indicator flip (Ctrl+P).
    TogglePrivacy,
}
```

A single `ApplicationHandler<WinitUserEvent>` owned by `run_video_window` (and a separate one for `run_tiled_view` — they are never alive at the same time) handles:

- `resumed(...)` → create wgpu instance, request adapter, request device + queue, create surface, configure swapchain.
- `window_event(WindowEvent::RedrawRequested, ...)` → drain channel, upload texture, encode pass, submit, request another redraw.
- `window_event(WindowEvent::CloseRequested | KeyboardInput(Escape), ...)` → drop everything, break the loop, send `WinitUserEvent::Exit` to wake the main loop.
- `user_event(WinitUserEvent::FrameReady)` → request a redraw.
- `user_event(WinitUserEvent::ToggleStats | Tile | Stream | Privacy)` → toggle the bits in `StreamRegistry` / `StatsCollector` and request a redraw.

This consolidates the input pump and hotkey handling that today lives inline at `main.rs:1686-1734` (and `:1817-1861` for the tiled path). The migration preserves every observable behavior in §10.

#### 4.3 Swapchain config

Surface configuration inside the `Resumed` handler:

```rust
let surface_caps = surface.get_capabilities(&adapter);
let fmt = surface_caps
    .formats
    .iter()
    .copied()
    .find(|f| f.is_bgra())
    .unwrap_or(surface_caps.formats[0]);

let present_mode = if surface_caps.present_modes.contains(&PresentMode::Mailbox) {
    PresentMode::Mailbox        // sub-vblank latency on idle
} else {
    PresentMode::Fifo           // V-sync fallback
};

let config = SurfaceConfiguration {
    usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_DST,
    format: fmt,                // typically Bgra8Unorm
    width, height,
    present_mode,
    desired_maximum_frame_latency: 1,   // mailbox requires low latency
    alpha_mode: surface_caps.alpha_modes[0],
    view_formats: vec![],
};
surface.configure(&device, &config);
```

Why `Mailbox` first, `Fifo` second (and never `Immediate`): macOS Metal and Linux Vulkan refuse `Immediate`; `Fifo` is universally supported but adds a frame of latency. `Mailbox` is the sweet spot — no tearing, sub-vblank latency, requires V-sync OFF (which is fine for a remote desktop frame-locked to host capture).

#### 4.4 Render graph

Three render pipelines owned by `WgpuRenderer`:

1. `video_blit` — texture → swapchain blit. WGSL shader:

   ```wgsl
   @vertex fn vs(@builtin(vertex_index) idx: u32) -> @builtin(position) vec4<f32> {
       let x = f32((idx & 1u) << 2u) - 1.0;
       let y = f32((idx & 2u) << 1u) - 1.0;
       return vec4<f32>(x, y, 0.0, 1.0);
   }

   @group(0) @binding(0) var tex: texture_2d<f32>;
   @group(0) @binding(1) var samp: sampler;

   @fragment fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
       let uv = (pos.xy + vec2(0.5)) / vec2(textureDimensions(tex));
       return textureSample(tex, samp, uv);
   }
   ```

   Uses zero-vertex / `vertex_index` technique (no vertex buffer; minimal GPU upload).
2. `overlay_solid` — paints a solid RGBA over a sub-rectangle. Used by the privacy indicator and the black-out for blank overlay mode.
3. `overlay_text` — `wgpu_glyph` atlas blit. Used by stats overlay. See §4.6.

#### 4.5 File layout for the winit integration

A new file `apps/client-cli/src/winit_app.rs` (separate from the renderer; the same `WinitUserEvent` is shared with `decoder_hw.rs`):

```rust
//! Process-wide winit application glue. Owns the EventLoop singleton.

#[derive(Debug)]
pub enum WinitUserEvent { /* see §4.2 */ }

pub trait AppState: 'static {
    fn resumed(&mut self, event_loop: &ActiveEventLoop);
    fn redraw(&mut self, window: &Window);
    fn user_event(&mut self, event: WinitUserEvent, event_loop: &ActiveEventLoop);
    fn window_event(&mut self, window: &Window, event: WindowEvent);
}
```

`run_video_window` builds an `EventLoop::with_user_event()`, wraps it in `event_loop.run_app(&mut app_state)`, and never touches `EventLoop::run` again. Project rule #3 (one EventLoop per process) is enforced by the type system — only one place creates an `EventLoop`.

#### 4.6 Text rendering (stats overlay migration)

`wgpu_glyph` 23.x is the existing choice. It lazy-builds a `Texture` atlas on first call, so the first frame paints text slowly (acceptable for hidden-by-default overlays). The `stats_overlay.rs` `paint_overlay` function (which today mutates the CPU BGRA buffer at `stats_overlay.rs:1-...`) is split into:

- `render_wgpu::paint_overlay(pass, viewport, snapshot) -> Vec<TextSection>` — builds the text sections and lets `wgpu_glyph` queue the draws into our render pass.
- `render_minifb::paint_overlay(buffer, w, h, snapshot)` — the existing CPU path, retained.

`hotkey_pressed(&window)` (`stats_overlay.rs:...`) is replaced by an `ApplicationHandler::window_event` match on `WindowEvent::KeyboardInput { state: Pressed, logical_key: Key::S }` with the modified-bit flag check.

#### 4.7 Privacy indicator

CPU path today is `privacy_indicator::apply_red_overlay(&mut frame)` which sets every BGRA byte (analogous to the bit-blit). wgpu path renders a full-screen red quad through the `overlay_solid` pipeline after the video blit, gated by `stream_registry.should_show_privacy_indicator() && stream_registry.privacy_state() == Active::Privacy`. Both renderers share `StreamRegistry` and `StatsCollector` (already `Arc`).

### 5. Threading model

```
  ┌─────────────────┐
  │  winit main     │   single thread; owns EventLoop, GPU device/queue,
  │  thread         │   swapchain, decode→render wakeup
  └─────────────────┘
        ▲                 │
        │  WinitUserEvent │
        │  via waker      │
        │                 ▼
  ┌─────────────────┐ ┌──────────────────────────┐
  │  decoder thread │ │  network thread (tokio)  │
  │  std::thread    │ │  receive_media_stream_   │
  │  (per session)  │ │  registry (existing)     │
  └─────────────────┘ └──────────────────────────┘
        ▲                         │
        │    crossbeam-channel    │
        │    bounded(2)           ▼
        └────── both write to same encoded_rx (std mpsc) ──────┘
```

- **Network thread** (existing, unchanged at `main.rs:1394-1455`): consumes `NativeQuicMediaReceiver::read_access_unit`, writes raw annex-b bytes onto `encoded_tx: std::sync::mpsc::Sender<Vec<u8>>` (compatible with the existing subprocess path; see below).
- **Decoder thread** (new, replaces the subprocess): receives `Vec<u8>` from `encoded_rx`, drives `avcodec_send_packet` / `avcodec_receive_frame`, transfers to BGRA, sends `DecodedFrame` on `decoded_tx: crossbeam_channel::Sender<DecodedFrame>`.
- **Render thread** (main, winit): reads `decoded_rx: crossbeam_channel::Receiver<DecodedFrame>` inside the `RedrawRequested` handler. The first frame is uploaded to a wgpu texture via `queue.write_texture`; the next frame reuses the same texture (re-upload to same texture).
- **Audio thread** (existing, unchanged at `main.rs:265-339`, `RunningAudioPlayback::start`): `cpal::Stream` callback. Independent of decoder/render pipeline.

Why `crossbeam-channel` for the decoder→renderer hop and **not** `tokio-mpsc`:

- The binary is `#[tokio::main]` (at `main.rs:427`), but the winit `EventLoop::run_app` blocks the main thread. Once a `RedrawRequested` is being handled we cannot `await` a tokio sender and we cannot yield to the runtime without breaking v-sync. `crossbeam-channel::send` is a synchronous blocking call with bounded depth — the decoder's backpressure logic in §3.7 stays correct and the render loop is fully synchronous.
- `crossbeam-channel` is already a transitive dep of `quinn` and `tokio`; adding the dep adds ~1 KB to the binary.

**Why a bounded depth of 2 (not 1):** with depth=1, the decoder halts the moment the render thread is busy with `queue.write_texture`. With depth=2, the decoder can finish the current frame (sitting in the GPU readback path) while the render thread uploads the previous one. Empirically this matches the existing subprocess reader-loop depth (`main.rs:1247-1252`).

**Wake-up design.** The decoder thread holds a `WinitEventLoop::EventLoopProxy<WinitUserEvent>` clone (passed in by the renderer when it started the decoder). After each successful `decoded_tx.send`, it calls `event_proxy.send_event(WinitUserEvent::FrameReady).ok()`. The renderer's `user_event` handler then sets a `redraw_pending: bool`, and on the next winit tick (which is the next vsync when `PresentMode::Mailbox` is set) the `RedrawRequested` handler is invoked.

This is the exact pattern documented in P0-3 research and is the standard winit + GPU-programming wake-up contract. Avoids a busy `set_control_flow(WaitUntil)` loop while keeping latency bounded.

**Codec error frames.** If `avcodec_receive_frame` returns an error other than `EAGAIN`, the decoder thread logs at `warn!`, signals the error via `event_proxy.send_event(WinitUserEvent::Exit)` with a side channel `Result<(), String>` payload, and exits its loop. The renderer surfaces the error to `run_native_quic_viewer` at `main.rs:1288-1305`.

### 6. Migration path

#### 6.1 Phase-by-phase checklist

Mirrors the ADR prompt's "Phase E to merge" list. Each step lands on its own commit so bisecting is easy.

| # | Step | Files | Notes |
|---|------|-------|-------|
| 1 | Add `wgpu`, `wgpu_glyph`, `pollster`, `crossbeam-channel` workspace + per-crate deps | `Cargo.toml`, `apps/client-cli/Cargo.toml` | No code change yet |
| 2 | Define `WinitUserEvent` enum + `AppState` trait | new `apps/client-cli/src/winit_app.rs` | No behavior change |
| 3 | Lift minifb paint loop into `render_minifb.rs` (zero behavior change) | new `render_minifb.rs`, drop minifb refs from `main.rs` | Verify with existing tests |
| 4 | Lift `run_video_window` into `run_video_window.rs` (zero behavior change) | new `run_video_window.rs`; `main.rs:1288-1305` becomes a single `run_video_window(...)?` call | Bisect to confirm |
| 5 | Same lift for `run_tiled_view` | new `run_tiled_view.rs`; `main.rs:1307-1323` | Bisect |
| 6 | Implement `frame_pipeline::DecodedFrame` + `Renderer` trait | new `frame_pipeline.rs` | Both renderers implement `Renderer` |
| 7 | Rewrite `decoder_hw.rs` with the §3 architecture (no HW device yet; just SW in-process) | `decoder_hw.rs` | CI stays green; subprocess retained as `--decoder=subprocess` |
| 8 | Implement `render_wgpu.rs` for the existing SW decoder output (BGRA bytes → texture → blit) | new `render_wgpu.rs` | Flag-gated; `PresentMode::Fifo` only at first |
| 9 | Wire `--renderer {wgpu,minifb}` and `--decoder {subprocess,hw,sw}` to the dispatcher in `run_video_window.rs` / `run_tiled_view.rs` | `winit_app.rs`, the dispatchers | CI uses `--renderer=minifb --decoder=subprocess` |
| 10 | Wire `wgpu` + `winit` event loop including migrating `blank_overlay.rs` to a winit subwindow | `blank_overlay.rs`, `winit_app.rs` | Honors project rule #3 |
| 11 | Add `PresentMode::Mailbox` toggle | `render_wgpu.rs` | Confirms via wgpu trace |
| 12 | Add real `get_format` + `av_hwframe_transfer_data` per §3.5–3.6 | `decoder_hw.rs` | First GPU frame |
| 13 | Add `wgpu_glyph` stats overlay migration | `stats_overlay.rs`, `render_wgpu.rs` | `--renderer=wgpu` shows text; `--renderer=minifb` unchanged |
| 14 | Update P1-9 / P1-10 audio reference tap to listen on the decoder's waker if needed | `main.rs`, already Phase A | Per §10 open question |
| 15 | `RunningFrameDecoder` (subprocess) becomes `--decoder=subprocess` only; default `--decoder=hw` on boxes with GPU, `--decoder=sw` otherwise | `main.rs` | Feature flag stays for `hw-decode` so no libav* build dep on dev boxes |
| 16 | Delete `decoder_writer_loop` (`main.rs:1994-2012`), `decoder_reader_loop` (`main.rs:2025-2055`), `decoder_stderr_loop` (`main.rs:2057-2068`), `is_benign_decoder_pipe_end` (`main.rs:2014-2023`), `bgra_to_window_frame` (`main.rs:2070-2079`) | `main.rs` | Only after `--decoder=subprocess` is gone from the default path |

#### 6.2 Compatibility flags

| Flag | Values | Default | Behavior |
|------|--------|---------|----------|
| `--renderer` | `wgpu`, `minifb` | `wgpu` (when available) | Picks `render_wgpu` or `render_minifb` |
| `--decoder` | `hw`, `sw`, `subprocess` | `hw` if a GPU adapter opens; else `sw`; never `subprocess` unless `--decoder=subprocess` is explicit | Picks `RunningHwFrameDecoder` (HW or SW), falls back to `RunningFrameDecoder` (subprocess) |
| `--present-mode` | `mailbox`, `fifo` | `mailbox` | Per-surface override |

`--renderer=minifb --decoder=subprocess` is the exact pair the subprocess-and-minifb path of today runs under. It is also the CI pair — no GPU/CI-driver dependency.

`--renderer=wgpu --decoder=hw` is the production desktop pair.

#### 6.3 Forward / backward compat

The minifb renderer stays in the build tree (step 16 only deletes the subprocess I/O loops). A user can always pin to the old path with `--renderer=minifb --decoder=subprocess`. Same wire format. Same protocol. Same C API surface for `client-cli`. The only user-visible change is "video is smoother" once `wgpu` is enabled by default.

### 7. Test strategy

#### 7.1 Unit tests — `apps/client-cli/src/` (no GPU)

- `decoder_hw.rs::tests::sw_decode_yuv420p` — initialize a `RunningHwFrameDecoder` with `cfg.preferred=[]` (forces SW path), feed it a single H.264 keyframe (synthesized via `libswscale` + the test's own `ffmpeg` invocation), assert the resulting `DecodedFrame` bytes are `width*height*4` of valid BGRA.
- `decoder_hw.rs::tests::codec_selection` — for each `(codec, hw_device_type)` pair in a matrix, assert that `pick_hw_device_type` returns the expected `AVHWDeviceType` (or `None` if the codec doesn't support HW).
- `render_wgpu.rs::tests::pipeline_state_compiles` — request an adapter/device via `pollster::block_on(...)`, skip if no adapter, build the `video_blit` pipeline against a 64×64 dummy texture, assert no `wgpu::Error`.
- `frame_pipeline.rs::tests::decoded_frame_clone_is_zero_copy` — same underlying buffer on both ends (important for the `PixelData::GpuHandle` future).

Skip HW-specific tests when libav* is not present (CI may not have `libavcodec-dev` installed).

#### 7.2 Integration tests — `apps/client-cli/tests/` (gated on `#[cfg(feature = "hw-decode")]`)

- `tests/hw_decode_e2e.rs::h264_vaapi_round_trip` — synthesize a 5-frame H.264 stream with `ffmpeg -f lavfi -i testsrc=size=320x240:rate=30 -c:v libx264 -f h264 pipe:1`, pipe through `RunningHwFrameDecoder`, assert five `DecodedFrame`s and a deterministic pixel checksum on frame 0.
- `tests/hw_decode_e2e.rs::get_format_receives_correct_device_type` — instrument `get_format` with a global counter; assert the negotiated device type matches the probing order.
- `tests/hw_decode_e2e.rs::graceful_hw_to_sw_fallback` — same as above but force `av_hwdevice_ctx_create` to return an error via `LD_PRELOAD=/path/to/libav_fake.so` or by running with `LIBVA_DRIVER_NAME=fakesw`; assert decoder still produces frames via SW path.

#### 7.3 Manual checklist (documented; not CI)

- [ ] 1080p60 H.264 decode-to-present latency < 16 ms (RTX 3060 / Arc A770 / M2 Pro)
- [ ] 4K60 H.265 decode-to-present latency < 16 ms
- [ ] AV1 8-bit 1080p60 decode-to-present latency < 16 ms
- [ ] `PresentMode::Mailbox` confirmed via `wgpu` trace (`pollster::block_on` adapter debug output)
- [ ] No frame drops over a 60-second soak at 1080p120
- [ ] `cargo check --workspace --exclude client-gui` clean
- [ ] `cargo build -p client-gui` clean (`client-gui` does not link `ffmpeg-next` so its build is independent)
- [ ] `--renderer=minifb --decoder=subprocess` reproduces current behavior on the same host
- [ ] Privacy indicator + stats overlay both render correctly on both renderers
- [ ] `--list-streams` path (`main.rs:1268-1287`) does not regress
- [ ] cpal playback (audio) does not glitch while video is rendering
- [ ] `RunningFrameDecoder::shutdown` subprocess-cleanup behavior is preserved on its code path

### 8. Dependency manifest

#### 8.1 Workspace `Cargo.toml` (additions)

```toml
# After line 56 (line 58 is the last existing entry '# P0-6 gamepad')

# P0-3 + P0-5: zero-copy GPU presentation.
wgpu        = { version = "23", default-features = false, features = ["vulkan", "metal", "dx12", "wgsl"] }
wgpu_glyph  = { version = "23" }
glyph_brush = "0.7"             # transitive; wgpu_glyph depends on it
pollster    = "0.3"             # wgpu async-init runtime for the Resumed handler
crossbeam-channel = "0.5"       # bounded decoder→renderer channel
```

`default-features = false` plus the explicit `vulkan / metal / dx12 / wgsl` features is the wgpu 23.x convention — enables only the backends we actually need and the WGSL frontend. `wgpu` 23 also pulls `raw-window-handle` 0.6 transitively (already in the tree, no conflict).

`wgpu_glyph` is the actively-maintained successor to `wgpu_glyph 0.2x` from earlier epochs. It defaults to using `glyph_brush` for layout.

#### 8.2 `apps/client-cli/Cargo.toml` (additions)

```toml
# After the existing `ffmpeg-next = { version = "8.1", optional = true }` at line 37.

wgpu             = { workspace = true }
wgpu_glyph       = { workspace = true }
pollster         = { workspace = true }
crossbeam-channel = { workspace = true }
```

The existing `ffmpeg-next` dep at `:37` and the `hw-decode` feature at `:39-40` remain unchanged. CI builds without `--features hw-decode` (no `bindgen`, no libav dep), which keeps CI image bloat flat.

#### 8.3 No changes elsewhere

- `crates/qubox-proto/Cargo.toml` — unchanged. Zero wire-format changes.
- `crates/qubox-transport/Cargo.toml` — unchanged.
- `crates/qubox-display/Cargo.toml` — unchanged.
- `apps/client-gui/src-tauri/Cargo.toml` — unchanged. The Tauri GUI continues to import from `apps/client-cli/src/lib.rs` (project rule #5); the new `mod` declarations in §2 expose only what the Tauri GUI already consumes.

### 9. Risk register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `ffmpeg-next 8.1` API breaks `bindgen` on this `libclang-18-dev` install | Medium | High (full block) | Pin to `ffmpeg-next = "8"` semver with a `Cargo.lock` revision; existing `build.rs` already emits a warning when libclang is missing. Workspace dep includes `ffmpeg-next = "8.1"` not `"^8.1"` to allow lockfile pinning. |
| GPU drivers absent on dev box (Linux VM, headless CI) | Medium | Medium (can't smoke-test HW locally) | `render_minifb` + `--decoder=subprocess` is the always-works pair; CI matrix runs both `--renderer=minifb` and `--renderer=wgpu` where available. |
| winit event loop + `blank_overlay` migration breaks project rule #3 | High (concurrent state) | High (must avoid two `EventLoop::run`s) | Single `WinitUserEvent` enum + `AppState` trait, both windows driven by the same `EventLoopProxy`. `cargo clippy -- -D warnings` flags accidental `EventLoop::new`. |
| `wgpu::Surface` creation fails on Mesa/i915/other niche drivers | Medium | High (visual artifacts / no fallback) | Adapter enumeration iterates `request_adapter` power-pref → low-power → fallback. Surface format falls back from `Bgra8Unorm` to whatever the adapter lists first. Final fallback: crash with actionable error in `--renderer=wgpu` and recommend `--renderer=minifb`. |
| `queue.write_texture` adds 1–2 ms latency vs a direct `copy_texture_to_texture` from a staging buffer | Low | Low | Benchmark in §7.3; switch to a triple-buffered staging buffer if `write_texture` shows up in `wgpu-profiler` traces. |
| Stats overlay text rendering via glyph atlas is heavier than expected | Low | Medium | First frame is slow (atlas build) but cached; subsequent frames are ~µs. Fallback: keep `render_minifb::paint_overlay` for both renderers when `--stats-overlay=off`. |
| tokio mpsc bridge needed because legacy call sites pass `tokio_mpsc::UnboundedSender<Vec<u32>>` (scaffold at `decoder_hw.rs:68`) | Medium | Medium | Replace scaffold signature with crossbeam_channel explicitly; update only the single call site in `main.rs:1241`. |
| `WinitUserEvent::FrameReady` storm on a high-FPS session floods winit | Low | Low | The renderer's `redraw_pending: bool` coalesces; one `RedrawRequested` per vsync. Already documented in ADR-003 §"Latency". |
| `minifb` paint path is forgotten during bisect and silently regresses | Low | Medium | The `render_minifb.rs` smoke test (frame from a synthetic BGRA buffer through `bgra_to_window_frame`) plus the existing 236 tests keep it lit. |
| `RunningFrameDecoder::shutdown` retry path (`main.rs:294-321` with `is_benign_decoder_pipe_end`) is left orphaned | Low | Low | Delete it in step 16 once `--decoder=subprocess` is gone from the default. Until then it stays alive alongside the HW decoder (the call site at `main.rs:1241` is replaced anyway). |

### 10. Open questions

1. **YUV → RGB colorspace conversion location.** GPU shader (cheap on discrete, expensive on integrated) vs CPU via libswscale (current plan in §3.6). **Recommendation:** CPU via libswscale for the cutover. The DMA readback of `av_hwframe_transfer_data` already pays the GPU→CPU cost; pushing YUV→RGB onto the GPU would require re-uploading the raw YUV as a 3-plane `texture_3d`, which loses zero-copy and complicates the swapchain. Document as a Phase 3 follow-up.
2. **10-bit HDR (P2-14).** Out of scope for this ADR. `Rgba16Float` is reserved in the `PixelFormat` enum at §3.2 but no `wgpu::TextureFormat::Rgba16Float` swapchain is exercised yet. Defer to Path 3 (ADR-010).
3. **Audio reference tap + new decoder thread compatibility.** Already addressed in Phase A (commit `911427f`). The cpal `RunningAudioPlayback` at `main.rs:265-339` already exposes its queue to `qubox_mic::ReferenceAudioTap::new(960)` at `main.rs:1217`. The decoder thread is on a separate `std::thread` independent of the cpal callback thread. **No additional work needed**; verify with the existing Phase A integration tests.
4. **`RunningFrameDecoder::shutdown` API for HW decoder.** `shutdown(self) -> anyhow::Result<()>` (consuming, §3.1) matches the spirit of `RunningFrameDecoder::shutdown(mut self, allow_pipe_end: bool)` at `main.rs:294-321` but drops the `allow_pipe_end` argument — the HW decoder has no pipe-end race. Document this asymmetry.
5. **`frame_pacing.rs::FramePacer` integration with winit `RedrawRequested`.** `frame_pacing.rs:109` already documents the contract: "on every winit `RedrawRequested`; if it returns `Skip`, do not ..." The wgpu renderer honors this directly — drain `decoded_rx`, ask the pacer if we should present, and if `Skip` is returned, just request another redraw. This is a clean merge, not a future open question. (Recorded here for completeness.)
6. **TOC of `wgpu_glyph` vs raw glyph atlas.** `wgpu_glyph` 23.x dropped support for some font loaders that earlier `0.2x` versions shipped. Decide on font asset path during step 13 (text renderer migration). Safe default: ship the existing minifb `paint_overlay` font as a TTF and load it with `wgpu_glyph::glyph_brush::ab_glyph::FontArc::try_from_vec(ttf_bytes)`.
7. **Headless GPU boxes.** `wgpu` cannot run with no display server. Linux servers without X11/Wayland (e.g. Steam Deck gaming mode, kiosks) need `--renderer=minifb --decoder=sw`. The dispatcher's `--renderer=wgpu` path probes winit first; a failure at `EventLoop::run_app` exits with a clear message.

## Appendix A — file:line references consumed by this ADR

Substrate line numbers cited in this document (verified against `main` at `4f45658`):

- `apps/client-cli/src/main.rs:50-71` — `struct Args` (CLI parse)
- `apps/client-cli/src/main.rs:74-153` — `enum Command` (subcommands)
- `apps/client-cli/src/main.rs:215-220` — `struct RunningFrameDecoder` (subprocess shell)
- `apps/client-cli/src/main.rs:233-322` — `RunningFrameDecoder` impl (spawn, shutdown)
- `apps/client-cli/src/main.rs:265-339` — `RunningAudioPlayback::start` (cpal)
- `apps/client-cli/src/main.rs:427-738` — `main()`
- `apps/client-cli/src/main.rs:1032-1338` — `run_native_quic_viewer`
- `apps/client-cli/src/main.rs:1241-1242` — call site of `RunningFrameDecoder::spawn`
- `apps/client-cli/src/main.rs:1288-1305` — call sites of `run_video_window` / `run_tiled_view`
- `apps/client-cli/src/main.rs:1394-1455` — `receive_media_stream_registry` (network thread)
- `apps/client-cli/src/main.rs:1632-1764` — `run_video_window` body
- `apps/client-cli/src/main.rs:1768-1903` — `run_tiled_view` body
- `apps/client-cli/src/main.rs:1994-2012` — `decoder_writer_loop` (subprocess stdin → ffmpeg)
- `apps/client-cli/src/main.rs:2025-2055` — `decoder_reader_loop` (subprocess ffmpeg stdout → BGRA)
- `apps/client-cli/src/main.rs:2014-2023` — `is_benign_decoder_pipe_end`
- `apps/client-cli/src/main.rs:2070-2079` — `bgra_to_window_frame`
- `apps/client-cli/src/main.rs:1217` — `qubox_mic::ReferenceAudioTap::new(960)`
- `apps/client-cli/src/decoder_hw.rs:1-85` — entire scaffold (replaced)
- `apps/client-cli/src/decoder_hw.rs:41` — `#![cfg(feature = "hw-decode")]`
- `apps/client-cli/src/decoder_hw.rs:68` — `tokio_mpsc::UnboundedSender<Vec<u32>>` (replaced)
- `apps/client-cli/src/lib.rs:1-11` — pub re-exports (project rule #5 surface)
- `apps/client-cli/src/lib.rs:9-11` — `start_session` re-export
- `apps/client-cli/src/blank_overlay.rs:50-100` — `BlankOverlayWindow` (minifb → winit migration)
- `apps/client-cli/src/blank_overlay.rs:60-82` — lazy overlay window creation
- `apps/client-cli/src/frame_pacing.rs:1-193` — `FramePacer` (already targets winit contract)
- `apps/client-cli/src/frame_pacing.rs:27,109` — winit contract docs
- `apps/client-cli/src/stats_overlay.rs:20` — "long-term plan is to replace minifb with a winit + wgpu surface"
- `apps/client-cli/build.rs:29-55` — libclang detection
- `apps/client-cli/Cargo.toml:37` — `ffmpeg-next = { version = "8.1", optional = true }`
- `apps/client-cli/Cargo.toml:39-40` — `hw-decode` feature
- `Cargo.toml:36-45` — existing workspace deps (winit, minifb, softbuffer, raw-window-handle)
- `crates/qubox-proto/src/lib.rs:90-96` — `VideoCodec` enum
- `crates/qubox-proto/src/lib.rs:108-123` — `ffmpeg_demux_format`, `ffmpeg_mux_format`
- `crates/qubox-proto/src/lib.rs:470-476` — `VideoStreamParams` (already carries all four fields)
- `crates/qubox-transport/src/media/mod.rs:38-89` — 14-byte media header (unchanged)

## Appendix B — terminology

- **HW path** — `RunningHwFrameDecoder` with a non-empty `cfg.preferred`; `get_format` returns a backend pixfmt (`AV_PIX_FMT_VAAPI`, `AV_PIX_FMT_D3D11`, etc.).
- **SW path** — `RunningHwFrameDecoder` with `cfg.preferred=[]`; `get_format` returns codec-native (typically `AV_PIX_FMT_YUV420P`); frames go through `libswscale`.
- **Subprocess path** — `RunningFrameDecoder::spawn` at `main.rs:233-322`; survives as `--decoder=subprocess` only.
- **wgpu path** — `--renderer=wgpu`; `render_wgpu.rs` drives a `wgpu::Surface` via `winit`.
- **minifb path** — `--renderer=minifb`; `render_minifb.rs` paints to a `minifb::Window` via `update_with_buffer` (CPU side; existing behavior).
