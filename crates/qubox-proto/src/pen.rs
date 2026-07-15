//! P2-15 pen / tablet + P2-14 HDR wire format extensions.
//!
//! All wire format additions in ADR-010 live here. The proto surface
//! is additive-only: every new field is `#[serde(default)]`, every new
//! enum variant is appended, and packed structs are documented with
//! their on-wire byte layout.
//!
//! ## Datagram discriminators
//!
//! | Discriminator | Family                       |
//! |---------------|------------------------------|
//! | `0x47` `'G'`  | Gamepad (`WireGamepadState`) |
//! | `0x4D` `'M'`  | Mic (`WireMicHeader`)        |
//! | `0x50` `'P'`  | Pen (`WirePenEvent`)         |
//!
//! The 0x50..0x5F range is reserved by ADR-010 §13 risk 14 for the
//! stylus family of datagrams; the second nibble is room for future
//! extension (pressure-only / tilt-only / rotation-only variants).

use serde::{Deserialize, Serialize};

/// Discriminator byte for pen / tablet datagrams. ASCII `'P'`. Placed
/// at offset 2 immediately after `MEDIA_DATAGRAM_MAGIC`. Distinct
/// from gamepad (`0x47`) and mic (`0x4D`) so the single shared
/// dispatch byte checks all three kinds.
pub const PEN_DATAGRAM_DISCRIMINATOR: u8 = 0x50;

/// Total pen datagram wire size in bytes. Mirrors the ADR §1.2 table.
pub const PEN_WIRE_SIZE: usize = 36;

/// Total header size on the wire (magic + discriminator + flags +
/// device_id + tool_id + reserved). The remaining bytes are the
/// payload (x/y/pressure/tilt/rotation/timestamp).
pub const PEN_WIRE_HEADER_SIZE: usize = 8;

bitflags::bitflags! {
    /// Flag bits packed into `WirePenEvent::flags`. The on-wire layout
    /// reserves the bottom three bits for the most common states and
    /// leaves the upper bits for future expansion (tool-specific
    /// buttons, contact geometry, etc.).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct PenEventFlags: u8 {
        /// This is the last packet in a burst; coalescer reports a
        /// terminator to the host so the host can flush its input
        /// queue deterministically.
        const FLAG_LAST_IN_BURST = 0b0000_0001;
        /// Set when the hover distance is meaningful (i.e. the pen is
        /// in proximity but not yet in contact). The hover distance is
        /// encoded in the lower 4 bits of byte 7 of the header as a
        /// 0..=15 quantization.
        const FLAG_HAS_HOVER     = 0b0000_0010;
        /// Barrel button (BTN_STYLUS) is currently pressed.
        const FLAG_BARREL        = 0b0000_0100;
        /// Eraser tip is engaged (BTN_TOOL_RUBBER).
        const FLAG_ERASER_TIP    = 0b0000_1000;
        /// Reserved: previously bit 4; must be zero in v1.
        const _RESERVED          = 0b0001_0000;
    }
}

/// Logical pen tool taxonomy. Mirrors the libinput / Wintab / NSToolbar
/// tool classification (P2-15 §4.3).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "wire-rkyv-v2", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
#[cfg_attr(feature = "wire-rkyv-v2", rkyv(derive(Debug, PartialEq, Eq, Hash)))]
#[serde(rename_all = "snake_case")]
pub enum PenTool {
    #[default]
    /// Standard stylus tip (`Pen`).
    Pen,
    /// Eraser tip (`Eraser`).
    Eraser,
    /// Brush tip (`Brush`).
    Brush,
    /// Airbrush tool with built-in pressure / wheel input (`Airbrush`).
    Airbrush,
    /// Finger / touch (`Finger`).
    Finger,
    /// Mouse-style device that reports pen events (`Mouse`).
    Mouse,
    /// Lens cursor (legacy CAD-style) (`Lens`).
    Lens,
}

impl PenTool {
    /// Stable wire id used in `WirePenEvent::tool_id`. Reserves `0xFF`
    /// for "unknown" to keep wire forward-compatibility when a new tool
    /// variant ships in a future protocol revision.
    pub const fn wire_id(self) -> u8 {
        match self {
            PenTool::Pen => 0x01,
            PenTool::Eraser => 0x02,
            PenTool::Brush => 0x03,
            PenTool::Airbrush => 0x04,
            PenTool::Finger => 0x05,
            PenTool::Mouse => 0x06,
            PenTool::Lens => 0x07,
        }
    }

