//! Tiled view — renders all display streams in a single window as a grid.
//!
//! Uses `minifb` (matching the existing single-stream window) for rendering.
//! Each stream occupies a cell in the grid layout. The privacy indicator
//! paints a red border around cells whose stream is in `Privacy` state.

use std::collections::HashMap;

use minifb::{Window, WindowOptions};
use qubox_display::{DisplayId, DisplayState, Rect, Size};

use crate::stream_registry::StreamRegistry;

/// Grid layout derived from the number of streams.
#[derive(Debug, Clone, Copy)]
pub struct GridLayout {
    pub rows: u32,
    pub cols: u32,
    pub cell_width: u32,
    pub cell_height: u32,
    pub padding: u32,
}

impl GridLayout {
    /// Compute the grid layout for `n` streams at a given total window size.
    pub fn compute(n: usize, total_width: u32, total_height: u32, padding: u32) -> Self {
        if n == 0 {
            return Self {
                rows: 1,
                cols: 1,
                cell_width: total_width,
                cell_height: total_height,
                padding,
            };
        }
        let (cols, rows) = match n {
            1 => (1, 1),
            2 => (2, 1),
            3..=4 => (2, 2),
            5..=6 => (3, 2),
            7..=9 => (3, 3),
            10..=12 => (4, 3),
            _ => {
                let cols = (n as f64).sqrt().ceil() as u32;
                let rows = ((n as f64) / cols as f64).ceil() as u32;
                (cols, rows)
            }
        };
        let cell_width = (total_width - padding * (cols + 1)) / cols;
        let cell_height = (total_height - padding * (rows + 1)) / rows;
        Self {
            rows,
            cols,
            cell_width,
            cell_height,
            padding,
        }
    }

    /// Get the pixel rectangle for cell at (row, col).
    pub fn cell_rect(&self, row: u32, col: u32) -> Rect {
        let x = self.padding as i32 + col as i32 * (self.cell_width as i32 + self.padding as i32);
        let y = self.padding as i32 + row as i32 * (self.cell_height as i32 + self.padding as i32);
        Rect {
            origin: qubox_display::Point { x, y },
            size: Size {
                width: self.cell_width,
                height: self.cell_height,
            },
        }
    }
}

/// Manages a single minifb window that renders all streams in a grid.
pub struct TiledView {
    pub window: Window,
    stream_registry: StreamRegistry,
    pub grid_layout: GridLayout,
    /// Stores the latest rendered frame for each stream (BGRA u32 pixels).
    stream_buffers: HashMap<DisplayId, Vec<u32>>,
    /// Cached per-stream dimensions for resizing.
    stream_sizes: HashMap<DisplayId, (usize, usize)>,
}

/// Error type for tiled view operations.
#[derive(Debug)]
pub enum TiledViewError {
    Window(String),
    NoStreams,
    BufferSize(DisplayId, u32, u32),
}

impl std::fmt::Display for TiledViewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TiledViewError::Window(msg) => write!(f, "window creation failed: {msg}"),
            TiledViewError::NoStreams => write!(f, "no streams available"),
            TiledViewError::BufferSize(id, w, h) => {
                write!(
                    f,
                    "buffer size mismatch for display {id:?}: expected {w}x{h}"
                )
            }
        }
    }
}

impl std::error::Error for TiledViewError {}

impl TiledView {
    /// Create the tiled view window. Defaults to 1920x1080 if no streams exist yet.
    pub fn new(stream_registry: StreamRegistry) -> Result<Self, TiledViewError> {
        let window = Window::new("qubox tiled view", 1920, 1080, WindowOptions::default())
            .map_err(|e| TiledViewError::Window(e.to_string()))?;

        let streams = stream_registry.list_streams();
        let grid_layout = GridLayout::compute(streams.len(), 1920, 1080, 4);

        Ok(Self {
            window,
            stream_registry,
            grid_layout,
            stream_buffers: HashMap::new(),
            stream_sizes: HashMap::new(),
        })
    }

    /// Render a frame from a specific stream into the tiled view.
    /// `frame` should be BGRA u32 pixels (matching minifb's native format).
    pub fn render_frame(
        &mut self,
        display_id: DisplayId,
        frame: &[u32],
        width: u32,
        height: u32,
    ) -> Result<(), TiledViewError> {
        self.stream_buffers.insert(display_id, frame.to_vec());
        self.stream_sizes
            .insert(display_id, (width as usize, height as usize));
        self.redraw()
    }

