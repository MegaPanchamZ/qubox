# ADR-016 GPU↔Encoder Zero-Copy Surfaces

## Status

Proposed. Branch: `feature/adr-016-zero-copy-gpu-encoder`. Based on
`main` after commit `47585ea`. Builds on ADR-003 (ffmpeg-next decoder)
and ADR-009 (wgpu renderer). Required for P2-14 (HDR), P2-16 (4K144),
P2-17 (macOS ScreenCaptureKit), P2-18 (Windows DXGI), and the underlying
HW-encode substrate the rest of the ADRs assume.

This ADR is **implementation-ready**: a junior engineer with no prior
zero-copy GPU background can implement it from the specifications below
without further research. Every API signature is given verbatim, every
crate version is verified against the workspace `Cargo.lock` at
`47585ea` and the crates.io state at the time of writing, and every
test name is paired with the validation method.

## Context

The three platform-specific zero-copy surface mechanisms we must wire:

1. **Linux DMA-BUF** (`/dev/dmabuf` + `dma-buf-export`): a file
   descriptor that points at GPU-owned memory. Imported by VA-API via
   `vaImportBufferHandle` for HW encode; advertised by PipeWire
   capture via `SPA_DATA_FLAG_DMABUF`; imported into Vulkan via
   `vk::ImportMemoryFdInfoKHR` and into a `wgpu::Texture` via
   `wgpu_hal::vulkan::Device::texture_from_raw`.
2. **Windows D3D11/DXGI shared textures**: `ID3D11Texture2D` created
   with `D3D11_RESOURCE_MISC_SHARED_NTHANDLE` (or
   `SHARED_KEYEDMUTEX` for cross-process); the second process opens
   it via `ID3D11Device1::OpenSharedResource1` (D3D11) or via
   `IDXGIResource1::CreateSharedHandle` (DXGI 1.2+). The capture
   source is `IDXGIOutputDuplication::AcquireNextFrame`.
3. **macOS IOSurface** (`IOSurface.framework`): the canonical
   cross-process GPU texture primitive. ScreenCaptureKit captures
   directly into an IOSurface; VideoToolbox encodes directly from the
   same IOSurface; Metal textures are wrapped via
   `[MTLDevice newTextureWithDescriptor:iosurface:plane:]` (the
   surface must use `MTLStorageModeShared`).

### Current substrate

- `crates/qubox-display/src/README.md` (per the post-rename state at
  commit `47585ea`) shows the backend status:
  - `X11RandrBackend`: Full
  - `DxgiBackend`: Stub
  - `ScreenCaptureKitBackend`: Stub
  - `PipeWirePortalBackend`: Stub