    /// Inverse of [`Self::wire_id`]. Returns `None` for unknown /
    /// reserved ids.
    pub fn from_wire_id(id: u8) -> Option<Self> {
        Some(match id {
            0x01 => PenTool::Pen,
            0x02 => PenTool::Eraser,
            0x03 => PenTool::Brush,
            0x04 => PenTool::Airbrush,
            0x05 => PenTool::Finger,
            0x06 => PenTool::Mouse,
            0x07 => PenTool::Lens,
            _ => return None,
        })
    }
}

/// 36-byte packed pen event. Mirrors the layout from ADR-010 §1.2.
///
/// | Offset | Bytes | Field             | Notes                                  |
/// |--------|-------|-------------------|----------------------------------------|
/// | 0..2   | 2     | `magic`           | `[0x51, 0x42]` (`MEDIA_DATAGRAM_MAGIC`)|
/// | 2      | 1     | `discriminator`   | `PEN_DATAGRAM_DISCRIMINATOR` = `0x50`  |
/// | 3      | 1     | `flags`           | `PenEventFlags` bitmask                |
/// | 4..6   | 2     | `device_id`       | Index into device list                 |
/// | 6      | 1     | `tool_id`         | `PenTool::wire_id`                     |
/// | 7      | 1     | `hover_distance`  | 0..=15 quantised millimetres; 0=contact |
/// | 8..12  | 4     | `x`               | Pixels, screen-space                   |
/// | 12..16 | 4     | `y`               | Pixels, screen-space                   |
/// | 16..20 | 4     | `pressure`        | 0..=1 (f32 LE)                         |
/// | 20..24 | 4     | `tilt_x`          | Degrees, -90..=90 (f32 LE)             |
/// | 24..28 | 4     | `tilt_y`          | Degrees, -90..=90 (f32 LE)             |
/// | 28..32 | 4     | `rotation`        | Degrees, 0..=360 (f32 LE)              |
/// | 32..36 | 4     | `timestamp_us`    | Wraps every ~71 minutes (u32 LE)       |
///
/// `hover_distance` and `button_state` are encoded into the `flags`
/// byte and the dedicated `hover_distance` nibble respectively — there
/// is no separate `button_state` field on the wire, keeping the
/// structure aligned at 36 bytes.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WirePenEvent {
    pub magic: [u8; 2],
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

impl WirePenEvent {
    /// Total wire size on the wire.
    pub const SIZE: usize = PEN_WIRE_SIZE;

