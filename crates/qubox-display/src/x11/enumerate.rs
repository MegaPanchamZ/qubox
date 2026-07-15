use x11rb::connection::Connection as X11Connection;
use x11rb::protocol::randr;
use x11rb::protocol::xproto;

use crate::error::CaptureError;
use crate::types::{ColorSpaceId, DisplayId, DisplayInfo, Point, Size};

/// Enumerate all connected displays via RandR.
/// Returns a list of DisplayInfo, one per connected output with an active CRTC.
pub fn enumerate_outputs<C: X11Connection>(
    conn: &C,
    root: xproto::Window,
) -> Result<Vec<DisplayInfo>, CaptureError> {
    let resources = randr::get_screen_resources(conn, root)
        .map_err(|e| CaptureError::X11(format!("get_screen_resources failed: {e}")))?
        .reply()
        .map_err(|e| CaptureError::X11(format!("get_screen_resources reply failed: {e}")))?;

    let mut displays = Vec::new();
    let mut index = 0u32;

    for &output in &resources.outputs {
        let output_info = randr::get_output_info(conn, output, 0)
            .map_err(|e| CaptureError::X11(format!("get_output_info failed: {e}")))?
            .reply()
            .map_err(|e| CaptureError::X11(format!("get_output_info reply failed: {e}")))?;

        // Skip disconnected outputs
        if output_info.connection != randr::Connection::CONNECTED {
            continue;
        }

        let name = String::from_utf8_lossy(&output_info.name).to_string();

        // Get the CRTC info for position and size.
        // Crtc is u32; 0 means "no CRTC" in X11/RandR.
        let (position, size, refresh_hz) = if output_info.crtc != 0 {
            match randr::get_crtc_info(conn, output_info.crtc, 0)
                .map_err(|e| CaptureError::X11(format!("get_crtc_info failed: {e}")))?
                .reply()
            {
                Ok(crtc_info) => {
                    let pos = Point {
                        x: crtc_info.x as i32,
                        y: crtc_info.y as i32,
                    };
                    let sz = Size {
                        width: crtc_info.width as u32,
                        height: crtc_info.height as u32,
                    };
                    // Calculate refresh rate from the mode
                    let refresh = if let Some(mode_info) =
                        resources.modes.iter().find(|m| m.id == crtc_info.mode)
                    {
                        if mode_info.htotal > 0 && mode_info.vtotal > 0 {
                            (mode_info.dot_clock as f64)
                                / (mode_info.htotal as f64 * mode_info.vtotal as f64)
                        } else {
                            60.0
                        }
                    } else {
                        60.0
                    };
                    (pos, sz, refresh as f32)
                }
                Err(e) => {
                    tracing::warn!(?output, name, error = %e, "failed to get CRTC info");
                    (
                        Point { x: 0, y: 0 },
                        Size {
                            width: 0,
                            height: 0,
                        },
                        60.0,
                    )
                }
            }
        } else {
            // Output is connected but has no active CRTC (not in use)
            (
                Point { x: 0, y: 0 },
                Size {
                    width: 0,
                    height: 0,
                },
                60.0,
            )
        };

        // Skip outputs with zero size (not active)
        if size.width == 0 || size.height == 0 {
            continue;
        }

        displays.push(DisplayInfo {
            id: DisplayId(index),
            name,
            position,
            size,
            refresh_hz,
            // X11 does not expose HiDPI via RandR; use 1.0 as default.
            scale_factor: 1.0,
            // X11 does not have native HDR support in standard RandR.
            color_space: ColorSpaceId::Srgb,
            hdr_capable: false,
            is_virtual: false,
        });

        index += 1;
    }

    Ok(displays)
}
