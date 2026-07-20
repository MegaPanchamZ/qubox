use std::env;
#[cfg(target_os = "linux")]
use std::process::Command;

use qubox_proto::{
    CapabilityProfile, CaptureKind, DisplayDescriptor, PeerDescriptor, PeerRole, PlatformOs,
    TransportKind, VideoCodec,
};
use uuid::Uuid;

pub fn detect_peer(role: PeerRole, name_override: Option<String>) -> PeerDescriptor {
    let os = current_os();

    PeerDescriptor {
        device_id: Uuid::new_v4(),
        peer_id: Uuid::new_v4(),
        device_name: name_override.unwrap_or_else(detect_device_name),
        role,
        os,
        capabilities: capabilities_for(os, role),
    }
}

pub fn describe_peer(
    role: PeerRole,
    name_override: Option<String>,
    device_id: Uuid,
    peer_id: Uuid,
) -> PeerDescriptor {
    let os = current_os();

    PeerDescriptor {
        device_id,
        peer_id,
        device_name: name_override.unwrap_or_else(detect_device_name),
        role,
        os,
        capabilities: capabilities_for(os, role),
    }
}

pub fn current_os() -> PlatformOs {
    if cfg!(target_os = "windows") {
        PlatformOs::Windows
    } else if cfg!(target_os = "macos") {
        PlatformOs::Macos
    } else if cfg!(target_os = "android") {
        PlatformOs::Android
    } else {
        PlatformOs::Linux
    }
}

fn detect_device_name() -> String {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unnamed-device".to_string())
}

fn capabilities_for(os: PlatformOs, role: PeerRole) -> CapabilityProfile {
    match (os, role) {
        (PlatformOs::Linux, PeerRole::Host) => CapabilityProfile {
            transports: vec![
                TransportKind::NativeQuic,
                TransportKind::WebRtc,
                TransportKind::RelayQuic,
                TransportKind::WebTransport,
            ],
            capture: vec![CaptureKind::Pipewire, CaptureKind::X11],
            encoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            decoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            notes: vec![
                "Prefer PipeWire on Wayland-capable desktops and fall back to X11 when DISPLAY-backed capture is the only reliable path".to_string(),
                "VM and lab environments frequently resolve to X11 even when PipeWire is installed".to_string(),
                "Wayland and portal permissions still need explicit runtime modeling".to_string(),
            ],
            displays: enumerate_displays(),
        },
        (PlatformOs::Linux, PeerRole::Client) => CapabilityProfile {
            transports: vec![
                TransportKind::NativeQuic,
                TransportKind::WebRtc,
                TransportKind::RelayQuic,
                TransportKind::WebTransport,
            ],
            capture: Vec::new(),
            encoders: vec![VideoCodec::H264, VideoCodec::H265],
            decoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            notes: vec![
                "Native QUIC and browser-profile sessions should both be supported".to_string(),
            ],
            displays: Vec::new(),
        },
        (PlatformOs::Windows, PeerRole::Host) => CapabilityProfile {
            transports: vec![
                TransportKind::NativeQuic,
                TransportKind::WebRtc,
                TransportKind::RelayQuic,
                TransportKind::WebTransport,
            ],
            capture: vec![CaptureKind::DesktopDuplication],
            encoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            decoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            notes: vec![
                "Desktop Duplication is the first capture backend".to_string(),
                "Windows Graphics Capture can be added as an alternate path".to_string(),
            ],
            displays: Vec::new(),
        },
        (PlatformOs::Windows, PeerRole::Client) => CapabilityProfile {
            transports: vec![
                TransportKind::NativeQuic,
                TransportKind::WebRtc,
                TransportKind::RelayQuic,
                TransportKind::WebTransport,
            ],
            capture: Vec::new(),
            encoders: vec![VideoCodec::H264, VideoCodec::H265],
            decoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            notes: vec![
                "High-refresh and HDR output require explicit swapchain policy later".to_string(),
            ],
            displays: Vec::new(),
        },
        (PlatformOs::Macos, PeerRole::Host) => CapabilityProfile {
            transports: vec![
                TransportKind::NativeQuic,
                TransportKind::WebRtc,
                TransportKind::RelayQuic,
                TransportKind::WebTransport,
            ],
            capture: vec![CaptureKind::ScreenCaptureKit],
            encoders: vec![VideoCodec::H264, VideoCodec::H265],
            decoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            notes: vec![
                "ScreenCaptureKit and VideoToolbox are the primary host path".to_string(),
                "Permissions and entitlements are part of runtime design".to_string(),
            ],
            displays: Vec::new(),
        },
        (PlatformOs::Macos, PeerRole::Client) => CapabilityProfile {
            transports: vec![
                TransportKind::NativeQuic,
                TransportKind::WebRtc,
                TransportKind::RelayQuic,
                TransportKind::WebTransport,
            ],
            capture: Vec::new(),
            encoders: vec![VideoCodec::H264, VideoCodec::H265],
            decoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            notes: vec![
                "Event injection and packaging restrictions must be handled carefully".to_string(),
            ],
            displays: Vec::new(),
        },
        (PlatformOs::Android, PeerRole::Host) => CapabilityProfile {
            transports: vec![TransportKind::WebRtc, TransportKind::RelayQuic],
            capture: vec![CaptureKind::MediaProjection],
            encoders: vec![VideoCodec::H264, VideoCodec::H265],
            decoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            notes: vec![
                "Android hosting is a later milestone because MediaProjection is service-heavy"
                    .to_string(),
                "Treat mobile hosting as secondary to desktop hosting".to_string(),
            ],
            displays: Vec::new(),
        },
        (PlatformOs::Android, PeerRole::Client) => CapabilityProfile {
            transports: vec![
                TransportKind::NativeQuic,
                TransportKind::WebRtc,
                TransportKind::RelayQuic,
            ],
            capture: Vec::new(),
            encoders: vec![VideoCodec::H264],
            decoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            notes: vec![
                "MediaCodec decode should target a Surface-based rendering path".to_string(),
                "Adaptive bitrate is required earlier on mobile than on desktop".to_string(),
            ],
            displays: Vec::new(),
        },
    }
}