    /// Render the struct as a 36-byte wire buffer in the same order
    /// `from_bytes` reads. Used by the transport's datagram encoder
    /// so we do not depend on `bytemuck` for a `packed` struct.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0] = self.magic[0];
        out[1] = self.magic[1];
        out[2] = self.discriminator;
        out[3] = self.flags;
        out[4] = self.device_id[0];
        out[5] = self.device_id[1];
        out[6] = self.tool_id;
        out[7] = self.hover_distance;
        out[8..12].copy_from_slice(&self.x);
        out[12..16].copy_from_slice(&self.y);
        out[16..20].copy_from_slice(&self.pressure);
        out[20..24].copy_from_slice(&self.tilt_x);
        out[24..28].copy_from_slice(&self.tilt_y);
        out[28..32].copy_from_slice(&self.rotation);
        out[32..36].copy_from_slice(&self.timestamp_us);
        out
    }

    /// Build a wire event from in-memory field values. The struct is
    /// `packed`, so direct field assignment must use byte arrays; this
    /// helper exists so callers don't write `to_le_bytes` everywhere.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        device_id: u16,
        tool: PenTool,
        flags: PenEventFlags,
        hover_distance: u8,
        x: f32,
        y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
        rotation: f32,
        timestamp_us: u32,
    ) -> Self {
        Self {
            magic: [0x51, 0x42],
            discriminator: PEN_DATAGRAM_DISCRIMINATOR,
            flags: flags.bits(),
            device_id: device_id.to_le_bytes(),
            tool_id: tool.wire_id(),
            hover_distance: hover_distance.min(0x0F),
            x: x.to_le_bytes(),
            y: y.to_le_bytes(),
            pressure: pressure.to_le_bytes(),
            tilt_x: tilt_x.to_le_bytes(),
            tilt_y: tilt_y.to_le_bytes(),
            rotation: rotation.to_le_bytes(),
            timestamp_us: timestamp_us.to_le_bytes(),
        }
    }

    /// Parse a 36-byte wire event into structured fields. Returns
    /// `Err(PenEventError)` on a too-short buffer, a bad magic prefix,
    /// or a wrong discriminator.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, PenEventError> {
        if buf.len() < Self::SIZE {
            return Err(PenEventError::Short);
        }
        if buf[0..2] != [0x51, 0x42] {
            return Err(PenEventError::BadMagic);
        }
        if buf[2] != PEN_DATAGRAM_DISCRIMINATOR {
            return Err(PenEventError::BadDiscriminator);
        }
        Ok(Self {
            magic: [buf[0], buf[1]],
            discriminator: buf[2],
            flags: buf[3],
            device_id: [buf[4], buf[5]],
            tool_id: buf[6],
            hover_distance: buf[7],
            x: [buf[8], buf[9], buf[10], buf[11]],
            y: [buf[12], buf[13], buf[14], buf[15]],
            pressure: [buf[16], buf[17], buf[18], buf[19]],
            tilt_x: [buf[20], buf[21], buf[22], buf[23]],
            tilt_y: [buf[24], buf[25], buf[26], buf[27]],
            rotation: [buf[28], buf[29], buf[30], buf[31]],
            timestamp_us: [buf[32], buf[33], buf[34], buf[35]],
        })
    }

    /// Reconstruct the `u16` device id from the little-endian pair.
    pub fn device_id_value(&self) -> u16 {
        u16::from_le_bytes(self.device_id)
    }

    /// Reconstruct the `f32` x coordinate.
    pub fn x_value(&self) -> f32 {
        f32::from_le_bytes(self.x)
    }

    /// Reconstruct the `f32` y coordinate.
    pub fn y_value(&self) -> f32 {
        f32::from_le_bytes(self.y)
    }

    /// Reconstruct the `f32` pressure (0..=1).
    pub fn pressure_value(&self) -> f32 {
        f32::from_le_bytes(self.pressure)
    }

    /// Reconstruct the `f32` tilt-x degrees.
    pub fn tilt_x_value(&self) -> f32 {
        f32::from_le_bytes(self.tilt_x)
    }

    /// Reconstruct the `f32` tilt-y degrees.
    pub fn tilt_y_value(&self) -> f32 {
        f32::from_le_bytes(self.tilt_y)
    }

    /// Reconstruct the `f32` rotation degrees.
    pub fn rotation_value(&self) -> f32 {
        f32::from_le_bytes(self.rotation)
    }

    /// Reconstruct the `u32` microsecond timestamp.
    pub fn timestamp_us_value(&self) -> u32 {
        u32::from_le_bytes(self.timestamp_us)
    }

    /// Decode the on-wire flags byte into a typed `PenEventFlags`.
    pub fn decoded_flags(&self) -> PenEventFlags {
        PenEventFlags::from_bits_truncate(self.flags)
    }
}

/// Errors returned from [`WirePenEvent::from_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenEventError {
    /// Buffer was shorter than `WirePenEvent::SIZE`.
    Short,
    /// Magic prefix `[0x51, 0x42]` did not match.
    BadMagic,
    /// Discriminator byte was not `PEN_DATAGRAM_DISCRIMINATOR`.
    BadDiscriminator,
}

impl std::fmt::Display for PenEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PenEventError::Short => write!(f, "pen datagram too short for header"),
            PenEventError::BadMagic => write!(f, "pen datagram magic prefix mismatch"),
            PenEventError::BadDiscriminator => {
                write!(f, "pen datagram discriminator byte mismatch")
            }
        }
    }
}

impl std::error::Error for PenEventError {}

