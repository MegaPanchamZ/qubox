//! P0-3 + P0-5 shared frame carrier type.
//!
//! This module owns the cross-thread data type that flows from
//! [`RunningHwFrameDecoder`](crate::decoder_hw::RunningHwFrameDecoder)
//! to either the wgpu renderer or the legacy minifb renderer. It is
//! intentionally GPU-agnostic: no `wgpu::Device`, no `ffmpeg-next`
//! imports, no platform code. The struct is plain old data plus a
//! timestamp and the captured raw pixel bytes.
//!
//! ## Why this lives in its own module
//!
//! Both [`crate::render_wgpu`] and the legacy [`crate::main`] video
//! loop need to consume the same carrier; binding both to the decoder
//! module would create a circular import and force the wgpu types
//! into the build unit even when the wgpu feature is gated off. By
//! keeping the carrier free of platform-specific imports, the
//! `hw-decode` feature and the `--renderer wgpu` flag can each be
//! toggled independently.
//!
//! The two implementations of the carrier (`Owned` vs `GpuHandle`)
//! document the future zero-copy direction without forcing the work
//! today. The desktop cutover always materialises as `Owned(Vec<u8>)`
//! because the `RunningHwFrameDecoder` performs the DMA readback
//! (`av_hwframe_transfer_data`) before forwarding the frame.

use std::time::Instant;

/// Pixel layout carried by a [`DecodedFrame`]. One of the
/// `wgpu::TextureFormat`-compatible layouts the renderer can upload
/// without re-conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// Four bytes per pixel, byte order B, G, R, A in memory.
    /// Matches `wgpu::TextureFormat::Bgra8Unorm`.
    Bgra8Unorm,
    /// Four bytes per pixel, byte order R, G, B, A in memory.
    /// Matches `wgpu::TextureFormat::Rgba8Unorm`.
    Rgba8Unorm,
    /// Sixteen bytes per pixel (half-float RGBA), reserved for the
    /// 10-bit HDR cutover in Path 3 (ADR-010). Not used today.
    Rgba16Float,
}

impl PixelFormat {
    /// Bytes per pixel for this format.
    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            PixelFormat::Bgra8Unorm | PixelFormat::Rgba8Unorm => 4,
            PixelFormat::Rgba16Float => 8,
        }
    }
}

/// Backing storage for the decoded pixel buffer. Today only
/// [`PixelData::Owned`] is constructed; the `GpuHandle` variant
/// documents the direction for the future zero-copy work without
/// forcing it on this cutover.
#[derive(Debug, Clone)]
pub enum PixelData {
    /// Owned CPU-side buffer. Always used today.
    Owned(Vec<u8>),
    /// Reserved for a future iteration where the HW decoder hands the
    /// renderer a GPU-resident texture directly. Constructing this
    /// variant is currently a type-system error.
    GpuHandle,
}

impl PixelData {
    /// Borrow the pixel bytes. Returns `Some(&[u8])` for the
    /// `Owned` variant, `None` for the (unused) `GpuHandle`.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            PixelData::Owned(bytes) => Some(bytes.as_slice()),
            PixelData::GpuHandle => None,
        }
    }
}

/// One decoded video frame, ready to upload to a GPU texture or paint
/// into a CPU backbuffer. The struct is `Clone` so that the renderer
/// can hand a copy to its upload path without owning the decoder's
/// allocation; the `Cow`-style `PixelData` variant will eventually
/// let the two share a single allocation.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// Pixels wide.
    pub width: u32,
    /// Pixels tall.
    pub height: u32,
    /// Bytes per row of the pixel data. Equal to `width *
    /// format.bytes_per_pixel()` for tightly-packed frames; may be
    /// larger if the underlying decoder emits padded rows.
    pub bytes_per_row: u32,
    /// Pixel layout of the buffer.
    pub pixel_format: PixelFormat,
    /// Raw pixel bytes. Always `Owned(Vec<u8>)` today.
    pub data: PixelData,
    /// Time the decoder finished producing this frame. The renderer
    /// uses it to drive the frame-pacer decisions and to stamp the
    /// stats overlay.
    pub captured_at: Instant,
}

impl DecodedFrame {
    /// Bytes per row assuming tightly packed (no padding). This is a
    /// convenience used by the GPU upload path; the actual
    /// `bytes_per_row` may be larger.
    pub const fn packed_bytes_per_row(&self) -> u32 {
        self.width * self.pixel_format.bytes_per_pixel()
    }

    /// Validate that the pixel data length is consistent with
    /// `width`, `height`, `bytes_per_row`, and `pixel_format`.
    /// Returns `Ok(())` on success, an `anyhow::Error` describing the
    /// mismatch on failure.
    pub fn validate(&self) -> anyhow::Result<()> {
        let expected = self.bytes_per_row as usize * self.height as usize;
        let actual = self.data.as_bytes().map(|b| b.len()).ok_or_else(|| {
            anyhow::anyhow!("DecodedFrame::data is GpuHandle (not yet supported)")
        })?;
        if actual < expected {
            anyhow::bail!(
                "DecodedFrame payload under-sized: expected {expected} bytes ({}x{} stride {}) \
                 but data has {actual} bytes",
                self.width,
                self.height,
                self.bytes_per_row
            );
        }
        Ok(())
    }

