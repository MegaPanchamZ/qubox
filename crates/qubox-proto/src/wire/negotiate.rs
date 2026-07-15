use crate::wire::WireFormatVersion;

pub fn negotiate_wire_format(remote_caps: &WireFormatVersion) -> WireFormatVersion {
    match remote_caps {
        WireFormatVersion::V2Rkyv => {
            #[cfg(feature = "wire-rkyv-v2")]
            {
                return WireFormatVersion::V2Rkyv;
            }
            #[cfg(not(feature = "wire-rkyv-v2"))]
            {
                WireFormatVersion::V1Json
            }
        }
        WireFormatVersion::V1Json => WireFormatVersion::V1Json,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_v1_json_when_remote_is_v1() {
        assert_eq!(
            negotiate_wire_format(&WireFormatVersion::V1Json),
            WireFormatVersion::V1Json
        );
    }

    #[test]
    fn negotiate_v2_rkyv_when_both_support() {
        #[cfg(feature = "wire-rkyv-v2")]
        {
            assert_eq!(
                negotiate_wire_format(&WireFormatVersion::V2Rkyv),
                WireFormatVersion::V2Rkyv
            );
        }
        #[cfg(not(feature = "wire-rkyv-v2"))]
        {
            assert_eq!(
                negotiate_wire_format(&WireFormatVersion::V2Rkyv),
                WireFormatVersion::V1Json
            );
        }
    }
}
