pub const ST2086_PAYLOAD_SIZE: usize = 24;
pub const CLLI_PAYLOAD_SIZE: usize = 4;
pub const SEI_TYPE_MDCV: u8 = 137; // 0x89
pub const SEI_TYPE_CLLI: u8 = 144; // 0x90

pub fn pack_st2086(
    primaries: [(u16, u16); 3],
    white_point: (u16, u16),
    min_lum: u32,
    max_lum: u32,
) -> [u8; 24] {
    let mut buf = [0u8; 24];
    let (rx, ry) = primaries[0];
    let (gx, gy) = primaries[1];
    let (bx, by) = primaries[2];
    let (wx, wy) = white_point;

    // Display primaries X/Y (each u16, big-endian)
    buf[0..2].copy_from_slice(&rx.to_be_bytes());
    buf[2..4].copy_from_slice(&ry.to_be_bytes());
    buf[4..6].copy_from_slice(&gx.to_be_bytes());
    buf[6..8].copy_from_slice(&gy.to_be_bytes());
    buf[8..10].copy_from_slice(&bx.to_be_bytes());
    buf[10..12].copy_from_slice(&by.to_be_bytes());
    // White point X/Y
    buf[12..14].copy_from_slice(&wx.to_be_bytes());
    buf[14..16].copy_from_slice(&wy.to_be_bytes());
    // Max luminance (u32, big-endian, 0.0001 cd/m² units)
    buf[16..20].copy_from_slice(&max_lum.to_be_bytes());
    // Min luminance (u32, big-endian, 0.0001 cd/m² units)
    buf[20..24].copy_from_slice(&min_lum.to_be_bytes());

    buf
}

type St2086Unpacked = ([(u16, u16); 3], (u16, u16), u32, u32);

pub fn unpack_st2086(buf: &[u8; 24]) -> Option<St2086Unpacked> {
    let rx = u16::from_be_bytes([buf[0], buf[1]]);
    let ry = u16::from_be_bytes([buf[2], buf[3]]);
    let gx = u16::from_be_bytes([buf[4], buf[5]]);
    let gy = u16::from_be_bytes([buf[6], buf[7]]);
    let bx = u16::from_be_bytes([buf[8], buf[9]]);
    let by = u16::from_be_bytes([buf[10], buf[11]]);
    let wx = u16::from_be_bytes([buf[12], buf[13]]);
    let wy = u16::from_be_bytes([buf[14], buf[15]]);
    let max_lum = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
    let min_lum = u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]);

    Some(([(rx, ry), (gx, gy), (bx, by)], (wx, wy), max_lum, min_lum))
}

pub fn pack_clli(max_cll: u16, max_fall: u16) -> [u8; 4] {
    let mut buf = [0u8; 4];
    buf[0..2].copy_from_slice(&max_cll.to_be_bytes());
    buf[2..4].copy_from_slice(&max_fall.to_be_bytes());
    buf
}

pub fn unpack_clli(buf: &[u8; 4]) -> (u16, u16) {
    let max_cll = u16::from_be_bytes([buf[0], buf[1]]);
    let max_fall = u16::from_be_bytes([buf[2], buf[3]]);
    (max_cll, max_fall)
}

