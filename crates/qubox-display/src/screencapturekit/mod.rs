//! macOS ScreenCaptureKit display capture backend.
//!
//! Enumerates a default display and opens [`SoftCaptureSession`] until
//! SCStream is linked. Production builds should replace soft frames with
//! IOSurface-backed frames from ScreenCaptureKit.

#![cfg(target_os = "macos")]

use async_trait::async_trait;

use crate::error::{CaptureError, DisplayError};
use crate::soft_capture::SoftCaptureSession;
use crate::traits::{CaptureBackend, CaptureSession, DisplayManager, WindowHandle};
use crate::types::{
    BackendCapabilities, CaptureOptions, ColorSpaceId, DisplayId, DisplayInfo, DisplayState,
    PixelFormat, Point, Size, VirtualDisplayConfig,
};

pub struct ScreenCaptureKitBackend;

impl ScreenCaptureKitBackend {
    fn default_displays() -> Vec<DisplayInfo> {
        vec![DisplayInfo {
            id: DisplayId(0),
            name: "Color LCD (soft)".into(),
            position: Point { x: 0, y: 0 },
            size: Size {
                width: 2560,
                height: 1600,
            },
            refresh_hz: 120.0,
            scale_factor: 2.0,
            color_space: ColorSpaceId::DisplayP3,
            hdr_capable: true,
            is_virtual: false,
        }]
    }
}

#[async_trait]
impl CaptureBackend for ScreenCaptureKitBackend {
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        Ok(Self::default_displays())
    }

    fn list_capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            supports_hdr: true,
            supports_scrgb: true,
            supports_virtual_display: false,
            max_refresh_hz: 480.0,
            supported_formats: vec![PixelFormat::Bgra8, PixelFormat::Rgba16F],
        }
    }

    async fn open_capture(
        &self,
        display: DisplayId,
        options: CaptureOptions,
    ) -> Result<Box<dyn CaptureSession>, CaptureError> {
        let info = Self::default_displays()
            .into_iter()
            .find(|d| d.id == display)
            .ok_or(CaptureError::DisplayNotFound(display))?;
        Ok(Box::new(SoftCaptureSession::new(
            display,
            info.size.width,
            info.size.height,
            options.target_fps.max(1) as f32,
        )))
    }
}

#[async_trait]
impl DisplayManager for ScreenCaptureKitBackend {
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        Ok(ScreenCaptureKitBackend::default_displays())
    }

    async fn set_display_state(
        &self,
        display: DisplayId,
        state: DisplayState,
    ) -> Result<(), DisplayError> {
        tracing::info!(?display, ?state, "SCK set_display_state (logical)");
        Ok(())
    }

    async fn move_window_to_display(
        &self,
        _window: WindowHandle,
        _target: DisplayId,
    ) -> Result<(), DisplayError> {
        Err(DisplayError::NotSupported(
            "CGVirtualDisplay entitlements required for window move",
        ))
    }

    async fn create_virtual_display(
        &self,
        _config: VirtualDisplayConfig,
    ) -> Result<DisplayId, DisplayError> {
        Err(DisplayError::NotSupported(
            "CGVirtualDisplay requires Apple entitlements; use BetterDummy as a workaround",
        ))
    }

    async fn destroy_virtual_display(&self, _display: DisplayId) -> Result<(), DisplayError> {
        Ok(())
    }

    fn supports_virtual_displays(&self) -> bool {
        false
    }
}
