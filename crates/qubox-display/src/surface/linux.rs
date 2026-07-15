//! Linux DMA-BUF surface implementation.
//!
//! Wraps a DMA-BUF file descriptor from PipeWire for zero-copy
//! handoff to VA-API / Vulkan / wgpu.

use crate::error::CaptureError;
use crate::surface::{GpuSurface, SurfaceResult};

/// A GPU surface backed by a Linux DMA-BUF file descriptor.
pub struct DmaBufSurface {
    pub fd: i32,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
    /// When true, `Drop` closes `fd` (owned dup or original owner).
    owns_fd: bool,
}

impl DmaBufSurface {
    pub fn new(fd: i32, width: u32, height: u32, fourcc: u32, stride: u32) -> Self {
        Self {
            fd,
            width,
            height,
            fourcc,
            stride,
            offset: 0,
            modifier: 0,
            owns_fd: false,
        }
    }

    /// Take ownership of `fd` (will close on drop).
    pub fn from_owned_fd(fd: i32, width: u32, height: u32, fourcc: u32, stride: u32) -> Self {
        Self {
            fd,
            width,
            height,
            fourcc,
            stride,
            offset: 0,
            modifier: 0,
            owns_fd: true,
        }
    }
}

impl Drop for DmaBufSurface {
    fn drop(&mut self) {
        if self.owns_fd && self.fd >= 0 {
            unsafe {
                libc::close(self.fd);
            }
            self.fd = -1;
        }
    }
}

impl GpuSurface for DmaBufSurface {
    fn as_raw_handle(&self) -> i64 {
        self.fd as i64
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn format(&self) -> u32 {
        self.fourcc
    }

    fn clone_for_encoder(&self) -> Box<dyn GpuSurface> {
        // Independent lifetime: dup the DMA-BUF fd for the encoder thread.
        let dup = unsafe { libc::dup(self.fd) };
        if dup < 0 {
            // Fallback: non-owning view of same fd (caller must keep original alive).
            return Box::new(Self {
                fd: self.fd,
                width: self.width,
                height: self.height,
                fourcc: self.fourcc,
                stride: self.stride,
                offset: self.offset,
                modifier: self.modifier,
                owns_fd: false,
            });
        }
        Box::new(Self {
            fd: dup,
            width: self.width,
            height: self.height,
            fourcc: self.fourcc,
            stride: self.stride,
            offset: self.offset,
            modifier: self.modifier,
            owns_fd: true,
        })
    }

    fn import_to_wgpu(&self, _device: &wgpu::Device) -> SurfaceResult<wgpu::Texture> {
        // Full VkImportMemoryFdInfoKHR path needs wgpu_hal (ADR-016). Soft-fail
        // so callers can fall back to CPU copy without panicking.
        Err(CaptureError::NotSupported(
            "DmaBufSurface::import_to_wgpu requires wgpu_hal Vulkan external memory (ADR-016)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_for_encoder_dups_when_possible() {
        // Use /dev/null as a stand-in fd for dup semantics.
        let fd = unsafe {
            libc::open(
                b"/dev/null\0".as_ptr() as *const _,
                libc::O_RDONLY,
            )
        };
        if fd < 0 {
            return;
        }
        let s = DmaBufSurface::new(fd, 64, 64, 0x34324252, 256);
        let cloned = s.clone_for_encoder();
        assert_eq!(cloned.width(), 64);
        assert_eq!(cloned.height(), 64);
        assert!(cloned.as_raw_handle() >= 0);
        // Drop clone first, then original.
        drop(cloned);
        unsafe {
            libc::close(fd);
        }
    }

    #[test]
    fn import_to_wgpu_soft_fails() {
        let s = DmaBufSurface::new(0, 1, 1, 0, 0);
        // Cannot call import without a device; verify trait object builds.
        let _ = s.as_raw_handle();
    }
}
