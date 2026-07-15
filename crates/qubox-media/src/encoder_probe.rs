//! Hardware encoder probe (P0-1). Per the `p0-01-hw-encode.md` spec, the
//! host must decide which encoder to use before starting the capture
//! pipeline. Three checks, in order of cost:
//!
//! 1. `ffmpeg -encoders` — fast, lists what the ffmpeg build supports.
//! 2. `vainfo` (Linux) — VAAPI profile availability.
//! 3. **No-op encode test** — `ffmpeg -f lavfi -i color=… -frames:v 1 -c:v
//!    <encoder> -f null -` and check the exit code. This catches
//!    driver/library mismatches that the first two checks miss.
//!
//! `detect_best_encoder` walks a per-platform priority list and returns
//! the first encoder that passes the no-op test. Returns `None` to fall
//! back to software (libx264/libx265/libaom-av1).

use std::process::Command;

use crate::{EncoderBackend, VideoEncoderKind};

/// Per-encoder result. Names mirror ffmpeg's encoder name (e.g.
/// `h264_nvenc`); `family` is the abstract backend family.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HwEncoder {
    pub name: String,
    pub family: EncoderBackend,
    pub kind: VideoEncoderKind,
}

impl HwEncoder {
    pub fn ffmpeg_name(&self) -> &str {
        &self.name
    }
}

/// Probe a single encoder by running a 1-frame `lavfi` → `null` encode.
/// This catches the "ffmpeg -encoders lists h264_nvenc but the driver
/// segfaults on the first frame" failure mode that pure
/// `ffmpeg -encoders` parsing misses.
pub fn try_no_op_encode(encoder_name: &str) -> Result<(), String> {
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x64:d=0.04:r=25",
            "-frames:v",
            "1",
            "-c:v",
            encoder_name,
            "-f",
            "null",
            "-",
        ])
        .output()
        .map_err(|error| format!("failed to spawn ffmpeg: {error}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "ffmpeg probe of {encoder_name} failed (exit {:?}): {}",
        output.status.code(),
        stderr.trim()
    ))
}

/// `vainfo` probe for VAAPI H.264 high profile support. Returns `true`
/// when VAAPI H.264 encode is available. Returns `false` on non-Linux or
/// when `vainfo` is not installed.
pub fn vaapi_h264_encode_available() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }

    let Ok(output) = Command::new("vainfo").arg("--display").arg(":0").output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    text.contains("VAProfileH264High") && text.contains("VAEntrypointEnc")
}

/// Per-platform probe priority. Walks the list in order; the first
/// encoder that is listed by `ffmpeg -encoders` AND passes the no-op
/// encode test wins. Returns `None` if no HW encoder is functional.
pub fn detect_best_encoder(kind: VideoEncoderKind) -> Option<HwEncoder> {
    let candidates = candidates_for_kind(kind);
    let ffmpeg_encoders = list_ffmpeg_encoders().unwrap_or_default();

    for name in candidates {
        if !ffmpeg_encoders.iter().any(|n| n == name) {
            continue;
        }
        // VAAPI needs the render node to be accessible; skip the
        // no-op test if the path doesn't exist to avoid spurious
        // failures.
        if name.contains("vaapi") && !cfg!(target_os = "linux") {
            continue;
        }
        if name.contains("vaapi") && !vaapi_h264_encode_available() {
            continue;
        }
        if try_no_op_encode(name).is_err() {
            continue;
        }
        let family = family_for_name(name)?;
        return Some(HwEncoder {
            name: name.to_string(),
            family,
            kind,
        });
    }
    None
}

