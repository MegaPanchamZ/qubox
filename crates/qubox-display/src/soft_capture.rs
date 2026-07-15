//! Software / synthetic capture session for CI and platforms mid-port.
//! Produces solid-color BGRA frames so encode/QUIC/FileSync paths can run.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::CaptureError;
use crate::traits::CaptureSession;
use crate::types::{CapturedFrame, ColorSpaceId, DisplayId, PixelFormat, Point, Rect, Size};

pub struct SoftCaptureSession {
    display: DisplayId,
    width: u32,
    height: u32,
    fps: f32,
    frame_n: u64,
    closed: bool,
}

impl SoftCaptureSession {
    pub fn new(display: DisplayId, width: u32, height: u32, fps: f32) -> Self {
        Self {
            display,
            width: width.max(16),
            height: height.max(16),
            fps: fps.max(1.0),
            frame_n: 0,
            closed: false,
        }
    }

    pub fn primary() -> Self {
        Self::new(DisplayId(0), 1920, 1080, 60.0)
    }
}

impl CaptureSession for SoftCaptureSession {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        if self.closed {
            return Ok(None);
        }
        let period = Duration::from_secs_f32(1.0 / self.fps);
        if !timeout.is_zero() {
            std::thread::sleep(period.min(timeout));
        }
        self.frame_n = self.frame_n.saturating_add(1);
        let shade = ((self.frame_n % 255) as u8).saturating_add(16);
        let pixel = [shade, shade / 2, 40u8, 255u8];
        let mut bytes = Vec::with_capacity((self.width * self.height * 4) as usize);
        for _ in 0..(self.width * self.height) {
            bytes.extend_from_slice(&pixel);
        }
        Ok(Some(CapturedFrame {
            display_id: self.display,
            width: self.width,
            height: self.height,
            bytes: Arc::new(bytes),
            format: PixelFormat::Bgra8,
            captured_at: Instant::now(),
            frame_index: self.frame_n,
        }))
    }

    fn capture_region(&self) -> Rect {
        Rect {
            origin: Point { x: 0, y: 0 },
            size: Size {
                width: self.width,
                height: self.height,
            },
        }
    }

    fn display_id(&self) -> DisplayId {
        self.display
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

pub fn soft_capture_enabled() -> bool {
    matches!(
        std::env::var("QUBOX_SOFT_CAPTURE").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes")
    ) || std::env::var("CI").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_capture_yields_bgra_frame() {
        let mut s = SoftCaptureSession::new(DisplayId(0), 64, 48, 120.0);
        let f = s
            .next_frame(Duration::from_millis(1))
            .unwrap()
            .expect("frame");
        assert_eq!(f.width, 64);
        assert_eq!(f.bytes.len(), 64 * 48 * 4);
        s.close().unwrap();
        assert!(s.next_frame(Duration::ZERO).unwrap().is_none());
    }
}
