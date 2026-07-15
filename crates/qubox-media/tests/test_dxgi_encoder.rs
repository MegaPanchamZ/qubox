//! Windows-only DXGI Output Duplication + soft/ffmpeg fallback smoke.
//! Skips cleanly when D3D11 or DuplicateOutput is unavailable (CI/headless).

#![cfg(windows)]

use std::time::Duration;

use qubox_display::dxgi::{
    create_duplication_interface, init_d3d11_device, CaptureStatus, DxgiBackend,
};
use qubox_display::types::{CaptureOptions, DisplayId};
use qubox_display::CaptureBackend;

#[tokio::test]
async fn dxgi_backend_opens_session_and_yields_frame_or_soft() {
    let backend = DxgiBackend::new().expect("DxgiBackend::new");
    let displays = backend.enumerate_displays().expect("enumerate");
    assert!(!displays.is_empty());

    let mut session = backend
        .open_capture(
            DisplayId(0),
            CaptureOptions {
                region: None,
                color_space: None,
                target_fps: 30,
                capture_cursor: true,
            },
        )
        .await
        .expect("open_capture");

    // Soft path always yields; real DXGI may timeout if no desktop change.
    let frame = session
        .next_frame(Duration::from_millis(100))
        .expect("next_frame");
    if let Some(f) = frame {
        assert!(f.width >= 16);
        assert_eq!(f.bytes.len() as u32, f.width * f.height * 4);
    }
    session.close().expect("close");
}

#[test]
fn dxgi_d3d11_init_or_skip() {
    match init_d3d11_device() {
        Ok((device, _ctx)) => {
            match create_duplication_interface(&device, 0) {
                Ok(_dup) => {
                    println!("DXGI DuplicateOutput OK");
                }
                Err(e) => {
                    println!("skip: DuplicateOutput unavailable: {e}");
                }
            }
        }
        Err(e) => {
            println!("skip: D3D11 unavailable: {e}");
        }
    }
}

#[test]
fn capture_status_variants_exist() {
    assert_ne!(CaptureStatus::Ok, CaptureStatus::Timeout);
    assert_ne!(CaptureStatus::Timeout, CaptureStatus::AccessLost);
}
