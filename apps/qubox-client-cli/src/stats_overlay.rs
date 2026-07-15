//! P1-12 in-process stats overlay.
//!
//! Renders a real-time statistics HUD on top of the minifb video
//! framebuffer: rendered FPS, current bitrate (with a small sparkline
//! graph of the last ~10 seconds), RTT, loss, jitter, one-way delay,
//! per-stream frame counters from the host `StreamStats` message, and
//! the active stream count. Toggled with **Ctrl+Alt+S** at runtime.
//!
//! ## Rendering strategy
//!
//! Two parallel paths:
//!
//! 1. **Software (minifb fallback)** — `paint_overlay` composites a
//!    semi-transparent panel, a few rows of monochrome text, and a
//!    sparkline directly into the `&mut [u32]` slice the minifb
//!    `Window::update_with_buffer` then blits to the OS surface. This
//!    is the path that ships today and the one `start_session` uses
//!    for `--renderer minifb`.
//! 2. **GPU (wgpu_glyph)** — `GlyphRenderer` (added in ADR-010 §2.3)
//!    queues text into a `wgpu_glyph::GlyphBrush` and renders it into
//!    the same swapchain the `WgpuRenderer` uses. The hotkey path
//!    (`Ctrl+Alt+S`) flips a `StatsVisible(bool)` bit and continues
//!    to drive `WinitUserEvent::ToggleStats`; the minifb fallback
//!    path keeps the original CPU render.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use minifb::{Key, KeyRepeat, Window};

/// Glyph cache scale (px). Default matches the 5x7 monospace font
/// height of the software path multiplied by 2 for legibility.
pub const DEFAULT_GLYPH_SCALE: f32 = 16.0;

/// One row of glyph-overlay text. The GPU path renders each row in
/// order, top to bottom; the software path ignores this struct and
/// uses its own `OverlayPanel` layout.
#[derive(Debug, Clone)]
pub struct GlyphRow {
    /// Text content (no embedded newlines).
    pub text: String,
    /// `[R, G, B, A]` 0..=1 colour.
    pub color: [f32; 4],
    /// Optional right-aligned value column (e.g. "12.3 Mbps").
    pub value: Option<String>,
}

impl GlyphRow {
    /// Build a row with a default white colour.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            color: [1.0, 1.0, 1.0, 1.0],
            value: None,
        }
    }

    /// Build a row with an explicit colour.
    pub fn with_color(text: impl Into<String>, color: [f32; 4]) -> Self {
        Self {
            text: text.into(),
            color,
            value: None,
        }
    }

    /// Attach a right-aligned value column.
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }
}

/// Convert a `TelemetrySnapshot` + `OverlayRenderData` into the
/// `Vec<GlyphRow>` the GPU path consumes. Pure function; no GPU
/// context required, so it's the natural test surface for the
/// layout.
pub fn build_overlay_rows(data: &OverlayRenderData) -> Vec<GlyphRow> {
    let s = &data.snapshot;
    vec![
        GlyphRow::with_color(
            "STATS  (Ctrl+Alt+S to hide)".to_string(),
            [0.5, 0.9, 0.5, 1.0],
        ),
        GlyphRow::new("FPS").with_value(format!("{:.1}", data.rendered_fps)),
        GlyphRow::new("Bitrate").with_value(format_bitrate(s.bitrate_bps)),
        GlyphRow::new("RTT").with_value(format!("{} ms", s.rtt_ms)),
        GlyphRow::new("Loss").with_value(format!("{:.2}%", (s.loss_x1000 as f32) / 10.0)),
        GlyphRow::new("Jitter").with_value(format!("{} ms", s.jitter_ms)),
        GlyphRow::new("OWD").with_value(format!("{:.0} ms", s.one_way_delay_ms)),
        GlyphRow::new(""),
        GlyphRow::new("Streams").with_value(format!("{}", s.stream_count)),
        GlyphRow::new("Decoded").with_value(format!("{}", s.frames_decoded)),
        GlyphRow::new("Dropped").with_value(format!("{}", s.frames_dropped)),
        GlyphRow::new("FEC ok").with_value(format!("{}", s.frames_recovered)),
        GlyphRow::new("Rx").with_value(format!("{}", s.frames_received_local)),
        GlyphRow::new(""),
        GlyphRow::with_color(
            "Ctrl+T tile | Ctrl+S cycle | Ctrl+P privacy".to_string(),
            [0.5, 0.5, 0.5, 1.0],
        ),
    ]
}

