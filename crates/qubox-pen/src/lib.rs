//! P2-15 pen / tablet capture and injection.
//!
//! Mirrors the `qubox-mic` and `qubox-clipboard` shape:
//! platform-agnostic trait surface, feature-gated per-platform
//! implementations, and a coalescer that bounds CPU usage at the
//! receive side.
//!
//! ## Architecture
//!
//! ```text
//!  ┌──────────────────┐      ┌─────────────────┐      ┌────────────────┐
//!  │ libinput / WinTab│ ───▶ │ PenCapture      │ ───▶ │ WirePenEvent   │ ───▶ QUIC
//!  │ (per-platform)   │      │ + coalesce 240→1k│      │                │      datagram
//!  └──────────────────┘      └─────────────────┘      └────────────────┘
//!                                                                       
//!  ┌──────────────────┐      ┌─────────────────┐      ┌────────────────┐
//!  │ uinput / WinTab  │ ◀─── │ PenInjector     │ ◀─── │ WirePenEvent   │ ◀─── QUIC
//!  │ (per-platform)   │      │                 │      │                │      datagram
//!  └──────────────────┘      └─────────────────┘      └────────────────┘
//! ```
//!
//! ## Platform support matrix
//!
//! | Platform | Capture            | Injection              |
//! |----------|--------------------|------------------------|
//! | Linux    | libinput (feature) | uinput  (feature)      |
//! | Windows  | WM_POINTER (stub)  | WinTab  (stub)         |
//! | macOS    | deferred per §14   | deferred per §14       |
//!
//! macOS is deferred per ADR-010 §14 (TCC `Input Monitoring`
//! permission is fragile in CLI tools). The crate compiles and runs
//! on macOS but capture / injection are no-ops until a future phase.

#![cfg_attr(all(target_os = "macos"), allow(dead_code))]

pub mod coalesce;
pub mod error;
pub mod platform;
pub mod traits;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod macos;

pub use coalesce::{CoalesceConfig, PenCoalescer};
pub use error::{PenCaptureError, PenInjectError};
pub use platform::{stub_capture, stub_injector, CurrentPlatformPen};
pub use traits::{PenCapture, PenDeviceInfo, PenEvent, PenInjector};

/// Re-export the wire types so callers do not need to depend on
/// `qubox-proto` directly.
pub use qubox_proto::{
    PenDeviceDescriptor, PenEventError, PenEventFlags, PenTool, WirePenEvent,
    PEN_DATAGRAM_DISCRIMINATOR, PEN_WIRE_HEADER_SIZE, PEN_WIRE_SIZE,
};

/// Library version, derived from Cargo.toml. Useful for
/// `tracing::info!` lines and Tauri GUI status displays.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn wire_pen_event_size_matches_discriminator_constants() {
        assert_eq!(PEN_WIRE_SIZE, 36);
        assert_eq!(PEN_WIRE_HEADER_SIZE, 8);
        assert_eq!(PEN_DATAGRAM_DISCRIMINATOR, 0x50);
    }

    #[test]
    fn pen_device_info_round_trips_through_serde() {
        let info = PenDeviceInfo {
            descriptor: PenDeviceDescriptor {
                device_id: 2,
                name: "Wacom Intuos PT M".to_string(),
                tools: vec![PenTool::Pen, PenTool::Eraser],
                max_pressure: 8191,
                max_tilt_degrees: 60,
                rotation_supported: true,
            },
        };
        let payload = serde_json::to_string(&info.descriptor).unwrap();
        let decoded: PenDeviceDescriptor = serde_json::from_str(&payload).unwrap();
        assert_eq!(info.descriptor, decoded);
    }
}
