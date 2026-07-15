//! macOS IOSurface surface implementation.
//!
//! Wraps an IOSurfaceRef (from ScreenCaptureKit) for zero-copy
//! handoff to VideoToolbox / Metal. VideoToolbox encodes directly
//! from the IOSurface, bypassing ffmpeg.

use crate::error::CaptureError;
use crate::surface::{GpuSurface, SurfaceResult};

/// A GPU surface backed by a macOS IOSurface.
pub struct IoSurfaceSurface {
    pub surface: *mut std::ffi::c_void,
    pub width: u32,
    pub height: u32,
    pub format: u32,
}

unsafe impl Send for IoSurfaceSurface {}
unsafe impl Sync for IoSurfaceSurface {}

impl IoSurfaceSurface {
    pub fn new(surface: *mut std::ffi::c_void, width: u32, height: u32, format: u32) -> Self {
        Self {
            surface,
            width,
            height,
            format,
        }
    }
}

impl GpuSurface for IoSurfaceSurface {
    fn as_raw_handle(&self) -> i64 {
        self.surface as i64
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn format(&self) -> u32 {
        self.format
    }

    fn clone_for_encoder(&self) -> Box<dyn GpuSurface> {
        // IOSurface is reference-counted; CFRetain is wired when SCK feature
        // links CoreFoundation. Until then, share the raw pointer for handoff
        // (caller must keep the parent session alive).
        Box::new(Self {
            surface: self.surface,
            width: self.width,
            height: self.height,
            format: self.format,
        })
    }

    fn import_to_wgpu(&self, _device: &wgpu::Device) -> SurfaceResult<wgpu::Texture> {
        Err(CaptureError::NotSupported(
            "IoSurfaceSurface::import_to_wgpu requires Metal external texture (ADR-016)",
        ))
    }
}
