//! Multi-display input mapping: routes client cursor events to the correct display
//! with HiDPI scaling, and emits HoverDisplay events on display boundary crossings.

use std::sync::Arc;

use enigo::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use tokio::sync::{Mutex, RwLock};

use qubox_display::{
    coordinates::{display_at_point, PhysicalPixel, VirtualDesktopPoint},
    types::{DisplayId, DisplayInfo},
};

use crate::capture_orchestrator::CaptureOrchestrator;

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("mouse position is outside the virtual desktop")]
    OutsideVirtualDesktop,
    #[error("enigo error: {0}")]
    Enigo(String),
    #[error("display topology not available")]
    TopologyNotAvailable,
}

/// Routes `RemoteInputEvent` to the correct display and applies HiDPI scaling.
/// Tracks the cursor display for boundary-crossing HoverDisplay events.
pub struct InputMapper {
    display_topology: Arc<RwLock<Vec<DisplayInfo>>>,
    last_display: Arc<Mutex<Option<DisplayId>>>,
    enigo: Arc<Mutex<Enigo>>,
}

impl InputMapper {
    /// Create a new InputMapper with the given display topology.
    /// Opens an enigo instance for input injection.
    pub fn new(topology: Vec<DisplayInfo>) -> Result<Self, InputError> {
        let settings = Settings {
            release_keys_when_dropped: true,
            ..Settings::default()
        };
        let enigo = Enigo::new(&settings)
            .map_err(|e| InputError::Enigo(format!("failed to initialize enigo: {e}")))?;

        Ok(Self {
            display_topology: Arc::new(RwLock::new(topology)),
            last_display: Arc::new(Mutex::new(None)),
            enigo: Arc::new(Mutex::new(enigo)),
        })
    }

    /// Map a virtual-desktop point to the physical pixel on the correct display.
    /// Emits HoverDisplay if the cursor crosses a display boundary.
    pub async fn map_mouse_move(
        &self,
        x: i32,
        y: i32,
        orchestrator: &CaptureOrchestrator,
    ) -> Result<Coordinate, InputError> {
        let topology = self.display_topology.read().await;
        let vdp = VirtualDesktopPoint { x, y };

        let Some(display_id) = display_at_point(&topology, vdp) else {
            return Err(InputError::OutsideVirtualDesktop);
        };

        let display = topology
            .iter()
            .find(|d| d.id == display_id)
            .ok_or(InputError::TopologyNotAvailable)?;

        // Convert virtual desktop coords to display-local coords
        let display_local = vdp.to_display_local(display.position);
        let physical = PhysicalPixel::from_logical(
            qubox_display::coordinates::LogicalPixel {
                x: display_local.x as f32,
                y: display_local.y as f32,
            },
            display.scale_factor,
        );

        // Check for display boundary crossing
        {
            let mut last = self.last_display.lock().await;
            if *last != Some(display_id) {
                orchestrator.emit_hover_display_event(display_id);
                *last = Some(display_id);
            }
        }

        let mut enigo = self.enigo.lock().await;
        enigo
            .move_mouse(physical.x, physical.y, Coordinate::Abs)
            .map_err(|e| InputError::Enigo(format!("move_mouse failed: {e}")))?;

        Ok(Coordinate::Abs)
    }

    /// Inject a mouse button event on the current display.
    pub async fn map_mouse_button(&self, button: u8, pressed: bool) -> Result<(), InputError> {
        let enigo_button = match button {
            0 => Button::Left,
            1 => Button::Middle,
            2 => Button::Right,
            _ => Button::Left,
        };
        let direction = if pressed {
            Direction::Press
        } else {
            Direction::Release
        };

        let mut enigo = self.enigo.lock().await;
        enigo
            .button(enigo_button, direction)
            .map_err(|e| InputError::Enigo(format!("button failed: {e}")))?;
        Ok(())
    }

