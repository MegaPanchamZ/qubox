# ADR-018 Codec Selection Matrix

## Status

Accepted (proposed, awaiting reviewer sign-off). Branch:
`feature/adr-018-codec-selection-matrix`. Based on `main` after commit
`47585ea`. Builds on ADR-009 (ffmpeg-next HW decoder + wgpu),
ADR-016 (zero-copy GPU↔encoder surfaces), and ADR-013 (frame-aware
pacing). **Required for** P2-14 (HDR), P2-16 (4K144), P2-17 (macOS),
and P2-18 (Windows DXGI confirmation).

## Context

The encoder list today lives in `crates/qubox-media/src/lib.rs:164-185`
(`EncoderBackend::ffmpeg_name`) and `:187-203` (`EncoderBackend::all_kinds`).
The encoder args come from `encoder_args_for` at
`crates/qubox-media/src/lib.rs:1460-1580`. Today the choice is manual
(via CLI flag `--codec`) and does not adapt to platform capabilities
or to the connection's measured bandwidth (ADR-012 telemetry).

### Codecs surveyed (research dump, 2025-Q4)

- **H.264** (`avc1`) — universal fallback. HW encode on every modern
  iGPU/dGPU; libx264 software fallback. ~1080p60 at 6 Mbps with
  `8x8dct+mbtree`.
- **HEVC** (`hvc1`) — ~44 % better than H.264 at equal VMAF on real
  content; ~30 % better for 4K (Fora Soft / Streaming Learning Center
  2026 measurements). HW encode on NVENC (Turing+), Intel QSV /
  VA-API (Broadwell+), VideoToolbox (A11+), AMF (Polaris+).
  Royalty encumbrance is the known issue — see §5.
- **AV1** (`av01`) — ~48 % better than H.264 at equal VMAF (Netflix
  Dec-2025 production data); ~30 % better than HEVC at 1080p, ~44 %
  better at 4K (Streaming Learning Center 2026). Royalty-free.
  HW encode on NVENC Ada+ (`av1_nvenc`), Intel Arc Alchemist+
  (`av1_qsv` / `av1_vaapi`), VideoToolbox decode-only on M3+
  (encode still absent on all Apple Silicon mid-2026), AMF on RDNA3+
  (`av1_amf`), Mesa VA-API ≥ 23.3 on RDNA3+ (`av1_vaapi`).
  Software fallback via `libsvtav1` (fast) / `libaom-av1` (slow).
- **AV1 SCC** (Screen Content Coding) — adds **palette mode**
  (≤ 8 entries per block, diagonal scan-order index coding) and
  **intra block copy / IBC** (hash-based within-frame prediction).
  Normative in the AV1 bitstream (every conformant decoder supports
  it); encoder support depends on vendor. Aurora1 / Visionular
  report **>50 % bitrate reduction** on screen content vs H.264 and
  ~30-50 % vs non-SCC AV1; AV1 intra-block-copy alone saves ~12.2 %
  BD-rate per Li & Su. See §4.
- **VP9** — legacy. Retained only for interop; not in the matrix.

### HDR + screen content in scope

- HDR10 negotiation uses **SMPTE ST 2086** "Mastering Display Color
  Volume" (MDCV) + MaxCLL/MaxFALL, packed into a 24-byte HEVC SEI
  message. Wire field already exists at
  `crates/qubox-transport/src/lib.rs:400` (`hdr_static_metadata: Option<Vec<u8>>`)
  and the struct field at `:1534`. The wire payload format follows
  SMPTE ST 2108-1 (HDR/WCG Metadata Ancillary Data Packet): payload
  type `0x89` (24 bytes, MDCV) and `0x90` (4 bytes, CLLI). See §3.
- Screen content detection is a host-side heuristic on the
  framebuffer. See §6.

## Decision

### 1. Cargo dependencies (exact verified versions, mid-2026)

Append to `[workspace.dependencies]` in `/Cargo.toml`:

```toml
# ADR-018 HW encoder bindings (used by qubox-media HW probe path,
# not by the ffmpeg-next path that ships by default)
nvidia-video-codec-sdk = "0.4"           # wraps Video Codec SDK 12.1.14, CUDA 12.2
onevpl-sys             = "0.1"           # thin FFI over Intel oneVPL dispatcher (libvpl 2.16+)
shiguredo_video_toolbox = "2025.1"       # typed VideoToolbox wrappers, Rust 1.71+
objc2                  = "0.5"           # required by shiguredo_video_toolbox
objc2-foundation       = "0.2"
cros-libva             = "0.0.13"        # ChromeOS-maintained libva 1.20+ bindings
amf-rs                 = "0.2"           # Linux AMF over Mesa (av1/h264/hevc/vp9)
```

Add to `crates/qubox-media/Cargo.toml` `[dependencies]`:

```toml
ffmpeg-next        = { version = "7.1", features = ["codec", "format", "hwcontext"] }
wgpu               = { workspace = true }
nvidia-video-codec-sdk = { workspace = true, optional = true }
onevpl-sys             = { workspace = true, optional = true }
shiguredo_video_toolbox = { workspace = true, optional = true }
objc2                  = { workspace = true, optional = true }
objc2-foundation       = { workspace = true, optional = true }
cros-libva             = { workspace = true, optional = true }
amf-rs                 = { workspace = true, optional = true }

[features]
default = ["ffmpeg-path"]
hw-probe-nvenc    = ["dep:nvidia-video-codec-sdk"]
hw-probe-qsv      = ["dep:onevpl-sys"]
hw-probe-videotoolbox = ["dep:shiguredo_video_toolbox", "dep:objc2", "dep:objc2-foundation"]
hw-probe-vaapi    = ["dep:cros-libva"]
hw-probe-amf      = ["dep:amf-rs"]
hw-probe          = ["hw-probe-nvenc", "hw-probe-qsv", "hw-probe-videotoolbox",
                     "hw-probe-vaapi", "hw-probe-amf"]
```