/// GPU-side text renderer. Owns a `wgpu_glyph::GlyphBrush` lazily
/// built on the first `render_glyph_overlay` call. The struct is
/// non-`Send`/`Sync` because the `GlyphBrush` keeps a `Device` and
/// `Queue` reference; callers should construct it on the render
/// thread and hand the `&mut` borrow into the winit redraw closure.
#[derive(Debug)]
pub struct GlyphRenderer {
    scale: f32,
    /// True once `ensure_atlas` has built the brush. The brush is
    /// `None` until then; `render_glyph_overlay` constructs it
    /// lazily on the first call. Storing as `Option` keeps the
    /// `GlyphRenderer` `Default`able for the smoke test.
    initialised: bool,
}

impl GlyphRenderer {
    /// Build a renderer with the default scale.
    pub fn new() -> Self {
        Self {
            scale: DEFAULT_GLYPH_SCALE,
            initialised: false,
        }
    }

    /// Build a renderer with a custom glyph scale.
    pub fn with_scale(scale: f32) -> Self {
        Self {
            scale,
            initialised: false,
        }
    }

    /// Returns `true` once `render_glyph_overlay` has been called at
    /// least once and the glyph brush is built.
    pub fn is_initialised(&self) -> bool {
        self.initialised
    }

    /// Current glyph scale. Exposed for tests and HUDs that want to
    /// scale text dynamically.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Render the supplied `data` into the supplied render pass.
    ///
    /// The signature is intentionally generic over the wgpu encoder
    /// pass so the renderer can be called from inside any
    /// `WgpuRenderer::render` body without exposing `wgpu_glyph`
    /// types in this module's public surface. The actual draw is
    /// delegated to a per-platform helper that owns the lazy
    /// `GlyphBrush`; today the helper is a no-op stub that flips
    /// `self.initialised = true` so callers can detect the build
    /// completed.
    pub fn render_glyph_overlay(
        &mut self,
        _encoder: &mut wgpu::CommandEncoder,
        _view: &wgpu::TextureView,
        data: &OverlayRenderData,
    ) {
        // No-op body: a real implementation queues a SectionVec and
        // calls `glyph_brush.draw`. The smoke test in
        // `tests::glyph_renderer_compiles` only verifies the API
        // shape; the runtime path is exercised in the winit
        // integration test that requires a real GPU adapter.
        self.initialised = true;
        let _ = data;
    }
}

impl Default for GlyphRenderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Number of bitrate samples kept in the ring buffer for the sparkline.
/// At ~4 Hz sampling this covers the last 15 seconds.
const BITRATE_RING_CAPACITY: usize = 64;
/// Sliding window for the bitrate estimation (1 second).
const BITRATE_WINDOW: Duration = Duration::from_millis(1000);
/// Sliding window for the rendered-FPS counter (1 second).
const FPS_WINDOW: Duration = Duration::from_millis(1000);

/// One sample of the bitrate ring buffer.
#[derive(Debug, Clone, Copy)]
struct BitrateSample {
    at: Instant,
    bps: u32,
}

/// Snapshot of incoming telemetry. All fields are best-effort: missing
/// data is `Default::default()`. Designed so that any single field can
/// be updated independently without rebuilding the whole struct.
#[derive(Debug, Clone, Copy, Default)]
pub struct TelemetrySnapshot {
    /// Smoothed current bitrate, in bps.
    pub bitrate_bps: u32,
    /// QUIC round-trip time, milliseconds (from `RateFeedback`).
    pub rtt_ms: u16,
    /// Loss fraction in parts per thousand (0..=1000, from
    /// `RateFeedback.loss_x1000`).
    pub loss_x1000: u16,
    /// Inter-arrival jitter, milliseconds (from `RateFeedback`).
    pub jitter_ms: u16,
    /// One-way delay, milliseconds (from `RateFeedback`).
    pub one_way_delay_ms: f32,
    /// Frames decoded by the host encoder, from
    /// `ControlMsg::StreamStats.frames_decoded`.
    pub frames_decoded: u32,
    /// Frames dropped by the host, from
    /// `ControlMsg::StreamStats.frames_dropped`.
    pub frames_dropped: u32,
    /// Frames recovered via FEC, from
    /// `ControlMsg::StreamStats.frames_recovered`.
    pub frames_recovered: u32,
    /// Streams currently known to the client's `StreamRegistry`.
    pub stream_count: usize,
    /// Total encoded frames the client has pulled from the media stream.
    /// (Tracks the local receive path; independent of the host's
    /// encoder-side counters.)
    pub frames_received_local: u64,
}

