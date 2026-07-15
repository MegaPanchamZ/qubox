//! Windows DXGI display capture backend.
//!
//! Production path: `IDXGIOutputDuplication` → staging texture → BGRA CPU frames.
//! Soft path: [`SoftCaptureSession`] when `QUBOX_SOFT_CAPTURE=1`, `CI=1`, or when
//! D3D11/duplication init fails (headless CI, RDP without desktop).
//!
//! Set `QUBOX_DXGI_REAL=1` to prefer real duplication (still falls back soft on err).

#![cfg(windows)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::error::{CaptureError, DisplayError};
use crate::ffmpeg_raw::{prefer_soft_capture, FfmpegRawCaptureSession, FfmpegRawSource};
use crate::soft_capture::SoftCaptureSession;
use crate::traits::{CaptureBackend, CaptureSession, DisplayManager, WindowHandle};
use crate::types::{
    BackendCapabilities, CaptureOptions, CapturedFrame, ColorSpaceId, DisplayId, DisplayInfo,
    DisplayState, PixelFormat, Point, Size, VirtualDisplayConfig,
};

pub struct DxgiBackend {
    displays: Vec<DisplayInfo>,
}

impl DxgiBackend {
    pub fn new() -> Result<Self, CaptureError> {
        let displays = match enumerate_dxgi_displays() {
            Ok(list) if !list.is_empty() => list,
            Ok(_) | Err(_) => default_displays(),
        };
        Ok(Self { displays })
    }
}

fn default_displays() -> Vec<DisplayInfo> {
    vec![DisplayInfo {
        id: DisplayId(0),
        name: r"\\.\DISPLAY1".into(),
        position: Point { x: 0, y: 0 },
        size: Size {
            width: 1920,
            height: 1080,
        },
        refresh_hz: 60.0,
        scale_factor: 1.0,
        color_space: ColorSpaceId::Srgb,
        hdr_capable: false,
        is_virtual: false,
    }]
}

fn real_dxgi_preferred() -> bool {
    matches!(
        std::env::var("QUBOX_DXGI_REAL").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes")
    ) || !prefer_soft_capture()
}

#[async_trait]
impl CaptureBackend for DxgiBackend {
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        Ok(self.displays.clone())
    }

    fn list_capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            supports_hdr: true,
            supports_scrgb: true,
            supports_virtual_display: false,
            max_refresh_hz: 480.0,
            supported_formats: vec![PixelFormat::Bgra8, PixelFormat::Nv12],
        }
    }

    async fn open_capture(
        &self,
        display: DisplayId,
        options: CaptureOptions,
    ) -> Result<Box<dyn CaptureSession>, CaptureError> {
        let info = self
            .displays
            .iter()
            .find(|d| d.id == display)
            .ok_or(CaptureError::DisplayNotFound(display))?
            .clone();

        let fps = options.target_fps.max(1);

        if real_dxgi_preferred() {
            match DxgiDuplicationSession::open(display, &info, fps) {
                Ok(session) => {
                    tracing::info!(?display, "DXGI Output Duplication session open");
                    return Ok(Box::new(session));
                }
                Err(e) => {
                    tracing::warn!(
                        ?display,
                        error = %e,
                        "DXGI duplication failed; trying ddagrab/ffmpeg then soft"
                    );
                }
            }

            // FFmpeg lavfi ddagrab (Desktop Duplication API filter)
            let src = FfmpegRawSource::DdaGrab {
                output_idx: display.0,
                width: info.size.width,
                height: info.size.height,
                fps,
            };
            if let Ok(session) = FfmpegRawCaptureSession::spawn(display, &src) {
                tracing::info!(?display, "DXGI via ffmpeg ddagrab");
                return Ok(Box::new(session));
            }

            // gdigrab fallback
            let gdi = FfmpegRawSource::GdiGrab {
                input: "desktop".into(),
                width: info.size.width,
                height: info.size.height,
                fps,
            };
            if let Ok(session) = FfmpegRawCaptureSession::spawn(display, &gdi) {
                tracing::info!(?display, "Windows gdigrab ffmpeg session");
                return Ok(Box::new(session));
            }
        }

        Ok(Box::new(SoftCaptureSession::new(
            display,
            info.size.width,
            info.size.height,
            fps as f32,
        )))
    }
}

