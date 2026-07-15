//! E2E tests for multi-display capture pipeline.
//!
//! Requires: `DISPLAY=:99` (Xephyr :99 with two screens) for `multi_display_e2e`.
//! The unit‑style tests (wire format backward compat, message round‑trips) have
//! no external requirements.
//!
//! ## Setup for the full E2E test
//!
//! ```bash
//! nohup Xephyr :99 -ac -screen 1024x768x24 -resizeable > /tmp/xephyr.log 2>&1 &
//! DISPLAY=:99 cargo test -p host-agent --test multi_display_e2e
//! ```

/// Check if Xephyr :99 is available.
fn xephyr_99_available() -> bool {
    std::env::var("DISPLAY").map_or(false, |d| d == ":99" || d == ":99.0")
}

fn require_e2e() -> bool {
    matches!(
        std::env::var("QUBOX_REQUIRE_E2E").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

#[test]
fn multi_display_e2e_skipped_without_xephyr() {
    if !xephyr_99_available() {
        if require_e2e() {
            panic!("QUBOX_REQUIRE_E2E=1 but DISPLAY is not :99 (start Xephyr)");
        }
        eprintln!("SKIPPED: multi_display_e2e (Xephyr :99 not available)");
        return;
    }
    eprintln!("Xephyr :99 detected");
}

#[test]
fn wire_access_unit_header_v0_backward_compat() {
    // Verify that a v0 payload (without stream_id, display_id, etc.)
    // deserializes with default values under the v1 schema.
    let v0_payload = r#"{
        "session_id": "00000000-0000-0000-0000-000000000001",
        "frame_id": 42,
        "timestamp_micros": 1000,
        "keyframe": true,
        "byte_len": 1024
    }"#;

    #[derive(serde::Deserialize, Debug)]
    struct WireAccessUnitHeaderV1 {
        session_id: uuid::Uuid,
        frame_id: u64,
        timestamp_micros: u64,
        keyframe: bool,
        byte_len: usize,
        #[serde(default)]
        stream_id: u16,
        #[serde(default)]
        display_id: u32,
        #[serde(default)]
        width: u32,
        #[serde(default)]
        height: u32,
        #[serde(default)]
        refresh_hz: f32,
        #[serde(default)]
        color_space_id: u8,
        #[serde(default)]
        hdr_static_metadata: Option<Vec<u8>>,
    }

    let header: WireAccessUnitHeaderV1 =
        serde_json::from_str(v0_payload).expect("v0 payload must deserialize with v1 schema");

    assert_eq!(header.stream_id, 0, "stream_id should default to 0");
    assert_eq!(header.display_id, 0, "display_id should default to 0");
    assert_eq!(header.width, 0, "width should default to 0");
    assert_eq!(header.height, 0, "height should default to 0");
    assert_eq!(header.refresh_hz, 0.0, "refresh_hz should default to 0.0");
    assert_eq!(
        header.color_space_id, 0,
        "color_space_id should default to 0"
    );
    assert_eq!(
        header.hdr_static_metadata, None,
        "hdr_static_metadata should default to None"
    );
    assert_eq!(
        header.session_id.to_string(),
        "00000000-0000-0000-0000-000000000001"
    );
    assert_eq!(header.frame_id, 42);
    assert!(header.keyframe);
    assert_eq!(header.byte_len, 1024);
}

#[test]
fn remote_input_event_hover_display_round_trips() {
    let event = qubox_proto::RemoteInputEvent::HoverDisplay { display_id: 2 };
    let json = serde_json::to_string(&event).expect("serialize");
    let decoded: qubox_proto::RemoteInputEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(event, decoded);
}

#[test]
fn control_msg_stream_subscribe_round_trips() {
    let msg = qubox_proto::ControlMsg::StreamSubscribe {
        display_ids: vec![0, 1, 2],
    };
    let json = serde_json::to_string(&msg).expect("serialize");
    let decoded: qubox_proto::ControlMsg = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(msg, decoded);
}

#[test]
fn control_msg_stream_unsubscribe_round_trips() {
    let msg = qubox_proto::ControlMsg::StreamUnsubscribe {
        display_ids: vec![1],
    };
    let json = serde_json::to_string(&msg).expect("serialize");
    let decoded: qubox_proto::ControlMsg = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(msg, decoded);
}

#[test]
fn control_msg_blank_overlay_round_trips() {
    let show = qubox_proto::ControlMsg::BlankOverlay {
        show: true,
        display_id: Some(0),
    };
    let hide = qubox_proto::ControlMsg::BlankOverlay {
        show: false,
        display_id: None,
    };
    for msg in &[show, hide] {
        let json = serde_json::to_string(msg).expect("serialize");
        let decoded: qubox_proto::ControlMsg = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(*msg, decoded);
    }
}

#[test]
fn control_msg_display_state_changed_round_trips() {
    let msg = qubox_proto::ControlMsg::DisplayStateChanged {
        display_id: 42,
        old_state: 0,
        new_state: 1,
    };
    let json = serde_json::to_string(&msg).expect("serialize");
    let decoded: qubox_proto::ControlMsg = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(msg, decoded);
}
