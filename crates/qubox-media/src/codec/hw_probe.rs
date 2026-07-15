use crate::codec::matrix::*;
use crate::EncoderBackend;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Intel,
    Amd,
    Apple,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct HwProbeReport {
    pub vendor: GpuVendor,
    pub generation: GpuGeneration,
    pub backends: Vec<EncoderBackend>,
    pub matrix: &'static CodecMatrix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuGeneration {
    NvidiaPreAda,
    NvidiaAda,
    NvidiaBlackwell,
    IntelIgpu,
    IntelArc,
    AmdPreRdna3,
    AmdRdna3,
    AmdRdna4,
    AppleM1M2,
    AppleM3Plus,
    Unknown,
}

pub fn probe() -> HwProbeReport {
    let ffmpeg_encoders = ffmpeg_available_encoders();
    let vendor = detect_vendor();
    let generation = detect_generation(vendor);

    let (matrix, backends) = match (vendor, generation) {
        (GpuVendor::Nvidia, GpuGeneration::NvidiaPreAda) => {
            (&NVIDIA_PRE_ADA_CODECS, nvenc_then_sw_h264())
        }
        (GpuVendor::Nvidia, _) => (&NVIDIA_CODECS, nvenc_then_sw()),
        (GpuVendor::Intel, GpuGeneration::IntelArc) => (&INTEL_ARC_CODECS, qsv_then_vaapi_then_sw()),
        (GpuVendor::Intel, _) => (&INTEL_IGPU_CODECS, qsv_then_vaapi_then_sw()),
        (GpuVendor::Apple, _) => (&APPLE_VIDEO_TOOLBOX_CODECS, vt_then_sw()),
        (GpuVendor::Amd, GpuGeneration::AmdRdna3 | GpuGeneration::AmdRdna4) => {
            (&AMD_RDNA_CODECS, vaapi_then_amf_then_sw())
        }
        (GpuVendor::Amd, _) => (&AMD_PRE_RDNA3_CODECS, vaapi_then_amf_then_sw()),
        _ => (&SOFTWARE_FALLBACK, sw_only()),
    };

    let backends = backends
        .into_iter()
        .filter(|b| backend_available(*b, &ffmpeg_encoders))
        .collect();

    HwProbeReport {
        vendor,
        generation,
        backends,
        matrix,
    }
}

fn ffmpeg_available_encoders() -> Vec<String> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output();
    let Ok(out) = out else {
        return vec![];
    };
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(|s| s.to_string())
        .collect()
}

fn backend_available(b: EncoderBackend, ffmpeg_list: &[String]) -> bool {
    use EncoderBackend::*;
    let names: &[&str] = match b {
        Nvenc => &["h264_nvenc", "hevc_nvenc", "av1_nvenc"],
        Vaapi => &["h264_vaapi", "hevc_vaapi", "av1_vaapi"],
        Qsv => &["h264_qsv", "hevc_qsv", "av1_qsv"],
        Amf => &["h264_amf", "hevc_amf", "av1_amf"],
        VideoToolbox => &["h264_videotoolbox", "hevc_videotoolbox"],
        Software => &["libx264"],
    };
    names.iter().any(|n| ffmpeg_list.iter().any(|x| x == n))
}

fn detect_vendor() -> GpuVendor {
    if cfg!(target_os = "macos") {
        return GpuVendor::Apple;
    }
    if std::path::Path::new("/dev/nvidia0").exists() {
        return GpuVendor::Nvidia;
    }
    if std::path::Path::new("/sys/module/amdgpu").exists() {
        return GpuVendor::Amd;
    }
    if std::path::Path::new("/sys/module/i915").exists() {
        return GpuVendor::Intel;
    }
    GpuVendor::Unknown
}

fn detect_generation(vendor: GpuVendor) -> GpuGeneration {
    match vendor {
        GpuVendor::Nvidia => {
            let out = Command::new("nvidia-smi")
                .args(["--query-gpu=compute_cap", "--format=csv,noheader"])
                .output();
            if let Ok(o) = out {
                let cap = String::from_utf8_lossy(&o.stdout).trim().to_string();
                return match cap
                    .split('.')
                    .next()
                    .and_then(|s| s.parse::<u32>().ok())
                {
                    Some(8) => GpuGeneration::NvidiaPreAda,
                    Some(9) => GpuGeneration::NvidiaBlackwell,
                    _ => GpuGeneration::NvidiaAda,
                };
            }
            GpuGeneration::Unknown
        }
        GpuVendor::Intel => GpuGeneration::IntelArc,
        GpuVendor::Amd => GpuGeneration::AmdRdna3,
        GpuVendor::Apple => {
            let out = Command::new("sysctl")
                .args(["-n", "machdep.cpu.brand_string"])
                .output();
            let stdout_bytes = out.map(|o| o.stdout).unwrap_or_default();
            let s = String::from_utf8_lossy(&stdout_bytes);
            if s.contains("M3") || s.contains("M4") || s.contains("M5") {
                GpuGeneration::AppleM3Plus
            } else {
                GpuGeneration::AppleM1M2
            }
        }
        GpuVendor::Unknown => GpuGeneration::Unknown,
    }
}

