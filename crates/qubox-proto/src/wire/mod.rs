pub mod negotiate;

pub const MEDIA_DATAGRAM_MAGIC: [u8; 2] = [0x52, 0x42];
pub const WIRE_FORMAT_VERSION: u8 = 0x02;

pub const PEN_DATAGRAM_DISCRIMINATOR: u8 = 0x50;
pub const GAMEPAD_DATAGRAM_DISCRIMINATOR: u8 = 0x47;
pub const MIC_DATAGRAM_DISCRIMINATOR: u8 = 0x4D;
pub const ACCESS_UNIT_DISCRIMINATOR: u8 = 0x41;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormatVersion {
    V1Json,
    V2Rkyv,
}

// ---------------------------------------------------------------------------
// rkyv v2 wire structs
// ---------------------------------------------------------------------------
#[cfg(feature = "wire-rkyv-v2")]
mod rkyv_structs {
    use rkyv::{Archive, Deserialize, Serialize};

    use crate::rkyv_wire::{
        wire_error, CheckTypeError, MEDIA_DATAGRAM_MAGIC_V2, WIRE_FORMAT_VERSION_RKYV_V2,
    };

    pub const PEN_RKYV_WIRE_SIZE: usize = 37;

    #[repr(C)]
    #[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
    #[rkyv(derive(Debug, PartialEq))]
    #[rkyv(attr(derive(serde::Serialize, serde::Deserialize)))]
    pub struct RemotePenEvent {
        pub magic: [u8; 2],
        pub version: u8,
        pub discriminator: u8,
        pub flags: u8,
        pub device_id: [u8; 2],
        pub tool_id: u8,
        pub hover_distance: u8,
        pub x: [u8; 4],
        pub y: [u8; 4],
        pub pressure: [u8; 4],
        pub tilt_x: [u8; 4],
        pub tilt_y: [u8; 4],
        pub rotation: [u8; 4],
        pub timestamp_us: [u8; 4],
    }

    pub fn pen_to_rkyv_bytes(value: &RemotePenEvent) -> Result<crate::rkyv_wire::RkyvBytes, CheckTypeError> {
        crate::rkyv_wire::to_rkyv_bytes(value)
    }