/// Run `ffmpeg -encoders` and return the subset of names that are listed.
pub fn list_ffmpeg_encoders() -> Result<Vec<String>, String> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .map_err(|error| format!("failed to spawn ffmpeg: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "ffmpeg -encoders exited with {:?}",
            output.status.code()
        ));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut names = Vec::new();
    for line in text.lines() {
        // ffmpeg -encoders lines look like:
        //  V..... = Video
        //  V..... h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)
        let mut parts = line.split_whitespace();
        let _flags = parts.next();
        let Some(name) = parts.next() else { continue };
        if name.ends_with("_nvenc")
            || name.ends_with("_vaapi")
            || name.ends_with("_qsv")
            || name.ends_with("_amf")
            || name.ends_with("_videotoolbox")
            || name == "libx264"
            || name == "libx265"
            || name == "libaom-av1"
            || name == "libsvtav1"
        {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

fn candidates_for_kind(kind: VideoEncoderKind) -> &'static [&'static str] {
    let os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    match (os, kind) {
        ("linux", VideoEncoderKind::H264) => &[
            "h264_nvenc",
            "h264_vaapi",
            "h264_qsv",
            "h264_amf",
            "libx264",
        ],
        ("linux", VideoEncoderKind::H265) => &[
            "hevc_nvenc",
            "hevc_vaapi",
            "hevc_qsv",
            "hevc_amf",
            "libx265",
        ],
        ("linux", VideoEncoderKind::Av1) => &[
            "av1_nvenc",
            "av1_vaapi",
            "av1_qsv",
            "av1_amf",
            "libsvtav1",
            "libaom-av1",
        ],
        ("windows", VideoEncoderKind::H264) => &["h264_nvenc", "h264_amf", "h264_qsv", "libx264"],
        ("windows", VideoEncoderKind::H265) => &["hevc_nvenc", "hevc_amf", "hevc_qsv", "libx265"],
        ("windows", VideoEncoderKind::Av1) => {
            &["av1_nvenc", "av1_amf", "av1_qsv", "libsvtav1", "libaom-av1"]
        }
        ("macos", VideoEncoderKind::H264) => &["h264_videotoolbox", "libx264"],
        ("macos", VideoEncoderKind::H265) => &["hevc_videotoolbox", "libx265"],
        ("macos", VideoEncoderKind::Av1) => &["av1_videotoolbox", "libsvtav1", "libaom-av1"],
        _ => &["libx264"],
    }
}

fn family_for_name(name: &str) -> Option<EncoderBackend> {
    if name.contains("nvenc") {
        Some(EncoderBackend::Nvenc)
    } else if name.contains("vaapi") {
        Some(EncoderBackend::Vaapi)
    } else if name.contains("qsv") {
        Some(EncoderBackend::Qsv)
    } else if name.contains("amf") {
        Some(EncoderBackend::Amf)
    } else if name.contains("videotoolbox") {
        Some(EncoderBackend::VideoToolbox)
    } else if name.starts_with("libx264")
        || name.starts_with("libx265")
        || name.starts_with("libaom")
        || name.starts_with("libsvtav1")
    {
        Some(EncoderBackend::Software)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_ffmpeg_encoders_returns_known_names_when_ffmpeg_present() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let list = list_ffmpeg_encoders().unwrap();
        assert!(list.iter().any(|name| name == "libx264"));
    }

    #[test]
    fn family_for_name_classifies_backends() {
        assert_eq!(family_for_name("h264_nvenc"), Some(EncoderBackend::Nvenc));
        assert_eq!(family_for_name("hevc_vaapi"), Some(EncoderBackend::Vaapi));
        assert_eq!(family_for_name("av1_qsv"), Some(EncoderBackend::Qsv));
        assert_eq!(family_for_name("h264_amf"), Some(EncoderBackend::Amf));
        assert_eq!(
            family_for_name("h264_videotoolbox"),
            Some(EncoderBackend::VideoToolbox)
        );
        assert_eq!(family_for_name("libx264"), Some(EncoderBackend::Software));
        assert_eq!(
            family_for_name("libaom-av1"),
            Some(EncoderBackend::Software)
        );
    }

    #[test]
    fn candidates_h264_linux_ordering() {
        let list = candidates_for_kind(VideoEncoderKind::H264);
        assert!(list[0] == "h264_nvenc" || list[0] == "h264_vaapi");
        assert!(list.contains(&"libx264"));
    }
}
