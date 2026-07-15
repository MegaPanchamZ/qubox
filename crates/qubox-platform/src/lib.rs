use std::env;

use qubox_proto::{
    CapabilityProfile, CaptureKind, PeerDescriptor, PeerRole, PlatformOs, TransportKind, VideoCodec,
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
            ],
            capture: vec![CaptureKind::Pipewire, CaptureKind::X11],
            encoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            decoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            notes: vec![
                "Prefer PipeWire on Wayland-capable desktops and fall back to X11 when DISPLAY-backed capture is the only reliable path".to_string(),
                "VM and lab environments frequently resolve to X11 even when PipeWire is installed".to_string(),
                "Wayland and portal permissions still need explicit runtime modeling".to_string(),
            ],
        },
        (PlatformOs::Linux, PeerRole::Client) => CapabilityProfile {
            transports: vec![
                TransportKind::NativeQuic,
                TransportKind::WebRtc,
                TransportKind::RelayQuic,
            ],
            capture: Vec::new(),
            encoders: vec![VideoCodec::H264, VideoCodec::H265],
            decoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            notes: vec![
                "Native QUIC and browser-profile sessions should both be supported".to_string(),
            ],
        },
        (PlatformOs::Windows, PeerRole::Host) => CapabilityProfile {
            transports: vec![
                TransportKind::NativeQuic,
                TransportKind::WebRtc,
                TransportKind::RelayQuic,
            ],
            capture: vec![CaptureKind::DesktopDuplication],
            encoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            decoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            notes: vec![
                "Desktop Duplication is the first capture backend".to_string(),
                "Windows Graphics Capture can be added as an alternate path".to_string(),
            ],
        },
        (PlatformOs::Windows, PeerRole::Client) => CapabilityProfile {
            transports: vec![
                TransportKind::NativeQuic,
                TransportKind::WebRtc,
                TransportKind::RelayQuic,
            ],
            capture: Vec::new(),
            encoders: vec![VideoCodec::H264, VideoCodec::H265],
            decoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            notes: vec![
                "High-refresh and HDR output require explicit swapchain policy later".to_string(),
            ],
        },
        (PlatformOs::Macos, PeerRole::Host) => CapabilityProfile {
            transports: vec![
                TransportKind::NativeQuic,
                TransportKind::WebRtc,
                TransportKind::RelayQuic,
            ],
            capture: vec![CaptureKind::ScreenCaptureKit],
            encoders: vec![VideoCodec::H264, VideoCodec::H265],
            decoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            notes: vec![
                "ScreenCaptureKit and VideoToolbox are the primary host path".to_string(),
                "Permissions and entitlements are part of runtime design".to_string(),
            ],
        },
        (PlatformOs::Macos, PeerRole::Client) => CapabilityProfile {
            transports: vec![
                TransportKind::NativeQuic,
                TransportKind::WebRtc,
                TransportKind::RelayQuic,
            ],
            capture: Vec::new(),
            encoders: vec![VideoCodec::H264, VideoCodec::H265],
            decoders: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
            notes: vec![
                "Event injection and packaging restrictions must be handled carefully".to_string(),
            ],
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
        },
    }
}
