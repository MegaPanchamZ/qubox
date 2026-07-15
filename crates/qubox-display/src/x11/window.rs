//! X11 window management utilities for the DisplayManager.
//!
//! Provides `move_window_to_output` which uses `xrandr --output --primary`
//! to set the target output as the primary display, which causes the window
//! manager to move active windows to it. This is a best-effort operation;
//! window managers may ignore the change or respond differently.

use x11rb::connection::Connection;
use x11rb::protocol::xproto;

use crate::error::DisplayError;

/// Move a window to a specific RandR output.
///
/// Calls `xrandr --output <name> --primary` to make the target output the
/// primary display. This is an indirect approach: the window manager
/// typically moves focused windows to the new primary display.
///
/// A more precise approach using `_NET_WM_MOVERESIZE` EWMH client messages
/// is planned but deferred (needs x11rb's atom infrastructure which is
/// verbose). For Phase C, the xrandr fallback used by `move_window_to_display`
/// in `manager.rs` is sufficient.
pub fn move_window_to_output<C: Connection>(
    _conn: &C,
    _root: xproto::Window,
    _window: xproto::Window,
    _target_output: &str,
) -> Result<(), DisplayError> {
    // Phase C: return NotSupported to trigger the xrandr fallback in manager.rs.
    // The fallback calls `xrandr --output <name> --primary`.
    // A real EWMH implementation will be added in a follow-up.
    Err(DisplayError::NotSupported(
        "window move via x11rb deferred; use xrandr fallback",
    ))
}