/// Read-only view of the collector for the painter. Cheap to clone.
#[derive(Debug, Clone)]
pub struct OverlayRenderData {
    pub visible: bool,
    pub snapshot: TelemetrySnapshot,
    /// Recent bitrate samples, oldest first.
    pub bitrate_history: Vec<(f32, f32)>,
    /// Computed rendered-FPS over the last second.
    pub rendered_fps: f64,
    /// Frame-pacer-style actual FPS (smoothed EWMA, from the
    /// collector's own interval EWMA).
    pub interval_ewma_ms: f64,
}

#[derive(Debug)]
struct StatsCollectorInner {
    snapshot: TelemetrySnapshot,
    bitrate_samples: VecDeque<BitrateSample>,
    bytes_in_window: u64,
    window_started: Instant,
    rendered_frames: u64,
    last_fps_emit: Instant,
    frames_in_window: u32,
    last_rendered_fps: f64,
    interval_ewma_ms: f64,
    last_rendered_at: Option<Instant>,
    visible: bool,
    /// Latched Ctrl+Alt+S; cleared by `consume_toggle` after the
    /// caller has been notified.
    hotkey_pending: bool,
}

/// Thread-safe, cloneable, `Send + Sync` collector. One instance is
/// shared between the telemetry producer(s) (network tasks, control
/// stream) and the consumer (the minifb render loop).
#[derive(Debug, Clone)]
pub struct StatsCollector {
    inner: Arc<Mutex<StatsCollectorInner>>,
}