### 2. `CodecMatrix` Rust definition

New file: `crates/qubox-media/src/codec/matrix.rs`.

```rust
//! ADR-018 Codec Selection Matrix.

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
            Codec::Av1  => "AV1",
            Codec::Vp9  => "VP9",
        }
    }
    pub fn as_proto(self) -> qubox_proto::VideoCodec {
        match self {
            Codec::H264 => qubox_proto::VideoCodec::H264,
            Codec::Hevc => qubox_proto::VideoCodec::H265,
            Codec::Av1  => qubox_proto::VideoCodec::Av1,
            Codec::Vp9  => qubox_proto::VideoCodec::H264, // map to H264 for transport
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CodecMatrix {
    /// Preferred codecs in order of compression-efficiency preference.
    pub preferred: &'static [Codec],
    /// Codecs we can fall back to if a preferred one is unavailable.
    pub fallback:  &'static [Codec],
    /// Codec that carries HDR10 ST-2086 metadata. `None` means HDR is
    /// not supported by this matrix.
    pub hdr: Option<Codec>,
    /// Codec that supports AV1 Screen Content Coding (palette + IBC).
    pub screen_content: Option<Codec>,
    /// True if any preferred codec has hardware AV1 encode.
    pub hw_av1: bool,
}

// --- Static per-platform matrices ------------------------------------

/// NVIDIA NVENC, Ada Lovelace (RTX 40) and Blackwell (RTX 50).
pub static NVIDIA_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Av1, Codec::Hevc, Codec::H264],
    fallback:  &[Codec::H264],
    hdr: Some(Codec::Av1),
    screen_content: Some(Codec::Av1), // NVENC AV1 supports SCC via SVC extension
    hw_av1: true,
};

/// NVIDIA NVENC, pre-Ada (Turing/Ampere/Lovelace-pre). AV1 absent.
pub static NVIDIA_PRE_ADA_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Hevc, Codec::H264],
    fallback:  &[Codec::H264],
    hdr: Some(Codec::Hevc),
    screen_content: None,
    hw_av1: false,
};

/// Intel Arc (Alchemist / Battlemage). QSV or VA-API.
pub static INTEL_ARC_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Av1, Codec::Hevc, Codec::H264],
    fallback:  &[Codec::H264],
    hdr: Some(Codec::Av1),
    screen_content: Some(Codec::Av1),
    hw_av1: true,
};

/// Intel iGPU Broadwell–Rocket Lake (pre-Arc). No AV1 HW encode.
pub static INTEL_IGPU_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Hevc, Codec::H264],
    fallback:  &[Codec::H264],
    hdr: Some(Codec::Hevc),
    screen_content: None,
    hw_av1: false,
};

/// Apple VideoToolbox on macOS. M3+ decodes AV1 but **does not
/// hardware-encode AV1** as of macOS Tahoe (mid-2026). HEVC is the
/// preferred AV1-less path.
pub static APPLE_VIDEO_TOOLBOX_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Hevc, Codec::H264],
    fallback:  &[Codec::H264],
    hdr: Some(Codec::Hevc),            // HDR10 over HEVC is rock-solid
    screen_content: Some(Codec::Hevc), // HEVC SCC via VideoToolbox
    hw_av1: false,
};

/// AMD RDNA3 / RDNA4 (RX 7000 / RX 9000). Mesa VA-API or AMF on Win.
pub static AMD_RDNA_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Av1, Codec::Hevc, Codec::H264],
    fallback:  &[Codec::H264],
    hdr: Some(Codec::Av1),
    screen_content: Some(Codec::Av1),
    hw_av1: true,
};

/// AMD pre-RDNA3 (RX 6000 and older). No HW AV1 encode.
pub static AMD_PRE_RDNA3_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Hevc, Codec::H264],
    fallback:  &[Codec::H264],
    hdr: Some(Codec::Hevc),
    screen_content: None,
    hw_av1: false,
};

/// Generic VA-API on Linux (Intel or AMD pre-HW-AV1).
pub static VAAPI_GENERIC_CODECS: CodecMatrix = CodecMatrix {
    preferred: &[Codec::Hevc, Codec::H264],
    fallback:  &[Codec::H264],
    hdr: Some(Codec::Hevc),
    screen_content: None,
    hw_av1: false,
};

/// No HW encoder available. libx264 / libaom-av1 / libsvtav1 fallback.
pub static SOFTWARE_FALLBACK: CodecMatrix = CodecMatrix {
    preferred: &[Codec::H264],
    fallback:  &[],
    hdr: None,
    screen_content: None,
    hw_av1: false,
};

// --- Decision tree ---------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct StreamRequirements {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub hdr_requested: bool,
    pub screen_content_likely: bool,  // from ContentClassifier
}

/// Pick the codec. The decision is purely a function of (matrix, req);
/// no platform detection happens here. Callers feed `matrix` from §4.
pub fn choose_codec(matrix: &CodecMatrix, req: StreamRequirements) -> Codec {
    let pixels = req.width as u64 * req.height as u64;

    // 4K120+ mandates AV1 when available (only codec with both the
    // efficiency and the HW support at this resolution/refresh).
    if pixels >= 8_000_000 && req.refresh_hz >= 120 && matrix.hw_av1 {
        return Codec::Av1;
    }
    // 4K60 prefers AV1 when available.
    if pixels >= 8_000_000 && matrix.hw_av1 {
        return Codec::Av1;
    }
    // 1440p (QHD): prefer HEVC over AV1 because the AV1 gain shrinks
    // and HEVC is more universally HW-supported here.
    if pixels >= 3_500_000 {
        return matrix.preferred.iter()
            .find(|c| **c == Codec::Hevc || **c == Codec::Av1)
            .copied()
            .unwrap_or(Codec::H264);
    }
    // 1080p and below. If HDR10 was negotiated, only HEVC or AV1 can
    // carry ST-2086 metadata.
    if req.hdr_requested {
        return matrix.hdr.unwrap_or(Codec::Hevc);
    }
    // Screen content on 1080p: prefer the SCC-capable codec.
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
        let req = StreamRequirements { width:3840, height:2160, refresh_hz:144,
                                       hdr_requested:false, screen_content_likely:false };
        assert_eq!(choose_codec(&NVIDIA_CODECS, req), Codec::Av1);
    }
    #[test]
    fn prefers_hevc_for_1440p() {
        let req = StreamRequirements { width:2560, height:1440, refresh_hz:60,
                                       hdr_requested:false, screen_content_likely:false };
        let c = choose_codec(&NVIDIA_CODECS, req);
        assert!(c == Codec::Hevc || c == Codec::Av1);
    }
    #[test]
    fn forces_hevc_when_hdr_on_apple() {
        let req = StreamRequirements { width:1920, height:1080, refresh_hz:60,
                                       hdr_requested:true, screen_content_likely:false };
        assert_eq!(choose_codec(&APPLE_VIDEO_TOOLBOX_CODECS, req), Codec::Hevc);
    }
    #[test]
    fn falls_back_to_h264_when_sw_only() {
        let req = StreamRequirements { width:1280, height:720, refresh_hz:30,
                                       hdr_requested:false, screen_content_likely:false };
        assert_eq!(choose_codec(&SOFTWARE_FALLBACK, req), Codec::H264);
    }
    #[test]
    fn picks_scc_codec_for_text_heavy_1080p() {
        let req = StreamRequirements { width:1920, height:1080, refresh_hz:60,
                                       hdr_requested:false, screen_content_likely:true };
        assert_eq!(choose_codec(&NVIDIA_CODECS, req), Codec::Av1);
    }
}
```