    /// Redraw the entire tiled window from cached buffers.
    pub fn redraw(&mut self) -> Result<(), TiledViewError> {
        let (win_w, win_h) = {
            let (w, h) = self.window.get_size();
            (w as u32, h as u32)
        };
        let streams = self.stream_registry.list_streams();
        let layout = GridLayout::compute(streams.len(), win_w, win_h, 4);
        self.grid_layout = layout;

        let mut backbuffer = vec![0u32; (win_w * win_h) as usize];

        for (idx, entry) in streams.iter().enumerate() {
            let row = idx as u32 / layout.cols;
            let col = idx as u32 % layout.cols;
            let cell = layout.cell_rect(row, col);
            let cw = cell.size.width as usize;
            let ch = cell.size.height as usize;

            // Get the cached frame buffer for this stream, or use a black screen
            let src_frame = self
                .stream_buffers
                .get(&entry.display_id)
                .map(|v| v.as_slice())
                .unwrap_or_else(|| &[]);

            // Copy (or letterbox) the source frame into the cell
            for cy in 0..ch {
                for cx in 0..cw {
                    let src_idx = if !src_frame.is_empty() {
                        let src_w = self
                            .stream_sizes
                            .get(&entry.display_id)
                            .map(|(w, _)| *w)
                            .unwrap_or(cw);
                        let src_h = self
                            .stream_sizes
                            .get(&entry.display_id)
                            .map(|(_, h)| *h)
                            .unwrap_or(ch);
                        // Scale the cell coordinates to source coordinates
                        let sx = (cx as f64 / cw as f64 * src_w as f64) as usize;
                        let sy = (cy as f64 / ch as f64 * src_h as f64) as usize;
                        sy * src_w + sx
                    } else {
                        0
                    };

                    let pixel = src_frame.get(src_idx).copied().unwrap_or(0);
                    let dst_idx = (cell.origin.y as usize + cy) * win_w as usize
                        + (cell.origin.x as usize + cx);
                    if dst_idx < backbuffer.len() {
                        backbuffer[dst_idx] = pixel;
                    }
                }
            }

            // ── Privacy indicator: red 4px border ──
            let show_indicator = self.stream_registry.should_show_privacy_indicator();
            if show_indicator && entry.privacy_state == DisplayState::Privacy {
                draw_red_border(&mut backbuffer, win_w as usize, &cell, 4);
            }
        }

        self.window
            .update_with_buffer(&backbuffer, win_w as usize, win_h as usize)
            .map_err(|e| TiledViewError::Window(e.to_string()))?;

        Ok(())
    }

    /// Is the window still open?
    pub fn is_open(&self) -> bool {
        self.window.is_open()
    }
}

/// Draw a red border around a cell rectangle in the backbuffer.
fn draw_red_border(buffer: &mut [u32], stride: usize, cell: &Rect, thickness: u32) {
    let x0 = cell.origin.x.max(0) as usize;
    let y0 = cell.origin.y.max(0) as usize;
    let x1 = (cell.origin.x + cell.size.width as i32).max(0) as usize;
    let y1 = (cell.origin.y + cell.size.height as i32).max(0) as usize;
    let t = thickness as usize;

    // Red pixel (0x00FF0000 in minifb's 0x00RRGGBB format)
    let red: u32 = 0x00FF0000;

    // Top edge
    for y in y0..y0.saturating_add(t).min(y1) {
        for x in x0..x1 {
            if let Some(p) = buffer.get_mut(y * stride + x) {
                *p = red;
            }
        }
    }
    // Bottom edge
    for y in y1.saturating_sub(t)..y1 {
        for x in x0..x1 {
            if let Some(p) = buffer.get_mut(y * stride + x) {
                *p = red;
            }
        }
    }
    // Left edge
    for y in y0..y1 {
        for x in x0..x0.saturating_add(t).min(x1) {
            if let Some(p) = buffer.get_mut(y * stride + x) {
                *p = red;
            }
        }
    }
    // Right edge
    for y in y0..y1 {
        for x in x1.saturating_sub(t)..x1 {
            if let Some(p) = buffer.get_mut(y * stride + x) {
                *p = red;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_layout_compute_4_returns_2x2() {
        let layout = GridLayout::compute(4, 1920, 1080, 4);
        assert_eq!(layout.cols, 2);
        assert_eq!(layout.rows, 2);
        let cell = layout.cell_rect(0, 0);
        assert!(cell.size.width > 0);
        assert!(cell.size.height > 0);
    }

    #[test]
    fn grid_layout_compute_2_returns_2x1() {
        let layout = GridLayout::compute(2, 1920, 1080, 4);
        assert_eq!(layout.cols, 2);
        assert_eq!(layout.rows, 1);
    }

    #[test]
    fn grid_layout_compute_1_returns_1x1() {
        let layout = GridLayout::compute(1, 1920, 1080, 4);
        assert_eq!(layout.cols, 1);
        assert_eq!(layout.rows, 1);
    }

    #[test]
    fn grid_layout_compute_0_returns_1x1() {
        let layout = GridLayout::compute(0, 1920, 1080, 4);
        assert_eq!(layout.cols, 1);
        assert_eq!(layout.rows, 1);
    }

    #[test]
    fn cell_rects_are_non_overlapping() {
        let layout = GridLayout::compute(4, 1920, 1080, 4);
        let rects: Vec<Rect> = (0..2)
            .flat_map(|row| (0..2).map(move |col| layout.cell_rect(row, col)))
            .collect();
        // Check no two rects overlap (simplified: check origin difference)
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                let a = &rects[i];
                let b = &rects[j];
                let no_overlap = a.origin.x + a.size.width as i32 <= b.origin.x
                    || b.origin.x + b.size.width as i32 <= a.origin.x
                    || a.origin.y + a.size.height as i32 <= b.origin.y
                    || b.origin.y + b.size.height as i32 <= a.origin.y;
                assert!(no_overlap, "rects {:?} and {:?} overlap", a, b);
            }
        }
    }

    #[test]
    fn draw_red_border_paints_red_pixels() {
        let mut buffer = vec![0u32; 100 * 100];
        let cell = Rect {
            origin: qubox_display::Point { x: 10, y: 10 },
            size: Size {
                width: 20,
                height: 20,
            },
        };
        draw_red_border(&mut buffer, 100, &cell, 2);

        // Check top-left corner pixel is red
        assert_eq!(buffer[10 * 100 + 10], 0x00FF0000);
        // Check a pixel inside the cell (not on border) is black
        assert_eq!(buffer[15 * 100 + 15], 0);
        // Check bottom-right corner
        assert_eq!(buffer[(10 + 19) * 100 + (10 + 19)], 0x00FF0000);
    }
}
