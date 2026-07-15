//! Linux X11 display capture and management backend.
//!
//! Uses `x11rb` with the RandR extension for display enumeration
//! and `xproto::get_image(ZPixmap)` for frame capture.
//!
//! ## Backend Capabilities (Phase A)
//!
//! - Enumerate: full support via RandR.
//! - Capture: full support via `get_image`, producing BGRA8 frames.
//! - Virtual displays: stub (Phase C: vkms + xrandr).
//! - Privacy mode: stub (Phase C: DPMS + vkms).
//! - Window move: stub (Phase C: `_NET_WM_MOVERESIZE`).
//!
//! ## Detection
//!
//! The backend detects X11 vs Wayland by checking `$WAYLAND_DISPLAY`.
//! On Wayland with the `pipewire` feature, the PipeWire backend is preferred.

mod capture;
pub mod coords;
mod enumerate;
mod manager;
mod window;

#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr;
use x11rb::protocol::xproto;
use x11rb::rust_connection::RustConnection;

use crate::error::CaptureError;
use crate::traits::{CaptureBackend, CaptureSession};
use crate::types::{
    BackendCapabilities, CaptureOptions, DisplayId, DisplayInfo, PixelFormat, Point, Rect, Size,
};

pub use manager::X11RandrDisplayManager;

/// Shared X11 connection context used by both X11RandrBackend and X11RandrDisplayManager.
/// Holds a single connection to avoid opening multiple connections to the same X server.
pub struct X11RandrContext {
    pub(crate) conn: Mutex<RustConnection>,
    pub(crate) root: xproto::Window,
    #[allow(dead_code)]
    pub(crate) screen_num: usize,
}

impl X11RandrContext {
    /// Connect to the X11 display and wrap in an Arc for sharing.
    pub fn new() -> Result<Arc<Self>, CaptureError> {
        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|e| CaptureError::X11(format!("failed to connect to X11: {e}")))?;
        let root = conn.setup().roots[screen_num].root;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
            root,
            screen_num,
        }))
    }

    /// Check if the RandR extension is available.
    pub fn has_randr(&self) -> bool {
        self.conn
            .lock()
            .unwrap()
            .extension_information(randr::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some()
    }
}

/// X11 implementation of CaptureBackend using RandR for enumeration
/// and `xproto::get_image(ZPixmap)` for frame capture.
pub struct X11RandrBackend {
    context: Arc<X11RandrContext>,
}

impl X11RandrBackend {
    /// Connect to the X11 display (uses `$DISPLAY` or `:0`).
    pub fn new() -> Result<Self, CaptureError> {
        let context = X11RandrContext::new()?;
        Ok(Self { context })
    }

    /// Create a new X11RandrBackend sharing an existing context.
    pub fn from_context(context: Arc<X11RandrContext>) -> Self {
        Self { context }
    }

    /// Access the shared context.
    pub fn context(&self) -> &Arc<X11RandrContext> {
        &self.context
    }

    /// Derive max_refresh_hz from actual RandR CRTC modes.
    fn max_refresh_from_modes(&self) -> f32 {
        let conn = self.context.conn.lock().unwrap();
        let resources = match randr::get_screen_resources(&*conn, self.context.root)
            .ok()
            .and_then(|r| r.reply().ok())
        {
            Some(r) => r,
            None => return 60.0,
        };

        let mut max_hz = 0.0_f64;
        for mode in &resources.modes {
            if mode.htotal > 0 && mode.vtotal > 0 {
                let hz = mode.dot_clock as f64 / (mode.htotal as f64 * mode.vtotal as f64);
                if hz > max_hz {
                    max_hz = hz;
                }
            }
        }
        if max_hz > 0.0 {
            max_hz as f32
        } else {
            60.0
        }
    }
}

#[async_trait]
impl CaptureBackend for X11RandrBackend {
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        let conn = self.context.conn.lock().unwrap();
        enumerate::enumerate_outputs(&*conn, self.context.root)
    }

    fn list_capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            supports_hdr: false,
            supports_scrgb: false,
            supports_virtual_display: false,
            max_refresh_hz: self.max_refresh_from_modes(),
            supported_formats: vec![PixelFormat::Bgra8],
        }
    }

    async fn open_capture(
        &self,
        display: DisplayId,
        options: CaptureOptions,
    ) -> Result<Box<dyn CaptureSession>, CaptureError> {
        let displays = self.enumerate_displays()?;
        let info = displays
            .iter()
            .find(|d| d.id == display)
            .ok_or(CaptureError::DisplayNotFound(display))?;

        let region = options.region.unwrap_or(Rect {
            origin: Point {
                x: info.position.x,
                y: info.position.y,
            },
            size: Size {
                width: info.size.width,
                height: info.size.height,
            },
        });

        // Open a separate X11 connection for the capture session
        let (new_conn, _) = x11rb::connect(None)
            .map_err(|e| CaptureError::X11(format!("failed to open capture connection: {e}")))?;

        let session = crate::x11::capture::X11RandrCaptureSession::new(
            new_conn,
            display,
            self.context.root,
            region,
            &options,
            info.refresh_hz,
        );

        Ok(Box::new(session))
    }
}