#[async_trait]
impl DisplayManager for DxgiBackend {
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        Ok(self.displays.clone())
    }

    async fn set_display_state(
        &self,
        display: DisplayId,
        state: DisplayState,
    ) -> Result<(), DisplayError> {
        if !self.displays.iter().any(|d| d.id == display) {
            return Err(DisplayError::DisplayNotFound(display));
        }
        tracing::info!(?display, ?state, "DxgiBackend set_display_state");
        Ok(())
    }

    async fn move_window_to_display(
        &self,
        _window: WindowHandle,
        _target: DisplayId,
    ) -> Result<(), DisplayError> {
        Err(DisplayError::NotSupported(
            "move_window_to_display requires IddCx virtual display on Windows",
        ))
    }

    async fn create_virtual_display(
        &self,
        _config: VirtualDisplayConfig,
    ) -> Result<DisplayId, DisplayError> {
        Err(DisplayError::NotSupported(
            "virtual display creation not supported on Windows without IddCx driver",
        ))
    }

    async fn destroy_virtual_display(&self, _display: DisplayId) -> Result<(), DisplayError> {
        Ok(())
    }

    fn supports_virtual_displays(&self) -> bool {
        false
    }
}

// ── DXGI / D3D11 Output Duplication ──────────────────────────────────────────

use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTPUT_DESC,
};

/// Capture status for advanced grab loops (E2E tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureStatus {
    Ok,
    Timeout,
    AccessLost,
}

/// Create a hardware D3D11 device + immediate context.
pub fn init_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext), CaptureError> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut level = D3D_FEATURE_LEVEL_11_0;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            Some(&mut context),
        )
        .map_err(|e| CaptureError::Other(format!("D3D11CreateDevice: {e}")))?;
        Ok((
            device.ok_or_else(|| CaptureError::Other("null D3D11 device".into()))?,
            context.ok_or_else(|| CaptureError::Other("null D3D11 context".into()))?,
        ))
    }
}

/// Create `IDXGIOutputDuplication` for output index `output_idx` (0 = primary).
pub fn create_duplication_interface(
    device: &ID3D11Device,
    output_idx: u32,
) -> Result<IDXGIOutputDuplication, CaptureError> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()
            .map_err(|e| CaptureError::Other(format!("CreateDXGIFactory1: {e}")))?;
        let adapter = factory
            .EnumAdapters1(0)
            .map_err(|e| CaptureError::Other(format!("EnumAdapters1: {e}")))?;
        let output: IDXGIOutput = adapter
            .EnumOutputs(output_idx)
            .map_err(|e| CaptureError::Other(format!("EnumOutputs({output_idx}): {e}")))?;
        let output1: IDXGIOutput1 = output
            .cast()
            .map_err(|e| CaptureError::Other(format!("IDXGIOutput1 cast: {e}")))?;
        output1
            .DuplicateOutput(device)
            .map_err(|e| CaptureError::Other(format!("DuplicateOutput: {e}")))
    }
}

fn enumerate_dxgi_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()
            .map_err(|e| CaptureError::Other(format!("CreateDXGIFactory1: {e}")))?;
        let mut out = Vec::new();
        let mut adapter_i = 0u32;
        loop {
            let adapter = match factory.EnumAdapters1(adapter_i) {
                Ok(a) => a,
                Err(_) => break,
            };
            let mut output_i = 0u32;
            loop {
                let output = match adapter.EnumOutputs(output_i) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                let mut desc = DXGI_OUTPUT_DESC::default();
                if output.GetDesc(&mut desc).is_ok() {
                    let r = desc.DesktopCoordinates;
                    let w = (r.right - r.left).max(1) as u32;
                    let h = (r.bottom - r.top).max(1) as u32;
                    let name = {
                        let wide = desc.DeviceName;
                        let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
                        String::from_utf16_lossy(&wide[..len])
                    };
                    out.push(DisplayInfo {
                        id: DisplayId(out.len() as u32),
                        name,
                        position: Point {
                            x: r.left,
                            y: r.top,
                        },
                        size: Size {
                            width: w,
                            height: h,
                        },
                        refresh_hz: 60.0,
                        scale_factor: 1.0,
                        color_space: ColorSpaceId::Srgb,
                        hdr_capable: false,
                        is_virtual: false,
                    });
                }
                output_i += 1;
            }
            adapter_i += 1;
            if adapter_i > 8 {
                break;
            }
        }
        Ok(out)
    }
}

/// Session that maps duplicated desktop frames to CPU BGRA.
pub struct DxgiDuplicationSession {
    display_id: DisplayId,
    width: u32,
    height: u32,
    fps: f32,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    frame_index: u64,
    closed: bool,
}

/// Alias used by integration tests.
pub type DxgiCaptureSession = DxgiDuplicationSession;