    pub fn pen_from_rkyv_bytes<'a>(buf: &'a [u8]) -> Result<&'a ArchivedRemotePenEvent, CheckTypeError> {
        let archived = rkyv::access::<ArchivedRemotePenEvent, CheckTypeError>(buf)?;
        validate_pen(archived)?;
        Ok(archived)
    }

    pub fn validate_pen(value: &ArchivedRemotePenEvent) -> Result<(), CheckTypeError> {
        if value.magic != MEDIA_DATAGRAM_MAGIC_V2
            || value.version != WIRE_FORMAT_VERSION_RKYV_V2
            || value.discriminator != 0x50
        {
            return Err(wire_error("bad pen rkyv header"));
        }
        Ok(())
    }

    pub const GAMEPAD_RKYV_WIRE_SIZE: usize = 20;

    #[repr(C)]
    #[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
    #[rkyv(derive(Debug, PartialEq))]
    #[rkyv(attr(derive(serde::Serialize, serde::Deserialize)))]
    pub struct RemoteGamepadEvent {
        pub magic: [u8; 2],
        pub version: u8,
        pub discriminator: u8,
        pub gamepad_id: u8,
        pub flags: u8,
        pub buttons_lo: u8,
        pub buttons_hi: u8,
        pub lt: u8,
        pub rt: u8,
        pub lx: [u8; 2],
        pub ly: [u8; 2],
        pub rx: [u8; 2],
        pub ry: [u8; 2],
        pub _pad: [u8; 2],
    }

    pub fn gamepad_to_rkyv_bytes(value: &RemoteGamepadEvent) -> Result<crate::rkyv_wire::RkyvBytes, CheckTypeError> {
        crate::rkyv_wire::to_rkyv_bytes(value)
    }

    pub fn gamepad_from_rkyv_bytes<'a>(buf: &'a [u8]) -> Result<&'a ArchivedRemoteGamepadEvent, CheckTypeError> {
        let archived = rkyv::access::<ArchivedRemoteGamepadEvent, CheckTypeError>(buf)?;
        validate_gamepad(archived)?;
        Ok(archived)
    }

    pub fn validate_gamepad(value: &ArchivedRemoteGamepadEvent) -> Result<(), CheckTypeError> {
        if value.magic != MEDIA_DATAGRAM_MAGIC_V2
            || value.version != WIRE_FORMAT_VERSION_RKYV_V2
            || value.discriminator != 0x47
            || value._pad != [0, 0]
        {
            return Err(wire_error("bad gamepad rkyv header"));
        }
        Ok(())
    }

    pub const MIC_RKYV_WIRE_SIZE: usize = 9;

    #[repr(C)]
    #[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
    #[rkyv(derive(Debug, PartialEq))]
    #[rkyv(attr(derive(serde::Serialize, serde::Deserialize)))]
    pub struct RemoteMicHeader {
        pub magic: [u8; 2],
        pub version: u8,
        pub discriminator: u8,
        pub flags: u8,
        pub sequence: [u8; 2],
        pub _reserved: [u8; 2],
    }

    pub fn mic_to_rkyv_bytes(value: &RemoteMicHeader) -> Result<crate::rkyv_wire::RkyvBytes, CheckTypeError> {
        crate::rkyv_wire::to_rkyv_bytes(value)
    }

    pub fn mic_from_rkyv_bytes<'a>(buf: &'a [u8]) -> Result<&'a ArchivedRemoteMicHeader, CheckTypeError> {
        let archived = rkyv::access::<ArchivedRemoteMicHeader, CheckTypeError>(buf)?;
        validate_mic(archived)?;
        Ok(archived)
    }

    pub fn validate_mic(value: &ArchivedRemoteMicHeader) -> Result<(), CheckTypeError> {
        if value.magic != MEDIA_DATAGRAM_MAGIC_V2
            || value.version != WIRE_FORMAT_VERSION_RKYV_V2
            || value.discriminator != 0x4D
            || value._reserved != [0, 0]
        {
            return Err(wire_error("bad mic rkyv header"));
        }
        Ok(())
    }

    pub const MEDIA_RKYV_HEADER_SIZE: usize = 15;

    #[repr(C)]
    #[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
    #[rkyv(derive(Debug, PartialEq))]
    #[rkyv(attr(derive(serde::Serialize, serde::Deserialize)))]
    pub struct MediaDatagramHeader {
        pub magic: [u8; 2],
        pub version: u8,
        pub flags: u8,
        pub codec: u8,
        pub stream_id: [u8; 2],
        pub frame_id: [u8; 4],
        pub chunk_id: [u8; 2],
        pub chunk_count: [u8; 2],
    }

    pub fn media_to_rkyv_bytes(value: &MediaDatagramHeader) -> Result<crate::rkyv_wire::RkyvBytes, CheckTypeError> {
        crate::rkyv_wire::to_rkyv_bytes(value)
    }

    pub fn media_from_rkyv_bytes<'a>(buf: &'a [u8]) -> Result<&'a ArchivedMediaDatagramHeader, CheckTypeError> {
        let archived = rkyv::access::<ArchivedMediaDatagramHeader, CheckTypeError>(buf)?;
        validate_media(archived)?;
        Ok(archived)
    }

    pub fn validate_media(value: &ArchivedMediaDatagramHeader) -> Result<(), CheckTypeError> {
        if value.magic != MEDIA_DATAGRAM_MAGIC_V2 || value.version != WIRE_FORMAT_VERSION_RKYV_V2 {
            return Err(wire_error("bad media rkyv header"));
        }
        Ok(())
    }

    pub const ACCESS_UNIT_RKYV_HEADER_SIZE: usize = 63;

    #[repr(C)]
    #[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    #[rkyv(derive(Debug, PartialEq))]
    #[rkyv(attr(derive(serde::Serialize, serde::Deserialize)))]
    pub struct WireAccessUnitHeader {
        pub magic: [u8; 2],
        pub version: u8,
        pub discriminator: u8,
        pub session_id: [u8; 16],
        pub frame_id: [u8; 8],
        pub timestamp_micros: [u8; 8],
        pub keyframe: bool,
        pub byte_len: [u8; 4],
        pub codec: u8,
        pub stream_id: [u8; 2],
        pub display_id: [u8; 4],
        pub width: [u8; 4],
        pub height: [u8; 4],
        pub refresh_hz_millihz: [u8; 4],
        pub color_space_id: u8,
        pub hdr_static_metadata_len: [u8; 2],
    }

    pub fn au_to_rkyv_bytes(value: &WireAccessUnitHeader) -> Result<crate::rkyv_wire::RkyvBytes, CheckTypeError> {
        crate::rkyv_wire::to_rkyv_bytes(value)
    }

    pub fn au_from_rkyv_bytes<'a>(buf: &'a [u8]) -> Result<&'a ArchivedWireAccessUnitHeader, CheckTypeError> {
        let archived = rkyv::access::<ArchivedWireAccessUnitHeader, CheckTypeError>(buf)?;
        validate_au(archived)?;
        Ok(archived)
    }

    pub fn validate_au(value: &ArchivedWireAccessUnitHeader) -> Result<(), CheckTypeError> {
        if value.magic != MEDIA_DATAGRAM_MAGIC_V2
            || value.version != WIRE_FORMAT_VERSION_RKYV_V2
            || value.discriminator != 0x41
        {
            return Err(wire_error("bad access-unit rkyv header"));
        }
        Ok(())
    }

    // ── ADR-019: RemoteCriticalInput (reliable input stream) ──────

    use crate::{InputMouseButton, PenTool};

    /// Critical input events for the **reliable** input stream.
    ///
    /// Only keyboard, mouse button and pen-down/up events ride this path.
    /// Motion (mouse, pen, gamepad axis) uses the datagram path.
    #[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq)]
    #[rkyv(derive(Debug, PartialEq))]
    pub enum RemoteCriticalInput {
        MouseButton { button: InputMouseButton, pressed: bool },
        Keyboard { key: [u8; 16], key_len: u8, pressed: bool },
        PenDown {
            device_id: u16,
            tool: PenTool,
            x: u16,
            y: u16,
            pressure: f32,
            tilt_x: f32,
            tilt_y: f32,
            rotation: f32,
            button_state: u32,
            timestamp_us: u32,
        },
        PenUp { device_id: u16, tool: PenTool, timestamp_us: u32 },
        GamepadLifecycle { connect: bool, device_id: u16, vendor_id: u16, product_id: u16 },
        MouseDown { button: InputMouseButton, x: u16, y: u16, timestamp_us: u32 },
        MouseUp { button: InputMouseButton, x: u16, y: u16, timestamp_us: u32 },
        MouseClick { button: InputMouseButton, x: u16, y: u16, timestamp_us: u32 },
    }

    pub fn critical_input_to_rkyv_bytes(
        value: &RemoteCriticalInput,
    ) -> Result<crate::rkyv_wire::RkyvBytes, CheckTypeError> {
        crate::rkyv_wire::to_rkyv_bytes(value)
    }

    pub fn critical_input_from_rkyv_bytes<'a>(
        buf: &'a [u8],
    ) -> Result<&'a ArchivedRemoteCriticalInput, CheckTypeError> {
        let archived = rkyv::access::<ArchivedRemoteCriticalInput, CheckTypeError>(buf)?;
        Ok(archived)
    }
}

