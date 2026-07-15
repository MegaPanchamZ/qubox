//! Windows D3D11 shared-texture surface implementation.
//!
//! Wraps an ID3D11Texture2D with a shared NT handle for zero-copy
//! handoff to D3D11VA / DX12 / wgpu.

use crate::error::CaptureError;
use crate::surface::{GpuSurface, SurfaceResult};

/// A GPU surface backed by a D3D11 shared texture.
pub struct D3D11Surface {
    pub raw_texture: *mut std::ffi::c_void,
    pub raw_handle: i64,
    pub width: u32,
    pub height: u32,
    pub format: u32,
}

unsafe impl Send for D3D11Surface {}
unsafe impl Sync for D3D11Surface {}

impl D3D11Surface {
    pub fn new(
        raw_texture: *mut std::ffi::c_void,
        raw_handle: i64,
        width: u32,
        height: u32,
        format: u32,
    ) -> Self {
        Self {
            raw_texture,
            raw_handle,
            width,
            height,
            format,
        }
    }
}

impl GpuSurface for D3D11Surface {
    fn as_raw_handle(&self) -> i64 {
        self.raw_handle
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
        // NT shared handles are already shareable across processes/threads.
        // Full AddRef of the ID3D11Texture2D is deferred to ADR-016 DXGI path;
        // handle value is duplicated by value for encoder handoff.
        Box::new(Self {
            raw_texture: self.raw_texture,
            raw_handle: self.raw_handle,
            width: self.width,
            height: self.height,
            format: self.format,
        })
    }

    fn import_to_wgpu(&self, _device: &wgpu::Device) -> SurfaceResult<wgpu::Texture> {
        Err(CaptureError::NotSupported(
            "D3D11Surface::import_to_wgpu requires DX12 OpenSharedHandle via wgpu_hal (ADR-016)",
        ))
    }
}
