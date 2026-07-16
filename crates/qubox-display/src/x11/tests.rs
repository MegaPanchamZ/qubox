//! Integration tests for the X11 backend.
//!
//! Tests 7-9 require a running X11 display (Xephyr :99 or similar).
//! They are gated behind `#[cfg(feature = "e2e")]` for CI control.
//! When `$DISPLAY` is not set, they print a skip message and return `Ok(())`.

use crate::error::DisplayError;
use crate::traits::{CaptureBackend, DisplayManager};
use crate::types::{
    ColorSpaceId, DisplayId, DisplayState, PixelFormat, Point, Size, VirtualDisplayConfig,
};

/// Check if an X11 display is available for testing.
fn x11_available() -> bool {
    // Empty string counts as unset (CI sets DISPLAY="" to force skip).
    let Ok(d) = std::env::var("DISPLAY") else {
        return false;
    };
    if d.is_empty() {
        return false;
    }
    // Probe connectivity so a dead Xvfb does not panic unit tests.
    super::X11RandrBackend::new().is_ok()
}

/// Skip message helper for display-less environments.
fn skip_message(test_name: &str) {
    eprintln!("SKIPPED: {test_name} (no X11 display available)");
}

// ──────────────────────────────────────────────
// Test 7: enumerate_displays returns at least 1 display
// ──────────────────────────────────────────────
#[test]
#[cfg(feature = "e2e")]
fn x11_enumerate_displays_returns_at_least_one_display() {
    use crate::x11::X11RandrBackend;

    if !x11_available() {
        skip_message("x11_enumerate_displays_returns_at_least_one_display");
        return;
    }

    let backend = X11RandrBackend::new().expect("failed to connect to X11 for test");
    let displays = backend
        .enumerate_displays()
        .expect("enumerate_displays failed");

    assert!(!displays.is_empty(), "expected at least 1 display, got 0");
    let first = &displays[0];
    assert!(!first.name.is_empty(), "display name must not be empty");
    assert!(first.size.width > 0, "display width must be > 0");
    assert!(first.size.height > 0, "display height must be > 0");
    assert!(first.refresh_hz > 0.0, "refresh_hz must be > 0");
}

// ──────────────────────────────────────────────
// Test 8: open_capture + next_frame returns a non-empty BGRA8 frame
// ──────────────────────────────────────────────
#[test]
#[cfg(feature = "e2e")]
fn x11_open_capture_returns_non_empty_frame() {
    use crate::traits::CaptureBackend;
    use crate::types::{CaptureOptions, DisplayId};
    use crate::x11::X11RandrBackend;
    use std::time::Duration;

    if !x11_available() {
        skip_message("x11_open_capture_returns_non_empty_frame");
        return;
    }

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime for test");

    rt.block_on(async {
        let backend = X11RandrBackend::new().expect("failed to connect to X11 for test");
        let displays = backend
            .enumerate_displays()
            .expect("enumerate_displays failed");

        if displays.is_empty() {
            eprintln!("SKIPPED: no displays available");
            return;
        }

        let options = CaptureOptions {
            region: None,
            color_space: None,
            target_fps: 30,
            capture_cursor: false,
        };

        let mut session = backend
            .open_capture(displays[0].id, options)
            .await
            .expect("open_capture failed");

        let frame = session
            .next_frame(Duration::from_secs(5))
            .expect("next_frame failed")
            .expect("next_frame returned None (timeout)");

        assert_eq!(frame.format, PixelFormat::Bgra8);
        assert!(frame.width > 0, "frame width must be > 0");
        assert!(frame.height > 0, "frame height must be > 0");
        assert!(!frame.bytes.is_empty(), "frame bytes must not be empty");
        assert_eq!(
            frame.bytes.len(),
            (frame.width * frame.height * 4) as usize,
            "BGRA8 frame size mismatch: expected {} bytes, got {}",
            frame.width * frame.height * 4,
            frame.bytes.len()
        );

        session.close().expect("close failed");
    });
}

// ──────────────────────────────────────────────
// Test 9: open_capture respects target_fps rate limit
// ──────────────────────────────────────────────
#[test]
#[cfg(feature = "e2e")]
fn x11_open_capture_respects_fps_limit() {
    use crate::error::CaptureError;
    use crate::traits::CaptureBackend;
    use crate::types::{CaptureOptions, DisplayId};
    use crate::x11::X11RandrBackend;
    use std::time::{Duration, Instant};

    if !x11_available() {
        skip_message("x11_open_capture_respects_fps_limit");
        return;
    }

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime for test");

    rt.block_on(async {
        let backend = X11RandrBackend::new().expect("failed to connect to X11 for test");
        let displays = backend
            .enumerate_displays()
            .expect("enumerate_displays failed");

        if displays.is_empty() {
            eprintln!("SKIPPED: no displays available");
            return;
        }

        let options = CaptureOptions {
            region: None,
            color_space: None,
            target_fps: 30,
            capture_cursor: false,
        };

        let mut session = backend
            .open_capture(displays[0].id, options)
            .await
            .expect("open_capture failed");

        // Capture frames for ~1 second at 30 fps, expect ~5-50 frames
        let start = Instant::now();
        let mut frame_count = 0u64;

        while start.elapsed() < Duration::from_secs(1) {
            match session.next_frame(Duration::from_millis(100)) {
                Ok(Some(_)) => frame_count += 1,
                Ok(None) => break,
                Err(e) => {
                    // If the display disconnects during test, that's ok
                    if matches!(e, CaptureError::X11(ref msg) if msg.contains("BadDrawable")) {
                        break;
                    }
                    panic!("next_frame failed: {e}");
                }
            }
        }

        assert!(
            frame_count >= 5,
            "expected at least 5 frames at 30 fps, got {frame_count}"
        );
        assert!(
            frame_count <= 60,
            "expected at most 60 frames at 30 fps, got {frame_count}"
        );

        session.close().expect("close failed");
    });
}