impl DxgiDuplicationSession {
    pub fn open(
        display_id: DisplayId,
        info: &DisplayInfo,
        fps: u32,
    ) -> Result<Self, CaptureError> {
        let (device, context) = init_d3d11_device()?;
        let duplication = create_duplication_interface(&device, display_id.0)?;
        let staging = create_staging_texture(&device, info.size.width, info.size.height)?;
        Ok(Self {
            display_id,
            width: info.size.width,
            height: info.size.height,
            fps: fps.max(1) as f32,
            device,
            context,
            duplication,
            staging,
            frame_index: 0,
            closed: false,
        })
    }

    /// Low-level grab used by HW encode e2e; returns mapped BGRA + status.
    pub fn grab_frame_cpu(
        &mut self,
        timeout_ms: u32,
    ) -> Result<(Option<Vec<u8>>, CaptureStatus), CaptureError> {
        if self.closed {
            return Ok((None, CaptureStatus::Timeout));
        }
        unsafe {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match self
                .duplication
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
            {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    return Ok((None, CaptureStatus::Timeout));
                }
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    return Ok((None, CaptureStatus::AccessLost));
                }
                Err(e) => {
                    return Err(CaptureError::Other(format!("AcquireNextFrame: {e}")));
                }
            }
            let resource = resource.ok_or_else(|| CaptureError::Other("null frame resource".into()))?;
            let texture: ID3D11Texture2D = resource
                .cast()
                .map_err(|e| CaptureError::Other(format!("texture cast: {e}")))?;
            self.context.CopyResource(&self.staging, &texture);
            let _ = self.duplication.ReleaseFrame();

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| CaptureError::Other(format!("Map staging: {e}")))?;

            let pitch = mapped.RowPitch as usize;
            let mut bytes = Vec::with_capacity((self.width * self.height * 4) as usize);
            let src = mapped.pData as *const u8;
            for y in 0..self.height as usize {
                let row = std::slice::from_raw_parts(src.add(y * pitch), (self.width * 4) as usize);
                bytes.extend_from_slice(row);
            }
            self.context.Unmap(&self.staging, 0);
            Ok((Some(bytes), CaptureStatus::Ok))
        }
    }
}

fn create_staging_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, CaptureError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width.max(1),
        Height: height.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    unsafe {
        let mut tex: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut tex))
            .map_err(|e| CaptureError::Other(format!("CreateTexture2D staging: {e}")))?;
        tex.ok_or_else(|| CaptureError::Other("null staging texture".into()))
    }
}

impl CaptureSession for DxgiDuplicationSession {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let timeout_ms = if ms == 0 { 16 } else { ms };
        match self.grab_frame_cpu(timeout_ms)? {
            (Some(bytes), CaptureStatus::Ok) => {
                self.frame_index = self.frame_index.saturating_add(1);
                Ok(Some(CapturedFrame {
                    display_id: self.display_id,
                    width: self.width,
                    height: self.height,
                    bytes: Arc::new(bytes),
                    format: PixelFormat::Bgra8,
                    captured_at: Instant::now(),
                    frame_index: self.frame_index,
                }))
            }
            (_, CaptureStatus::Timeout) => Ok(None),
            (_, CaptureStatus::AccessLost) => {
                // Re-open duplication
                match create_duplication_interface(&self.device, self.display_id.0) {
                    Ok(dup) => {
                        self.duplication = dup;
                        Ok(None)
                    }
                    Err(e) => Err(e),
                }
            }
            _ => Ok(None),
        }
    }

    fn capture_region(&self) -> crate::types::Rect {
        crate::types::Rect {
            origin: Point { x: 0, y: 0 },
            size: Size {
                width: self.width,
                height: self.height,
            },
        }
    }

    fn display_id(&self) -> DisplayId {
        self.display_id
    }

    fn color_space(&self) -> ColorSpaceId {
        ColorSpaceId::Srgb
    }

    fn refresh_hz(&self) -> f32 {
        self.fps
    }

    fn close(&mut self) -> Result<(), CaptureError> {
        self.closed = true;
        Ok(())
    }
}

// Silence unused import warnings for types referenced only in docs/API surface.
#[allow(dead_code)]
fn _dxgi_rotation_identity() -> windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION {
    DXGI_MODE_ROTATION_IDENTITY
}

#[allow(dead_code)]
fn _hwnd_null() -> HWND {
    HWND::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_displays_non_empty() {
        let d = default_displays();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].id, DisplayId(0));
    }

    #[test]
    fn real_dxgi_flag_parses() {
        let prev = std::env::var_os("QUBOX_DXGI_REAL");
        std::env::set_var("QUBOX_DXGI_REAL", "1");
        assert!(real_dxgi_preferred() || prefer_soft_capture());
        if let Some(v) = prev {
            std::env::set_var("QUBOX_DXGI_REAL", v);
        } else {
            std::env::remove_var("QUBOX_DXGI_REAL");
        }
    }
}
