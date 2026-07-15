use std::time::Duration;

use async_trait::async_trait;

use crate::error::{CaptureError, DisplayError};
use crate::types::{
    BackendCapabilities, CaptureOptions, CapturedFrame, DisplayId, DisplayInfo, DisplayState,
    VirtualDisplayConfig,
};

/// An opaque handle to a window, used by move_window_to_display.
/// Platform-specific: X11 Window (u32), Windows HWND (isize), macOS NSWindow*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowHandle {
    X11(u32),
    Windows(isize),
    Mac(*const std::ffi::c_void),
}

// Safety: WindowHandle::Mac wraps a raw pointer that is never dereferenced
// by Rust code — it is only passed back to OS APIs.
unsafe impl Send for WindowHandle {}
unsafe impl Sync for WindowHandle {}

/// Data-plane abstraction for capturing pixel data from displays.
/// Implementations are per-platform (X11+RandR, DXGI, ScreenCaptureKit, PipeWire).
#[async_trait]
pub trait CaptureBackend: Send + Sync + 'static {
    /// Enumerate all displays visible to the OS at this moment.
    /// Returns the full list of physical + virtual displays.
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, CaptureError>;

    /// Report which color spaces, HDR metadata, scaling modes,
    /// and per-frame pixel formats this backend supports.
    fn list_capabilities(&self) -> BackendCapabilities;

    /// Open a capture session for the given `display`.
    /// Returns a boxed `CaptureSession` that produces frames.
    async fn open_capture(
        &self,
        display: DisplayId,
        options: CaptureOptions,
    ) -> Result<Box<dyn CaptureSession>, CaptureError>;
}

/// A single display's capture session. Produces frames until closed.
/// The implementor owns the platform capture handle (e.g. IDXGIOutputDuplication,
/// SCStream, x11rb get_image loop thread).
pub trait CaptureSession: Send {
    /// Block for up to `timeout` waiting for the next captured frame.
    /// Returns `Ok(Some(frame))` on new frame, `Ok(None)` on timeout or EOF,
    /// `Err(error)` on capture failure.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError>;

    /// The actual region being captured in the display's local coordinate space.
    /// This may differ from the display's full size if a sub-region was requested.
    fn capture_region(&self) -> crate::types::Rect;

    /// The display ID this session was opened for.
    fn display_id(&self) -> DisplayId;

    /// The color space of the captured frames (e.g. sRGB, scRGB, Display P3, HDR10).
    fn color_space(&self) -> crate::types::ColorSpaceId;

    /// The display's current refresh rate in Hz (e.g. 60.0, 144.0, 240.0).
    fn refresh_hz(&self) -> f32;

    /// Close the capture session. The implementor should release any
    /// OS handles (e.g. ReleaseFrame for DXGI, stopCapture for SCK).
    fn close(&mut self) -> Result<(), CaptureError>;
}

/// Control-plane abstraction for display topology management:
/// virtual display creation/destruction, privacy blanking, window movement.
#[async_trait]
pub trait DisplayManager: Send + Sync + 'static {
    /// Enumerate all displays with full metadata.
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, DisplayError>;

    /// Set a display's state: Active (normal operation), Privacy (blanked, game
    /// moved away), or Blanked (DPMS off / disconnected).
    async fn set_display_state(
        &self,
        display: DisplayId,
        state: DisplayState,
    ) -> Result<(), DisplayError>;

    /// Move a window to a different display. Used by Privacy Mode to shift the
    /// game window from the physical display to the virtual display.
    async fn move_window_to_display(
        &self,
        window: WindowHandle,
        target: DisplayId,
    ) -> Result<(), DisplayError>;

    /// Create a virtual display (vkms on Linux, IddCx on Windows,
    /// CGVirtualDisplay on macOS). Returns the DisplayId of the new display.
    async fn create_virtual_display(
        &self,
        config: VirtualDisplayConfig,
    ) -> Result<DisplayId, DisplayError>;

    /// Destroy a virtual display.
    async fn destroy_virtual_display(&self, display: DisplayId) -> Result<(), DisplayError>;

    /// Whether this platform supports virtual display creation.
    fn supports_virtual_displays(&self) -> bool;
}
