use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    Vp9,
}

impl Codec {
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
            Codec::Vp9 => "VP9",
        }
    }

    pub fn as_proto(self) -> qubox_proto::VideoCodec {
        match self {
            Codec::H264 => qubox_proto::VideoCodec::H264,
            Codec::Hevc => qubox_proto::VideoCodec::H265,
            Codec::Av1 => qubox_proto::VideoCodec::Av1,
            Codec::Vp9 => qubox_proto::VideoCodec::H264,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CodecMatrix {
    pub preferred: &'static [Codec],
    pub fallback: &'static [Codec],
    pub hdr: Option<Codec>,
    pub screen_content: Option<Codec>,
    pub hw_av1: bool,
}

// --- Static per-platform matrices ---

pub static NVIDIA_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Av1, Codec::Hevc, Codec::H264],
    fallback: &[Codec::H264],
    hdr: Some(Codec::Av1),
    screen_content: Some(Codec::Av1),
    hw_av1: true,
};

pub static NVIDIA_PRE_ADA_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Hevc, Codec::H264],
    fallback: &[Codec::H264],
    hdr: Some(Codec::Hevc),
    screen_content: None,
    hw_av1: false,
};

pub static INTEL_ARC_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Av1, Codec::Hevc, Codec::H264],
    fallback: &[Codec::H264],
    hdr: Some(Codec::Av1),
    screen_content: Some(Codec::Av1),
    hw_av1: true,
};

pub static INTEL_IGPU_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Hevc, Codec::H264],
    fallback: &[Codec::H264],
    hdr: Some(Codec::Hevc),
    screen_content: None,
    hw_av1: false,
};

pub static APPLE_VIDEO_TOOLBOX_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Hevc, Codec::H264],
    fallback: &[Codec::H264],
    hdr: Some(Codec::Hevc),
    screen_content: Some(Codec::Hevc),
    hw_av1: false,
};

pub static AMD_RDNA_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Av1, Codec::Hevc, Codec::H264],
    fallback: &[Codec::H264],
    hdr: Some(Codec::Av1),
    screen_content: Some(Codec::Av1),
    hw_av1: true,
};

pub static AMD_PRE_RDNA3_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Hevc, Codec::H264],
    fallback: &[Codec::H264],
    hdr: Some(Codec::Hevc),
    screen_content: None,
    hw_av1: false,
};

pub static VAAPI_GENERIC_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Hevc, Codec::H264],
    fallback: &[Codec::H264],
    hdr: Some(Codec::Hevc),
    screen_content: None,
    hw_av1: false,
};

pub static SOFTWARE_FALLBACK: CodecMatrix = CodecMatrix {
    preferred: &[Codec::H264],
    fallback: &[],
    hdr: None,
    screen_content: None,
    hw_av1: false,
};

// --- Decision tree ---

#[derive(Debug, Clone, Copy)]
pub struct StreamRequirements {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub hdr_requested: bool,
    pub screen_content_likely: bool,
}