fn nvenc_then_sw() -> Vec<EncoderBackend> {
    vec![EncoderBackend::Nvenc, EncoderBackend::Software]
}

fn nvenc_then_sw_h264() -> Vec<EncoderBackend> {
    vec![EncoderBackend::Nvenc, EncoderBackend::Software]
}

fn qsv_then_vaapi_then_sw() -> Vec<EncoderBackend> {
    vec![
        EncoderBackend::Qsv,
        EncoderBackend::Vaapi,
        EncoderBackend::Software,
    ]
}

fn vt_then_sw() -> Vec<EncoderBackend> {
    vec![EncoderBackend::VideoToolbox, EncoderBackend::Software]
}

fn vaapi_then_amf_then_sw() -> Vec<EncoderBackend> {
    vec![
        EncoderBackend::Vaapi,
        EncoderBackend::Amf,
        EncoderBackend::Software,
    ]
}

fn sw_only() -> Vec<EncoderBackend> {
    vec![EncoderBackend::Software]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_vendor_no_gpu_returns_unknown() {
        if !cfg!(target_os = "macos")
            && !std::path::Path::new("/dev/nvidia0").exists()
            && !std::path::Path::new("/sys/module/amdgpu").exists()
            && !std::path::Path::new("/sys/module/i915").exists()
        {
            assert_eq!(detect_vendor(), GpuVendor::Unknown);
        }
    }

    #[test]
    fn backend_software_always_avaliable_from_ffmpeg() {
        assert!(backend_available(
            EncoderBackend::Software,
            &["libx264".to_string()]
        ));
    }

    #[test]
    fn nvenc_backend_requires_ffmpeg_listing() {
        assert!(backend_available(
            EncoderBackend::Nvenc,
            &[
                "h264_nvenc".to_string(),
                "hevc_nvenc".to_string(),
                "av1_nvenc".to_string()
            ]
        ));
        assert!(!backend_available(EncoderBackend::Nvenc, &["libx264".to_string()]));
    }

    #[test]
    fn matrices_are_non_empty() {
        assert!(!NVIDIA_CODECS.preferred.is_empty());
        assert!(!NVIDIA_PRE_ADA_CODECS.preferred.is_empty());
        assert!(!INTEL_ARC_CODECS.preferred.is_empty());
        assert!(!INTEL_IGPU_CODECS.preferred.is_empty());
        assert!(!APPLE_VIDEO_TOOLBOX_CODECS.preferred.is_empty());
        assert!(!AMD_RDNA_CODECS.preferred.is_empty());
        assert!(!AMD_PRE_RDNA3_CODECS.preferred.is_empty());
        assert!(!VAAPI_GENERIC_CODECS.preferred.is_empty());
        assert!(!SOFTWARE_FALLBACK.preferred.is_empty());
    }

    #[test]
    fn hw_av1_flags_correct() {
        assert!(NVIDIA_CODECS.hw_av1);
        assert!(!NVIDIA_PRE_ADA_CODECS.hw_av1);
        assert!(INTEL_ARC_CODECS.hw_av1);
        assert!(!INTEL_IGPU_CODECS.hw_av1);
        assert!(!APPLE_VIDEO_TOOLBOX_CODECS.hw_av1);
        assert!(AMD_RDNA_CODECS.hw_av1);
        assert!(!AMD_PRE_RDNA3_CODECS.hw_av1);
        assert!(!VAAPI_GENERIC_CODECS.hw_av1);
        assert!(!SOFTWARE_FALLBACK.hw_av1);
    }

    #[test]
    fn screen_content_caps() {
        assert_eq!(NVIDIA_CODECS.screen_content, Some(Codec::Av1));
        assert_eq!(NVIDIA_PRE_ADA_CODECS.screen_content, None);
        assert_eq!(APPLE_VIDEO_TOOLBOX_CODECS.screen_content, Some(Codec::Hevc));
        assert_eq!(INTEL_IGPU_CODECS.screen_content, None);
    }

    #[test]
    fn hdr_caps() {
        assert_eq!(NVIDIA_CODECS.hdr, Some(Codec::Av1));
        assert_eq!(APPLE_VIDEO_TOOLBOX_CODECS.hdr, Some(Codec::Hevc));
        assert_eq!(NVIDIA_PRE_ADA_CODECS.hdr, Some(Codec::Hevc));
        assert_eq!(SOFTWARE_FALLBACK.hdr, None);
    }
}