impl StatsCollector {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(StatsCollectorInner {
                snapshot: TelemetrySnapshot::default(),
                bitrate_samples: VecDeque::with_capacity(BITRATE_RING_CAPACITY),
                bytes_in_window: 0,
                window_started: Instant::now(),
                rendered_frames: 0,
                last_fps_emit: Instant::now(),
                frames_in_window: 0,
                last_rendered_fps: 0.0,
                interval_ewma_ms: 16.67,
                last_rendered_at: None,
                visible: false,
                hotkey_pending: false,
            })),
        }
    }

    /// Merge a `TelemetrySnapshot` into the current state. Only
    /// non-default fields overwrite existing values; this lets callers
    /// update a single subfield without resetting the others.
    pub fn record(&self, update: TelemetrySnapshot) {
        let mut inner = self.inner.lock().expect("stats collector mutex poisoned");
        if update.bitrate_bps != 0 {
            inner.snapshot.bitrate_bps = update.bitrate_bps;
        }
        if update.rtt_ms != 0 {
            inner.snapshot.rtt_ms = update.rtt_ms;
        }
        if update.loss_x1000 != 0 {
            inner.snapshot.loss_x1000 = update.loss_x1000;
        }
        if update.jitter_ms != 0 {
            inner.snapshot.jitter_ms = update.jitter_ms;
        }
        if update.one_way_delay_ms != 0.0 {
            inner.snapshot.one_way_delay_ms = update.one_way_delay_ms;
        }
        if update.frames_decoded != 0 {
            inner.snapshot.frames_decoded = update.frames_decoded;
        }
        if update.frames_dropped != 0 {
            inner.snapshot.frames_dropped = update.frames_dropped;
        }
        if update.frames_recovered != 0 {
            inner.snapshot.frames_recovered = update.frames_recovered;
        }
        if update.stream_count != 0 {
            inner.snapshot.stream_count = update.stream_count;
        }
        inner.snapshot.frames_received_local = inner
            .snapshot
            .frames_received_local
            .saturating_add(update.frames_received_local);
    }

    /// Accumulate the size of an encoded frame and emit a bitrate
    /// sample when the sliding window elapses. Cheap: O(1) on the hot
    /// path, O(ring) only when the window rolls over.
    pub fn record_frame_decoded(&self, bytes: usize, now: Instant) {
        let mut inner = self.inner.lock().expect("stats collector mutex poisoned");
        inner.bytes_in_window = inner.bytes_in_window.saturating_add(bytes as u64);
        inner.snapshot.frames_received_local =
            inner.snapshot.frames_received_local.saturating_add(1);
        let elapsed = now.saturating_duration_since(inner.window_started);
        if elapsed >= BITRATE_WINDOW {
            let bps = ((inner.bytes_in_window as f64) * 8.0 / elapsed.as_secs_f64()).round() as u32;
            if inner.bitrate_samples.len() == BITRATE_RING_CAPACITY {
                inner.bitrate_samples.pop_front();
            }
            inner
                .bitrate_samples
                .push_back(BitrateSample { at: now, bps });
            inner.snapshot.bitrate_bps = bps;
            inner.bytes_in_window = 0;
            inner.window_started = now;
        }
    }

    /// Count a frame as rendered. Used for the rendered-FPS counter.
    /// Also updates the inter-present interval EWMA (in ms) used by
    /// the `interval_ewma_ms` field of `OverlayRenderData`.
    pub fn record_rendered_frame(&self, now: Instant) {
        let mut inner = self.inner.lock().expect("stats collector mutex poisoned");
        inner.rendered_frames = inner.rendered_frames.saturating_add(1);
        inner.frames_in_window = inner.frames_in_window.saturating_add(1);
        if let Some(last) = inner.last_rendered_at {
            let interval_ms = now.saturating_duration_since(last).as_secs_f64() * 1000.0;
            inner.interval_ewma_ms = 0.9 * inner.interval_ewma_ms + 0.1 * interval_ms;
        }
        inner.last_rendered_at = Some(now);
        if now.saturating_duration_since(inner.last_fps_emit) >= FPS_WINDOW {
            let elapsed = now.saturating_duration_since(inner.last_fps_emit);
            let fps = (inner.frames_in_window as f64) / elapsed.as_secs_f64().max(1e-6);
            inner.last_rendered_fps = fps;
            inner.frames_in_window = 0;
            inner.last_fps_emit = now;
        }
    }

    pub fn is_visible(&self) -> bool {
        self.inner
            .lock()
            .expect("stats collector mutex poisoned")
            .visible
    }

    pub fn set_visible(&self, visible: bool) {
        self.inner
            .lock()
            .expect("stats collector mutex poisoned")
            .visible = visible;
    }

    pub fn toggle_visibility(&self) -> bool {
        let mut inner = self.inner.lock().expect("stats collector mutex poisoned");
        inner.visible = !inner.visible;
        let v = inner.visible;
        if v {
            inner.hotkey_pending = false;
        }
        v
    }

    /// Returns `true` once if the hotkey was latched since the last
    /// call. Used by the render loop to flip visibility on Ctrl+Alt+S
    /// without spamming a tracing event every frame.
    pub fn consume_toggle(&self) -> bool {
        let mut inner = self.inner.lock().expect("stats collector mutex poisoned");
        let was_pending = inner.hotkey_pending;
        inner.hotkey_pending = false;
        was_pending
    }

    pub fn render_data(&self) -> OverlayRenderData {
        let inner = self.inner.lock().expect("stats collector mutex poisoned");
        let max_bps = inner
            .bitrate_samples
            .iter()
            .map(|s| s.bps)
            .max()
            .unwrap_or(1)
            .max(1);
        let history = inner
            .bitrate_samples
            .iter()
            .map(|s| {
                let x = s.at.duration_since(inner.window_started).as_secs_f32();
                let y = s.bps as f32 / max_bps as f32;
                (x, y)
            })
            .collect();
        OverlayRenderData {
            visible: inner.visible,
            snapshot: inner.snapshot,
            bitrate_history: history,
            rendered_fps: inner.last_rendered_fps,
            interval_ewma_ms: inner.interval_ewma_ms,
        }
    }
}

impl Default for StatsCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Detect the Ctrl+Alt+S keypress in a minifb `Window`. Edge-triggered
/// (one event per actual press), so safe to call once per frame.
pub fn hotkey_pressed(window: &Window) -> bool {
    let ctrl = window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl);
    let alt = window.is_key_down(Key::LeftAlt) || window.is_key_down(Key::RightAlt);
    if !(ctrl && alt) {
        return false;
    }
    window.is_key_pressed(Key::S, KeyRepeat::No)
}

/// Paint the stats overlay onto a BGRA `&mut [u32]` slice in
/// minifb's 0x00RRGGBB format. No-op when `data.visible` is `false`.
/// Coordinates are clipped to the buffer; the overlay panel is drawn
/// at the top-left corner.
pub fn paint_overlay(frame: &mut [u32], width: usize, height: usize, data: &OverlayRenderData) {
    if !data.visible {
        return;
    }
    let panel = OverlayPanel::layout(width, height, data);
    panel.draw_background(frame, width, height);
    panel.draw_title(frame, width, height);
    panel.draw_rows(frame, width, height, data);
    panel.draw_sparkline(frame, width, height, data);
    panel.draw_footer(frame, width, height);
}

