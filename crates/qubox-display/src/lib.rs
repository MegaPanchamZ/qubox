//! # qubox-display
//!
//! Unified display capture and virtualization API for Qubox.
//!
//! This crate provides the platform abstraction layer for enumerating,
//! capturing, and managing displays. It defines three core traits:
//!
//! - [`CaptureBackend`]: data-plane — produce pixel data from displays.
//! - [`CaptureSession`]: a single display's capture lifecycle.
//! - [`DisplayManager`]: control-plane — create/destroy virtual displays,
//!   set privacy state, move windows between displays.
//!
//! Per-platform implementations:
//! - **Linux X11 + RandR**: first-class in Phase A (full implementation).
//! - **Windows DXGI**: Output Duplication + soft/ffmpeg fallback.
//! - **macOS ScreenCaptureKit**: compile-only stub in Phase A (deferred).
//! - **Linux Wayland PipeWire**: FFmpeg pipewire demuxer + soft fallback.

use std::env;

pub mod coordinates;
pub mod error;
pub mod ffmpeg_raw;
pub mod soft_capture;
pub mod traits;
pub mod types;

pub use ffmpeg_raw::{resolve_pipewire_node, FfmpegRawCaptureSession, FfmpegRawSource};
pub use soft_capture::{soft_capture_enabled, SoftCaptureSession};

#[cfg(all(target_os = "linux", feature = "x11"))]
pub mod x11;

#[cfg(all(target_os = "windows", feature = "dxgi"))]
pub mod dxgi;

#[cfg(all(target_os = "macos", feature = "screencapturekit"))]
pub mod screencapturekit;

#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub mod pipewire;

pub use error::{CaptureError, DisplayError};
pub use traits::{CaptureBackend, CaptureSession, DisplayManager};
pub use types::{
    BackendCapabilities, CaptureOptions, CapturedFrame, ColorSpaceId, DisplayId, DisplayInfo,
    DisplayState, PixelFormat, Point, Rect, Size, VirtualDisplayConfig,
};

/// Detect the best capture backend for the current platform and runtime session.
/// On Linux + X11, returns an X11RandrBackend.
/// On Linux + Wayland + pipewire feature, returns PipeWirePortalBackend.
/// On Windows, returns a DxgiBackend stub.
/// On macOS, returns a ScreenCaptureKitBackend stub.
#[allow(clippy::needless_return)]
pub fn detect_backend() -> Result<Box<dyn CaptureBackend>, CaptureError> {
    #[cfg(all(target_os = "linux", feature = "x11"))]
    {
        // Check for Wayland session first
        if is_wayland_session() {
            #[cfg(feature = "pipewire")]
            {
                return Ok(Box::new(crate::pipewire::PipeWirePortalBackend));
            }
            #[cfg(not(feature = "pipewire"))]
            {
                return Err(CaptureError::NotSupported(
                    "Wayland session detected but pipewire feature is not enabled; rebuild with --features pipewire",
                ));
            }
        }

        return crate::x11::X11RandrBackend::new().map(|b| Box::new(b) as Box<dyn CaptureBackend>);
    }

    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    {
        return Ok(Box::new(crate::pipewire::PipeWirePortalBackend));
    }

    #[cfg(all(target_os = "windows", feature = "dxgi"))]
    {
        return crate::dxgi::DxgiBackend::new()
            .map(|b| Box::new(b) as Box<dyn CaptureBackend>)
            .map_err(|e| CaptureError::Other(e.to_string()));
    }

    #[cfg(all(target_os = "macos", feature = "screencapturekit"))]
    {
        return Ok(Box::new(crate::screencapturekit::ScreenCaptureKitBackend));
    }

    #[cfg(not(any(
        all(target_os = "linux", feature = "x11"),
        all(target_os = "linux", feature = "pipewire"),
        all(target_os = "windows", feature = "dxgi"),
        all(target_os = "macos", feature = "screencapturekit"),
    )))]
    Err(CaptureError::NotSupported(
        "no display backend compiled for this platform; enable one of the features: x11, dxgi, screencapturekit, pipewire",
    ))
}