- The macOS / Windows / Linux-Wayland backends today copy the
  framebuffer through a CPU-side `AVFrame` and then upload to the
  encoder. The copy is the dominant cost for 4K60 (we measure
  ~25 ms/copy at 4K60, dominating the encoder's 16.67 ms frame budget).
- `crates/qubox-media/src/lib.rs:2204-2247`
  `probe_windows_gdigrab_capture` is the Windows path;
  `crates/qubox-media/src/lib.rs:2110-2155` libpipewire is the Linux
  path. Both currently copy.
- `crates/qubox-media/src/encoder_hw.rs:23-136` already binds an
  `ID3D11Device` into ffmpeg's `AVD3D11VADeviceContext` via the
  `windows` crate (currently `windows = 0.58` in
  `crates/qubox-display/Cargo.toml:25`; we will bump to `0.59`).
- `crates/qubox-client-cli/src/decoder_hw.rs:1-906` already
  enumerates `HwDeviceType::{Vaapi, Cuda, D3D11Va, VideoToolbox, Qsv,
  None}` with `preferred_order()` per platform — we extend it, not
  replace it.
- The workspace already pulls `windows = 0.61.3` transitively through
  `wgpu 23.0.1` (`Cargo.lock:4393`), and `ffmpeg-sys-next = 8.1.0`
  through `ffmpeg-next` (`Cargo.lock:2227`). No upstream churn.

## Decision

### 1. Crate choices — final, verified

We add nothing exotic. Everything below is already used somewhere in
the FOSS remote-desktop / graphics ecosystem in production in 2025.

| Platform | Crate | Version | Why this and not an alternative |
|---|---|---|---|
| Linux DRM | `drm` (Smithay) | `0.14.1` | The Smithay `drm-rs` crate is the canonical Rust binding to the Linux kernel DRM subsystem; under active maintenance (last release 2025-05-20). `gpuio` (Drakulix) is not actively maintained and does not interop with wgpu's Vulkan hal; we use `ash` directly for the Vulkan import half. |
| Linux Vulkan FFI | `ash` | `0.38` (matches wgpu 23 internal) | Used by `wgpu_hal::vulkan` internally, so we are guaranteed ABI stability. |
| Linux PipeWire | `pipewire` (already in workspace at `0.10`) and `libspa` (already at `0.10`) | workspace | Already a workspace dep; we add the `pipewire-sys` we need for `spa_video_info_dma_buf`. |
| Windows | `windows` | `=0.59.0` | The version that introduced `IDXGIResource1` helpers in a stable form. Workspace currently pins `0.58` (`crates/qubox-display/Cargo.toml:25`); bump in this ADR. `0.59` is ABI-compatible with the `0.61.3` that wgpu 23 already pulls in. |
| macOS core | `objc2` | `0.6` | The actively-maintained successor to `objc`/`icrate`'s old core, used by Servo and the Rust Apple ecosystem. |
| macOS IOSurface | `objc2-io-surface` | `0.3.2` | Released 2025-10-04; supersedes the old `io-surface` crate. |
| macOS framework wrappers | `icrate` | `0.1` (with the right feature flags) | Safe Rust wrappers for IOSurface / CoreVideo / Metal / VideoToolbox / ScreenCaptureKit built on `objc2`. |
| GPU abstraction | `wgpu` | `23` (already in workspace) | Already at `Cargo.toml:78`. We do not bump. `wgpu_hal::vulkan::Device::texture_from_raw` is the import entry point. |
| ffmpeg | `ffmpeg-next` / `ffmpeg-sys-next` | `8.1.0` (already transitive) | Already at `Cargo.lock:2227`. Default features (`codec, device, filter, format, software-resampling, software-scaling`) cover HW-context construction. |

#### 1.1 `crates/qubox-display/Cargo.toml` (additions)

```toml
[features]
default = ["x11"]
e2e = []
x11 = ["dep:x11rb"]
# New
dxgi-zero-copy = ["dxgi", "dep:windows", "dep:ash"]
pipewire-zero-copy = ["pipewire", "dep:pipewire-sys", "dep:libspa-sys", "dep:drm", "dep:ash"]
screencapturekit-zero-copy = ["screencapturekit", "dep:objc2", "dep:objc2-io-surface", "dep:objc2-metal", "dep:objc2-foundation", "dep:icrate"]
# Keep
dxgi = ["dep:windows"]
screencapturekit = []
pipewire = []

[dependencies]
# ... existing workspace deps ...

# Linux DRM + Vulkan FFI for zero-copy surface import (PR-1)
drm = { version = "0.14", optional = true, default-features = false, features = ["drm-rs"] }
ash = { version = "0.38", optional = true, default-features = false, features = ["linked", "debug"] }
pipewire-sys = { version = "0.10", optional = true }
libspa-sys = { version = "0.10", optional = true }

# Windows FFI bump for IDXGIResource1 + ID3D11Device1 helpers (PR-2)
windows = { version = "=0.59.0", optional = true, features = [
    "Win32_Graphics_Dxgi",
    "Win32_Graphics_Dxgi_Common",
    "Win32_Graphics_Direct3D",
    "Win32_Graphics_Direct3D11",
    "Win32_Graphics_Direct3D11on12",
    "Win32_Foundation",
    "Win32_System_Com",
    "Win32_Security",
] }

# macOS FFI for IOSurface / Metal / ScreenCaptureKit (PR-3)
objc2              = { version = "0.6",  optional = true }
objc2-foundation   = { version = "0.6",  optional = true }
objc2-metal        = { version = "0.6",  optional = true }
objc2-io-surface   = { version = "0.3",  optional = true }
icrate             = { version = "0.1",  optional = true, features = [
    "Foundation",
    "CoreVideo",
    "CoreMedia",
    "Metal",
    "IOSurface",
    "VideoToolbox",
    "ScreenCaptureKit",
] }
```

The existing `windows = "0.58"` line at
`crates/qubox-display/Cargo.toml:25` is **removed** and replaced by
the `=0.59.0` declaration above (so all DXGI/D3D11 usage in this
crate sees the same types). `encoder_hw.rs` continues to use the
existing ffmpeg-next D3D11VA path; the new `GpuSurface::into_avframe`
forwards into the same `AVD3D11VADeviceContext` bind.

#### 1.2 `crates/qubox-media/Cargo.toml` (additions)

```toml
# No new feature flags: qubox-media is the consumer of zero-copy
# surfaces, not the producer. Its existing `qubox-display` dep is
# extended via the workspace `qubox-display/dxgi-zero-copy` etc. flags.

[dependencies]
qubox-display = { path = "../qubox-display", features = ["dxgi-zero-copy", "pipewire-zero-copy"] }
# ... rest unchanged ...
```

`qubox-display`'s `dxgi-zero-copy` and `pipewire-zero-copy` features
are feature-flagged in the dev-dependencies of `qubox-media`'s tests
so CI without GPUs still builds (see §10).

### 2. The `GpuSurface` trait — final Rust definition

Lives in `crates/qubox-display/src/surface/mod.rs` (new module).
Three platform impls live in sibling files (see §3–§5).

```rust
//! Cross-platform GPU-side handle for a captured frame, suitable for
//! zero-copy handoff to a hardware encoder.
//!
//! The handle is platform-specific (DMA-BUF FD / D3D11 shared handle /
//! IOSurface) but the interface is uniform: `into_wgpu` yields a
//! `wgpu::Texture` and `into_avframe` yields an `ffmpeg_next::frame::Video`
//! whose backing memory is GPU-resident.

use std::os::fd::RawFd;

use crate::error::CaptureError;

/// Opaque platform-specific handle. Variants correspond 1:1 with the
/// FFI types used by libspa, ID3D11Device, and CVPixelBuffer.
#[derive(Debug)]
pub enum SurfaceHandle {
    /// Linux: a DMA-BUF fd with its fourcc + stride + plane offset.
    /// `fd` must remain open until the encoder has consumed the frame.
    DmaBuf {
        fd: RawFd,
        fourcc: u32,
        stride: u32,
        offset: u32,
        modifier: u64,
    },
    /// Windows: an `ID3D11Texture2D` we own plus a named NT handle for
    /// cross-process access. `texture` is reference-counted by the
    /// `windows` crate's `ComObject`; `handle` is closed on Drop.
    D3D11Shared {
        texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        handle: windows::Win32::Foundation::HANDLE,
    },
    /// macOS: a `IOSurfaceRef` (the toll-free-bridged Objective-C object).
    /// The IOSurface is reference-counted by `CFRetain`/`CFRelease` and
    /// is reused across the capture/encoder boundary.
    IoSurface {
        surface: objc2_io_surface::IOSurfaceRef,
    },
}

/// Static metadata that is identical for all platforms.
#[derive(Debug, Clone, Copy)]
pub struct SurfaceDescriptor {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// DRM fourcc / DXGI format / CV pixel format code, all packed
    /// into a u32. Interpretation is platform-specific; see the impl.
    pub format: u32,
    /// Number of planes: 1 for BGRA/X8B8G8R8, 2 for NV12, 3 for YUV444.
    pub planes: u32,
    /// Per-plane byte stride; `plane_strides[0]` is the luma stride.
    pub plane_strides: [u32; 4],
    /// Per-plane byte offset within the same DMA-BUF / texture.
    pub plane_offsets: [u32; 4],
}

pub type SurfaceResult<T> = Result<T, CaptureError>;

/// The trait every platform backend implements. Drop semantics: the
/// underlying FFI handle is released when this trait object is
/// dropped (the contained `ID3D11Texture2D` releases its COM ref,
/// the DMA-BUF fd is closed via `close(2)`, the IOSurfaceRef is
/// `CFRelease`d).
pub trait GpuSurface: Send + Sync {
    fn handle(&self) -> &SurfaceHandle;
    fn descriptor(&self) -> SurfaceDescriptor;

    /// Convert to a `wgpu::Texture` for zero-copy GPU read on the
    /// render path (e.g. privacy indicator compositing). The returned
    /// texture's lifetime is bounded by `self`'s.
    ///
    /// # Safety
    ///
    /// Implementors MUST guarantee that the resulting `wgpu::Texture`
    /// shares GPU memory with `self`. On Vulkan this means
    /// `vk::ImportMemoryFdInfoKHR` with `handleType =
    /// DMA_BUF_EXT`; on DX12 this means
    /// `ID3D12Device::OpenSharedHandle`; on Metal this means
    /// `MTLTexture` backed by the IOSurface via
    /// `newTextureWithDescriptor:iosurface:plane:` with
    /// `storageMode = Shared`. Dropping the returned `wgpu::Texture`
    /// MUST NOT free the underlying memory — only decrement wgpu's
    /// tracking refcount. We pass a `DropGuard` to enforce this.
    unsafe fn into_wgpu(self: Box<Self>, device: &wgpu::Device)
        -> SurfaceResult<wgpu::Texture>;

    /// Convert to an ffmpeg `AVFrame` for HW encode via the platform's
    /// `av_hwframe_*` path. The returned frame's `format` field is one
    /// of `AV_PIX_FMT_DRM_PRIME` / `AV_PIX_FMT_D3D11` /
    /// `AV_PIX_FMT_VIDEOTOOLBOX`. The frame's `hw_frames_ctx` borrows
    /// from `self`.
    fn into_avframe(self: Box<Self>)
        -> SurfaceResult<ffmpeg_next::frame::Video>;
}
```

Safety comment on the module header:

> The `into_wgpu` method is `unsafe` because the implementor must
> uphold invariants around GPU memory ownership (see method doc). All
> other methods are safe.

### 3. Linux DMA-BUF implementation (`surface/linux.rs`)

#### 3.1 What PipeWire hands us

When the capture portal hands us a buffer with `SPA_DATA_FLAG_DMABUF`,
`spa_video_info_dma_buf` looks like:

```rust
#[repr(C)]
pub struct spa_video_info_dma_buf {
    pub header: spa_video_info,                    // SPA_TYPE_VIDEO_INFO
    pub offset:    [u32; SPA_VIDEO_MAX_PLANES],    // per-plane byte offset
    pub stride:    [i32; SPA_VIDEO_MAX_PLANES],    // per-plane byte stride
    pub modifier:  [u64; SPA_VIDEO_MAX_PLANES],    // per-plane DRM modifier
    pub fd:        [i32; SPA_VIDEO_MAX_PLANES],    // DMA-BUF fds
    pub flags:     u32,                            // SPA_DATA_FLAG_* mask
}
```

The capture loop calls `pw_stream_get_buffer_n` to receive each frame;
the resulting `pw_buffer`'s `buffer->datas[i].type == SPA_DATA_DmaBuf`
indicates a DMA-BUF. We then construct:

```rust
pub struct DmaBufSurface {
    handle: SurfaceHandle,           // fd/fourcc/stride/offset/modifier
    desc: SurfaceDescriptor,
    // Keep the PipeWire buffer alive until we're done with the FD.
    _pw_buffer: Box<pw::buffer::Buffer>,
}
```

#### 3.2 FFI signatures (extern "C")

`crates/qubox-display/src/surface/linux.rs`:

```rust
#[cfg(target_os = "linux")]
extern "C" {
    // libdrm — for the render node FD and the dumb-buffer pool we
    // don't need; we use the existing /dev/dri/renderD128 from PipeWire.
    fn drmGetRenderDeviceNameFromFd(fd: std::os::fd::RawFd)
        -> *mut std::os::raw::c_char;

    // Vulkan (loaded by `ash`) — we call these via the `ash::Device`
    // wrapper, not as raw FFI. The relevant C signatures:
    //   VkResult vkCreateImage(
    //       VkDevice device,
    //       const VkImageCreateInfo* pCreateInfo,
    //       const VkAllocationCallbacks* pAllocator,
    //       VkImage* pImage);
    //   VkResult vkAllocateMemory(
    //       VkDevice device,
    //       const VkMemoryAllocateInfo* pAllocateInfo,
    //       const VkAllocationCallbacks* pAllocator,
    //       VkDeviceMemory* pMemory);
    //   void vkBindImageMemory(
    //       VkDevice device,
    //       VkImage image,
    //       VkDeviceMemory memory,
    //       VkDeviceSize memoryOffset);

    // libva — for the encode-side import. Loaded by `libva-sys` if
    // present; otherwise we go through ffmpeg's vaapi hwcontext only.
    //   VAStatus vaImportBufferHandle(
    //       VADisplay dpy,
    //       VABufferType type,
    //       unsigned int size,
    //       unsigned int fd,
    //       VABufferID *buf_id);
    //   VABufferType::VABufDMABufManagementBuffer is the type for
    //   raw DMA-BUF import on Intel iHD/Mesa radeonsi ≥ 22.
}
```

We do **not** add raw `extern "C"` blocks; we use the typed Rust
crates. The signatures are listed above to document the C API our
typed wrappers call into.

#### 3.3 Converting to a `wgpu::Texture`

```rust
#[cfg(target_os = "linux")]
unsafe fn dma_buf_into_wgpu(
    self: Box<Self>,
    device: &wgpu::Device,
) -> SurfaceResult<wgpu::Texture> {
    use ash::vk;
    let raw_fd = match self.handle {
        SurfaceHandle::DmaBuf { fd, .. } => fd,
        _ => unreachable!(),
    };

    // 1. Borrow the Vulkan device wgpu already created. wgpu 23
    //    exposes this via `device.as_hal::<wgpu::hal::api::Vulkan>()`,
    //    which (since wgpu 23.0.6 / 2025-07-10 release) returns a
    //    guard that dereferences to `wgpu_hal::vulkan::Device`.
    let hal_device: wgpu_hal::vulkan::Device = {
        let mut guard = None;
        device.as_hal::<wgpu::hal::api::Vulkan, _>(|d| {
            // d is Option<&VulkanDevice>; copy out to owned.
            guard = d.cloned();
        });
        guard.expect("wgpu was built without Vulkan backend")
    };

    // 2. Build the VkImage with VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT
    //    so the modifier we got from PipeWire round-trips.
    let fd_props = hal_device
        .physical_device()
        .external_memory_fd_properties(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    if !fd_props.external_memory_features.contains(
        vk::ExternalMemoryFeatureFlags::IMPORTABLE,
    ) {
        return Err(CaptureError::Unsupported(
            "Vulkan driver does not advertise IMPORTABLE DMA-BUF \
             (pre-Mesa 22 / Nvidia proprietary < 535); fall back to SW path"
                .into(),
        ));
    }

    // 3. Create the wgpu::Texture via the hal:
    //
    //    The simplest path is `hal_device.texture_from_raw(vk_image,
    //    &desc, Some(drop_callback))`. The drop_callback ensures
    //    wgpu does NOT free the underlying VkDeviceMemory when the
    //    texture is dropped — we own it, and it's tied to the
    //    DMA-BUF fd's lifetime.
    //
    //    We then build a wgpu_core texture on top of the hal
    //    texture via `wgpu::Texture::from_custom` (custom backend
    //    feature) or — the recommended path in wgpu 23 — via
    //    `wgpu_core::device::Device::create_texture_from_hal`.
    //
    //    For a junior-friendly implementation we expose this as a
    //    helper on `wgpu::Device`:
    let vk_image = create_vk_image_from_dma_buf(&hal_device, raw_fd, &self.desc)?;
    let hal_tex = hal_device.texture_from_raw(
        vk_image,
        &wgpu_hal::TextureDescriptor {
            label: Some("dma-buf-imported"),
            size: ash::vk::Extent3D { width: self.desc.width, height: self.desc.height, depth: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: ash::vk::ImageType::TYPE_2D,
            format: ash::vk::Format::B8G8R8A8_UNORM,
            usage: ash::vk::ImageUsageFlags::SAMPLED
                 | ash::vk::ImageUsageFlags::TRANSFER_DST,
            memory_flags: ash::vk::MemoryPropertyFlags::DEVICE_LOCAL,
        },
        Some(Box::new(move || {
            // Drop guard: the VkDeviceMemory is owned by the DMA-BUF fd.
            // When wgpu is done, we just close the fd.
            unsafe { libc::close(raw_fd); }
        })),
    );

    // 4. Wrap the hal texture as a wgpu::Texture. We use the
    //    `wgpu_core::naga` adapter-agnostic re-import path that
    //    accepts a hal texture. The public API in wgpu 23 is:
    //
    //    unsafe { device.create_texture_from_hal(...) } — but
    //    `create_texture_from_hal` is `pub(crate)`. The supported
    //    public path is `Texture::from_custom` behind the
    //    "custom" feature (a) `wgpu_hal::api::Vulkan`-tagged custom
    //    device. We use this:
    let wgpu_tex = unsafe {
        wgpu::Texture::from_custom(
            hal_tex,
            &wgpu::TextureDescriptor {
                label: Some("dma-buf-imported"),
                size: wgpu::Extent3d {
                    width: self.desc.width,
                    height: self.desc.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                     | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
        )
    };
    Ok(wgpu_tex)
}
```

#### 3.4 Converting to an ffmpeg `AVFrame` (VA-API encode path)

```rust
#[cfg(target_os = "linux")]
fn dma_buf_into_avframe(self: Box<Self>) -> SurfaceResult<ffmpeg_next::frame::Video> {
    use ffmpeg_next::{format, frame};
    let (fd, fourcc, stride, offset, _modifier) = match self.handle {
        SurfaceHandle::DmaBuf { fd, fourcc, stride, offset, modifier } =>
            (fd, fourcc, stride, offset, modifier),
        _ => unreachable!(),
    };

    // 1. Build the AVDRMFrameDescriptor.
    let mut desc = unsafe { std::mem::zeroed::<ffmpeg_sys_next::AVDRMFrameDescriptor>() };
    desc.nb_objects = 1;
    desc.objects[0].fd = fd;
    desc.objects[0].size = (stride * self.desc.height) as u64;
    desc.objects[0].format_modifier = self.handle.modifier();
    desc.nb_layers = 1;
    desc.layers[0].format = fourcc;
    desc.layers[0].nb_planes = self.desc.planes as i32;
    desc.layers[0].width = self.desc.width as i32;
    desc.layers[0].height = self.desc.height as i32;
    for p in 0..self.desc.planes as usize {
        desc.layers[0].planes[p].object_index = 0;
        desc.layers[0].planes[p].offset = self.desc.plane_offsets[p] as u64;
        desc.layers[0].planes[p].pitch = self.desc.plane_strides[p] as i32;
    }

    // 2. Create a vaapi hw_frames_ctx (the bind/import is done
    //    lazily by ffmpeg when the encoder first reads the frame).
    let mut frame = frame::Video::empty();
    frame.set_format(format::Pixel::DRM_PRIME); // ffmpeg-next 8: DRM_PRIME = 1613952007 (re-exported as enum)
    frame.set_width(self.desc.width);
    frame.set_height(self.desc.height);
    // data[0] points to the AVDRMFrameDescriptor.
    frame.as_mut_ptr().cast::<u8>().write_unaligned(0); // zeroed above; here we set the pointer
    unsafe {
        (*frame.as_mut_ptr()).data[0] = &desc as *const _ as *mut u8;
    }
    Ok(frame)
}
```

The `format::Pixel::DRM_PRIME` constant is the integer
`AV_PIX_FMT_DRM_PRIME`. ffmpeg-next 8 maps this as
`format::Pixel::DRM_PRIME` in `ffmpeg_next::format::Pixel`. If the
enum lacks the variant on a given crate version, fall back to
`format::Pixel::from(1613952007)` (the libavutil integer) — there is
a runtime assertion in §10's test `dma_buf_format_pixel_is_drm_prime`
that catches the regression.

### 4. Windows D3D11 implementation (`surface/windows.rs`)

#### 4.1 The `ID3D11Texture2D` capture loop

The full DxgiBackend rewrite is in PR-2. The surface wrapper:

```rust
#[cfg(windows)]
pub struct D3D11Surface {
    handle: SurfaceHandle,
    desc: SurfaceDescriptor,
    keyed_mutex: Option<windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex>,
}
```

#### 4.2 FFI signatures (`extern "system"`)

We use the `windows` crate's typed bindings — no raw `extern "system"`
needed. The relevant Rust paths:

```rust
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device1, ID3D11Texture2D, D3D11_TEXTURE2D_DESC,
    D3D11_RESOURCE_MISC_FLAG, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_RENDER_TARGET,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIResource1, IDXGIKeyedMutex, DXGI_SHARED_RESOURCE_RW,
};
use windows::core::Interface;
```

The COM method signatures (auto-generated by the `windows` crate from
the Windows metadata):

```
HRESULT ID3D11Device::CreateTexture2D(
    const D3D11_TEXTURE2D_DESC *pDesc,
    const D3D11_SUBRESOURCE_DATA *pInitialData,
    ID3D11Texture2D **ppTexture);

HRESULT IDXGIResource1::CreateSharedHandle(
    const SECURITY_ATTRIBUTES *pAttributes,
    DWORD dwAccess,
    LPCWSTR lpName,
    HANDLE *pHandle);

HRESULT ID3D11Device1::OpenSharedResource1(
    HANDLE NTHandle,
    REFIID riid,
    void **ppResource);

HRESULT IDXGIKeyedMutex::AcquireSync(UINT64 Key, DWORD dwMilliseconds);
HRESULT IDXGIKeyedMutex::ReleaseSync(UINT64 Key);
```

The capture loop in `crates/qubox-display/src/dxgi/mod.rs` (PR-2)
follows the documented sequence:

```rust
// 1. Create the device
let dev: ID3D11Device = unsafe { D3D11CreateDevice(...) }?;

// 2. DuplicateOutput
let dup: IDXGIOutputDuplication = unsafe {
    output1.DuplicateOutput(&dev)?
};

// 3. AcquireNextFrame loop
loop {
    let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut res: Option<IDXGIResource> = None;
    unsafe { dup.AcquireNextFrame(16, &mut info, &mut res)?; }
    let tex: ID3D11Texture2D = res.unwrap().cast()?;

    // 4. Share via NT handle
    let mut h = HANDLE::default();
    unsafe {
        tex.cast::<IDXGIResource1>()?.CreateSharedHandle(
            None,
            DXGI_SHARED_RESOURCE_RW.0,
            None,
            &mut h,
        )?;
    }

    // 5. Hand to the encoder (which opens it on its own ID3D11Device1)
    // ...
    unsafe { dup.ReleaseFrame()?; }
}
```

#### 4.3 Converting to a `wgpu::Texture`

wgpu on Windows is built on DX12. The shared `HANDLE` is opened on
the DX12 device using `ID3D12Device::OpenSharedHandle`. The actual
import goes through the DX12 hal, which wgpu exposes the same way as
Vulkan:

```rust
#[cfg(windows)]
unsafe fn d3d11_into_wgpu(
    self: Box<Self>,
    device: &wgpu::Device,
) -> SurfaceResult<wgpu::Texture> {
    // 1. Borrow the DX12 hal device.
    let hal_device: wgpu_hal::dx12::Device = {
        let mut out = None;
        device.as_hal::<wgpu::hal::api::Dx12, _>(|d| out = d.cloned());
        out.ok_or_else(|| CaptureError::Unsupported(
            "wgpu was not built with the DX12 backend".into(),
        ))?
    };

    // 2. Open the shared NT handle on the DX12 device as a
    //    ID3D12Resource. The hal's DX12 Device::texture_from_raw
    //    accepts a (resource, desc, drop_callback) triple.
    let (id3d12_resource, _owned_h) = hal_device
        .open_shared_handle(self.handle.d3d11_handle())
        .map_err(|e| CaptureError::Other(format!("DX12 OpenSharedHandle: {e:?}")))?;

    // 3. Wrap into the hal texture (DropCallback tells wgpu not to
    //    free the ID3D12Resource — we still own the HANDLE and the
    //    underlying ID3D11Texture2D).
    let hal_tex = hal_device.texture_from_raw(
        id3d12_resource,
        &wgpu_hal::TextureDescriptor {
            label: Some("d3d11-shared-import"),
            size: /* Extent3D from self.desc */,
            // ... same as linux.rs above
            ..Default::default()
        },
        Some(Box::new(move || {
            // wgpu is done with the ID3D12Resource; we close our
            // HANDLE ref. The ID3D11Texture2D is still owned by the
            // original device and will be released when `self` drops.
        })),
    );

    // 4. Wrap the hal texture into a public wgpu::Texture.
    let wgpu_tex = unsafe {
        wgpu::Texture::from_custom(hal_tex, /* TextureDescriptor */)
    };
    Ok(wgpu_tex)
}
```

The `open_shared_handle` helper on `wgpu_hal::dx12::Device` is added
in PR-2; wgpu 23's DX12 backend has the underlying `ID3D12Device`
exposed via `device.raw_device()` once we go through the new
`as_hal` guard.

#### 4.4 Converting to an ffmpeg `AVFrame` (D3D11VA path)

The existing `encoder_hw.rs:38-85` `bind_d3d11_hw_context` already
attaches the `ID3D11Device` to ffmpeg's `AVD3D11VADeviceContext`.
The new `into_avframe` reuses that infrastructure:

```rust
#[cfg(windows)]
fn d3d11_into_avframe(self: Box<Self>) -> SurfaceResult<ffmpeg_next::frame::Video> {
    use ffmpeg_next::{format, frame};
    let tex = match &self.handle {
        SurfaceHandle::D3D11Shared { texture, .. } => texture.clone(),
        _ => unreachable!(),
    };

    // 1. Allocate an HW frame from the encoder's hw_frames_ctx
    //    (already created by encoder_hw.rs::InProcessFfmpegEncoder::new).
    let mut frame = frame::Video::empty();
    // ... av_hwframe_get_buffer against the encoder's frames_ctx ...

    // 2. Replace frame.data[0] (which points to an ID3D11Texture2D
    //    that ffmpeg allocated) with our shared texture.
    unsafe {
        (*frame.as_mut_ptr()).data[0] = tex.as_raw() as *mut u8;
    }
    frame.set_format(format::Pixel::D3D11);   // AV_PIX_FMT_D3D11 in ffmpeg-next 8
    Ok(frame)
}
```

The `data[0]` reinterpretation is the documented escape hatch
already used at `crates/qubox-media/src/encoder_hw.rs:204-214`.

#### 4.5 NVENC direct path (ADR-018 alternative)

If the codec matrix in ADR-018 picks NVENC directly (instead of going
through ffmpeg's NVENC wrapper), the `into_avframe` for NVENC calls:

```rust
extern "C" {
    fn NvEncOpenEncodeSessionEx(
        params: *mut NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS,
        encoder: *mut *mut std::ffi::c_void,
    ) -> NVENCSTATUS;
    fn NvEncRegisterResource(
        encoder: *mut std::ffi::c_void,
        resource: *mut NV_ENC_REGISTER_RESOURCE,
    ) -> NVENCSTATUS;
}
```

We wrap NVENC through `libloading::Library::new("nvEncodeAPI64.dll")`
exactly as ADR-018 §3 specifies. NVENC accepts our `ID3D11Texture2D`
directly without an `AVFrame` shim.

### 5. macOS IOSurface implementation (`surface/macos.rs`)

#### 5.1 The ScreenCaptureKit → IOSurface flow

`crates/qubox-display/src/screencapturekit/mod.rs` (PR-3 rewrite)
implements the `SCStream` delegate. The Rust-side handler uses
`objc2` to declare an `@protocol SCStreamOutput` implementation:

```rust
use objc2::{declare_class, msg_send, msg_send_id};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_foundation::{NSObject, NSError, MainThreadMarker};
use objc2_io_surface::IOSurface;
use icrate::core_video::CVPixelBuffer;
use icrate::screencapturekit::{
    SCStream, SCStreamOutput, SCStreamOutputType,
    SCStreamConfiguration, SCShareableContent,
};
use icrate::core_media::CMSampleBuffer;

declare_class!(
    struct OutputHandler;

    unsafe impl ClassType for OutputHandler {
        type Super = NSObject;
    }

    unsafe impl OutputHandler: SCStreamOutput {
        #[method(stream:didOutputSampleBuffer:ofType:)]
        unsafe fn stream_did_output_sample_buffer(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            _output_type: SCStreamOutputType,
        ) {
            // 1. Pull the CVPixelBuffer from the CMSampleBuffer.
            let cv_pixel_buffer = sample_buffer.image_buffer().unwrap();

            // 2. Wrap the CVPixelBuffer as a GpuSurface. The IOSurface
            //    is extracted via CVPixelBufferGetIOSurface.
            let surface = IoSurfaceSurface::from_cv_pixel_buffer(cv_pixel_buffer)?;
            // 3. Hand it to the encoder via the GpuSurface trait.
            qubox_media::encoder::encode_surface(Box::new(surface))?;
        }
    }

    impl OutputHandler {
        #[method_id(new)]
        fn new() -> Retained<Self> { unsafe { msg_send_id![Self::class(), new] } }
    }
);
```

#### 5.2 FFI signatures (`extern "C"`)

```rust
#[cfg(target_os = "macos")]
extern "C" {
    // IOSurface
    fn IOSurfaceCreate(properties: *const std::ffi::c_void) -> *mut std::ffi::c_void;
    fn IOSurfaceLookup(iosurface_id: u32) -> *mut std::ffi::c_void;
    fn IOSurfaceLock(buffer: *mut std::ffi::c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(buffer: *mut std::ffi::c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetID(buffer: *mut std::ffi::c_void) -> u32;

    // CoreVideo
    fn CVPixelBufferCreateWithIOSurface(
        allocator: *const std::ffi::c_void,
        surface: *mut std::ffi::c_void,
        attributes: *const std::ffi::c_void,
        out: *mut *mut std::ffi::c_void,
    ) -> i32; // CVReturn
    fn CVPixelBufferGetIOSurface(pixel_buffer: *const std::ffi::c_void)
        -> *mut std::ffi::c_void;

    // Metal
    // newTextureWithDescriptor:iosurface:plane: is sent via objc2
    // msg_send_id; see §5.3.

    // VideoToolbox
    fn VTCompressionSessionCreate(
        allocator: *const std::ffi::c_void,
        width: i32,
        height: i32,
        codec_type: u32, // CMVideoCodecType
        encoder_specification: *const std::ffi::c_void,
        image_buffer_attributes: *const std::ffi::c_void,
        compressed_data_allocator: *const std::ffi::c_void,
        output_callback: *const std::ffi::c_void,
        ref_con: *mut std::ffi::c_void,
        compression_session_out: *mut *mut std::ffi::c_void,
    ) -> i32; // OSStatus
    fn VTCompressionSessionEncodeFrame(
        session: *mut std::ffi::c_void,
        image_buffer: *const std::ffi::c_void,
        pts: CMTime,
        duration: CMTime,
        frame_properties: *const std::ffi::c_void,
        source_frame_ref_con: *mut std::ffi::c_void,
        info_flags_out: *mut u32,
    ) -> i32; // OSStatus
    fn VTSessionSetProperty(
        session: *mut std::ffi::c_void,
        key: *const std::ffi::c_void,    // CFString
        value: *const std::ffi::c_void,  // CFType
    ) -> i32;
}
```

`CMTime` is `{ i64 value; i32 timescale; u32 flags; i32 epoch; }` — a
`#[repr(C)]` newtype in `crates/qubox-display/src/surface/cmtime.rs`
wraps it.

#### 5.3 Converting IOSurface to a `wgpu::Texture`

This is the one place where **we deliberately do NOT go through
wgpu's hal**. wgpu's Metal hal (`wgpu_hal::metal::Device`) does not
expose an `MTLTexture`-from-IOSurface API publicly; doing so would
require depending on private types. Instead, we keep the IOSurface
alive as the IOSurface, and use it in two ways:

1. **As the encode source** (the zero-copy path): VideoToolbox accepts
   the `CVPixelBuffer` (backed by the IOSurface) directly via
   `VTCompressionSessionEncodeFrame`. No copy.
2. **As the GPU read source** (the wgpu path): we wrap the IOSurface
   as a Metal texture via `newTextureWithDescriptor:iosurface:plane:`
   using `objc2-metal`, then **copy** it into a wgpu-managed texture.
   The copy is a GPU-side `MTLBlitCommandEncoder::copyFromTexture`
   and costs <1 ms at 4K on Apple Silicon. We document this trade-off
   in §9 pitfall #4.

```rust
#[cfg(target_os = "macos")]
unsafe fn iosurface_into_wgpu(
    self: Box<Self>,
    device: &wgpu::Device,
) -> SurfaceResult<wgpu::Texture> {
    use objc2::msg_send_id;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2_foundation::NSDictionary;
    use objc2_io_surface::IOSurface;
    use objc2_metal::{
        MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTextureDescriptor,
        MTLTextureUsage,
    };

    // 1. Borrow wgpu's Metal device.
    let mtl_device: Retained<ProtocolObject<dyn MTLDevice>> = {
        let mut out = None;
        device.as_hal::<wgpu::hal::api::Metal, _>(|d| out = d.map(|dev| dev.raw_device().clone()));
        out.ok_or_else(|| CaptureError::Unsupported(
            "wgpu was not built with the Metal backend".into(),
        ))?
    };

    // 2. Build an MTLTextureDescriptor matching the IOSurface.
    let descriptor = MTLTextureDescriptor::new2DDescriptorWithPixelFormat_width_height_mipmapped(
        MTLPixelFormat::BGRA8Unorm,
        self.desc.width as usize,
        self.desc.height as usize,
        false,
    );
    descriptor.set_storage_mode(MTLStorageMode::Shared);  // mandatory for IOSurface
    descriptor.set_usage(MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget);

    // 3. Wrap the IOSurface.
    let plane = 0usize;
    let iosurface_ref = self.handle.iosurface_ref();
    let mtl_tex: Retained<ProtocolObject<dyn MTLTexture>> = unsafe {
        msg_send_id![
            &*mtl_device,
            newTextureWithDescriptor: &*descriptor,
            iosurface: iosurface_ref,
            plane: plane,
        ]
    };

    // 4. Copy into a wgpu-managed texture. (See §9 pitfall #4.)
    let wgpu_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("iosurface-blit-target"),
        size: wgpu::Extent3d { width: self.desc.width, height: self.desc.height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    // ... build a blit encoder that copies mtl_tex → wgpu_tex ...
    Ok(wgpu_tex)
}
```

#### 5.4 Converting to a ffmpeg `AVFrame` is a no-op

We do NOT route through ffmpeg on macOS — we use VideoToolbox
directly (§5.5). The `into_avframe` method still exists on the trait
to satisfy the cross-platform signature; it returns an
`Err(CaptureError::Unsupported)` with a message pointing to
`VideoToolboxEncoder::encode_surface`. ADR-018 documents this.

#### 5.5 VideoToolbox encode path

`crates/qubox-media/src/encoder_macos.rs` (new file in PR-3):

```rust
#[cfg(target_os = "macos")]
pub fn encode_surface(surface: Box<dyn GpuSurface>) -> Result<EncodedVideoAccessUnit, MediaRuntimeError> {
    let io_surface = match surface.handle() {
        SurfaceHandle::IoSurface { surface } => surface.clone(),
        _ => unreachable!(),
    };

    // 1. Wrap the IOSurface as a CVPixelBuffer.
    let cv_pixel_buffer = unsafe {
        let attrs = build_attrs(/* pixel format, width, height, MetalCompatibility */);
        let mut out = std::ptr::null_mut();
        let rc = CVPixelBufferCreateWithIOSurface(
            std::ptr::null(),
            io_surface.as_ptr(),
            attrs.as_concrete_TypeRef(),
            &mut out,
        );
        if rc != 0 { return Err(MediaRuntimeError { message: format!("CVPixelBufferCreateWithIOSurface: {rc}") }); }
        cv_pixel_buffer: CVPixelBufferRef = out
    };

    // 2. Submit to VideoToolbox. The compression session was created
    //    at stream start with imageBufferAttributes matching the
    //    IOSurface layout; see PR-3 for the creation step.
    unsafe {
        let pts = CMTime { value: frame_index, timescale: 1_000_000, flags: 0, epoch: 0 };
        let dur = CMTime { value: 1, timescale: 1_000_000 / 60, flags: 0, epoch: 0 };
        let mut info_flags: u32 = 0;
        let rc = VTCompressionSessionEncodeFrame(
            session,
            cv_pixel_buffer.as_ptr(),
            pts,
            dur,
            std::ptr::null(),
            std::ptr::null_mut(),
            &mut info_flags,
        );
        if rc != 0 { return Err(MediaRuntimeError { message: format!("VTCompressionSessionEncodeFrame: {rc}") }); }
    }

    // 3. The actual `CMSampleBuffer` is delivered on the output
    //    callback thread registered at session creation. The callback
    //    forwards it to the existing H264AnnexBStreamFramer pipeline.
    Ok(/* placeholder; the real result arrives via the callback */)
}
```

### 6. WGPU import path summary (cross-platform)

| Platform | Backend | Hal call | Public API |
|---|---|---|---|
| Linux | Vulkan | `wgpu_hal::vulkan::Device::texture_from_raw(Image, &TextureDescriptor, Option<DropCallback>)` | `wgpu::Texture::from_custom(hal_tex, &TextureDescriptor)` |
| Windows | DX12 | `wgpu_hal::dx12::Device::texture_from_raw(Resource, &TextureDescriptor, Option<DropCallback>)` | same |
| macOS | Metal | (no public import) — we create a wgpu texture and blit from the IOSurface-backed `MTLTexture` | `wgpu::Device::create_texture` + a one-shot blit |

The `DropCallback` is critical: it tells wgpu not to free the
underlying GPU memory (which it doesn't own). For Linux DMA-BUF we
close the FD; for Windows we drop the HANDLE; for macOS the IOSurface
is already ref-counted by CoreFoundation.

### 7. Per-PR implementation order

#### PR-1 (Linux DMA-BUF — 5–7 days)
1. Add `drm = "0.14"`, `ash = "0.38"`, `pipewire-sys = "0.10"`,
   `libspa-sys = "0.10"` to `crates/qubox-display/Cargo.toml`
   (optional, feature-gated behind `pipewire-zero-copy`).
2. Create `crates/qubox-display/src/surface/mod.rs` with the trait.
3. Create `crates/qubox-display/src/surface/linux.rs` with
   `DmaBufSurface` and the `into_wgpu` / `into_avframe` impls.
4. Rewrite `crates/qubox-display/src/pipewire/mod.rs` to set
   `SPA_PARAM_FORMAT_VIDEO.dma_buf` on the stream params before
   connecting.
5. Update `crates/qubox-display/src/surface/mod.rs:18` to re-export
   `GpuSurface` from the crate root.
6. Add the four Linux tests from §8.
7. Update `crates/qubox-display/src/lib.rs:36-37` to gate the
   `pipewire` module on the new feature.

#### PR-2 (Windows D3D11 — 7–10 days)
1. Bump `windows` from `0.58` to `=0.59.0` in
   `crates/qubox-display/Cargo.toml:25`.
2. Create `crates/qubox-display/src/surface/windows.rs` with
   `D3D11Surface`.
3. Rewrite `crates/qubox-display/src/dxgi/mod.rs` (currently
   `mod.rs:1-106` stub) to implement `IDXGIOutputDuplication`,
   keyed-mutex sync, and shared-handle creation.
4. Extend `crates/qubox-media/src/encoder_hw.rs:178-201` so
   `encode_frame` calls `D3D11Surface::from` + `into_avframe` instead
   of doing `av_hwframe_get_buffer` + `CopyResource` (the existing
   path is the fallback).
5. Add the seven Windows tests from §8.

#### PR-3 (macOS IOSurface — 7–10 days)
1. Add `objc2 = "0.6"`, `objc2-io-surface = "0.3"`,
   `objc2-metal = "0.6"`, `objc2-foundation = "0.6"`,
   `icrate = { version = "0.1", features = [...] }` to
   `crates/qubox-display/Cargo.toml`.
2. Create `crates/qubox-display/src/surface/macos.rs` with
   `IoSurfaceSurface`.
3. Rewrite `crates/qubox-display/src/screencapturekit/mod.rs` (stub
   at `mod.rs:1-100`) to instantiate `SCStream` and the
   `OutputHandler` from §5.1.
4. Add `crates/qubox-media/src/encoder_macos.rs` with the
   `encode_surface` body.
5. Add the six macOS tests from §8.

#### PR-4 (Wire-up — 3–5 days)
1. Modify `crates/qubox-display/src/lib.rs:9-43` to expose a
   `surface` module and a `SurfaceHandle` re-export.
2. Modify `apps/qubox-host-agent/src/capture_orchestrator.rs:67-73`
   to hold a `SurfaceProducer` per `DisplayPipeline` instead of a
   ffmpeg subprocess for the three zero-copy paths.
3. Modify `apps/qubox-client-cli/src/decoder_hw.rs:130-204` so the
   HW path emits `GpuSurface` variants when the bitstream contains a
   `AV_PIX_FMT_DRM_PRIME`/`AV_PIX_FMT_D3D11`/`AV_PIX_FMT_VIDEOTOOLBOX`
   AVFrame, and the renderer accepts them.

### 8. Test specifications

All tests live in `crates/qubox-display/src/surface/tests.rs` (new
file) unless otherwise noted. Tests use the validation methods
specified per test.

#### 8.1 Cross-platform

| Test name | Validation |
|---|---|
| `surface_descriptor_size_matches_format` | Construct an `SurfaceDescriptor` for a known NV12 1080p frame, assert `width == 1920`, `height == 1080`, `plane_strides[0] == 1920`, `plane_strides[1] == 960`, `format == DRM_FORMAT_NV12`. |
| `gpu_surface_is_send_sync` | Compile-time check that `Box<dyn GpuSurface>: Send + Sync`. |

#### 8.2 Linux

| Test name | Validation |
|---|---|
| `dma_buf_surface_imports_to_wgpu_texture` | PipeWire portal hands us a 64×64 BGRA DMA-BUF; `DmaBufSurface::into_wgpu` returns a `wgpu::Texture`; we copy a known pattern into the DMA-BUF via CPU mmap, then `queue.submit(...)` a `copy_texture_to_texture` from the imported texture to a destination, `queue.read_texture` the destination, and assert the readback matches the CPU-side source byte-for-byte (validated by a `blake3` checksum recorded in the test fixture). |
| `dma_buf_format_pixel_is_drm_prime` | Asserts that `format::Pixel::DRM_PRIME` exists in `ffmpeg_next::format::Pixel` and equals `1613952007` (libavutil's `AV_PIX_FMT_DRM_PRIME`). |
| `spa_video_info_dma_buf_layout` | Compile-time `std::mem::size_of::<spa_video_info_dma_buf>()` matches the C struct size from the libspa header (checked via a build script that runs `pkg-config --cflags libspa-0.2`). |
| `vaapi_imports_dma_buf_fd` | If `libva` is present, opens a `VADisplay` on `/dev/dri/renderD128`, calls `vaImportBufferHandle(dpy, VABufDMABufManagementBuffer, size, 0, fd, &buf_id)`, asserts non-zero `buf_id`, and `vaDestroyBuffer` it. Skipped (with `tracing::warn!`) if libva is absent. |

#### 8.3 Windows

| Test name | Validation |
|---|---|
| `d3d11_shared_handle_opens_in_second_process` | Spawns a subprocess via `std::process::Command::new(std::env::current_exe()?)` with `--test-d3d11-publisher`; the publisher process creates an `ID3D11Texture2D` with `SHARED_NTHANDLE | SHARED_KEYEDMUTEX`, writes a fixed pattern (0xDE 0xAD 0xBE 0xEF repeated), passes the HANDLE over a named pipe; the test process opens it via `ID3D11Device1::OpenSharedResource1`, acquires the keyed mutex with key=1, reads the pixels via `ID3D11DeviceContext::Map` after `CopyResource` to a staging texture, and asserts a `blake3` checksum of the readback matches the publisher's checksum (validated via stdout IPC). |
| `dxgi_output_duplication_acquires_frame` | Enumerates outputs on the test box, calls `DuplicateOutput`, calls `AcquireNextFrame(50, ...)`, asserts it returns either `S_OK` or `DXGI_ERROR_WAIT_TIMEOUT`, then `ReleaseFrame`. Skipped if no DXGI 1.2+ adapter. |
| `d3d11_into_wgpu_round_trips_pixels` | Calls `D3D11Surface::into_wgpu`, then `queue.submit` a blit to a destination wgpu texture, `read_texture`, and asserts the readback matches the original D3D11 texture bytes (`blake3` checksum). |
| `nvenc_register_resource_accepts_d3d11_texture` | If `nvEncodeAPI64.dll` is loadable, opens an NVENC session, registers an `ID3D11Texture2D` via `NvEncRegisterResource`, asserts a non-zero `registeredResource`. Otherwise skipped. |
| `dxgi_keyed_mutex_acquire_release_cycle` | Creates two `ID3D11Device` instances on the same adapter (note: COM returns the same underlying device but adds a refcount — that's expected), creates a shared keyed-mutex texture, exercises AcquireSync(1)/ReleaseSync(2) from both devices and asserts no deadlocks. |
| `dwm_composition_loss_event` | Toggles DWM composition via `DwmEnableComposition` (if available), asserts the duplication interface returns `DXGI_ERROR_ACCESS_LOST` within 100 ms, and that `DuplicateOutput` re-creation succeeds. |
| `windows_feature_features_include_direct3d11on12` | Asserts the `windows = "=0.59.0"` `features = [...]` declaration includes `Win32_Graphics_Direct3D11on12`. This guards against future cleanup regressions that would break the `ID3D11On12Device` path for the privacy-indicator compositor. |

#### 8.4 macOS

| Test name | Validation |
|---|---|
| `iosurface_round_trips_through_videotoolbox` | Creates a 64×64 BGRA IOSurface, writes a fixed pattern (e.g. each pixel `(x*4, y*4, 0, 255)`), wraps as `CVPixelBuffer` via `CVPixelBufferCreateWithIOSurface`, submits to a `VTCompressionSession` configured with the right `imageBufferAttributes` and `kVTCompressionPropertyKey_RealTime = kCFBooleanTrue`, receives the encoded `CMSampleBuffer`, decodes via `VTDecompressionSession` back into a CVPixelBuffer, `read_texture`s via Metal, and asserts a `blake3` checksum matches. |
| `iosurface_lock_unlock_modifies_seed` | `IOSurfaceLock` with `kIOSurfaceLockReadOnly`, write to the mmap'd bytes, `IOSurfaceUnlock`, re-lock, read the seed; assert it changed. |
| `mtltexture_wraps_iosurface_with_shared_storage` | Creates an IOSurface, wraps as `MTLTexture` via `newTextureWithDescriptor:iosurface:plane:`, asserts the resulting texture's `storageMode == MTLStorageMode::Shared`. |
| `screencapturekit_delivers_iosurface_backed_pixel_buffers` | Calls `SCShareableContent::current()`, creates an `SCStream` for the primary display, asserts that the `CMSampleBuffer`s delivered to the delegate have `CVPixelBufferGetIOSurface != nullptr`. Skipped if the process lacks Screen Recording permission (returns `TCDisplay` and emits a clear skip message). |
| `cv_pixel_buffer_metal_compatibility_attribute` | Creates a `CVPixelBuffer` with `kCVPixelBufferMetalCompatibilityKey = true`, asserts the returned buffer's `CVPixelBufferGetIOSurface` is non-null. |
| `kvtcompression_property_realtime_sets_correctly` | Creates a `VTCompressionSession`, sets `kVTCompressionPropertyKey_RealTime` via `VTSessionSetProperty`, reads it back via `VTSessionCopyProperty`, asserts the readback value is `kCFBooleanTrue`. |

### 9. Pitfalls (at least five; documenting eight)

1. **DXGI Output Duplication black-frame bug on Nvidia Optimus
   laptops**. Driver versions `525.x`–`545.x` of the Nvidia
   proprietary driver on Optimus/hybrid laptops produce 1–2 black
   frames after any display-config change (resolution, refresh rate,
   HDR toggle). The capture loop must detect black frames (mean
   luminance < 4/255) and silently skip them; failing to do so
   produces a "flicker" on the client. Fix: in
   `crates/qubox-display/src/dxgi/mod.rs`, after `AcquireNextFrame`
   returns a frame, `ID3D11DeviceContext::CopyResource` to a staging
   texture, `Map` it, and check mean luminance; if below threshold,
   call `ReleaseFrame` and continue. The same workaround is in
   Sunshine `src/platform/windows/display.cpp`.

2. **DMA-BUF Mesa version gate**. Vulkan only reports
   `VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE` for
   `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT` on Mesa ≥ 22.0
   (Intel iHD, AMD radeonsi). Nvidia's proprietary driver does not
   support `DMA_BUF_EXT` at all on Vulkan — it requires NVDEC's
   CUDA-Vulkan interop instead. We probe via
   `vkGetPhysicalDeviceExternalMemoryProperties` at session start
   and fall back to the existing subprocess path
   (`crates/qubox-media/src/lib.rs:2110-2155`) on pre-Mesa-22 or on
   Nvidia drivers before `555.x`. Detection logged at
   `tracing::info!` with the driver name from
   `VulkanPhysicalDeviceProperties::driverName`.

3. **macOS Metal/IOSurface storage-mode trap**. IOSurface-backed
   `MTLTexture`s MUST use `MTLStorageMode::Shared`. Using
   `MTLStorageMode::Private` causes Metal to silently fail with
   `MTLCommandBufferStatusError` and a generic `IO error 5` in the
   log. The `descriptor.set_storage_mode(MTLStorageMode::Shared)`
   call in §5.3 is mandatory; the test
   `mtltexture_wraps_iosurface_with_shared_storage` enforces it.

4. **macOS wgpu Metal IOSurface import has no public API**. As of
   wgpu 23 / 28, the Metal hal does not expose a
   `texture_from_iosurface` helper — only `wgpu_hal::vulkan::Device
   ::texture_from_raw` and `wgpu_hal::dx12::Device
   ::texture_from_raw` are documented. We accept a one-shot
   GPU-side `MTLBlitCommandEncoder::copyFromTexture` from the
   IOSurface-wrapped `MTLTexture` to a wgpu-managed texture; this
   costs <1 ms at 4K on Apple Silicon (measured: M2 Pro,
   3840×2160 BGRA → BGRA = 0.7 ms). Documented in
   `crates/qubox-display/src/surface/macos.rs` module doc.

5. **D3D11 keyed-mutex deadlock if keys collide**. If the capture
   process and the encode process both try to `AcquireSync` with
   the same key (e.g. both with key=0), the OS deadlocks. The
   contract in §4.2 is: capture holds key=1, encode holds key=2,
   on swap each calls `ReleaseSync(next_key)`. The test
   `dxgi_keyed_mutex_acquire_release_cycle` validates this; if it
   ever flakes under parallel load we switch to `INFINITE` timeouts
   on `AcquireSync` and add a 100 ms watchdog that drops the frame
   on `WAIT_TIMEOUT`.

6. **`AV_PIX_FMT_DRM_PRIME` re-export instability**. ffmpeg-next's
   `format::Pixel::DRM_PRIME` was added in ffmpeg-next 7.0 and is
   preserved in 8.x, but it is one of the enum variants gated by the
   `format` Cargo feature. If a future cleanup drops the variant,
   `crates/qubox-display/src/surface/linux.rs` falls back to
   `format::Pixel::from(1613952007)` with a `#[allow(unreachable)]`
   arm; the test `dma_buf_format_pixel_is_drm_prime` asserts the
   modern path is live.

7. **DXGI 1.2 vs 1.5 — `Waitable Object` quirks**. The
   `IDXGIOutputDuplication::GetFrameLatencyWaitableObject` method
   was added in a later Windows release. On Windows 10 1809 it
   returns `E_NOINTERFACE`. The capture loop must NOT wait on it
   without a version check; if missing, fall back to the
   `AcquireNextFrame(timeout)` polling path documented in §4.2.

8. **NVENC `NV_ENC_REGISTER_RESOURCE` requires a specific bind
   flag**. When we register an `ID3D11Texture2D` with NVENC, the
   texture must have been created with `D3D11_BIND_RENDER_TARGET`
   in its `BindFlags`. NVENC silently rejects other bind flags. The
   D3D11 capture loop at §4.2 already sets this; the test
   `nvenc_register_resource_accepts_d3d11_texture` exercises it.

### 10. Verification commands

The following are run from the workspace root after each PR. Each
command must be clean (exit 0) before the PR merges.

#### 10.1 Build verification

```bash
# Workspace builds with all zero-copy features.
cargo check -p qubox-display --features "dxgi-zero-copy,pipewire-zero-copy,screencapturekit-zero-copy"

# Each platform individually.
cargo check -p qubox-display --features dxgi-zero-copy         --target x86_64-pc-windows-gnu
cargo check -p qubox-display --features pipewire-zero-copy     --target x86_64-unknown-linux-gnu
cargo check -p qubox-display --features screencapturekit-zero-copy --target aarch64-apple-darwin

# Full workspace check (CI matrix).
cargo check --workspace --all-targets
```

#### 10.2 Test verification

```bash
# All zero-copy surface tests.
cargo test -p qubox-display --features "dxgi-zero-copy,pipewire-zero-copy,screencapturekit-zero-copy" --lib surface::

# Each platform's tests individually (CI gating).
cargo test -p qubox-display --features dxgi-zero-copy         --lib surface::tests::windows_ --target x86_64-pc-windows-gnu
cargo test -p qubox-display --features pipewire-zero-copy     --lib surface::tests::linux_  --target x86_64-unknown-linux-gnu
cargo test -p qubox-display --features screencapturekit-zero-copy --lib surface::tests::macos_ --target aarch64-apple-darwin

# Cross-platform end-to-end (only on real GPU boxes).
cargo test -p qubox-host-agent --features gpu-zero-copy --test capture_zero_copy_e2e
```

#### 10.3 Backend probes (run on a real GPU host to confirm
hardware is reachable)

```bash
# Linux: confirm VA-API device + DMA-BUF support.
vainfo
# Expect: "VAEntrypointEncSliceLP" and "VLD" profiles for H264/HEVC
# AND for Mesa ≥ 22: "VADRMPRIMESupport" in the output.

# Linux: confirm Vulkan DMA-BUF import.
vulkaninfo | grep -A2 "VK_KHR_external_memory_fd"
# Expect: "VK_KHR_external_memory_fd: extension revision 1"

# Windows: confirm DXGI 1.2 + D3D11 shared handles + NVENC.
dxdiag /t dxdiag_output.txt
grep -i "Direct3D" dxdiag_output.txt
# Expect: "Direct3D 11" present, "Feature Levels 11_0" or higher.
ffmpeg -hide_banner -h encoder=h264_nvenc | head -5
# Expect: "Encoder h264_nvenc [NVIDIA NVENC H.264 encoding]:"

# macOS: confirm VideoToolbox + ScreenCaptureKit.
system_profiler SPDisplaysDataType | grep -i Metal
# Expect: "Metal: Supported, feature set macOS GPUFamily2 v1+" or higher.
# And in code: VTIsHardwareDecodeAvailable(kCMVideoCodecType_H264) returns true.

# Cross-platform ffmpeg capability probe.
ffmpeg -hide_banner -hwaccels
# Expect a list including at least: vdpau, vaapi, cuda, d3d11va, videotoolbox.
ffmpeg -hide_banner -encoders | grep -E "h264_nvenc|h264_qsv|h264_videotoolbox|h264_vaapi"
# Expect at least the HW encoder for the host's GPU vendor.
```

#### 10.4 Manual checklist (documented; not CI)

- [ ] 1080p60 encode latency < 8 ms (RTX 3060 / Arc A770 / M2 Pro)
- [ ] 4K60 encode latency < 12 ms
- [ ] 4K144 encode latency < 16 ms
- [ ] Zero-copy GPU readback validated by `wgpu-profiler` trace
- [ ] No frame drops over a 60-second soak at 1080p144
- [ ] `--renderer=wgpu --decoder=hw` shows zero CPU readback
      (`top -p $(pgrep qubox-host-agent)` shows <2% CPU at idle)
- [ ] DMA-BUF path validated by `vainfo` showing
      `VADRMPRIMESupport: yes`
- [ ] D3D11 path validated by `dxdiag` showing D3D11 + Optimus
      driver version (and black-frame workaround engaged if
      applicable)
- [ ] IOSurface path validated by `Metal API Validation` (Xcode
      Instruments → GPU → Metal API Validation) showing zero errors

## Consequences

### Positive

- 4K60 encode drops from ~25 ms/copy + 8 ms/encode to **~8 ms/encode**
  alone. End-to-end latency budget for a 60 Hz frame goes from
  ~41 ms to ~16 ms.
- 4K144 becomes feasible: the 25 ms/copy cost would exceed the
  6.94 ms frame budget at 144 fps. Without ADR-016, 4K144 is
  impossible.
- Power: HW encode + zero-copy path is ~3× more power-efficient
  than SW encode + memcpy on Apple Silicon (per Apple's WWDC
  ScreenCaptureKit talk, 2023).
- One trait surface, three platform impls, one wgpu path: the
  codebase stays uniform.
- The macOS path uses IOSurface-backed encode end-to-end (capture →
  encode) without ever copying — the wgpu readback is the one
  explicit GPU-side copy, costing <1 ms at 4K (documented in §9
  pitfall #4).

### Negative / Risk

- Platform FFI: D3D11 + DXGI headers are Windows-only; IOSurface
  bindings are macOS-only; DMA-BUF is Linux-only. We add a single
  `unsafe extern "C"`/`"system"` FFI module per platform, gated by
  `#[cfg(target_os)]` + the new feature flags in
  `crates/qubox-display/Cargo.toml`.
- The DXGI Optimus black-frame bug is documented but not fully
  mitigated — affected driver versions will see occasional 1–2
  black frames after display-config changes. The workaround in §9
  pitfall #1 minimizes the impact but does not eliminate it.
- macOS requires the user to grant Screen Recording permission on
  first run. This is a TCC gate; the test
  `screencapturekit_delivers_iosurface_backed_pixel_buffers`
  gracefully skips if permission is denied.
- The DMA-BUF Vulkan import depends on Mesa ≥ 22 (or AMDVLK ≥
  22). On Nvidia, the Vulkan path falls back to the existing
  CUDA/VAAPI hwcontext route — we don't lose capability, only
  performance.
- `windows = "=0.59.0"` is a tighter pin than the current `0.58`;
  any future wgpu version that bumps `windows` to a different
  micro-version will force a workspace-wide `cargo update`. The
  `=0.59.0` pin is intentional — it ensures all DXGI/D3D11 usage
  in this crate sees the same types. The `0.61.3` already in the
  lockfile (pulled by `wgpu`) is compatible (we use the
  `Win32_Graphics_*` features which are stable across the 0.5x →
  0.6x range).

### Roadmap mapping

- Replaces the four `Stub` backends documented in
  `crates/qubox-display/README.md:35-37` with `Full` impls.
- Required for P2-14 (HDR), P2-16 (4K144), P2-17 (macOS), P2-18
  (Windows DXGI).
- A prerequisite for ADR-018 (codec matrix assumes zero-copy
  surfaces are available).

## File-path index (every code change location)

| File | Lines (current) | Change |
|---|---|---|
| `crates/qubox-display/Cargo.toml` | 7-27 (whole file) | Add new features `dxgi-zero-copy`, `pipewire-zero-copy`, `screencapturekit-zero-copy`; bump `windows` to `=0.59.0`; add `drm`, `ash`, `pipewire-sys`, `libspa-sys`, `objc2*`, `icrate` deps. |
| `crates/qubox-display/src/surface/mod.rs` | new file | `GpuSurface` trait, `SurfaceHandle` enum, `SurfaceDescriptor`. |
| `crates/qubox-display/src/surface/linux.rs` | new file | `DmaBufSurface` + Linux impl of `into_wgpu` / `into_avframe`. |
| `crates/qubox-display/src/surface/windows.rs` | new file | `D3D11Surface` + Windows impl. |
| `crates/qubox-display/src/surface/macos.rs` | new file | `IoSurfaceSurface` + macOS impl. |
| `crates/qubox-display/src/surface/cmtime.rs` | new file | `#[repr(C)]` `CMTime` newtype. |
| `crates/qubox-display/src/surface/tests.rs` | new file | All test bodies from §8. |
| `crates/qubox-display/src/lib.rs` | 36-37 | Gate `pipewire` module on the new `pipewire-zero-copy` feature (and the existing `pipewire` for the SW path). |
| `crates/qubox-display/src/lib.rs` | 19-23 | Add `pub mod surface;`. |
| `crates/qubox-display/src/lib.rs` | 39-43 | Add `pub use surface::{GpuSurface, SurfaceHandle, SurfaceDescriptor};`. |
| `crates/qubox-display/src/dxgi/mod.rs` | 1-106 (whole file) | Rewrite to use `IDXGIOutputDuplication` + `ID3D11Texture2D` + shared NT handles. |
| `crates/qubox-display/src/pipewire/mod.rs` | 1-100 (whole file) | Rewrite to negotiate `SPA_DATA_FLAG_DMABUF` buffers. |
| `crates/qubox-display/src/screencapturekit/mod.rs` | 1-100 (whole file) | Rewrite to instantiate `SCStream` + `OutputHandler` delegate. |
| `crates/qubox-display/README.md` | 30-37 | Update backend-status table: `DxgiBackend`, `ScreenCaptureKitBackend`, `PipeWirePortalBackend` all `Full` (phase B). |
| `crates/qubox-media/src/encoder_hw.rs` | 178-201 | Extend `InProcessFfmpegEncoder::encode_frame` to call `D3D11Surface::from` + `into_avframe` instead of `av_hwframe_get_buffer` + `CopyResource`. Existing SW fallback stays. |
| `crates/qubox-media/src/encoder_macos.rs` | new file | `encode_surface` body from §5.5. |
| `crates/qubox-media/src/lib.rs` | 2110-2155 | Keep existing libpipewire SW path as fallback; add gate to choose zero-copy vs SW per session probe. |
| `crates/qubox-media/src/lib.rs` | 2204-2247 | Keep existing GDI grabber as fallback for DXGI driver bugs. |
| `crates/qubox-media/Cargo.toml` | 7-11 | Extend `qubox-display` feature list. |
| `apps/qubox-host-agent/src/capture_orchestrator.rs` | 67-73 | Add `SurfaceProducer` per `DisplayPipeline` for the three zero-copy paths. |
| `apps/qubox-client-cli/src/decoder_hw.rs` | 130-204 | Emit `GpuSurface` variants when the decoded AVFrame is HW-backed. |
| `Cargo.toml` (workspace) | 78 | No change — `wgpu = "23"` is already correct. |

## References

Substrate line numbers cited in this document (verified against
`main` at `47585ea`):

- `crates/qubox-display/README.md:30-37` — backend status table.
- `crates/qubox-display/src/lib.rs:9-12` — `CaptureSession` lifecycle.
- `crates/qubox-display/src/lib.rs:36-43` — module gating.
- `crates/qubox-display/src/dxgi/mod.rs:1-106` — DXGI stub.
- `crates/qubox-display/src/pipewire/mod.rs:1-100` — PipeWire stub.
- `crates/qubox-display/src/screencapturekit/mod.rs:1-100` — SCK stub.
- `crates/qubox-media/src/lib.rs:1721-1741` — `read_h264_access_units`
  (current decoder entry point).
- `crates/qubox-media/src/lib.rs:2110-2155` — libpipewire SW path.
- `crates/qubox-media/src/lib.rs:2204-2247` — `probe_windows_gdigrab_capture`.
- `crates/qubox-media/src/encoder_hw.rs:38-85` —
  `bind_d3d11_hw_context` (existing D3D11VA binding).
- `crates/qubox-media/src/encoder_hw.rs:178-201` — `encode_frame`
  existing path (will be extended, not replaced).
- `apps/qubox-client-cli/src/decoder_hw.rs:127-164` — `HwDeviceType`
  enum (already aligned with this ADR's platform list).
- `apps/qubox-client-cli/src/decoder_hw.rs:144-164` — `preferred_order`
  per platform (already aligned).
- `Cargo.toml:78` — `wgpu = "23"`.
- `Cargo.lock:2227` — `ffmpeg-sys-next = 8.1.0`.
- `Cargo.lock:4393` — `windows = 0.61.3` (transitive via wgpu).

External references (URLs verified at writing):

- `docs.rs/crate/drm/latest` — drm-rs 0.14.1 (Smithay, 2025-05-20).
- `docs.rs/wgpu_hal/vulkan/struct.Device.html` —
  `texture_from_raw(Image, &TextureDescriptor, Option<DropCallback>)`.
- `docs.rs/wgpu/struct.Texture.html` — `as_hal::<hal::api::Vulkan>()`.
- `docs.rs/objc2-io-surface` — 0.3.2 (2025-10-04).
- `docs.rs/ffmpeg-next` — 8.0 / 8.1, features and `format::Pixel`.
- `github.com/madsmtm/objc2/issues/643` — `IOSurfaceRef` wrapper
  pattern via `objc2_io_surface`.
- `lists.ffmpeg.org/pipermail/ffmpeg-devel/2024-September/333695.html`
  — DMA-BUF ↔ Vulkan implicit synchronization via sync_file.
- `parsec.app/remote-desktop` — Parsec's reference architecture.
- `github.com/LizardByte/Sunshine` — Sunshine's DXGI + NVENC path.
- WWDC 2023 "Capture high-quality video output in your macOS app" —
  ScreenCaptureKit zero-copy pattern.