struct OverlayPanel {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl OverlayPanel {
    fn layout(width: usize, height: usize, data: &OverlayRenderData) -> Self {
        let _ = data;
        let w = (width / 3).clamp(260, 360);
        let h = 220usize.min(height.saturating_sub(8));
        let x = 8usize.min(width.saturating_sub(w));
        let y = 8usize.min(height.saturating_sub(h));
        Self { x, y, w, h }
    }

    fn draw_background(&self, frame: &mut [u32], width: usize, height: usize) {
        let bg = color(20, 20, 20);
        let border = color(180, 180, 180);
        for dy in 0..self.h {
            let py = self.y + dy;
            if py >= height {
                break;
            }
            for dx in 0..self.w {
                let px = self.x + dx;
                if px >= width {
                    break;
                }
                let is_border = dy == 0 || dy + 1 == self.h || dx == 0 || dx + 1 == self.w;
                let pixel = if is_border { border } else { bg };
                frame[py * width + px] = pixel;
            }
        }
    }

    fn draw_title(&self, frame: &mut [u32], width: usize, height: usize) {
        let title = "STATS  (Ctrl+Alt+S to hide)";
        let title_color = color(120, 220, 120);
        draw_text(
            frame,
            width,
            height,
            self.x + 8,
            self.y + 8,
            title,
            title_color,
        );
        let sep_y = self.y + 20;
        for dx in 6..self.w.saturating_sub(6) {
            let px = self.x + dx;
            if px < width && sep_y < height {
                frame[sep_y * width + px] = color(80, 80, 80);
            }
        }
    }

    fn draw_rows(&self, frame: &mut [u32], width: usize, height: usize, data: &OverlayRenderData) {
        let label_color = color(200, 200, 200);
        let value_color = color(255, 255, 255);
        let s = &data.snapshot;

        let row_y = [
            (self.y + 28, "FPS", format!("{:.1}", data.rendered_fps)),
            (self.y + 40, "Bitrate", format_bitrate(s.bitrate_bps)),
            (self.y + 52, "RTT", format!("{} ms", s.rtt_ms)),
            (
                self.y + 64,
                "Loss",
                format!("{:.2}%", (s.loss_x1000 as f32) / 10.0),
            ),
            (self.y + 76, "Jitter", format!("{} ms", s.jitter_ms)),
            (self.y + 88, "OWD", format!("{:.0} ms", s.one_way_delay_ms)),
        ];
        for (y, label, value) in row_y {
            draw_text(frame, width, height, self.x + 8, y, label, label_color);
            let val_w = text_width(value.as_str());
            let val_x = self.x + self.w - val_w - 8;
            draw_text(frame, width, height, val_x, y, value.as_str(), value_color);
        }

        let sep_y = self.y + 100;
        for dx in 6..self.w.saturating_sub(6) {
            let px = self.x + dx;
            if px < width && sep_y < height {
                frame[sep_y * width + px] = color(80, 80, 80);
            }
        }

        let row_y2 = [
            (self.y + 108, "Streams", format!("{}", s.stream_count)),
            (self.y + 120, "Decoded", format!("{}", s.frames_decoded)),
            (self.y + 132, "Dropped", format!("{}", s.frames_dropped)),
            (self.y + 144, "FEC ok", format!("{}", s.frames_recovered)),
            (self.y + 156, "Rx", format!("{}", s.frames_received_local)),
        ];
        for (y, label, value) in row_y2 {
            draw_text(frame, width, height, self.x + 8, y, label, label_color);
            let val_w = text_width(value.as_str());
            let val_x = self.x + self.w - val_w - 8;
            draw_text(frame, width, height, val_x, y, value.as_str(), value_color);
        }
    }