/// Build the wire payload: 24-byte MDCV + 4-byte CLLI (28 bytes total).
pub fn build_hdr10_payload(
    primaries: [(u16, u16); 3],
    white_point: (u16, u16),
    min_lum: u32,
    max_lum: u32,
    max_cll: u16,
    max_fall: u16,
) -> Vec<u8> {
    let mdcv = pack_st2086(primaries, white_point, min_lum, max_lum);
    let clli = pack_clli(max_cll, max_fall);
    let mut payload = Vec::with_capacity(28);
    payload.extend_from_slice(&mdcv);
    payload.extend_from_slice(&clli);
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    // ST-2086 fixture: BT.2020 reference display
    // R(34000,16000) G(13250,34500) B(7500,3000) WP(15635,16450)
    const FIXTURE_PRIMARIES: [(u16, u16); 3] = [(34000, 16000), (13250, 34500), (7500, 3000)];
    const FIXTURE_WHITE: (u16, u16) = (15635, 16450);
    const FIXTURE_MAX_LUM: u32 = 10_000_000; // 1000 cd/m² in 0.0001 units
    const FIXTURE_MIN_LUM: u32 = 1; // 0.0001 cd/m² in 0.0001 units
    const FIXTURE_MAX_CLL: u16 = 1500;
    const FIXTURE_MAX_FALL: u16 = 400;

    fn expected_mdcv_bytes() -> [u8; 24] {
        let mut b = [0u8; 24];
        // R(34000, 16000)
        b[0..2].copy_from_slice(&34000u16.to_be_bytes());
        b[2..4].copy_from_slice(&16000u16.to_be_bytes());
        // G(13250, 34500)
        b[4..6].copy_from_slice(&13250u16.to_be_bytes());
        b[6..8].copy_from_slice(&34500u16.to_be_bytes());
        // B(7500, 3000)
        b[8..10].copy_from_slice(&7500u16.to_be_bytes());
        b[10..12].copy_from_slice(&3000u16.to_be_bytes());
        // WP(15635, 16450)
        b[12..14].copy_from_slice(&15635u16.to_be_bytes());
        b[14..16].copy_from_slice(&16450u16.to_be_bytes());
        // max_lum = 10_000_000
        b[16..20].copy_from_slice(&10_000_000u32.to_be_bytes());
        // min_lum = 1
        b[20..24].copy_from_slice(&1u32.to_be_bytes());
        b
    }

    #[test]
    fn st2086_pack_matches_fixture() {
        let packed = pack_st2086(
            FIXTURE_PRIMARIES,
            FIXTURE_WHITE,
            FIXTURE_MIN_LUM,
            FIXTURE_MAX_LUM,
        );
        assert_eq!(packed, expected_mdcv_bytes());
    }

    #[test]
    fn st2086_unpack_roundtrip() {
        let packed = pack_st2086(
            FIXTURE_PRIMARIES,
            FIXTURE_WHITE,
            FIXTURE_MIN_LUM,
            FIXTURE_MAX_LUM,
        );
        let unpacked = unpack_st2086(&packed).unwrap();
        assert_eq!(unpacked.0, FIXTURE_PRIMARIES);
        assert_eq!(unpacked.1, FIXTURE_WHITE);
        assert_eq!(unpacked.2, FIXTURE_MAX_LUM);
        assert_eq!(unpacked.3, FIXTURE_MIN_LUM);
    }

    #[test]
    fn clli_pack_unpack_roundtrip() {
        let packed = pack_clli(FIXTURE_MAX_CLL, FIXTURE_MAX_FALL);
        assert_eq!(packed, [0x05, 0xDC, 0x01, 0x90]); // 1500 = 0x05DC, 400 = 0x0190
        let (cll, fall) = unpack_clli(&packed);
        assert_eq!(cll, FIXTURE_MAX_CLL);
        assert_eq!(fall, FIXTURE_MAX_FALL);
    }

    #[test]
    fn hdr10_payload_is_28_bytes() {
        let payload = build_hdr10_payload(
            FIXTURE_PRIMARIES,
            FIXTURE_WHITE,
            FIXTURE_MIN_LUM,
            FIXTURE_MAX_LUM,
            FIXTURE_MAX_CLL,
            FIXTURE_MAX_FALL,
        );
        assert_eq!(payload.len(), 28);
        assert_eq!(&payload[0..24], &expected_mdcv_bytes());
        assert_eq!(&payload[24..28], &[0x05, 0xDC, 0x01, 0x90]);
    }

    #[test]
    fn constants_have_correct_values() {
        assert_eq!(ST2086_PAYLOAD_SIZE, 24);
        assert_eq!(CLLI_PAYLOAD_SIZE, 4);
        assert_eq!(SEI_TYPE_MDCV, 0x89);
        assert_eq!(SEI_TYPE_CLLI, 0x90);
    }
}
