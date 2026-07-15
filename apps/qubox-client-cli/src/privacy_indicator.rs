//! Privacy indicator — paints red borders/overlays on stream regions.
//!
//! Used by both the tiled view (red cell border) and the single-stream
//! view (red fullscreen overlay).

use minifb::Window;
use qubox_display::{Point, Rect, Size};

/// Thickness of the red border in pixels.
const BORDER_THICKNESS: u32 = 4;

/// Red color in minifb's 0x00RRGGBB format.
const RED: u32 = 0x00FF0000;

/// Semi-transparent red overlay: blends red at 30% alpha.
pub fn apply_red_overlay(frame: &mut [u32]) {
    for pixel in frame.iter_mut() {
        let r = (*pixel >> 16) & 0xFF;
        let g = (*pixel >> 8) & 0xFF;
        let b = *pixel & 0xFF;
        let nr = ((r as f32 * 0.7) + (255.0 * 0.3)) as u32;
        let ng = (g as f32 * 0.7) as u32;
        let nb = (b as f32 * 0.7) as u32;
        *pixel = (nr.min(255) << 16) | (ng.min(255) << 8) | nb.min(255);
    }
}

/// Paint a red border around a cell rectangle in a pixel buffer.
pub fn paint_red_border(buffer: &mut [u32], stride: usize, cell: &Rect, thickness: u32) {
    let x0 = cell.origin.x.max(0) as usize;
    let y0 = cell.origin.y.max(0) as usize;
    let x1 = (cell.origin.x + cell.size.width as i32).max(0) as usize;
    let y1 = (cell.origin.y + cell.size.height as i32).max(0) as usize;
    let t = thickness as usize;

    // Top edge
    for y in y0..y0.saturating_add(t).min(y1) {
        for x in x0..x1 {
            if let Some(p) = buffer.get_mut(y * stride + x) {
                *p = RED;
            }
        }
    }
    // Bottom edge
    for y in y1.saturating_sub(t)..y1 {
        for x in x0..x1 {
            if let Some(p) = buffer.get_mut(y * stride + x) {
                *p = RED;
            }
        }
    }
    // Left edge
    for y in y0..y1 {
        for x in x0..x0.saturating_add(t).min(x1) {
            if let Some(p) = buffer.get_mut(y * stride + x) {
                *p = RED;
            }
        }
    }
    // Right edge
    for y in y0..y1 {
        for x in x1.saturating_sub(t)..x1 {
            if let Some(p) = buffer.get_mut(y * stride + x) {
                *p = RED;
            }
        }
    }
}

/// Paint a semi-transparent red fullscreen overlay via minifb window updates.
/// This creates a small overlay window (alternative: paint directly into frame).
pub fn paint_fullscreen_red_overlay(window: &mut Window) {
    let (w, h) = window.get_size();
    let mut overlay = vec![RED; w * h];
    // Semi-transparent: draw a pattern
    for pixel in overlay.iter_mut() {
        let r = (*pixel >> 16) & 0xFF;
        let g = (*pixel >> 8) & 0xFF;
        let b = *pixel & 0xFF;
        let nr = ((r as f32 * 0.3) + (255.0 * 0.0)) as u32; // keep dark
        let ng = (g as f32 * 0.3) as u32;
        let nb = (b as f32 * 0.3) as u32;
        *pixel = (nr.min(255) << 16) | (ng.min(255) << 8) | nb.min(255);
    }
    let _ = window.update_with_buffer(&overlay, w, h);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_red_border_paints_red_pixels() {
        let mut buffer = vec![0u32; 100 * 100];
        let cell = Rect {
            origin: Point { x: 10, y: 10 },
            size: Size {
                width: 20,
                height: 20,
            },
        };
        paint_red_border(&mut buffer, 100, &cell, BORDER_THICKNESS);

        // Top-left corner pixel should be red
        assert_eq!(buffer[10 * 100 + 10], RED);
        // Pixel just inside the border (past thickness) should be black
        assert_eq!(buffer[(10 + 5) * 100 + (10 + 5)], 0);
        // Bottom-right corner pixel should be red
        assert_eq!(buffer[(10 + 19) * 100 + (10 + 19)], RED);
        // Pixel outside the cell should be black
        assert_eq!(buffer[9 * 100 + 10], 0);
    }

    #[test]
    fn apply_red_overlay_modifies_pixels() {
        let mut frame = vec![0x00FFFFFFu32; 100]; // white frame
        apply_red_overlay(&mut frame);
        // After blending with red at 0.3 alpha: each channel should be ~ (255*0.7) + (255*0.3*channel_select)
        // For red: 255*0.7 + 255*0.3 = 255 (full red)
        // For green: 255*0.7 + 0*0.3 = 178.5
        // For blue: 255*0.7 + 0*0.3 = 178.5
        let pixel = frame[0];
        let r = (pixel >> 16) & 0xFF;
        let g = (pixel >> 8) & 0xFF;
        let b = pixel & 0xFF;
        assert_eq!(r, 255, "red channel should stay 255");
        assert!(g < 200, "green channel should be reduced: got {g}");
        assert!(b < 200, "blue channel should be reduced: got {b}");
    }
}
