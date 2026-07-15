use crate::types::{DisplayId, DisplayInfo, Point, Rect};

/// Physical pixel on a specific display.
/// This is the raw framebuffer address. On a 3840×2160 display (no scaling),
/// a PhysicalPixel(0,0) is the top-left. On a Retina display (scale=2.0),
/// PhysicalPixel(0,0) is still the raw pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalPixel {
    pub x: i32,
    pub y: i32,
}

/// Logical (DPI-scaled) pixel.
/// On a Retina display at scale=2.0, LogicalPixel(1,0) maps to
/// PhysicalPixel(2,0). The client thinks in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalPixel {
    pub x: f32,
    pub y: f32,
}

/// A point in the OS's virtual desktop coordinate space.
/// This is the union of all display rectangles placed by the user
/// in the OS display settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VirtualDesktopPoint {
    pub x: i32,
    pub y: i32,
}

/// A rectangle in a display's local coordinate space.
/// Used by CaptureOptions::region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CaptureRegion {
    pub display_id: DisplayId,
    pub rect: Rect,
}

impl PhysicalPixel {
    /// Convert a logical pixel to a physical pixel using the given scale factor.
    /// `PhysicalPixel(round(lp.x * scale), round(lp.y * scale))`
    pub fn from_logical(lp: LogicalPixel, scale: f32) -> Self {
        Self {
            x: (lp.x * scale).round() as i32,
            y: (lp.y * scale).round() as i32,
        }
    }
}

impl VirtualDesktopPoint {
    /// Convert a virtual desktop point to a display-local point.
    /// Returns the point relative to the display's top-left origin.
    pub fn to_display_local(self, display_position: Point<i32>) -> Point<i32> {
        Point {
            x: self.x - display_position.x,
            y: self.y - display_position.y,
        }
    }

    /// Create a virtual desktop point from a display-local point
    /// plus the display's position.
    pub fn from_display_local(point: Point<i32>, display_position: Point<i32>) -> Self {
        Self {
            x: point.x + display_position.x,
            y: point.y + display_position.y,
        }
    }
}

impl CaptureRegion {
    /// Check whether a virtual desktop point falls within this capture region.
    pub fn is_point_within(&self, point: VirtualDesktopPoint) -> bool {
        let local = point.to_display_local(Point {
            x: self.rect.origin.x,
            y: self.rect.origin.y,
        });
        local.x >= 0
            && (local.x as u32) < self.rect.size.width
            && local.y >= 0
            && (local.y as u32) < self.rect.size.height
    }

    /// Check whether a given rectangle is fully contained within this capture region.
    pub fn contains(&self, other: &Rect) -> bool {
        let other_right = other.origin.x + other.size.width as i32;
        let other_bottom = other.origin.y + other.size.height as i32;
        let self_right = self.rect.origin.x + self.rect.size.width as i32;
        let self_bottom = self.rect.origin.y + self.rect.size.height as i32;

        other.origin.x >= self.rect.origin.x
            && other.origin.y >= self.rect.origin.y
            && other_right <= self_right
            && other_bottom <= self_bottom
    }
}

/// Find the display containing the given virtual-desktop point.
/// Returns `None` if the point is not within any display.
pub fn display_at_point(desktop: &[DisplayInfo], point: VirtualDesktopPoint) -> Option<DisplayId> {
    for display in desktop {
        let right = display.position.x + display.size.width as i32;
        let bottom = display.position.y + display.size.height as i32;
        if point.x >= display.position.x
            && point.x < right
            && point.y >= display.position.y
            && point.y < bottom
        {
            return Some(display.id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Size;

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
                color_space: crate::types::ColorSpaceId::Srgb,
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
                color_space: crate::types::ColorSpaceId::Srgb,
                hdr_capable: false,
                is_virtual: false,
            },
        ]
    }

    #[test]
    fn physical_pixel_from_logical_scale_2() {
        let lp = LogicalPixel { x: 100.0, y: 200.0 };
        let pp = PhysicalPixel::from_logical(lp, 2.0);
        assert_eq!(pp, PhysicalPixel { x: 200, y: 400 });
    }

    #[test]
    fn physical_pixel_from_logical_scale_1() {
        let lp = LogicalPixel { x: 100.0, y: 200.0 };
        let pp = PhysicalPixel::from_logical(lp, 1.0);
        assert_eq!(pp, PhysicalPixel { x: 100, y: 200 });
    }

    #[test]
    fn virtual_desktop_point_round_trips_with_display_local() {
        let display_pos = Point { x: 1920, y: 0 };
        let vdp = VirtualDesktopPoint { x: 2000, y: 500 };
        let local = vdp.to_display_local(display_pos);
        assert_eq!(local, Point { x: 80, y: 500 });
        let round_trip = VirtualDesktopPoint::from_display_local(local, display_pos);
        assert_eq!(round_trip, vdp);
    }

    #[test]
    fn display_at_point_finds_correct_display() {
        let displays = test_displays();
        // Point on display 0
        assert_eq!(
            display_at_point(&displays, VirtualDesktopPoint { x: 100, y: 100 }),
            Some(DisplayId(0))
        );
        // Point on display 1
        assert_eq!(
            display_at_point(&displays, VirtualDesktopPoint { x: 2000, y: 500 }),
            Some(DisplayId(1))
        );
        // Point outside any display (below both)
        assert_eq!(
            display_at_point(&displays, VirtualDesktopPoint { x: 100, y: 2000 }),
            None
        );
        // Point outside to the left
        assert_eq!(
            display_at_point(&displays, VirtualDesktopPoint { x: -100, y: 100 }),
            None
        );
        // Point on the right edge (1920+2560 = 4480)
        assert_eq!(
            display_at_point(&displays, VirtualDesktopPoint { x: 4480, y: 500 }),
            None
        );
    }

    #[test]
    fn capture_region_is_point_within() {
        let region = CaptureRegion {
            display_id: DisplayId(0),
            rect: Rect {
                origin: Point { x: 0, y: 0 },
                size: Size {
                    width: 1920,
                    height: 1080,
                },
            },
        };
        assert!(region.is_point_within(VirtualDesktopPoint { x: 100, y: 100 }));
        assert!(region.is_point_within(VirtualDesktopPoint { x: 0, y: 0 }));
        assert!(!region.is_point_within(VirtualDesktopPoint { x: -1, y: 0 }));
        assert!(!region.is_point_within(VirtualDesktopPoint { x: 1920, y: 0 }));
    }

    #[test]
    fn capture_region_contains_rect() {
        let region = CaptureRegion {
            display_id: DisplayId(0),
            rect: Rect {
                origin: Point { x: 0, y: 0 },
                size: Size {
                    width: 1920,
                    height: 1080,
                },
            },
        };
        let inner = Rect {
            origin: Point { x: 100, y: 100 },
            size: Size {
                width: 500,
                height: 500,
            },
        };
        assert!(region.contains(&inner));

        let partly_outside = Rect {
            origin: Point { x: 1900, y: 100 },
            size: Size {
                width: 100,
                height: 100,
            },
        };
        assert!(!region.contains(&partly_outside));
    }

    #[test]
    fn display_at_point_edge_cases() {
        let displays = test_displays();
        // Point on the right boundary of display 0 (1920 is exclusive)
        assert_eq!(
            display_at_point(&displays, VirtualDesktopPoint { x: 1919, y: 500 }),
            Some(DisplayId(0))
        );
        assert_eq!(
            display_at_point(&displays, VirtualDesktopPoint { x: 1920, y: 500 }),
            Some(DisplayId(1))
        );
    }
}