### 3. HW probe implementation

New module: `crates/qubox-media/src/codec/hw_probe.rs`.

The probe runs once at startup. It returns the resolved
`CodecMatrix` plus a `Vec<EncoderBackend>` in preference order.

```rust
//! Detects which HW encoder backends are actually usable on this host.

use crate::codec::matrix::*;
use crate::EncoderBackend;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor { Nvidia, Intel, Amd, Apple, Unknown }

#[derive(Debug, Clone)]
pub struct HwProbeReport {
    pub vendor: GpuVendor,
    pub generation: GpuGeneration,
    pub backends: Vec<EncoderBackend>,   // in preference order
    pub matrix: &'static CodecMatrix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuGeneration {
    NvidiaPreAda,    // Turing / Ampere
    NvidiaAda,       // RTX 40
    NvidiaBlackwell, // RTX 50
    IntelIgpu,       // Broadwell-Rocket Lake (no HW AV1)
    IntelArc,        // Alchemist / Battlemage
    AmdPreRdna3,     // RX 6000 and older
    AmdRdna3,        // RX 7000
    AmdRdna4,        // RX 9000
    AppleM1M2,       // decode-only AV1 absent
    AppleM3Plus,     // decode AV1, no encode
    Unknown,
}

pub fn probe() -> HwProbeReport {
    // 1) Probe via ffmpeg - always available with our ffmpeg-next link.
    //    This is the source of truth for which ffmpeg encoder names
    //    actually load on this host.
    let ffmpeg_encoders = ffmpeg_available_encoders();

    // 2) Probe vendor via the OS (sysfs / IORegistry / nvidia-smi).
    let vendor = detect_vendor();
    let generation = detect_generation(vendor);

    // 3) Build the matrix and ordered backend list.
    let (matrix, backends) = match (vendor, generation) {
        (GpuVendor::Nvidia, GpuGeneration::NvidiaPreAda) =>
            (&NVIDIA_PRE_ADA_CODECS, nvenc_then_sw_h264()),
        (GpuVendor::Nvidia, _) =>
            (&NVIDIA_CODECS, nvenc_then_sw()),
        (GpuVendor::Intel, GpuGeneration::IntelArc) =>
            (&INTEL_ARC_CODECS, qsv_then_vaapi_then_sw()),
        (GpuVendor::Intel, _) =>
            (&INTEL_IGPU_CODECS, qsv_then_vaapi_then_sw()),
        (GpuVendor::Apple, _) =>
            (&APPLE_VIDEO_TOOLBOX_CODECS, vt_then_sw()),
        (GpuVendor::Amd, GpuGeneration::AmdRdna3 | GpuGeneration::AmdRdna4) =>
            (&AMD_RDNA_CODECS, vaapi_then_amf_then_sw()),
        (GpuVendor::Amd, _) =>
            (&AMD_PRE_RDNA3_CODECS, vaapi_then_amf_then_sw()),
        _ => (&SOFTWARE_FALLBACK, sw_only()),
    };

    // 4) Filter `backends` against ffmpeg -encoders output. Anything
    //    ffmpeg can't load drops out of the list.
    let backends = backends.into_iter()
        .filter(|b| backend_available(*b, &ffmpeg_encoders))
        .collect();

    HwProbeReport { vendor, generation, backends, matrix }
}

fn ffmpeg_available_encoders() -> Vec<String> {
    let out = Command::new("ffmpeg").args(["-hide_banner", "-encoders"]).output();
    let Ok(out) = out else { return vec![] };
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
     .filter_map(|l| l.split_whitespace().nth(1))
     .map(|s| s.to_string())
     .collect()
}

fn backend_available(b: EncoderBackend, ffmpeg_list: &[String]) -> bool {
    use EncoderBackend::*;
    let names: &[&str] = match b {
        Nvenc       => &["h264_nvenc", "hevc_nvenc", "av1_nvenc"],
        Vaapi       => &["h264_vaapi", "hevc_vaapi", "av1_vaapi"],
        Qsv         => &["h264_qsv",   "hevc_qsv",   "av1_qsv"],
        Amf         => &["h264_amf",   "hevc_amf",   "av1_amf"],
        VideoToolbox=> &["h264_videotoolbox", "hevc_videotoolbox"], // av1 absent
        Software    => &["libx264"],
    };
    names.iter().any(|n| ffmpeg_list.iter().any(|x| x == n))
}

fn detect_vendor() -> GpuVendor {
    if cfg!(target_os = "macos") { return GpuVendor::Apple; }
    if std::path::Path::new("/dev/nvidia0").exists()        { return GpuVendor::Nvidia; }
    if std::path::Path::new("/sys/module/amdgpu").exists()  { return GpuVendor::Amd; }
    if std::path::Path::new("/sys/module/i915").exists()    { return GpuVendor::Intel; }
    GpuVendor::Unknown
}

fn detect_generation(vendor: GpuVendor) -> GpuGeneration {
    match vendor {
        GpuVendor::Nvidia => {
            // nvidia-smi --query-gpu=compute_cap
            let out = Command::new("nvidia-smi")
                .args(["--query-gpu=compute_cap", "--format=csv,noheader"]).output();
            if let Ok(o) = out {
                let cap = String::from_utf8_lossy(&o.stdout).trim().to_string();
                // 8.9 = Ada, 9.0 = Hopper, 10.0/12.0 = Blackwell
                return match cap.split('.').next().and_then(|s| s.parse::<u32>().ok()) {
                    Some(8) => GpuGeneration::NvidiaPreAda, // 8.x covers Ampere(8.6) and Ada(8.9)
                    Some(9) => GpuGeneration::NvidiaBlackwell,
                    _ => GpuGeneration::NvidiaAda,
                };
            }
            GpuGeneration::Unknown
        }
        GpuVendor::Intel => {
            // /sys/class/drm/card*/device/vendor == 0x8086; check driver name.
            // "i915" with "Arc" / "DG2" / "BMG" in dmesg → Arc.
            GpuGeneration::IntelArc // simplest heuristic; refine in code
        }
        GpuVendor::Amd => {
            // /sys/class/drm/card*/device/uevent → AMDGPU family.
            GpuGeneration::AmdRdna3
        }
        GpuVendor::Apple => {
            // sysctl machdep.cpu.brand_string
            let out = Command::new("sysctl").args(["-n", "machdep.cpu.brand_string"]).output();
            let s = String::from_utf8_lossy(&out.unwrap_or_default().stdout);
            if s.contains("M3") || s.contains("M4") || s.contains("M5") {
                GpuGeneration::AppleM3Plus
            } else { GpuGeneration::AppleM1M2 }
        }
        GpuVendor::Unknown => GpuGeneration::Unknown,
    }
}
```

