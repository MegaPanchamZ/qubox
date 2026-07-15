//! Linux PipeWire / Wayland portal display capture backend.
//!
//! Production path: FFmpeg `-f pipewire` raw BGRA stream (portal node from
//! `QUBOX_PIPEWIRE_NODE`, default `default`). Soft path in CI / when FFmpeg
//! pipewire demuxer is unavailable.

#![cfg(all(target_os = "linux", feature = "pipewire"))]

use async_trait::async_trait;

use crate::error::{CaptureError, DisplayError};
use crate::ffmpeg_raw::{
    prefer_soft_capture, resolve_pipewire_node, FfmpegRawCaptureSession, FfmpegRawSource,
};
use crate::soft_capture::SoftCaptureSession;
use crate::traits::{CaptureBackend, CaptureSession, DisplayManager, WindowHandle};
use crate::types::{
    BackendCapabilities, CaptureOptions, ColorSpaceId, DisplayId, DisplayInfo, DisplayState,
    PixelFormat, Point, Size, VirtualDisplayConfig,
};

pub struct PipeWirePortalBackend;

impl PipeWirePortalBackend {
    fn default_displays() -> Vec<DisplayInfo> {
        let (w, h) = env_geometry().unwrap_or((1920, 1080));
        vec![DisplayInfo {
            id: DisplayId(0),
            name: format!("Wayland-1 (pipewire:{})", resolve_pipewire_node()),
            position: Point { x: 0, y: 0 },
            size: Size {
                width: w,
                height: h,
            },
            refresh_hz: 60.0,
            scale_factor: 1.0,
            color_space: ColorSpaceId::Srgb,
            hdr_capable: false,
            is_virtual: false,
        }]
    }
}

fn env_geometry() -> Option<(u32, u32)> {
    let w: u32 = std::env::var("QUBOX_CAPTURE_WIDTH").ok()?.parse().ok()?;
    let h: u32 = std::env::var("QUBOX_CAPTURE_HEIGHT").ok()?.parse().ok()?;
    Some((w.max(16), h.max(16)))
}

#[async_trait]
impl CaptureBackend for PipeWirePortalBackend {
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        Ok(Self::default_displays())
    }

    fn list_capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            supports_hdr: false,
            supports_scrgb: false,
            supports_virtual_display: false,
            max_refresh_hz: 240.0,
            supported_formats: vec![PixelFormat::Bgra8, PixelFormat::Nv12],
        }
    }

    async fn open_capture(
        &self,
        display_id: DisplayId,
        options: CaptureOptions,
    ) -> Result<Box<dyn CaptureSession>, CaptureError> {
        let info = Self::default_displays()
            .into_iter()
            .find(|d| d.id == display_id)
            .ok_or(CaptureError::DisplayNotFound(display_id))?;
        let fps = options.target_fps.max(1);

        if !prefer_soft_capture() {
            let src = FfmpegRawSource::PipeWire {
                node: resolve_pipewire_node(),
                width: info.size.width,
                height: info.size.height,
                fps,
            };
            match FfmpegRawCaptureSession::spawn(display_id, &src) {
                Ok(session) => {
                    tracing::info!(
                        id = display_id.0,
                        node = %resolve_pipewire_node(),
                        "PipeWire ffmpeg raw capture open"
                    );
                    return Ok(Box::new(session));
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "PipeWire ffmpeg capture failed; soft session"
                    );
                }
            }
        }

        Ok(Box::new(SoftCaptureSession::new(
            display_id,
            info.size.width,
            info.size.height,
            fps as f32,
        )))
    }
}

#[async_trait]
impl DisplayManager for PipeWirePortalBackend {
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        Ok(PipeWirePortalBackend::default_displays())
    }

    async fn set_display_state(
        &self,
        display_id: DisplayId,
        state: DisplayState,
    ) -> Result<(), DisplayError> {
        tracing::info!(
            id = display_id.0,
            ?state,
            "PipeWire set_display_state (logical)"
        );
        Ok(())
    }

    async fn move_window_to_display(
        &self,
        _window: WindowHandle,
        _target: DisplayId,
    ) -> Result<(), DisplayError> {
        Err(DisplayError::NotSupported(
            "move_window_to_display requires compositor protocol",
        ))
    }

    async fn create_virtual_display(
        &self,
        _config: VirtualDisplayConfig,
    ) -> Result<DisplayId, DisplayError> {
        Err(DisplayError::NotSupported(
            "virtual display creation requires wlr-output-management protocol",
        ))
    }

    async fn destroy_virtual_display(&self, _display: DisplayId) -> Result<(), DisplayError> {
        Ok(())
    }

    fn supports_virtual_displays(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_displays_named_pipewire() {
        let d = PipeWirePortalBackend::default_displays();
        assert_eq!(d.len(), 1);
        assert!(d[0].name.contains("pipewire"));
    }

    #[tokio::test]
    async fn open_capture_soft_in_ci() {
        std::env::set_var("QUBOX_SOFT_CAPTURE", "1");
        let backend = PipeWirePortalBackend;
        let mut session = backend
            .open_capture(
                DisplayId(0),
                CaptureOptions {
                    region: None,
                    color_space: None,
                    target_fps: 30,
                    capture_cursor: true,
                },
            )
            .await
            .expect("open");
        let frame = session
            .next_frame(std::time::Duration::from_millis(5))
            .expect("frame")
            .expect("some");
        assert!(frame.bytes.len() >= 16 * 16 * 4);
        session.close().unwrap();
        std::env::remove_var("QUBOX_SOFT_CAPTURE");
    }
}
