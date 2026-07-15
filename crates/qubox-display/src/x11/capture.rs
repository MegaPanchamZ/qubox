use std::sync::Arc;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto;

use crate::error::CaptureError;
use crate::traits::CaptureSession;
use crate::types::{CaptureOptions, CapturedFrame, ColorSpaceId, DisplayId, PixelFormat, Rect};

/// A capture session for a single X11 display (output) using `get_image(ZPixmap)`.
pub struct X11RandrCaptureSession<C: Connection + Send> {
    conn: C,
    display_id: DisplayId,
    root: xproto::Window,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    color_space: ColorSpaceId,
    refresh_hz: f32,
    frame_index: u64,
    target_fps: u32,
}

impl<C: Connection + Send> X11RandrCaptureSession<C> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        conn: C,
        display_id: DisplayId,
        root: xproto::Window,
        region: Rect,
        options: &CaptureOptions,
        refresh_hz: f32,
    ) -> Self {
        Self {
            conn,
            display_id,
            root,
            x: region.origin.x as i16,
            y: region.origin.y as i16,
            width: region.size.width as u16,
            height: region.size.height as u16,
            color_space: options.color_space.unwrap_or(ColorSpaceId::Srgb),
            refresh_hz,
            frame_index: 0,
            target_fps: options.target_fps,
        }
    }

    fn capture_frame(&mut self) -> Result<CapturedFrame, CaptureError> {
        let image = xproto::get_image(
            &self.conn,
            xproto::ImageFormat::Z_PIXMAP,
            self.root,
            self.x,
            self.y,
            self.width,
            self.height,
            !0u32, // plane_mask = all planes
        )
        .map_err(|e| CaptureError::X11(format!("get_image failed: {e}")))?
        .reply()
        .map_err(|e| CaptureError::X11(format!("get_image reply failed: {e}")))?;

        let captured_at = Instant::now();
        let frame_index = self.frame_index;
        self.frame_index += 1;

        Ok(CapturedFrame {
            display_id: self.display_id,
            width: self.width as u32,
            height: self.height as u32,
            // get_image returns BGRA8 data in ZPixmap format on 32-bit depth.
            bytes: Arc::new(image.data),
            format: PixelFormat::Bgra8,
            captured_at,
            frame_index,
        })
    }
}

impl<C: Connection + Send> CaptureSession for X11RandrCaptureSession<C> {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        let frame_duration = Duration::from_secs_f64(1.0 / self.target_fps as f64);
        let deadline = Instant::now() + timeout;

        // Capture one frame
        let frame = self.capture_frame()?;

        // Rate limit: sleep for the remaining frame duration
        let elapsed = frame.captured_at.elapsed();
        let sleep_dur = if frame_duration > elapsed {
            frame_duration - elapsed
        } else {
            Duration::ZERO
        };

        if sleep_dur > Duration::ZERO && Instant::now() + sleep_dur <= deadline {
            std::thread::sleep(sleep_dur);
        }

        // Check if we've exceeded the deadline
        if Instant::now() > deadline {
            return Ok(None);
        }

        Ok(Some(frame))
    }

    fn capture_region(&self) -> Rect {
        Rect {
            origin: crate::types::Point {
                x: self.x as i32,
                y: self.y as i32,
            },
            size: crate::types::Size {
                width: self.width as u32,
                height: self.height as u32,
            },
        }
    }

    fn display_id(&self) -> DisplayId {
        self.display_id
    }

    fn color_space(&self) -> ColorSpaceId {
        self.color_space
    }

    fn refresh_hz(&self) -> f32 {
        self.refresh_hz
    }

    fn close(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}