#[cfg(feature = "wire-rkyv-v2")]
pub use rkyv_structs::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_version_constants() {
        assert_eq!(MEDIA_DATAGRAM_MAGIC, [0x52, 0x42]);
        assert_eq!(WIRE_FORMAT_VERSION, 0x02);
        assert_eq!(PEN_DATAGRAM_DISCRIMINATOR, 0x50);
        assert_eq!(GAMEPAD_DATAGRAM_DISCRIMINATOR, 0x47);
        assert_eq!(MIC_DATAGRAM_DISCRIMINATOR, 0x4D);
        assert_eq!(ACCESS_UNIT_DISCRIMINATOR, 0x41);
    }

    #[cfg(feature = "wire-rkyv-v2")]
    mod rkyv_tests {
        use crate::rkyv_wire::{MEDIA_DATAGRAM_MAGIC_V2, WIRE_FORMAT_VERSION_RKYV_V2};

        fn make_pen() -> super::rkyv_structs::RemotePenEvent {
            super::rkyv_structs::RemotePenEvent {
                magic: MEDIA_DATAGRAM_MAGIC_V2,
                version: WIRE_FORMAT_VERSION_RKYV_V2,
                discriminator: 0x50,
                flags: 0,
                device_id: [0, 1],
                tool_id: 1,
                hover_distance: 0,
                x: 100.0f32.to_le_bytes(),
                y: 200.0f32.to_le_bytes(),
                pressure: 0.5f32.to_le_bytes(),
                tilt_x: 10.0f32.to_le_bytes(),
                tilt_y: (-5.0f32).to_le_bytes(),
                rotation: 45.0f32.to_le_bytes(),
                timestamp_us: 1234u32.to_le_bytes(),
            }
        }

        #[test]
        fn remote_pen_event_round_trip() {
            let event = make_pen();
            let bytes = super::rkyv_structs::pen_to_rkyv_bytes(&event).unwrap();
            assert_eq!(bytes.len(), super::rkyv_structs::PEN_RKYV_WIRE_SIZE);
            let archived = super::rkyv_structs::pen_from_rkyv_bytes(&bytes).unwrap();
            assert_eq!(archived.magic, MEDIA_DATAGRAM_MAGIC_V2);
            assert_eq!(archived.version, WIRE_FORMAT_VERSION_RKYV_V2);
            assert_eq!(archived.discriminator, 0x50);
            assert_eq!(archived.device_id, [0, 1]);
            assert_eq!(archived.x, 100.0f32.to_le_bytes());
        }

        #[test]
        fn remote_pen_event_rejects_truncated() {
            let event = make_pen();
            let bytes = super::rkyv_structs::pen_to_rkyv_bytes(&event).unwrap();
            let truncated = &bytes[..bytes.len() - 1];
            assert!(super::rkyv_structs::pen_from_rkyv_bytes(truncated).is_err());
        }

        #[test]
        fn remote_pen_event_rejects_bad_magic() {
            let mut event = make_pen();
            event.magic = [0xDE, 0xAD];
            let bytes = super::rkyv_structs::pen_to_rkyv_bytes(&event).unwrap();
            assert!(super::rkyv_structs::pen_from_rkyv_bytes(&bytes).is_err());
        }

        #[test]
        fn remote_gamepad_event_round_trip() {
            use super::rkyv_structs::RemoteGamepadEvent;
            let event = RemoteGamepadEvent {
                magic: MEDIA_DATAGRAM_MAGIC_V2,
                version: WIRE_FORMAT_VERSION_RKYV_V2,
                discriminator: 0x47,
                gamepad_id: 0,
                flags: 0x11,
                buttons_lo: 0b0000_0011,
                buttons_hi: 0,
                lt: 128,
                rt: 64,
                lx: 12345i16.to_le_bytes(),
                ly: (-12000i16).to_le_bytes(),
                rx: 0i16.to_le_bytes(),
                ry: 0i16.to_le_bytes(),
                _pad: [0, 0],
            };
            let bytes = super::rkyv_structs::gamepad_to_rkyv_bytes(&event).unwrap();
            assert_eq!(bytes.len(), super::rkyv_structs::GAMEPAD_RKYV_WIRE_SIZE);
            let archived = super::rkyv_structs::gamepad_from_rkyv_bytes(&bytes).unwrap();
            assert_eq!(archived.discriminator, 0x47);
            assert_eq!(archived.gamepad_id, 0);
            assert_eq!(archived._pad, [0, 0]);
        }

        #[test]
        fn remote_gamepad_event_rejects_nonzero_pad() {
            use super::rkyv_structs::RemoteGamepadEvent;
            let event = RemoteGamepadEvent {
                magic: MEDIA_DATAGRAM_MAGIC_V2,
                version: WIRE_FORMAT_VERSION_RKYV_V2,
                discriminator: 0x47,
                gamepad_id: 0,
                flags: 0,
                buttons_lo: 0,
                buttons_hi: 0,
                lt: 0,
                rt: 0,
                lx: [0, 0],
                ly: [0, 0],
                rx: [0, 0],
                ry: [0, 0],
                _pad: [0x01, 0x00],
            };
            let bytes = super::rkyv_structs::gamepad_to_rkyv_bytes(&event).unwrap();
            assert!(super::rkyv_structs::gamepad_from_rkyv_bytes(&bytes).is_err());
        }

        #[test]
        fn remote_mic_header_round_trip() {
            use super::rkyv_structs::RemoteMicHeader;
            let event = RemoteMicHeader {
                magic: MEDIA_DATAGRAM_MAGIC_V2,
                version: WIRE_FORMAT_VERSION_RKYV_V2,
                discriminator: 0x4D,
                flags: 0,
                sequence: 42u16.to_be_bytes(),
                _reserved: [0, 0],
            };
            let bytes = super::rkyv_structs::mic_to_rkyv_bytes(&event).unwrap();
            assert_eq!(bytes.len(), super::rkyv_structs::MIC_RKYV_WIRE_SIZE);
            let archived = super::rkyv_structs::mic_from_rkyv_bytes(&bytes).unwrap();
            assert_eq!(archived.sequence, [0, 42]);
        }

        #[test]
        fn media_datagram_header_round_trip() {
            use super::rkyv_structs::MediaDatagramHeader;
            let hdr = MediaDatagramHeader {
                magic: MEDIA_DATAGRAM_MAGIC_V2,
                version: WIRE_FORMAT_VERSION_RKYV_V2,
                flags: 0,
                codec: 1,
                stream_id: 0u16.to_le_bytes(),
                frame_id: 42u32.to_le_bytes(),
                chunk_id: 1u16.to_le_bytes(),
                chunk_count: 10u16.to_le_bytes(),
            };
            let bytes = super::rkyv_structs::media_to_rkyv_bytes(&hdr).unwrap();
            assert_eq!(bytes.len(), super::rkyv_structs::MEDIA_RKYV_HEADER_SIZE);
            let archived = super::rkyv_structs::media_from_rkyv_bytes(&bytes).unwrap();
            assert_eq!(archived.chunk_count, 10u16.to_le_bytes());
        }

        #[test]
        fn wire_access_unit_header_round_trip() {
            use super::rkyv_structs::WireAccessUnitHeader;
            let hdr = WireAccessUnitHeader {
                magic: MEDIA_DATAGRAM_MAGIC_V2,
                version: WIRE_FORMAT_VERSION_RKYV_V2,
                discriminator: 0x41,
                session_id: [0u8; 16],
                frame_id: 1000u64.to_le_bytes(),
                timestamp_micros: 50000u64.to_le_bytes(),
                keyframe: true,
                byte_len: (65535u32).to_le_bytes(),
                codec: 1,
                stream_id: 1u16.to_le_bytes(),
                display_id: 0u32.to_le_bytes(),
                width: 1920u32.to_le_bytes(),
                height: 1080u32.to_le_bytes(),
                refresh_hz_millihz: (60000u32).to_le_bytes(),
                color_space_id: 0,
                hdr_static_metadata_len: 0u16.to_le_bytes(),
            };
            let bytes = super::rkyv_structs::au_to_rkyv_bytes(&hdr).unwrap();
            assert_eq!(bytes.len(), super::rkyv_structs::ACCESS_UNIT_RKYV_HEADER_SIZE);
            let archived = super::rkyv_structs::au_from_rkyv_bytes(&bytes).unwrap();
            assert_eq!(archived.discriminator, 0x41);
            assert!(archived.keyframe);
        }

        #[test]
        fn rkyv_pen_size_vs_json() {
            use crate::rkyv_wire::to_rkyv_bytes;
            let event = make_pen();
            let rkyv_bytes = to_rkyv_bytes(&event).unwrap();
            let json_bytes = serde_json::to_vec(&event).unwrap();
            eprintln!(
                "rkyv archive size: {} B, json size: {} B, ratio: {:.2}%",
                rkyv_bytes.len(),
                json_bytes.len(),
                100.0 * rkyv_bytes.len() as f64 / json_bytes.len() as f64
            );
            assert!(
                rkyv_bytes.len() as f64 <= json_bytes.len() as f64 * 0.25,
                "rkyv archive {} B exceeds 25% of JSON {} B",
                rkyv_bytes.len(),
                json_bytes.len()
            );
        }

        #[test]
        fn remote_critical_input_round_trip() {
            let event = super::rkyv_structs::RemoteCriticalInput::MouseButton {
                button: crate::InputMouseButton::Left,
                pressed: true,
            };
            let bytes =
                super::rkyv_structs::critical_input_to_rkyv_bytes(&event).unwrap();
            assert!(bytes.len() < 256, "archive too large: {}", bytes.len());
            let archived =
                super::rkyv_structs::critical_input_from_rkyv_bytes(&bytes).unwrap();
            match archived {
                rkyv::Archived::<super::rkyv_structs::RemoteCriticalInput>::MouseButton {
                    pressed, ..
                } => assert_eq!(*pressed, true),
                _ => panic!("unexpected variant"),
            }
        }

        #[test]
        fn remote_critical_input_all_variants_round_trip() {
            use super::rkyv_structs::RemoteCriticalInput;
            use crate::{InputMouseButton, PenTool};
            let variants: Vec<RemoteCriticalInput> = vec![
                RemoteCriticalInput::MouseButton { button: InputMouseButton::Left, pressed: false },
                RemoteCriticalInput::Keyboard { key: [0u8; 16], key_len: 0, pressed: true },
                RemoteCriticalInput::PenDown {
                    device_id: 1,
                    tool: PenTool::Pen,
                    x: 100, y: 200,
                    pressure: 0.5,
                    tilt_x: 10.0, tilt_y: -5.0,
                    rotation: 45.0,
                    button_state: 0,
                    timestamp_us: 1234,
                },
                RemoteCriticalInput::PenUp { device_id: 1, tool: PenTool::Eraser, timestamp_us: 5678 },
                RemoteCriticalInput::GamepadLifecycle { connect: true, device_id: 0, vendor_id: 0x45e, product_id: 0x28e },
                RemoteCriticalInput::MouseDown { button: InputMouseButton::Right, x: 10, y: 20, timestamp_us: 100 },
                RemoteCriticalInput::MouseUp { button: InputMouseButton::Middle, x: 30, y: 40, timestamp_us: 200 },
                RemoteCriticalInput::MouseClick { button: InputMouseButton::Left, x: 50, y: 60, timestamp_us: 300 },
            ];
            for event in &variants {
                let bytes = super::rkyv_structs::critical_input_to_rkyv_bytes(event).unwrap();
                assert!(bytes.len() < 256, "archive too large: {} B for {event:?}", bytes.len());
                let archived = super::rkyv_structs::critical_input_from_rkyv_bytes(&bytes).unwrap();
                // Round-trip through full deserialize to verify source==archived==source
                let deserialized: RemoteCriticalInput = rkyv::deserialize::<RemoteCriticalInput, rkyv::rancor::Error>(archived).unwrap();
                assert_eq!(*event, deserialized);
            }
        }

        #[test]
        fn rkyv_vs_json_pen_decode_benchmark() {
            let event = make_pen();
            let rkyv_bytes = crate::rkyv_wire::to_rkyv_bytes(&event).unwrap();
            let json_bytes = serde_json::to_vec(&event).unwrap();

            const ITERATIONS: u64 = 10_000;

            // JSON decode
            let start = std::time::Instant::now();
            for _ in 0..ITERATIONS {
                let _decoded: super::rkyv_structs::RemotePenEvent =
                    serde_json::from_slice(&json_bytes).unwrap();
            }
            let json_dur = start.elapsed();

            // rkyv checked decode
            let start = std::time::Instant::now();
            for _ in 0..ITERATIONS {
                let _archived = super::rkyv_structs::pen_from_rkyv_bytes(&rkyv_bytes).unwrap();
            }
            let rkyv_checked_dur = start.elapsed();

            // rkyv unchecked decode (access_unchecked after one check)
            let _check_once = super::rkyv_structs::pen_from_rkyv_bytes(&rkyv_bytes).unwrap();
            let start = std::time::Instant::now();
            for _ in 0..ITERATIONS {
                let _archived: &rkyv::Archived<super::rkyv_structs::RemotePenEvent> =
                    unsafe { rkyv::access_unchecked::<rkyv::Archived<super::rkyv_structs::RemotePenEvent>>(&rkyv_bytes) };
                std::hint::black_box(_archived);
            }
            let rkyv_unchecked_dur = start.elapsed();

            let json_ns = json_dur.as_nanos() as f64 / ITERATIONS as f64;
            let checked_ns = rkyv_checked_dur.as_nanos() as f64 / ITERATIONS as f64;
            let unchecked_ns = rkyv_unchecked_dur.as_nanos() as f64 / ITERATIONS as f64;

            eprintln!(
                "JSON decode: {:.0} ns/iter, rkyv checked: {:.0} ns/iter ({:.1}x), rkyv unchecked: {:.0} ns/iter ({:.1}x)",
                json_ns, checked_ns, json_ns / checked_ns, unchecked_ns, json_ns / unchecked_ns
            );

            assert!(
                json_ns / checked_ns >= 20.0,
                "rkyv checked access must be >=20x faster than JSON (was {:.1}x)",
                json_ns / checked_ns
            );
            assert!(
                json_ns / unchecked_ns >= 100.0,
                "rkyv unchecked access must be >=100x faster than JSON (was {:.1}x)",
                json_ns / unchecked_ns
            );
        }
    }
}
