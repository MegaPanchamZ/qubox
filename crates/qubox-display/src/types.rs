use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Unique identifier for a display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DisplayId(pub u32);

impl DisplayId {
    pub const fn primary() -> Self {
        Self(0)
    }

    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// A 2D point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Point<T> {
    pub x: T,
    pub y: T,
}

/// A 2D size (width × height).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Size<T> {
    pub width: T,
    pub height: T,
}

/// A rectangle, with origin (top-left) in signed integer coords and
/// size in unsigned integer pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    pub origin: Point<i32>,
    pub size: Size<u32>,
}

/// Full metadata about a single display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: DisplayId,
    /// Friendly name (e.g. "DP-1", "\\.\DISPLAY1", "DELL U2723QE" from EDID).
    pub name: String,
    /// Position of this display's top-left corner in the OS virtual desktop
    /// coordinate space. Negative values are valid (display left of primary).
    pub position: Point<i32>,
    /// Native resolution in physical pixels.
    pub size: Size<u32>,
    /// Current refresh rate in Hz.
    pub refresh_hz: f32,
    /// HiDPI / Retina scale factor (1.0 = standard DPI, 2.0 = Retina).
    pub scale_factor: f32,
    /// The native color space of the display.
    pub color_space: ColorSpaceId,
    /// Whether the display and its driver support HDR output.
    pub hdr_capable: bool,
    /// Whether this display is a virtual (software-created) display
    /// vs a physical monitor.
    pub is_virtual: bool,
}

/// Canonical color space identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColorSpaceId {
    /// sRGB / BT.709. Default.
    Srgb,
    /// scRGB (linear extended sRGB, Windows HDR path).
    Scrgb,
    /// Display P3 (Apple HDR path, macOS).
    DisplayP3,
    /// HDR10 (BT.2020 ST.2084 SMPTE 2084).
    Hdr10,
}

impl ColorSpaceId {
    /// QuickTime / Apple color space string for CMSampleBuffer creation.
    pub fn as_quicktime_codec_string(self) -> &'static str {
        match self {
            ColorSpaceId::Srgb => "sRGB",
            ColorSpaceId::Scrgb => "1-1-1",
            ColorSpaceId::DisplayP3 => "1-5-1",
            ColorSpaceId::Hdr10 => "1-9-9-10",
        }
    }
}

/// Options for opening a capture session.
#[derive(Debug, Clone)]
pub struct CaptureOptions {
    /// Optional sub-region of the display to capture. None = full display.
    pub region: Option<Rect>,
    /// Desired color space. None = use the display's native color space.
    pub color_space: Option<ColorSpaceId>,
    /// Target capture frame rate.
    pub target_fps: u32,
    /// Whether to include the OS cursor in the captured frame.
    pub capture_cursor: bool,
}

/// A single captured frame from a display.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub display_id: DisplayId,
    pub width: u32,
    pub height: u32,
    /// The pixel data, owned as an Arc<Vec<u8>> so it can be shared
    /// between the capture thread and the encoder pipeline.
    pub bytes: Arc<Vec<u8>>,
    pub format: PixelFormat,
    pub captured_at: Instant,
    pub frame_index: u64,
}

/// Supported pixel formats from capture backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PixelFormat {
    /// 8-bit BGRA (byte[0]=B, byte[1]=G, byte[2]=R, byte[3]=A).
    Bgra8,
    /// 8-bit YUV 4:2:0 semi-planar (preferred by encoders).
    Nv12,
    /// 16-bit-per-channel RGBA floating point (scRGB / HDR path).
    Rgba16F,
}

/// Display lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayState {
    /// Normal operation: the display shows whatever the desktop environment renders.
    Active,
    /// Privacy mode: the display is blanked (DPMS off or overlay) and the game
    /// has been moved to a virtual display.
    Privacy,
    /// The display is off — DPMS power-off, disconnected, or GPU-reset.
    Blanked,
}

/// Configuration for creating a virtual display.
#[derive(Debug, Clone)]
pub struct VirtualDisplayConfig {
    pub name: String,
    pub size: Size<u32>,
    pub refresh_hz: f32,
    pub color_space: ColorSpaceId,
    pub position: Point<i32>,
}

/// Capabilities reported by a CaptureBackend.
#[derive(Debug, Clone, Default)]
pub struct BackendCapabilities {
    pub supports_hdr: bool,
    pub supports_scrgb: bool,
    pub supports_virtual_display: bool,
    pub max_refresh_hz: f32,
    pub supported_formats: Vec<PixelFormat>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_id_primary_returns_id_0() {
        assert_eq!(DisplayId::primary(), DisplayId(0));
        assert_eq!(DisplayId::primary().as_u32(), 0);
    }

    #[test]
    fn color_space_id_as_quicktime_codec_string() {
        assert_eq!(ColorSpaceId::Srgb.as_quicktime_codec_string(), "sRGB");
        assert_eq!(ColorSpaceId::Scrgb.as_quicktime_codec_string(), "1-1-1");
        assert_eq!(ColorSpaceId::DisplayP3.as_quicktime_codec_string(), "1-5-1");
        assert_eq!(ColorSpaceId::Hdr10.as_quicktime_codec_string(), "1-9-9-10");
    }
}