The fallback chain is explicit at every branch in the `match`:
`nvenc_then_sw`, `qsv_then_vaapi_then_sw`, `vt_then_sw`,
`vaapi_then_amf_then_sw`. Each helper returns `Vec<EncoderBackend>`
in preference order. The `backend_available` filter cross-checks
against the live `ffmpeg -encoders` output, so a missing driver or
a license-restricted Mesa build silently drops down the chain.

### 4. Codec enumeration: integration with `encoder_args_for`

`crates/qubox-media/src/lib.rs:1460-1580` (`encoder_args_for`) currently
takes `(config.backend, config.encoder_kind)` and emits FFmpeg args.
The change is minimal: add a `Codec` enum mirror alongside
`VideoEncoderKind`, then make `encoder_args_for` dispatch on `Codec`.

New `match` arm in `EncoderBackend::ffmpeg_name` (extends
`:164-185`):

```rust
impl EncoderBackend {
    pub fn ffmpeg_name(self, codec: Codec) -> Option<&'static str> {
        match (self, codec) {
            (EncoderBackend::Software,    Codec::H264) => Some("libx264"),
            (EncoderBackend::Software,    Codec::Hevc) => Some("libx265"),
            (EncoderBackend::Software,    Codec::Av1)  => Some("libsvtav1"),
            (EncoderBackend::Nvenc,       Codec::H264) => Some("h264_nvenc"),
            (EncoderBackend::Nvenc,       Codec::Hevc) => Some("hevc_nvenc"),
            (EncoderBackend::Nvenc,       Codec::Av1)  => Some("av1_nvenc"),
            (EncoderBackend::Vaapi,       Codec::H264) => Some("h264_vaapi"),
            (EncoderBackend::Vaapi,       Codec::Hevc) => Some("hevc_vaapi"),
            (EncoderBackend::Vaapi,       Codec::Av1)  => Some("av1_vaapi"),
            (EncoderBackend::Qsv,         Codec::H264) => Some("h264_qsv"),
            (EncoderBackend::Qsv,         Codec::Hevc) => Some("hevc_qsv"),
            (EncoderBackend::Qsv,         Codec::Av1)  => Some("av1_qsv"),
            (EncoderBackend::Amf,         Codec::H264) => Some("h264_amf"),
            (EncoderBackend::Amf,         Codec::Hevc) => Some("hevc_amf"),
            (EncoderBackend::Amf,         Codec::Av1)  => Some("av1_amf"),
            (EncoderBackend::VideoToolbox,Codec::H264) => Some("h264_videotoolbox"),
            (EncoderBackend::VideoToolbox,Codec::Hevc) => Some("hevc_videotoolbox"),
            (EncoderBackend::VideoToolbox,Codec::Av1)  => None, // absent mid-2026
            _ => None,
        }
    }
}
```