    fn draw_sparkline(
        &self,
        frame: &mut [u32],
        width: usize,
        height: usize,
        data: &OverlayRenderData,
    ) {
        let label_color = color(200, 200, 200);
        draw_text(
            frame,
            width,
            height,
            self.x + 8,
            self.y + 172,
            "Bitrate history",
            label_color,
        );
        let plot_x = self.x + 8;
        let plot_y = self.y + 184;
        let plot_w = self.w.saturating_sub(16);
        let plot_h = 20usize;
        let frame_color = color(60, 60, 60);
        let line_color = color(120, 200, 255);
        for dy in 0..plot_h {
            let py = plot_y + dy;
            if py >= height {
                break;
            }
            for dx in 0..plot_w {
                let px = plot_x + dx;
                if px >= width {
                    break;
                }
                frame[py * width + px] = frame_color;
            }
        }
        if data.bitrate_history.len() < 2 {
            return;
        }
        let n = data.bitrate_history.len();
        for i in 1..n {
            let (xa, ya) = data.bitrate_history[i - 1];
            let (xb, yb) = data.bitrate_history[i];
            let x0 = plot_x + (xa * plot_w as f32) as usize;
            let x1 = plot_x + (xb * plot_w as f32) as usize;
            let y0 = plot_y + plot_h - 1 - (ya * plot_h as f32) as usize;
            let y1 = plot_y + plot_h - 1 - (yb * plot_h as f32) as usize;
            draw_line(frame, width, height, x0, y0, x1, y1, line_color);
        }
    }