/// Detect the best display manager for the current platform and runtime session.
/// Dispatch follows the same logic as `detect_backend`.
#[allow(clippy::needless_return)]
pub fn display_manager() -> Result<Box<dyn DisplayManager>, DisplayError> {
    #[cfg(all(target_os = "linux", feature = "x11"))]
    {
        // Check for Wayland session first
        if is_wayland_session() {
            #[cfg(feature = "pipewire")]
            {
                return Ok(Box::new(crate::pipewire::PipeWirePortalBackend));
            }
            #[cfg(not(feature = "pipewire"))]
            {
                return Err(DisplayError::NotSupported(
                    "Wayland session detected but pipewire feature is not enabled; rebuild with --features pipewire",
                ));
            }
        }

        let context =
            crate::x11::X11RandrContext::new().map_err(|e| DisplayError::Other(e.to_string()))?;
        return Ok(Box::new(crate::x11::X11RandrDisplayManager::new(context)));
    }

    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    {
        return Ok(Box::new(crate::pipewire::PipeWirePortalBackend));
    }

    #[cfg(all(target_os = "windows", feature = "dxgi"))]
    {
        return crate::dxgi::DxgiBackend::new()
            .map(|b| Box::new(b) as Box<dyn DisplayManager>)
            .map_err(|e| DisplayError::Other(e.to_string()));
    }

    #[cfg(all(target_os = "macos", feature = "screencapturekit"))]
    {
        return Ok(Box::new(crate::screencapturekit::ScreenCaptureKitBackend));
    }

    #[cfg(not(any(
        all(target_os = "linux", feature = "x11"),
        all(target_os = "linux", feature = "pipewire"),
        all(target_os = "windows", feature = "dxgi"),
        all(target_os = "macos", feature = "screencapturekit"),
    )))]
    Err(DisplayError::NotSupported(
        "no display manager compiled for this platform; enable one of the features: x11, dxgi, screencapturekit, pipewire",
    ))
}

/// Check whether the current session is a Wayland session by examining environment variables.
fn is_wayland_session() -> bool {
    env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
        && env::var_os("WAYLAND_DISPLAY").is_some()
}

#[cfg(test)]
#[cfg(all(target_os = "linux", feature = "x11"))]
mod tests {
    use super::*;

    #[test]
    fn detect_backend_on_linux_x11_works() {
        // Only test if DISPLAY is a non-empty usable value
        if std::env::var("DISPLAY")
            .map(|d| !d.is_empty())
            .unwrap_or(false)
        {
            let backend = detect_backend().expect("detect_backend should succeed on X11");
            let displays = backend.enumerate_displays();
            assert!(
                displays.is_ok(),
                "enumerate_displays failed on X11: {:?}",
                displays.err()
            );
            let caps = backend.list_capabilities();
            assert!(caps.supported_formats.contains(&PixelFormat::Bgra8));
        }
    }

    #[test]
    fn detect_backend_wayland_errs_without_pipewire() {
        // Simulate Wayland session by setting env vars
        let orig_session = std::env::var("XDG_SESSION_TYPE").ok();
        let orig_wayland = std::env::var_os("WAYLAND_DISPLAY");
        std::env::set_var("XDG_SESSION_TYPE", "wayland");
        std::env::set_var("WAYLAND_DISPLAY", "wayland-0");

        let result = detect_backend();

        // Restore original env
        if let Some(s) = orig_session {
            std::env::set_var("XDG_SESSION_TYPE", s);
        } else {
            std::env::remove_var("XDG_SESSION_TYPE");
        }
        if let Some(w) = orig_wayland {
            std::env::set_var("WAYLAND_DISPLAY", w);
        } else {
            std::env::remove_var("WAYLAND_DISPLAY");
        }

        #[cfg(feature = "pipewire")]
        assert!(
            result.is_ok(),
            "detect_backend should return Ok with pipewire feature on Wayland"
        );

        #[cfg(not(feature = "pipewire"))]
        if let Err(CaptureError::NotSupported(msg)) = &result {
            assert!(
                msg.contains("pipewire"),
                "error should mention pipewire: {msg}"
            );
        } else {
            panic!("expected Err(NotSupported) on Wayland without pipewire");
        }
    }
}