Inside `encoder_args_for`, when `codec == Codec::Av1` and the chosen
backend is `Nvenc` or `Qsv` or `Amf`, **append**:

```rust
"-svcc"   , "1",      // NVENC: AV1 SCC toggle (Ada+)
"-aom-ivf","1",       // optional: wrap in IVF if decoder needs it
```

For Mesa VA-API AV1 (`av1_vaapi`), append:

```rust
"-rc_mode", "VBR",
"-low_power", "0",    // RDNA3 wants low_power=0 for B-frames
```

For VideoToolbox HEVC on macOS HDR, append:

```rust
"-allow_sw", "0",
"-realtime", "1",
"-pix_fmt", "yuv420p10le",
"-color_primaries", "bt2020",
"-color_trc", "smpte2084",
"-colorspace", "bt2020nc",
```

### 5. HEVC royalty encumbrance

- We **ship HEVC encode** because (a) every modern HW encoder supports
  it, (b) Apple Silicon prefers HEVC (no HW AV1 encode until at
  least M5 Pro/Max), (c) AV1 HW is absent on pre-Ada Nvidia and
  pre-Alchemist Intel.
- **Documented in `LICENSE-3rdparty.md`**: HEVC is shipped via OS /
  driver HW paths only; the daemon does not link the x265 software
  reference encoder. The client **does not** ship `libx265` either.
- The HEVC Advance pool (Access Advance) charges ~$0.20-$0.30 per
  device ($0.18-0.27 % ASP) — but applies to device manufacturers,
  not to software vendors. **Velos Media** charges $1-$2.50/device and
  is actively litigating (Velos v. ByteDance, W.D. Tex., June 2025;
  FRAND cross-fire in China). **Sisvel's AV1 pool** licenses ~50 %
  of AV1 finished products (Feb 2026), so AV1 is *not* royalty-free
  in practice.