// ──────────────────────────────────────────────
// Test 10: list_capabilities returns correct X11 caps
// ──────────────────────────────────────────────
#[test]
fn x11_list_capabilities_returns_expected_caps() {
    if !x11_available() {
        skip_message("x11_list_capabilities_returns_expected_caps");
        return;
    }

    let backend = super::X11RandrBackend::new().expect("failed to connect to X11 for test");
    let caps = backend.list_capabilities();

    assert!(!caps.supports_hdr, "X11 backend should not support HDR");
    assert!(!caps.supports_scrgb, "X11 backend should not support scRGB");
    assert!(
        !caps.supports_virtual_display,
        "X11 backend should not support virtual displays in Phase A"
    );
    assert!(
        caps.max_refresh_hz >= 60.0,
        "max_refresh_hz should be >= 60"
    );
    assert_eq!(
        caps.supported_formats,
        vec![PixelFormat::Bgra8],
        "X11 should only support BGRA8"
    );
}

// ──────────────────────────────────────────────
// Test 11 (Phase C): DisplayManager set_display_state(Privacy) tries vkms
// ──────────────────────────────────────────────
#[test]
fn x11_display_manager_set_display_state_privacy_tries_vkms() {
    if !x11_available() {
        skip_message("x11_display_manager_set_display_state_privacy_tries_vkms");
        return;
    }

    let context = crate::x11::X11RandrContext::new().expect("failed to connect to X11");
    let mgr = crate::x11::X11RandrDisplayManager::new(context);

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");

    let result = rt.block_on(mgr.set_display_state(DisplayId::primary(), DisplayState::Privacy));
    // Phase C: tries modprobe vkms first. Without sudo, this returns VirtualDisplayFailed.
    // With sudo + vkms available, it may succeed.
    match result {
        Ok(()) => {} // vkms succeeded (sudo + module available)
        Err(DisplayError::VirtualDisplayFailed(_)) => {} // vkms failed as expected
        Err(DisplayError::DisplayNotFound(_)) => {} // enumeration may fail in some setups
        other => panic!("expected VirtualDisplayFailed or DisplayNotFound, got {other:?}"),
    }
}

// ──────────────────────────────────────────────
// Test 12 (Phase C): DisplayManager create_virtual_display tries vkms
// ──────────────────────────────────────────────
#[test]
fn x11_display_manager_create_virtual_display_tries_vkms() {
    if !x11_available() {
        skip_message("x11_display_manager_create_virtual_display_tries_vkms");
        return;
    }

    let context = crate::x11::X11RandrContext::new().expect("failed to connect to X11");
    let mgr = crate::x11::X11RandrDisplayManager::new(context);

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");

    let config = VirtualDisplayConfig {
        name: "test-virt".into(),
        size: Size {
            width: 1920,
            height: 1080,
        },
        refresh_hz: 60.0,
        color_space: ColorSpaceId::Srgb,
        position: Point { x: 0, y: 0 },
    };

    let result = rt.block_on(mgr.create_virtual_display(config));
    // Phase C: tries modprobe vkms. Without sudo, returns VirtualDisplayFailed.
    match result {
        Ok(_) => {}                                      // vkms succeeded
        Err(DisplayError::VirtualDisplayFailed(_)) => {} // vkms failed (no sudo)
        other => panic!("expected VirtualDisplayFailed, got {other:?}"),
    }

    assert!(
        mgr.supports_virtual_displays(),
        "Phase C should report virtual display support"
    );
}

// ──────────────────────────────────────────────
// Test 13 (Phase C): set_display_state(Active) is always ok
// ──────────────────────────────────────────────
#[test]
fn x11_display_manager_set_display_state_active_always_ok() {
    if !x11_available() {
        skip_message("x11_display_manager_set_display_state_active_always_ok");
        return;
    }

    let context = crate::x11::X11RandrContext::new().expect("failed to connect to X11");
    let mgr = crate::x11::X11RandrDisplayManager::new(context);

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");

    // Active on primary display should always return Ok (just restores DPMS)
    let result = rt.block_on(mgr.set_display_state(DisplayId::primary(), DisplayState::Active));
    assert!(
        result.is_ok(),
        "set_display_state(Active) should be ok: {result:?}"
    );

    // Active on non-existent display should also return Ok (best-effort DPMS restore)
    let result = rt.block_on(mgr.set_display_state(DisplayId(9999), DisplayState::Active));
    assert!(
        result.is_ok(),
        "set_display_state(Active, 9999) should be ok: {result:?}"
    );
}

// ──────────────────────────────────────────────
// Test 14 (Phase C): set_display_state(Blanked) is a no-op
// ──────────────────────────────────────────────
#[test]
fn x11_display_manager_set_display_state_blanked_is_noop() {
    if !x11_available() {
        skip_message("x11_display_manager_set_display_state_blanked_is_noop");
        return;
    }

    let context = crate::x11::X11RandrContext::new().expect("failed to connect to X11");
    let mgr = crate::x11::X11RandrDisplayManager::new(context);

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");

    let result = rt.block_on(mgr.set_display_state(DisplayId::primary(), DisplayState::Blanked));
    assert!(
        result.is_ok(),
        "set_display_state(Blanked) should be ok: {result:?}"
    );
}