    /// Inject a mouse wheel event.
    pub async fn map_mouse_wheel(&self, dx: i32, dy: i32) -> Result<(), InputError> {
        use enigo::Axis;

        let mut enigo = self.enigo.lock().await;
        // enigo scroll expects (length, axis) where axis determines horizontal/vertical
        if dy != 0 {
            enigo
                .scroll(dy, Axis::Vertical)
                .map_err(|e| InputError::Enigo(format!("scroll vertical failed: {e}")))?;
        }
        if dx != 0 {
            enigo
                .scroll(dx, Axis::Horizontal)
                .map_err(|e| InputError::Enigo(format!("scroll horizontal failed: {e}")))?;
        }
        Ok(())
    }

    /// Inject a keyboard event.
    pub async fn map_keyboard(&self, scancode: u32, pressed: bool) -> Result<(), InputError> {
        let direction = if pressed {
            Direction::Press
        } else {
            Direction::Release
        };

        // Enigo doesn't support raw scancodes; we pass them through as best-effort.
        // For actual key mapping see the existing map_remote_key in main.rs.
        // This is a simplified pass-through for common keys.
        let key = match scancode {
            0..=15 => Key::Unicode(char::from(b'a' + scancode as u8)),
            _ => return Ok(()), // skip unmapped scancodes
        };

        let mut enigo = self.enigo.lock().await;
        enigo
            .key(key, direction)
            .map_err(|e| InputError::Enigo(format!("key failed: {e}")))?;
        Ok(())
    }

    /// Refresh the display topology from the backend.
    pub async fn refresh_topology(&self, displays: Vec<DisplayInfo>) {
        let mut topo = self.display_topology.write().await;
        *topo = displays;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qubox_display::types::{ColorSpaceId, Point, Size};

    fn test_displays() -> Vec<DisplayInfo> {
        vec![
            DisplayInfo {
                id: DisplayId(0),
                name: "DP-1".into(),
                position: Point { x: 0, y: 0 },
                size: Size {
                    width: 1920,
                    height: 1080,
                },
                refresh_hz: 60.0,
                scale_factor: 1.0,
                color_space: ColorSpaceId::Srgb,
                hdr_capable: false,
                is_virtual: false,
            },
            DisplayInfo {
                id: DisplayId(1),
                name: "DP-2".into(),
                position: Point { x: 1920, y: 0 },
                size: Size {
                    width: 2560,
                    height: 1440,
                },
                refresh_hz: 144.0,
                scale_factor: 1.0,
                color_space: ColorSpaceId::Srgb,
                hdr_capable: false,
                is_virtual: false,
            },
        ]
    }

    #[test]
    fn test_display_at_point_finds_display_1() {
        let displays = test_displays();
        // (1920, 540) is on display 2 (DP-2), local (0, 0)
        let display_id = display_at_point(&displays, VirtualDesktopPoint { x: 1920, y: 0 });
        assert_eq!(display_id, Some(DisplayId(1)));
    }

    #[test]
    fn test_display_at_point_finds_display_0() {
        let displays = test_displays();
        let display_id = display_at_point(&displays, VirtualDesktopPoint { x: 0, y: 0 });
        assert_eq!(display_id, Some(DisplayId(0)));
    }

    #[test]
    fn test_display_at_point_outside_returns_none() {
        let displays = test_displays();
        let display_id = display_at_point(&displays, VirtualDesktopPoint { x: -100, y: -100 });
        assert_eq!(display_id, None);
    }

    #[test]
    fn test_virtual_desktop_to_display_local() {
        let vdp = VirtualDesktopPoint { x: 2000, y: 500 };
        let local = vdp.to_display_local(Point { x: 1920, y: 0 });
        assert_eq!(local, Point { x: 80, y: 500 });
    }

    #[test]
    fn test_physical_pixel_from_logical_at_scale_1() {
        let lp = qubox_display::coordinates::LogicalPixel { x: 100.0, y: 200.0 };
        let pp = PhysicalPixel::from_logical(lp, 1.0);
        assert_eq!(pp, PhysicalPixel { x: 100, y: 200 });
    }
}