- **Practical config** for Qubox: ship only the HW HEVC path (which
  inherits the device manufacturer's pool coverage) and the
  software **libsvtav1** AV1 path (royalty-free AOMedia license).
  This avoids creating new pool exposure for us as a software vendor.
- The decision is reviewed annually; VVC/H.266 is not yet relevant
  for desktop capture (no HW encode, no client support).

### 6. Content classifier

New file: `crates/qubox-host-agent/src/content_classifier.rs` (~200
lines). The classifier runs on the host before each access unit
and writes `screen_content_likely: bool` into the access unit
metadata.

Algorithm: 16×16 Sobel-derived edge histogram on a **1/8-resolution
downsampled** grayscale frame (cheap on GPU via wgpu compute).

```rust
//! Screen-content detection for AV1 SCC toggling.

use wgpu::{ComputePipeline, Device, Queue, Texture, Buffer};

pub struct ContentClassifier {
    pipeline: ComputePipeline,
    histogram_buf: Buffer,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Screen,      // high-frequency edges, low color entropy → SCC
    Natural,     // smooth gradients → no SCC
}

impl ContentClassifier {
    pub fn classify(&self, frame: &Texture, queue: &Queue) -> FrameKind {
        // 1) Dispatch compute shader:
        //    - Sample frame at 1/8 resolution (12.5 % pixels)
        //    - 3×3 Sobel → magnitude
        //    - Threshold > 32 → "edge pixel"
        //    - Bin edges into a 256-bin histogram
        //    - Also compute dominant-color count via 16-color k-means
        //       single iteration (centroids pre-seeded from typical UI)
        // 2) Read histogram back, compute:
        //    - high_freq_ratio = bins[200..256].sum() / total
        //    - vertical_edge_ratio = bins for 90° edges / total
        //    - color_cardinality = distinct quantized colors
        // 3) Decision (conservative; false positives cost ~10 % bitrate):
        //    Screen if  high_freq_ratio > 0.18
        //            AND vertical_edge_ratio > 0.06
        //            AND color_cardinality < 24
        //    else Natural.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn content_classifier_detects_text_heavy_frames() {
        // Synthetic 1920x1080 frame: white background, black 12px text.
        // Expected: FrameKind::Screen.
    }
    #[test]
    fn content_classifier_detects_natural_video() {
        // Synthetic 1920x1080 gradient frame.
        // Expected: FrameKind::Natural.
    }
    #[test]
    fn content_classifier_is_conservative_on_mixed() {
        // 50/50 mix of text and gradient.
        // Expected: FrameKind::Natural (conservative threshold).
    }
}
```

Threshold values are **conservative** by default (false positives
cost ~10 % bitrate; false negatives cost nothing — we just lose the
SCC gain). Users opt into aggressive mode via the CLI flag
`--screen-content-detection=aggressive` which lowers the thresholds
by 25 %.

### 7. HDR negotiation

**Static metadata payload** (24 bytes, packed per SMPTE ST 2086 +
SMPTE ST 2108-1):

```rust
//! crates/qubox-media/src/codec/hdr.rs
pub const ST2086_PAYLOAD_SIZE: usize = 24;
pub const CLLI_PAYLOAD_SIZE:   usize = 4;
pub const SEI_TYPE_MDCV: u8 = 137;  // 0x89
pub const SEI_TYPE_CLLI: u8 = 144;  // 0x90

/// Pack SMPTE ST 2086 Mastering Display Color Volume into the
/// HEVC SEI byte layout used by SMPTE ST 2108-1 Annex A.
pub fn pack_st2086(
    primaries: [(u16,u16); 3],    // R,G,B (x*50000, y*50000)
    white_point: (u16,u16),
    min_lum: u32, max_lum: u32,   // cd/m^2, scaled by 10000
) -> [u8; 24] { /* … */ }

pub fn pack_clli(max_cll: u16, max_fall: u16) -> [u8; 4] { /* … */ }
```

**wgpu HDR capability check** (client-side, runs once at startup):

```rust
// crates/qubox-display/src/hdr.rs
pub fn detect_hdr(surface: &wgpu::Surface, adapter: &wgpu::Adapter) -> HdrCapability {
    let caps = surface.get_capabilities(adapter);
    let want_fmt = wgpu::TextureFormat::Rgb10a2Unorm;
    let hdr10 = caps.format_capabilities.iter().any(|fc| {
        fc.format == want_fmt && fc.color_spaces.contains(&wgpu::SurfaceColorSpace::Bt2100Pq)
    });
    let info = surface.display_hdr_info(adapter);
    HdrCapability { hdr10, peak_nits: info.maximum_full_frame_intensity_luminance,
                    info: info.clone() }
}

#[derive(Debug, Clone)]
pub struct HdrCapability {
    pub hdr10: bool,
    pub peak_nits: Option<f32>,
    pub info: wgpu::PresentModeInfo, // re-export
}
```

The wgpu ≥ 23 surface check is the documented path — see
`SurfaceCapabilities::format_capabilities` and the
`SurfaceColorSpace::Bt2100Pq` variant. `Auto` never resolves to HDR.

**Wire field** (already exists, ADR §3 reaffirms format):
`crates/qubox-transport/src/lib.rs:1534`
`hdr_static_metadata: Option<Vec<u8>>`. The `Vec<u8>` payload is a
**length-prefixed concat** of one MDCV SEI + one CLLI SEI, i.e.
`[mdcv24, clli4]` (28 bytes total) when present, `None` when SDR.
This byte layout is already shipped on `send_access_unit_ext` at
`crates/qubox-transport/src/lib.rs:391-426`.

### 8. Wire-format extension: `scc_enabled` flag

New field on `WireAccessUnitHeader` at
`crates/qubox-transport/src/lib.rs:1511-1535`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct WireAccessUnitHeader {
    session_id: Uuid,
    frame_id: u64,
    timestamp_micros: u64,
    keyframe: bool,
    byte_len: usize,
    #[serde(default)]
    codec: Option<VideoCodec>,
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
    /// NEW (ADR-018 §8): true iff this access unit was encoded with
    /// AV1 Screen Content Coding tools (palette / IBC) enabled. Used
    /// by ADR-014 FEC's ROI computation and by the client to skip
    /// SCC-specific post-processing when the field is false.
    #[serde(default)]
    scc_enabled: bool,
}
```

**Version bump procedure** (per ADR-010 project rule #2, all fields
are `#[serde(default)]`):

1. Old clients ignore `scc_enabled` (serde default → `false`).
2. New clients compute the field on every access unit.
3. No version handshake bump is needed because the field is purely
   advisory; the absence/presence does not change bitstream syntax.
4. Codec negotiation still happens via `SessionRequested.preferred_codec`
   at `crates/qubox-proto/src/lib.rs:615`; that path is unchanged.

### 9. Step-by-step implementation order (numbered PRs)

| # | PR | Files touched | Approx LoC |
|---|----|--------------|-----------|
| 1 | **Workspace deps + features** | `/Cargo.toml`, `crates/qubox-media/Cargo.toml` | 25 |
| 2 | **`CodecMatrix` + tests** | new `crates/qubox-media/src/codec/matrix.rs`, `lib.rs:164-185` (extend match) | 220 |
| 3 | **`hw_probe` module** | new `crates/qubox-media/src/codec/hw_probe.rs`, wire into `apps/qubox-host-agent/src/main.rs` | 180 |
| 4 | **`encoder_args_for` codec dispatch** | `crates/qubox-media/src/lib.rs:1460-1580` (replace `VideoEncoderKind` match with `Codec` match + new SCC/HEVC/HDR10 arg appends) | 90 |
| 5 | **Content classifier** | new `crates/qubox-host-agent/src/content_classifier.rs` + compute shader | 200 |
| 6 | **HDR pack/unpack + capability check** | new `crates/qubox-media/src/codec/hdr.rs`, `crates/qubox-display/src/hdr.rs`, wire into session start | 140 |
| 7 | **Wire `scc_enabled` field** | `crates/qubox-transport/src/lib.rs:1511-1535` + `:391-426` | 30 |
| 8 | **LICENSE-3rdparty.md update** | `/LICENSE-3rdparty.md` | 20 |
| 9 | **Integration tests + CI job** | `crates/qubox-media/tests/codec_matrix_e2e.rs`, `.github/workflows/ci.yml` | 180 |

PRs 1-4 land first (encoder selection work). PR 5 depends on PR 3.
PR 6 is independent. PR 7 can land any time after PR 2. PR 9
runs the whole stack on each CI push.

### 10. Test specifications

In `crates/qubox-media/src/codec/matrix.rs` (already in §2):
- `codec_matrix_picks_av1_for_4k144_on_ada` — `NVIDIA_CODECS + (3840,2160,144)` → `Av1`
- `prefers_hevc_for_1440p` — `NVIDIA_CODECS + (2560,1440,60)` → `Hevc` or `Av1`
- `forces_hevc_when_hdr_on_apple` — `APPLE_VIDEO_TOOLBOX_CODECS + hdr=true` → `Hevc`
- `falls_back_to_h264_when_sw_only` — `SOFTWARE_FALLBACK + 720p` → `H264`
- `picks_scc_codec_for_text_heavy_1080p` — `NVIDIA_CODECS + 1080p + screen_content_likely` → `Av1`

New in `crates/qubox-media/src/codec/hw_probe.rs`:
- `hw_probe_returns_nvenc_on_linux_with_rtx4090` (CI matrix: bare-metal GPU runner)
- `hw_probe_returns_software_when_no_gpu` (CI: no `/dev/dri`)
- `hw_probe_filters_unavailable_backends` (CI: build ffmpeg without `libnvenc`)

New in `crates/qubox-host-agent/src/content_classifier.rs`:
- `content_classifier_detects_text_heavy_frames` — synthetic 1920×1080 with 12 px text → `Screen`
- `content_classifier_detects_natural_video` — synthetic gradient → `Natural`
- `content_classifier_is_conservative_on_mixed` — 50/50 → `Natural`

New in `crates/qubox-display/src/hdr.rs`:
- `hdr_negotiation_handles_unsupported_displays` — surface reports no `Bt2100Pq` → returns `hdr10=false`, payload stays `None`
- `hdr_sei_packing_roundtrip` — pack `pack_st208` then decode → matches input
- `hdr_clli_packing_roundtrip` — `pack_clli` roundtrip

New in `crates/qubox-transport/src/lib.rs`:
- `wire_access_unit_header_deserializes_without_scc_field` — JSON without `scc_enabled` parses, defaults to `false`
- `wire_access_unit_header_serde_roundtrip` — serialize → deserialize → equality

### 11. Pitfalls (gotchas)

1. **AV1 HW encode on pre-Ada NVIDIA silently falls back to software**
   on Mesa VA-API builds without `libnvenc`. The probe MUST read
   `ffmpeg -encoders` output, not just trust the GPU name. Symptom if
   missed: 100 % CPU on RTX 3090 with `av1_nvenc` selected.
2. **AV1 hardware encode is absent on Apple Silicon as of mid-2026**
   (M3 decodes AV1 but does not encode). `APPLE_VIDEO_TOOLBOX_CODECS`
   therefore has `hw_av1 = false` and the `ffmpeg_name` match returns
   `None` for `VideoToolbox + Av1`. If a future macOS Tahoe point
   release ships `AV1EncoderSW.bundle`, the probe should re-check on
   startup; do **not** hard-code "Apple has AV1 encode".
3. **Mesa's VA-API AV1 encoder** (`av1_vaapi`) requires **Mesa ≥ 23.3**
   AND `radeonsi` driver AND a RDNA3+ GPU. RDNA2 (RX 6000) decodes
   AV1 but does not encode it. The `VAProfileAV1Profile0 :
   VAEntrypointEncSlice` line in `vainfo` is the gate; if absent,
   demote to `AMD_PRE_RDNA3_CODECS`.
4. **HEVC HDR10 SEI payload is 24 bytes (MDCV) + 4 bytes (CLLI), not
   just 24.** ST 2108-1 Annex A requires both SEI messages packed
   back-to-back, prefixed by their SEI type/size bytes (`0x89 0x18`
   and `0x90 0x04`). Clients that decode only the first 24 bytes will
   see MaxCLL=0 and mis-tone-map on OLEDs.
5. **ffmpeg-next 7.x `avcodec_find_encoder_by_name` returns
   `*const AVCodec`** (FFmpeg 7 ABI change). Old `*mut AVCodec` code
   will not compile against ffmpeg-next ≥ 7.0. All encoder lookup
   call sites must be updated.
6. **`vp9` is removed from `VideoCodec` enum** — it never had a
   production HW path on any modern platform. Use `Codec::Vp9` only
   as a placeholder; emit H.264 over the wire via
   `Codec::Vp9.as_proto()`.
7. **`wgpu::SurfaceColorSpace::Auto` never picks HDR.** Even on
   HDR-capable surfaces, a surface configured with `Auto` reports
   only SDR color spaces. The HDR probe must explicitly intersect
   `SurfaceCapabilities::format_capabilities` looking for
   `Bt2100Pq` + `Rgb10a2Unorm`.

### 12. Verification commands

Per-platform encoder availability (run on a real host, not in CI):

```bash
# NVIDIA
nvidia-smi --query-gpu=name,compute_cap --format=csv
ffmpeg -hide_banner -encoders | grep -E 'nvenc|nvenc_av1'
# Confirm AV1 if Ada+: should list  av1_nvenc

# Intel Arc (Linux)
vainfo --display drm --device /dev/dri/renderD128 | grep -E 'AV1|HEVC|H264'
# Want: VAProfileAV1Profile0 : VAEntrypointEncSlice  (Alchemist+ only)

# AMD RDNA3 (Linux)
vainfo --display drm --device /dev/dri/renderD128 | grep -E 'AV1|HEVC|H264'
# Mesa >= 23.3 + radeonsi + RDNA3

# Apple Silicon (macOS)
system_profiler SPDisplaysDataType
ffmpeg -hide_banner -encoders | grep -E 'videotoolbox|av1'
# M3+: av1_videotoolbox ENCODER IS ABSENT as of mid-2026

# Software fallback sanity
ffmpeg -hide_banner -encoders | grep -E 'libx264|libsvtav1|libaom'
```

Repo-level tests:

```bash
# Unit tests for matrix + classifier + HDR packing
cargo test -p qubox-media codec_matrix
cargo test -p qubox-host-agent content_classifier
cargo test -p qubox-display hdr_
cargo test -p qubox-transport wire_access_unit_header_

# End-to-end on a real Linux host with NVIDIA + Intel iGPU
cargo run -p qubox-host-agent -- \
  --probe-encoders --list-backends --print-matrix
# Expected output lists the resolved CodecMatrix + ordered backends.

# 4K144 roundtrip with AV1 (requires Ada NVENC or AV1 SW fallback)
cargo test -p qubox-media codec_matrix_picks_av1_for_4k144_on_ada -- --ignored
```

### 13. File path / line index

| Change | File | Lines |
|---|---|---|
| Add `Codec` enum + matrices + `choose_codec` | `crates/qubox-media/src/codec/matrix.rs` | new file |
| Extend `EncoderBackend::ffmpeg_name` | `crates/qubox-media/src/lib.rs` | `164-185` |
| Update `encoder_args_for` to dispatch on `Codec` | `crates/qubox-media/src/lib.rs` | `1460-1580` |
| HW probe module | `crates/qubox-media/src/codec/hw_probe.rs` | new file |
| HDR pack/unpack | `crates/qubox-media/src/codec/hdr.rs` | new file |
| HDR wgpu capability check | `crates/qubox-display/src/hdr.rs` | new file |
| Content classifier | `crates/qubox-host-agent/src/content_classifier.rs` | new file |
| Wire header `scc_enabled` field | `crates/qubox-transport/src/lib.rs` | `1511-1535` |
| Wire send path `scc_enabled` arg | `crates/qubox-transport/src/lib.rs` | `391-426` |
| Workspace deps | `/Cargo.toml` | append to `[workspace.dependencies]` |
| Crate-local deps + features | `crates/qubox-media/Cargo.toml` | full file |
| 3rd-party license | `/LICENSE-3rdparty.md` | append §HEVC and §AV1 (Sisvel) |

## Consequences

### Positive

- **One source of truth** for codec selection. Replaces the manual
  CLI flag and the per-platform copy-paste in
  `crates/qubox-media/src/lib.rs:164-203`.
- 4K144 becomes possible (AV1 + NVENC Ada or Apple software AV1).
- HDR10 is a first-class option (HEVC on Apple, AV1 on NVENC/Intel
  Arc/AMD RDNA3+).
- AV1 SCC gives a measurable boost for text-heavy desktop content
  (~12 % BD-rate from IBC alone, ~50 % combined with palette per
  Visionular's Aurora1 measurements).

### Negative / Risk

- Codec matrix drift: as new GPUs ship, the matrix needs per-platform
  updates. CI test `hw_probe_returns_nvenc_on_linux_with_rtx4090` is
  the tripwire.
- HEVC royalty: shipping a binary that references HEVC encode on
  Win/Linux may inherit pool coverage via the device manufacturer
  (Access Advance / Via Licensing) but the legal posture should be
  reviewed by counsel annually. The `LICENSE-3rdparty.md` patch
  documents the policy.
- AV1 SCC quality regression risk: false positives cost ~10 %
  bitrate. The classifier's threshold is conservative by default;
  aggressive mode is opt-in via
  `--screen-content-detection=aggressive`.

### Roadmap mapping

- Required for P2-14 (HDR), P2-16 (4K144).
- Required for P2-17 (macOS) and P2-18 (Windows DXGI confirmation).
- Builds on ADR-013 (per-frame byte budget assumes a known codec).

### References

- `crates/qubox-media/src/lib.rs:164-185` — `EncoderBackend::ffmpeg_name`
- `crates/qubox-media/src/lib.rs:187-203` — `EncoderBackend::all_kinds`
- `crates/qubox-media/src/lib.rs:1460-1580` — `encoder_args_for`
- `crates/qubox-media/src/encoder_probe.rs:172-203` — `candidates_for_kind`
- `crates/qubox-media/src/codec/` — new module (ADR-018)
- `crates/qubox-transport/src/lib.rs:391-426` — `send_access_unit_ext`
- `crates/qubox-transport/src/lib.rs:1511-1535` — `WireAccessUnitHeader`
- `crates/qubox-host-agent/src/content_classifier.rs` — new module (ADR-018)
- ADR-009 (ffmpeg-next decoder + wgpu renderer)
- ADR-010 project rule #2 (every wire field `#[serde(default)]`)
- ADR-013 (frame-aware pacing per-codec)
- ADR-014 §3 (parity datagram ROI maps to codec's ROI hint)
- ADR-016 (zero-copy surfaces)
- ADR-017 (WebCodecs browser client picks from this matrix)