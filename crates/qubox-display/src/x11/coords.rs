//! X11-specific coordinate helpers.
//!
//! RandR output positions map to the OS virtual desktop coordinate space.
//! No additional conversion is needed beyond the generic coordinate types.

// Re-export the generic coordinate types for convenience.
pub use crate::coordinates::{PhysicalPixel, VirtualDesktopPoint};
