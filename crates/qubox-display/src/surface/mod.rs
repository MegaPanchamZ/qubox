//! Cross-platform GPU surface handles for zero-copy encoder handoff.
//!
//! Defines [`GpuSurface`], the uniform interface for platform-specific
//! GPU buffer types (DMA-BUF, D3D11 shared textures, IOSurface).
//! Platform implementations live in sibling modules behind `#[cfg]` gates.

use crate::error::CaptureError;

pub type SurfaceResult<T> = Result<T, CaptureError>;

/// Static metadata for a GPU surface: dimensions, layout, pixel format.
#[derive(Debug, Clone, Copy)]
pub struct SurfaceDescriptor {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// DRM fourcc / DXGI format / CV pixel format code packed as u32.
    pub format: u32,
    /// Number of planes (1 for BGRA, 2 for NV12, 3 for YUV444).
    pub planes: u32,
    /// Per-plane byte strides; `plane_strides[0]` is luma stride.
    pub plane_strides: [u32; 4],
    /// Per-plane byte offsets within the same DMA-BUF / texture.
    pub plane_offsets: [u32; 4],
}

/// A cross-platform GPU buffer handle suitable for zero-copy handoff
/// to a hardware encoder.
pub trait GpuSurface: Send + Sync {
    /// Platform-specific raw handle value.
    /// Linux: DMA-BUF fd; Windows: NT handle; macOS: IOSurfaceID.
    fn as_raw_handle(&self) -> i64;

    /// Width in pixels.
    fn width(&self) -> u32;

    /// Height in pixels.
    fn height(&self) -> u32;

    /// Pixel format fourcc code (DRM_FORMAT_*, DXGI_FORMAT_*, kCVPixelFormat*).
    fn format(&self) -> u32;

    /// Create an independent clone for handoff to the encoder thread.
    /// The clone shares the same GPU backing memory.
    fn clone_for_encoder(&self) -> Box<dyn GpuSurface>;

    /// Import into wgpu as a GPU texture for zero-copy readback.
    // TODO(adr-016): wire wgpu_hal::vulkan::Device::texture_from_raw
    fn import_to_wgpu(&self, device: &wgpu::Device) -> SurfaceResult<wgpu::Texture>;
}

// Platform-specific surface implementations
#[cfg(all(target_os = "linux", feature = "pipewire-zero-copy"))]
pub mod linux;
#[cfg(all(target_os = "windows", feature = "dxgi-zero-copy"))]
pub mod windows;
#[cfg(all(target_os = "macos", feature = "screencapturekit-zero-copy"))]
pub mod macos;

/// Mock surface for testing without GPU hardware.
pub struct MockSurface {
    handle: i64,
    w: u32,
    h: u32,
    fmt: u32,
}

impl MockSurface {
    pub fn new(width: u32, height: u32, format: u32) -> Self {
        Self { handle: 0, w: width, h: height, fmt: format }
    }

    pub fn with_handle(handle: i64, width: u32, height: u32, format: u32) -> Self {
        Self { handle, w: width, h: height, fmt: format }
    }
}

impl GpuSurface for MockSurface {
    fn as_raw_handle(&self) -> i64 {
        self.handle
    }

    fn width(&self) -> u32 {
        self.w
    }

    fn height(&self) -> u32 {
        self.h
    }

    fn format(&self) -> u32 {
        self.fmt
    }

    fn clone_for_encoder(&self) -> Box<dyn GpuSurface> {
        Box::new(Self { handle: 0, w: self.w, h: self.h, fmt: self.fmt })
    }

    fn import_to_wgpu(&self, _device: &wgpu::Device) -> SurfaceResult<wgpu::Texture> {
        Err(CaptureError::NotSupported("mock surface has no GPU backing"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_surface_trait_object_dispatch() {
        let surface: Box<dyn GpuSurface> = Box::new(MockSurface::new(1920, 1080, 0x34324252));
        assert_eq!(surface.width(), 1920);
        assert_eq!(surface.height(), 1080);
        assert_eq!(surface.format(), 0x34324252);
        // import_to_wgpu requires a real wgpu device — tested in integration
    }

    #[test]
    fn mock_surface_clone_for_encoder() {
        let surface = MockSurface::new(3840, 2160, 0x34324252);
        let cloned: Box<dyn GpuSurface> = surface.clone_for_encoder();
        assert_eq!(cloned.width(), 3840);
        assert_eq!(cloned.height(), 2160);
    }

    #[test]
    fn mock_surface_size_format_accessors() {
        let s = MockSurface::with_handle(42, 640, 480, 0x34324252);
        assert_eq!(s.width(), 640);
        assert_eq!(s.height(), 480);
        assert_eq!(s.format(), 0x34324252);
        assert_eq!(s.as_raw_handle(), 42);
    }

    #[test]
    fn gpu_surface_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<Box<dyn GpuSurface>>();
        assert_sync::<Box<dyn GpuSurface>>();
    }

    #[test]
    fn mock_surface_default_handle_is_zero() {
        let s = MockSurface::new(1024, 768, 0x34324252);
        assert_eq!(s.as_raw_handle(), 0);
    }
}