/// Description of a connected pen / tablet device. Sent at session
/// start over the reliable control stream so the host can allocate the
/// matching `uinput` device (Linux) or `WinTab` device (Windows).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PenDeviceDescriptor {
    /// Stable id chosen by the client at enumeration time. Used in
    /// `WirePenEvent::device_id` to disambiguate events from multiple
    /// tablets connected to the same machine.
    #[serde(default)]
    pub device_id: u16,
    /// Human-readable vendor / model string. Free-form; surfaces in
    /// the host's privacy indicator.
    #[serde(default)]
    pub name: String,
    /// Tools the device reports. A Wacom Intuos typically reports
    /// `[Pen, Eraser]`. A Surface Pen reports `[Pen, Eraser]`. A
    /// capacitive touch device reports `[Finger]`.
    #[serde(default)]
    pub tools: Vec<PenTool>,
    /// Reported axis ranges; `0` means unknown / not reported.
    #[serde(default)]
    pub max_pressure: u16,
    /// 0..=90; 0 if the device does not report tilt.
    #[serde(default)]
    pub max_tilt_degrees: u8,
    /// 0..=360; 0 if the device does not report rotation.
    #[serde(default)]
    pub rotation_supported: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_size_is_thirty_six_bytes() {
        assert_eq!(std::mem::size_of::<WirePenEvent>(), PEN_WIRE_SIZE);
        assert_eq!(PEN_WIRE_SIZE, 36);
        assert_eq!(PEN_WIRE_HEADER_SIZE, 8);
    }

    #[test]
    fn pen_tool_wire_id_round_trips() {
        for tool in [
            PenTool::Pen,
            PenTool::Eraser,
            PenTool::Brush,
            PenTool::Airbrush,
            PenTool::Finger,
            PenTool::Mouse,
            PenTool::Lens,
        ] {
            let id = tool.wire_id();
            assert_eq!(PenTool::from_wire_id(id), Some(tool));
        }
        assert_eq!(PenTool::from_wire_id(0xFF), None);
    }

    #[test]
    fn pen_event_flags_decode_correctly() {
        let f = PenEventFlags::FLAG_LAST_IN_BURST | PenEventFlags::FLAG_BARREL;
        assert!(f.contains(PenEventFlags::FLAG_LAST_IN_BURST));
        assert!(f.contains(PenEventFlags::FLAG_BARREL));
        assert!(!f.contains(PenEventFlags::FLAG_HAS_HOVER));
    }

    #[test]
    fn wire_pen_event_round_trips_through_bytes() {
        let event = WirePenEvent::build(
            2,
            PenTool::Pen,
            PenEventFlags::FLAG_BARREL,
            0,
            123.5,
            678.25,
            0.42,
            15.0,
            -7.0,
            180.5,
            1_234_567,
        );
        let mut buf = [0_u8; PEN_WIRE_SIZE];
        unsafe {
            std::ptr::copy_nonoverlapping(
                &event as *const WirePenEvent as *const u8,
                buf.as_mut_ptr(),
                PEN_WIRE_SIZE,
            );
        }
        let decoded = WirePenEvent::from_bytes(&buf).expect("round trip should succeed");
        assert_eq!(decoded.device_id_value(), 2);
        assert_eq!(decoded.tool_id, PenTool::Pen.wire_id());
        assert_eq!(decoded.x_value(), 123.5);
        assert_eq!(decoded.y_value(), 678.25);
        assert!((decoded.pressure_value() - 0.42).abs() < 1e-6);
        assert_eq!(decoded.tilt_x_value(), 15.0);
        assert_eq!(decoded.tilt_y_value(), -7.0);
        assert_eq!(decoded.rotation_value(), 180.5);
        assert_eq!(decoded.timestamp_us_value(), 1_234_567);
        assert_eq!(decoded.magic, [0x51, 0x42]);
        assert_eq!(decoded.discriminator, PEN_DATAGRAM_DISCRIMINATOR);
        assert!(decoded.decoded_flags().contains(PenEventFlags::FLAG_BARREL));
    }

    #[test]
    fn wire_pen_event_rejects_short_buffer() {
        let err = WirePenEvent::from_bytes(&[0x51, 0x42, 0x50]).unwrap_err();
        assert_eq!(err, PenEventError::Short);
    }

    #[test]
    fn wire_pen_event_rejects_bad_magic() {
        let mut buf = [0_u8; PEN_WIRE_SIZE];
        buf[0] = 0xDE;
        buf[1] = 0xAD;
        buf[2] = PEN_DATAGRAM_DISCRIMINATOR;
        let err = WirePenEvent::from_bytes(&buf).unwrap_err();
        assert_eq!(err, PenEventError::BadMagic);
    }

    #[test]
    fn wire_pen_event_rejects_bad_discriminator() {
        let mut buf = [0_u8; PEN_WIRE_SIZE];
        buf[0] = 0x51;
        buf[1] = 0x42;
        buf[2] = 0x47;
        let err = WirePenEvent::from_bytes(&buf).unwrap_err();
        assert_eq!(err, PenEventError::BadDiscriminator);
    }

    #[test]
    fn pen_device_descriptor_serde_is_backward_compatible() {
        // Older clients may emit a payload with only the device_id
        // field. Newer servers must accept it.
        let json = r#"{"device_id":3,"name":"Wacom","tools":[]}"#;
        let decoded: PenDeviceDescriptor = serde_json::from_str(json).unwrap();
        assert_eq!(decoded.device_id, 3);
        assert_eq!(decoded.name, "Wacom");
        assert!(decoded.tools.is_empty());
        assert_eq!(decoded.max_pressure, 0);
        assert_eq!(decoded.max_tilt_degrees, 0);
        assert!(!decoded.rotation_supported);
    }
}