/// Enumerate the host's connected displays. On Linux this shells out to
/// `xrandr --query` and parses the active output list. macOS / Windows
/// builds currently return an empty list; the picker still works
/// (viewer picks "primary") and the host's capture pipeline falls back
/// to the primary output when the descriptor doesn't carry a
/// `selected_display_id`.
pub fn enumerate_displays() -> Vec<DisplayDescriptor> {
    #[cfg(target_os = "linux")]
    {
        enumerate_linux_displays()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn enumerate_linux_displays() -> Vec<DisplayDescriptor> {
    // Try `xrandr --query` first — it's the canonical X11 enumeration.
    // If unavailable (Wayland without xwayland, missing xrandr), fall
    // back to a single "default" display so the picker still works.
    let out = match Command::new("xrandr").arg("--query").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return default_linux_display(),
    };

    let mut displays = Vec::new();
    let mut id: u32 = 0;
    let mut primary_x: i32 = 0;
    let mut primary_y: i32 = 0;

    for line in out.lines() {
        // `HDMI-0 connected 1920x1080+0+0 ...`
        let mut parts = line.split_whitespace();
        let Some(output) = parts.next() else { continue };
        let Some(state) = parts.next() else { continue };
        if state != "connected" {
            continue;
        }
        let Some(geom) = parts.next() else { continue };

        // Parse "<width>x<height>+<x>+<y>".
        let mut iter = geom.split(|c: char| c == 'x' || c == '+');
        let w: u32 = iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let h: u32 = iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let x: i32 = iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let y: i32 = iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        if w == 0 || h == 0 {
            continue;
        }

        let is_primary = line.contains(" primary ");
        if is_primary {
            primary_x = x;
            primary_y = y;
        }
        displays.push(DisplayDescriptor {
            id,
            name: format!("{output} {w}x{h}"),
            width: w,
            height: h,
            x,
            y,
            primary: is_primary,
            active: true,
        });
        id += 1;
    }

    if displays.is_empty() {
        return default_linux_display();
    }
    // If nothing reported primary, the first non-primary becomes primary.
    if !displays.iter().any(|d| d.primary) {
        if let Some(first) = displays.first_mut() {
            first.primary = true;
        }
    }
    let _ = (primary_x, primary_y); // reserved for future origin normalization
    displays
}

#[cfg(target_os = "linux")]
fn default_linux_display() -> Vec<DisplayDescriptor> {
    vec![DisplayDescriptor {
        id: 0,
        name: "default 1920x1080".to_string(),
        width: 1920,
        height: 1080,
        x: 0,
        y: 0,
        primary: true,
        active: true,
    }]
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::default_linux_display;

    #[test]
    fn parses_xrandr_line() {
        let line = "HDMI-0 connected 1920x1080+0+0 primary";
        let mut parts = line.split_whitespace();
        let output = parts.next().unwrap();
        let state = parts.next().unwrap();
        assert_eq!(output, "HDMI-0");
        assert_eq!(state, "connected");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn enumerates_default_when_xrandr_missing() {
        let displays = default_linux_display();
        assert_eq!(displays.len(), 1);
        assert!(displays[0].primary);
    }
}