    /// Convert this frame to minifb's 0x00RRGGBB `u32` representation
    /// if the format is BGRA8. Used by the legacy minifb renderer
    /// that the cutover retains as a safety net. Returns `None` for
    /// any other pixel format.
    pub fn to_minifb_pixels(&self) -> anyhow::Result<Vec<u32>> {
        let bytes = self.data.as_bytes().ok_or_else(|| {
            anyhow::anyhow!("GpuHandle payload cannot be converted to minifb pixels")
        })?;
        if self.pixel_format != PixelFormat::Bgra8Unorm {
            anyhow::bail!(
                "to_minifb_pixels requires PixelFormat::Bgra8Unorm, got {:?}",
                self.pixel_format
            );
        }
        if self.bytes_per_row != self.packed_bytes_per_row() {
            anyhow::bail!(
                "to_minifb_pixels requires tightly-packed rows: bytes_per_row={} packed={}",
                self.bytes_per_row,
                self.packed_bytes_per_row()
            );
        }
        Ok(bytes
            .chunks_exact(4)
            .map(|pixel| {
                let blue = u32::from(pixel[0]);
                let green = u32::from(pixel[1]);
                let red = u32::from(pixel[2]);
                (red << 16) | (green << 8) | blue
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bgra(width: u32, height: u32) -> DecodedFrame {
        let stride = width * 4;
        let mut bytes = Vec::with_capacity((stride * height) as usize);
        for _ in 0..(stride * height) {
            bytes.push(0);
        }
        DecodedFrame {
            width,
            height,
            bytes_per_row: stride,
            pixel_format: PixelFormat::Bgra8Unorm,
            data: PixelData::Owned(bytes),
            captured_at: Instant::now(),
        }
    }

    #[test]
    fn pixel_format_bytes_per_pixel_is_correct() {
        assert_eq!(PixelFormat::Bgra8Unorm.bytes_per_pixel(), 4);
        assert_eq!(PixelFormat::Rgba8Unorm.bytes_per_pixel(), 4);
        assert_eq!(PixelFormat::Rgba16Float.bytes_per_pixel(), 8);
    }

    #[test]
    fn validate_accepts_well_formed_bgra_frame() {
        let frame = make_bgra(64, 48);
        frame
            .validate()
            .expect("well-formed BGRA frame should validate");
    }

    #[test]
    fn validate_rejects_undersized_payload() {
        let mut frame = make_bgra(64, 48);
        frame.data = PixelData::Owned(vec![
            0_u8;
            (frame.bytes_per_row * frame.height) as usize / 2
        ]);
        assert!(frame.validate().is_err());
    }

    #[test]
    fn to_minifb_pixels_respects_byte_order() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[10, 20, 30, 255]);
        bytes.extend_from_slice(&[200, 150, 100, 255]);
        let frame = DecodedFrame {
            width: 2,
            height: 1,
            bytes_per_row: 8,
            pixel_format: PixelFormat::Bgra8Unorm,
            data: PixelData::Owned(bytes),
            captured_at: Instant::now(),
        };
        let pixels = frame
            .to_minifb_pixels()
            .expect("BGRA8 -> u32 should succeed");
        assert_eq!(pixels.len(), 2);
        assert_eq!(
            pixels[0], 0x001E140A,
            "expected 0x00RRGGBB from BGRA (10,20,30,255)"
        );
        assert_eq!(
            pixels[1], 0x006496C8,
            "expected 0x00RRGGBB from BGRA (200,150,100,255)"
        );
    }

    #[test]
    fn to_minifb_pixels_rejects_rgba() {
        let frame = DecodedFrame {
            width: 4,
            height: 4,
            bytes_per_row: 16,
            pixel_format: PixelFormat::Rgba8Unorm,
            data: PixelData::Owned(vec![0_u8; 64]),
            captured_at: Instant::now(),
        };
        assert!(frame.to_minifb_pixels().is_err());
    }

    #[test]
    fn cloned_frame_shares_owned_payload() {
        let frame = make_bgra(8, 8);
        let clone = frame.clone();
        assert_eq!(frame.width, clone.width);
        assert_eq!(frame.height, clone.height);
        assert_eq!(frame.bytes_per_row, clone.bytes_per_row);
        assert_eq!(frame.pixel_format, clone.pixel_format);
    }

    #[test]
    fn packed_bytes_per_row_matches_format() {
        let frame = DecodedFrame {
            width: 1920,
            height: 1080,
            bytes_per_row: 1920 * 4,
            pixel_format: PixelFormat::Bgra8Unorm,
            data: PixelData::Owned(vec![0_u8; 1920 * 1080 * 4]),
            captured_at: Instant::now(),
        };
        assert_eq!(frame.packed_bytes_per_row(), 1920 * 4);
    }
}