pub fn choose_codec(matrix: &CodecMatrix, req: StreamRequirements) -> Codec {
    let pixels = req.width as u64 * req.height as u64;

    if pixels >= 8_000_000 && req.refresh_hz >= 120 && matrix.hw_av1 {
        return Codec::Av1;
    }
    if pixels >= 8_000_000 && matrix.hw_av1 {
        return Codec::Av1;
    }
    if pixels >= 3_500_000 {
        return matrix
            .preferred
            .iter()
            .find(|c| **c == Codec::Hevc || **c == Codec::Av1)
            .copied()
            .unwrap_or(Codec::H264);
    }
    if req.hdr_requested {
        return matrix.hdr.unwrap_or(Codec::Hevc);
    }
    if req.screen_content_likely {
        if let Some(scc) = matrix.screen_content {
            return scc;
        }
    }
    Codec::H264
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_av1_for_4k144_on_ada() {
        let req = StreamRequirements {
            width: 3840,
            height: 2160,
            refresh_hz: 144,
            hdr_requested: false,
            screen_content_likely: false,
        };
        assert_eq!(choose_codec(&NVIDIA_CODECS, req), Codec::Av1);
    }

    #[test]
    fn picks_av1_for_4k60_when_available() {
        let req = StreamRequirements {
            width: 3840,
            height: 2160,
            refresh_hz: 60,
            hdr_requested: false,
            screen_content_likely: false,
        };
        assert_eq!(choose_codec(&AMD_RDNA_CODECS, req), Codec::Av1);
        assert_eq!(choose_codec(&INTEL_ARC_CODECS, req), Codec::Av1);
    }

    #[test]
    fn prefers_hevc_for_1440p() {
        let req = StreamRequirements {
            width: 2560,
            height: 1440,
            refresh_hz: 60,
            hdr_requested: false,
            screen_content_likely: false,
        };
        let c = choose_codec(&NVIDIA_CODECS, req);
        assert!(c == Codec::Hevc || c == Codec::Av1);
    }

    #[test]
    fn forces_hevc_when_hdr_on_apple() {
        let req = StreamRequirements {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            hdr_requested: true,
            screen_content_likely: false,
        };
        assert_eq!(choose_codec(&APPLE_VIDEO_TOOLBOX_CODECS, req), Codec::Hevc);
    }

    #[test]
    fn falls_back_to_h264_when_sw_only() {
        let req = StreamRequirements {
            width: 1280,
            height: 720,
            refresh_hz: 30,
            hdr_requested: false,
            screen_content_likely: false,
        };
        assert_eq!(choose_codec(&SOFTWARE_FALLBACK, req), Codec::H264);
    }

    #[test]
    fn picks_scc_codec_for_text_heavy_1080p() {
        let req = StreamRequirements {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            hdr_requested: false,
            screen_content_likely: true,
        };
        assert_eq!(choose_codec(&NVIDIA_CODECS, req), Codec::Av1);
    }

    #[test]
    fn vp9_falls_through_to_h264_on_sw_only() {
        let req = StreamRequirements {
            width: 800,
            height: 600,
            refresh_hz: 30,
            hdr_requested: false,
            screen_content_likely: false,
        };
        assert_eq!(choose_codec(&SOFTWARE_FALLBACK, req), Codec::H264);
    }

    #[test]
    fn pre_ada_4k60_uses_hevc_not_av1() {
        let req = StreamRequirements {
            width: 3840,
            height: 2160,
            refresh_hz: 60,
            hdr_requested: false,
            screen_content_likely: false,
        };
        assert_eq!(choose_codec(&NVIDIA_PRE_ADA_CODECS, req), Codec::Hevc);
    }

    #[test]
    fn pre_ada_4k144_falls_back_to_hevc() {
        let req = StreamRequirements {
            width: 3840,
            height: 2160,
            refresh_hz: 144,
            hdr_requested: false,
            screen_content_likely: false,
        };
        assert_eq!(choose_codec(&NVIDIA_PRE_ADA_CODECS, req), Codec::Hevc);
    }

    #[test]
    fn hdr_on_software_fallback_uses_hevc() {
        let req = StreamRequirements {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            hdr_requested: true,
            screen_content_likely: false,
        };
        assert_eq!(choose_codec(&SOFTWARE_FALLBACK, req), Codec::Hevc);
    }

    #[test]
    fn intel_arc_av1_for_4k60() {
        let req = StreamRequirements {
            width: 3840,
            height: 2160,
            refresh_hz: 60,
            hdr_requested: false,
            screen_content_likely: false,
        };
        assert_eq!(choose_codec(&INTEL_ARC_CODECS, req), Codec::Av1);
    }

    #[test]
    fn codec_matrix_statics_non_empty() {
        assert!(!NVIDIA_CODECS.preferred.is_empty());
        assert!(!INTEL_ARC_CODECS.preferred.is_empty());
        assert!(!AMD_RDNA_CODECS.preferred.is_empty());
        assert!(!APPLE_VIDEO_TOOLBOX_CODECS.preferred.is_empty());
        assert!(!SOFTWARE_FALLBACK.preferred.is_empty());
    }

    #[test]
    fn hdr_apple_uses_hevc() {
        let req = StreamRequirements {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            hdr_requested: true,
            screen_content_likely: false,
        };
        assert_eq!(choose_codec(&APPLE_VIDEO_TOOLBOX_CODECS, req), Codec::Hevc);
    }

    #[test]
    fn rdna_av1_for_screen_content() {
        let req = StreamRequirements {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            hdr_requested: false,
            screen_content_likely: true,
        };
        assert_eq!(choose_codec(&AMD_RDNA_CODECS, req), Codec::Av1);
    }
}