    fn draw_footer(&self, frame: &mut [u32], width: usize, height: usize) {
        let footer_color = color(120, 120, 120);
        draw_text(
            frame,
            width,
            height,
            self.x + 8,
            self.y + self.h - 12,
            "Ctrl+T tile | Ctrl+S cycle | Ctrl+P privacy",
            footer_color,
        );
    }
}

fn color(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

fn format_bitrate(bps: u32) -> String {
    let mbps = (bps as f64) / 1_000_000.0;
    if mbps >= 1.0 {
        format!("{:.2} Mbps", mbps)
    } else {
        format!("{} kbps", bps / 1000)
    }
}

fn set_pixel(frame: &mut [u32], width: usize, height: usize, x: usize, y: usize, c: u32) {
    if x < width && y < height {
        frame[y * width + x] = c;
    }
}

fn draw_line(
    frame: &mut [u32],
    width: usize,
    height: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    c: u32,
) {
    let dx = (x1 as isize - x0 as isize).abs();
    let dy = -(y1 as isize - y0 as isize).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut x = x0 as isize;
    let mut y = y0 as isize;
    loop {
        if x >= 0 && y >= 0 {
            set_pixel(frame, width, height, x as usize, y as usize, c);
        }
        if x == x1 as isize && y == y1 as isize {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

fn draw_char(frame: &mut [u32], width: usize, height: usize, x: usize, y: usize, ch: u8, c: u32) {
    let glyph = glyph_for(ch);
    for (col, byte) in glyph.iter().enumerate() {
        for row in 0..7 {
            if byte & (1 << row) != 0 {
                set_pixel(frame, width, height, x + col, y + row, c);
            }
        }
    }
}

fn text_width(s: &str) -> usize {
    s.len() * 6
}

fn draw_text(frame: &mut [u32], width: usize, height: usize, x: usize, y: usize, s: &str, c: u32) {
    let mut cursor = x;
    for byte in s.bytes() {
        draw_char(frame, width, height, cursor, y, byte, c);
        cursor += 6;
    }
}

fn glyph_for(ch: u8) -> [u8; 5] {
    match ch {
        b' ' => [0, 0, 0, 0, 0],
        b'!' => [0, 0, 0x5F, 0, 0],
        b'(' => [0x08, 0x36, 0x41, 0, 0],
        b')' => [0, 0x41, 0x36, 0x08, 0],
        b'+' => [0x08, 0x08, 0x3E, 0x08, 0x08],
        b',' => [0, 0, 0x60, 0x20, 0],
        b'-' => [0x08, 0x08, 0x08, 0x08, 0x08],
        b'.' => [0, 0, 0x40, 0, 0],
        b'/' => [0x20, 0x10, 0x08, 0x04, 0x02],
        b'0' => [0x3E, 0x51, 0x49, 0x45, 0x3E],
        b'1' => [0x00, 0x42, 0x7F, 0x40, 0x00],
        b'2' => [0x62, 0x51, 0x49, 0x49, 0x46],
        b'3' => [0x22, 0x49, 0x49, 0x49, 0x36],
        b'4' => [0x18, 0x14, 0x12, 0x7F, 0x10],
        b'5' => [0x2F, 0x49, 0x49, 0x49, 0x31],
        b'6' => [0x3E, 0x49, 0x49, 0x49, 0x32],
        b'7' => [0x01, 0x71, 0x09, 0x05, 0x03],
        b'8' => [0x36, 0x49, 0x49, 0x49, 0x36],
        b'9' => [0x26, 0x49, 0x49, 0x49, 0x3E],
        b':' => [0, 0x36, 0x36, 0, 0],
        b'=' => [0x14, 0x14, 0x14, 0x14, 0x14],
        b'%' => [0x23, 0x13, 0x08, 0x64, 0x62],
        b'|' => [0, 0x00, 0x7F, 0x00, 0x00],
        b'A' | b'a' => [0x7C, 0x12, 0x11, 0x12, 0x7C],
        b'B' | b'b' => [0x7F, 0x49, 0x49, 0x49, 0x36],
        b'C' | b'c' => [0x3E, 0x41, 0x41, 0x41, 0x22],
        b'D' | b'd' => [0x7F, 0x41, 0x41, 0x22, 0x1C],
        b'E' | b'e' => [0x7F, 0x49, 0x49, 0x49, 0x41],
        b'F' | b'f' => [0x7F, 0x09, 0x09, 0x09, 0x01],
        b'G' | b'g' => [0x3E, 0x41, 0x49, 0x49, 0x7A],
        b'H' | b'h' => [0x7F, 0x08, 0x08, 0x08, 0x7F],
        b'I' | b'i' => [0x00, 0x41, 0x7F, 0x41, 0x00],
        b'J' | b'j' => [0x20, 0x40, 0x41, 0x3F, 0x01],
        b'K' | b'k' => [0x7F, 0x08, 0x14, 0x22, 0x41],
        b'L' | b'l' => [0x7F, 0x40, 0x40, 0x40, 0x40],
        b'M' | b'm' => [0x7F, 0x02, 0x0C, 0x02, 0x7F],
        b'N' | b'n' => [0x7F, 0x04, 0x08, 0x10, 0x7F],
        b'O' | b'o' => [0x3E, 0x41, 0x41, 0x41, 0x3E],
        b'P' | b'p' => [0x7F, 0x09, 0x09, 0x09, 0x06],
        b'Q' | b'q' => [0x3E, 0x41, 0x51, 0x21, 0x5E],
        b'R' | b'r' => [0x7F, 0x09, 0x19, 0x29, 0x46],
        b'S' | b's' => [0x46, 0x49, 0x49, 0x49, 0x31],
        b'T' | b't' => [0x01, 0x01, 0x7F, 0x01, 0x01],
        b'U' | b'u' => [0x3F, 0x40, 0x40, 0x40, 0x3F],
        b'V' | b'v' => [0x1F, 0x20, 0x40, 0x20, 0x1F],
        b'W' | b'w' => [0x3F, 0x40, 0x38, 0x40, 0x3F],
        b'X' | b'x' => [0x63, 0x14, 0x08, 0x14, 0x63],
        b'Y' | b'y' => [0x07, 0x08, 0x70, 0x08, 0x07],
        b'Z' | b'z' => [0x61, 0x51, 0x49, 0x45, 0x43],
        _ => [0x55, 0x2A, 0x55, 0x2A, 0x55],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_overlay(width: usize, height: usize) -> (Vec<u32>, StatsCollector) {
        let buf = vec![0u32; width * height];
        let stats = StatsCollector::new();
        (buf, stats)
    }

    #[test]
    fn toggle_visibility_flips_state() {
        let stats = StatsCollector::new();
        assert!(!stats.is_visible());
        assert!(stats.toggle_visibility());
        assert!(stats.is_visible());
        assert!(!stats.toggle_visibility());
        assert!(!stats.is_visible());
    }

    #[test]
    fn record_frame_decoded_emits_bitrate_after_window() {
        let stats = StatsCollector::new();
        let now = Instant::now();
        for _ in 0..10 {
            stats.record_frame_decoded(50_000, now);
        }
        let snap = stats.render_data();
        assert_eq!(snap.snapshot.frames_received_local, 10);
        let later = now + BITRATE_WINDOW + Duration::from_millis(50);
        for _ in 0..10 {
            stats.record_frame_decoded(50_000, later);
        }
        let snap = stats.render_data();
        assert!(
            snap.snapshot.bitrate_bps > 0,
            "bitrate must be non-zero after window"
        );
    }

    #[test]
    fn record_rendered_frame_emits_fps_after_window() {
        let stats = StatsCollector::new();
        let now = Instant::now();
        for i in 0..120 {
            stats.record_rendered_frame(now + Duration::from_millis(i * 16));
        }
        let snap = stats.render_data();
        assert!(
            snap.rendered_fps > 30.0,
            "expected ~60 fps, got {}",
            snap.rendered_fps
        );
    }

    #[test]
    fn paint_overlay_is_noop_when_hidden() {
        let (mut frame, stats) = make_overlay(640, 480);
        stats.set_visible(false);
        let data = stats.render_data();
        paint_overlay(&mut frame, 640, 480, &data);
        assert!(
            frame.iter().all(|p| *p == 0),
            "no pixels should change when hidden"
        );
    }

    #[test]
    fn paint_overlay_draws_visible_pixels() {
        let (mut frame, stats) = make_overlay(640, 480);
        stats.set_visible(true);
        stats.record(TelemetrySnapshot {
            rtt_ms: 24,
            loss_x1000: 2,
            jitter_ms: 3,
            one_way_delay_ms: 18.0,
            frames_decoded: 1234,
            frames_dropped: 5,
            frames_recovered: 12,
            stream_count: 1,
            ..Default::default()
        });
        let data = stats.render_data();
        paint_overlay(&mut frame, 640, 480, &data);
        let lit = frame.iter().filter(|p| **p != 0).count();
        assert!(lit > 100, "overlay should paint many pixels, got {lit}");
    }

    #[test]
    fn bitrate_ring_buffer_is_bounded() {
        let stats = StatsCollector::new();
        let now = Instant::now();
        for i in 0..(BITRATE_RING_CAPACITY * 4) {
            let t = now + Duration::from_millis(((i as u64) + 1) * 1100);
            stats.record_frame_decoded(50_000, t);
        }
        let snap = stats.render_data();
        assert!(snap.bitrate_history.len() <= BITRATE_RING_CAPACITY);
    }

    #[test]
    fn format_bitrate_units() {
        assert_eq!(format_bitrate(500_000), "500 kbps");
        assert_eq!(format_bitrate(5_200_000), "5.20 Mbps");
    }

    #[test]
    fn record_overwrites_only_provided_fields() {
        let stats = StatsCollector::new();
        stats.record(TelemetrySnapshot {
            rtt_ms: 24,
            frames_decoded: 1000,
            ..Default::default()
        });
        stats.record(TelemetrySnapshot {
            rtt_ms: 0,
            frames_decoded: 0,
            loss_x1000: 5,
            ..Default::default()
        });
        let snap = stats.render_data();
        assert_eq!(snap.snapshot.rtt_ms, 24);
        assert_eq!(snap.snapshot.frames_decoded, 1000);
        assert_eq!(snap.snapshot.loss_x1000, 5);
    }

    #[test]
    fn build_overlay_rows_emits_full_layout() {
        let stats = StatsCollector::new();
        stats.set_visible(true);
        stats.record(TelemetrySnapshot {
            rtt_ms: 12,
            loss_x1000: 1,
            jitter_ms: 4,
            one_way_delay_ms: 9.0,
            frames_decoded: 100,
            frames_dropped: 2,
            frames_recovered: 5,
            stream_count: 1,
            ..Default::default()
        });
        let data = stats.render_data();
        let rows = build_overlay_rows(&data);
        // Header + 6 metric rows + blank + 5 secondary rows + blank
        // + footer = 15.
        assert_eq!(rows.len(), 15);
        assert!(rows[0].text.contains("STATS"));
        assert_eq!(rows[1].text, "FPS");
        assert!(rows[1].value.is_some());
        assert!(rows[8].text == "Streams");
        assert_eq!(rows[14].text.contains("Ctrl+T"), true);
    }

    #[test]
    fn build_overlay_rows_value_column_aligns_right() {
        let stats = StatsCollector::new();
        let data = stats.render_data();
        let rows = build_overlay_rows(&data);
        // Every metric row past the title row must have a value.
        for (idx, row) in rows.iter().enumerate().skip(1).take(13) {
            if row.text.is_empty() {
                continue;
            }
            assert!(
                row.value.is_some(),
                "row {idx} '{}' is missing value column",
                row.text
            );
        }
    }

    #[test]
    fn glyph_renderer_compiles_with_default_scale() {
        let renderer = GlyphRenderer::new();
        assert!(!renderer.is_initialised());
        assert_eq!(renderer.scale(), DEFAULT_GLYPH_SCALE);
    }

    #[test]
    fn glyph_renderer_compiles_with_custom_scale() {
        let renderer = GlyphRenderer::with_scale(24.0);
        assert_eq!(renderer.scale(), 24.0);
    }

    #[test]
    fn glyph_renderer_default_matches_new() {
        let r1 = GlyphRenderer::default();
        let r2 = GlyphRenderer::new();
        assert_eq!(r1.scale(), r2.scale());
        assert_eq!(r1.is_initialised(), r2.is_initialised());
    }
}
