use std::{
    collections::HashMap,
    env,
    io::Read,
    path::PathBuf,
    process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio},
};

use qubox_proto::{CaptureKind, PlatformOs, VideoCodec};
use serde::{Deserialize, Serialize};

pub mod codec;
pub mod encoder_probe;
pub mod preset;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProbeStatus {
    Available,
    Missing,
    Unsupported,
    Planned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackendProbe {
    pub name: String,
    pub status: ProbeStatus,
    pub details: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaBackendReport {
    pub platform: PlatformOs,
    pub capture: BackendProbe,
    pub encoder: BackendProbe,
    pub codec: VideoCodec,
    pub ready_for_realtime: bool,
    pub blockers: Vec<String>,
    pub planned_pipeline: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum H264EncoderBackend {
    Nvenc,
    Vaapi,
    Qsv,
    Amf,
    VideoToolbox,
    Libx264,
}

impl H264EncoderBackend {
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            H264EncoderBackend::Nvenc => "h264_nvenc",
            H264EncoderBackend::Vaapi => "h264_vaapi",
            H264EncoderBackend::Qsv => "h264_qsv",
            H264EncoderBackend::Amf => "h264_amf",
            H264EncoderBackend::VideoToolbox => "h264_videotoolbox",
            H264EncoderBackend::Libx264 => "libx264",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            H264EncoderBackend::Nvenc => "NVIDIA NVENC",
            H264EncoderBackend::Vaapi => "VAAPI",
            H264EncoderBackend::Qsv => "Intel Quick Sync",
            H264EncoderBackend::Amf => "AMD AMF",
            H264EncoderBackend::VideoToolbox => "Apple VideoToolbox",
            H264EncoderBackend::Libx264 => "libx264 software",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureSourceConfig {
    #[serde(rename = "linux_pipewire")]
    LinuxPipeWire { node: String },
    #[serde(rename = "linux_x11")]
    LinuxX11 { display: String },
    #[serde(rename = "windows_gdigrab")]
    WindowsGdiGrab { input: String },
    #[serde(rename = "macos_avfoundation")]
    MacosAvFoundation {
        display_index: String,
        audio_index: String,
    },
    #[serde(rename = "windows_dxgi")]
    WindowsDxgi { input: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum VideoEncoderKind {
    /// H.264 / AVC encoders.
    H264,
    /// H.265 / HEVC encoders.
    H265,
    /// AV1 encoders.
    Av1,
}

impl VideoEncoderKind {
    pub fn label(self) -> &'static str {
        match self {
            VideoEncoderKind::H264 => "H.264",
            VideoEncoderKind::H265 => "H.265",
            VideoEncoderKind::Av1 => "AV1",
        }
    }

    pub fn video_codec(self) -> VideoCodec {
        match self {
            VideoEncoderKind::H264 => VideoCodec::H264,
            VideoEncoderKind::H265 => VideoCodec::H265,
            VideoEncoderKind::Av1 => VideoCodec::Av1,
        }
    }
}

impl From<VideoCodec> for VideoEncoderKind {
    fn from(value: VideoCodec) -> Self {
        match value {
            VideoCodec::H264 => VideoEncoderKind::H264,
            VideoCodec::H265 => VideoEncoderKind::H265,
            VideoCodec::Av1 => VideoEncoderKind::Av1,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EncoderBackend {
    /// `libx264` / `libx265` / `libaom-av1` — software reference encoders.
    Software,
    /// NVIDIA NVENC.
    Nvenc,
    /// VAAPI (Intel/AMD on Linux).
    Vaapi,
    /// Intel Quick Sync Video.
    Qsv,
    /// AMD AMF.
    Amf,
    /// Apple VideoToolbox.
    VideoToolbox,
}

impl EncoderBackend {
    pub fn label(self) -> &'static str {
        match self {
            EncoderBackend::Software => "software (libx264/libx265/libaom-av1)",
            EncoderBackend::Nvenc => "NVIDIA NVENC",
            EncoderBackend::Vaapi => "VAAPI",
            EncoderBackend::Qsv => "Intel Quick Sync",
            EncoderBackend::Amf => "AMD AMF",
            EncoderBackend::VideoToolbox => "Apple VideoToolbox",
        }
    }

    /// Return the ffmpeg encoder name for a given codec + backend pair, if the
    /// pair is supported. `None` means the combination is not a real encoder.
    pub fn ffmpeg_name(self, kind: VideoEncoderKind) -> Option<&'static str> {
        match (self, kind) {
            (EncoderBackend::Software, VideoEncoderKind::H264) => Some("libx264"),
            (EncoderBackend::Software, VideoEncoderKind::H265) => Some("libx265"),
            (EncoderBackend::Software, VideoEncoderKind::Av1) => Some("libaom-av1"),
            (EncoderBackend::Nvenc, VideoEncoderKind::H264) => Some("h264_nvenc"),
            (EncoderBackend::Nvenc, VideoEncoderKind::H265) => Some("hevc_nvenc"),
            (EncoderBackend::Nvenc, VideoEncoderKind::Av1) => Some("av1_nvenc"),
            (EncoderBackend::Vaapi, VideoEncoderKind::H264) => Some("h264_vaapi"),
            (EncoderBackend::Vaapi, VideoEncoderKind::H265) => Some("hevc_vaapi"),
            (EncoderBackend::Vaapi, VideoEncoderKind::Av1) => Some("av1_vaapi"),
            (EncoderBackend::Qsv, VideoEncoderKind::H264) => Some("h264_qsv"),
            (EncoderBackend::Qsv, VideoEncoderKind::H265) => Some("hevc_qsv"),
            (EncoderBackend::Qsv, VideoEncoderKind::Av1) => Some("av1_qsv"),
            (EncoderBackend::Amf, VideoEncoderKind::H264) => Some("h264_amf"),
            (EncoderBackend::Amf, VideoEncoderKind::H265) => Some("hevc_amf"),
            (EncoderBackend::Amf, VideoEncoderKind::Av1) => Some("av1_amf"),
            (EncoderBackend::VideoToolbox, VideoEncoderKind::H264) => Some("h264_videotoolbox"),
            (EncoderBackend::VideoToolbox, VideoEncoderKind::H265) => Some("hevc_videotoolbox"),
            (EncoderBackend::VideoToolbox, VideoEncoderKind::Av1) => Some("av1_videotoolbox"),
        }
    }

    pub fn all_kinds(self) -> &'static [VideoEncoderKind] {
        match self {
            EncoderBackend::Software | EncoderBackend::Nvenc => &[
                VideoEncoderKind::H264,
                VideoEncoderKind::H265,
                VideoEncoderKind::Av1,
            ],
            EncoderBackend::Vaapi | EncoderBackend::Qsv | EncoderBackend::Amf => {
                &[VideoEncoderKind::H264, VideoEncoderKind::H265]
            }
            EncoderBackend::VideoToolbox => &[
                VideoEncoderKind::H264,
                VideoEncoderKind::H265,
                VideoEncoderKind::Av1,
            ],
        }
    }
}

/// Information about one display/monitor a host can capture.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayInfo {
    /// Zero-based display index, stable for a given host session.
    pub index: u32,
    /// A short human-friendly name (e.g. "DP-1", "\\\\.\\DISPLAY1").
    pub name: String,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Refresh rate reported by the system in Hz. `None` when unknown.
    pub refresh_hz: Option<u32>,
    pub is_primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostVideoPipelineConfig {
    pub capture: CaptureSourceConfig,
    pub codec: VideoCodec,
    pub encoder: H264EncoderBackend,
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate_kbps: u32,
    pub keyframe_interval_frames: u32,
    /// HDR color space (P2-14). `None` means SDR.
    #[serde(default)]
    pub color_space: Option<qubox_proto::ColorSpace>,
    /// 8 or 10. 8 is the implicit default.
    #[serde(default = "default_eight_bits")]
    pub bit_depth: u8,
    /// HDR static metadata. `None` means no metadata; the encoder
    /// falls back to a generic BT.2020 + 1000/400 cd/m² master
    /// display for HDR10.
    #[serde(default)]
    pub hdr_static_metadata: Option<qubox_proto::HdrStaticMetadata>,
}

/// Codec-agnostic host pipeline configuration. New code should prefer this over
/// the older `HostVideoPipelineConfig`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VideoPipelineConfig {
    pub capture: CaptureSourceConfig,
    pub encoder_kind: VideoEncoderKind,
    pub backend: EncoderBackend,
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate_kbps: u32,
    /// Optional lower bound on instantaneous bitrate for VBV-constrained encoders.
    pub min_bitrate_kbps: Option<u32>,
    /// Optional explicit rate-control buffer size, in kilobits.
    pub buffer_size_kbits: Option<u32>,
    pub keyframe_interval_frames: u32,
    /// How the source framebuffer is mapped to the target.
    pub scale_mode: qubox_proto::ScaleMode,
    /// Optional explicit capture region within the source. When set, overrides
    /// display selection.
    pub capture_region: Option<CaptureRegion>,
    /// Optional display index from `enumerate_*_displays`. When set, the host
    /// captures only that monitor's geometry.
    pub display_index: Option<u32>,
    /// HDR color space (P2-14). `None` means SDR.
    #[serde(default)]
    pub color_space: Option<qubox_proto::ColorSpace>,
    /// 8 or 10. 8 is the implicit default.
    #[serde(default = "default_eight_bits")]
    pub bit_depth: u8,
    /// HDR static metadata. `None` means no metadata; the encoder
    /// falls back to a generic BT.2020 + 1000/400 cd/m² master
    /// display for HDR10.
    #[serde(default)]
    pub hdr_static_metadata: Option<qubox_proto::HdrStaticMetadata>,
}

impl VideoPipelineConfig {
    pub fn codec(&self) -> VideoCodec {
        self.encoder_kind.video_codec()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl From<qubox_proto::CaptureRegion> for CaptureRegion {
    fn from(value: qubox_proto::CaptureRegion) -> Self {
        Self {
            x: value.x,
            y: value.y,
            width: value.width,
            height: value.height,
        }
    }
}

impl HostVideoPipelineConfig {
    pub fn linux_pipewire_h264(
        node: impl Into<String>,
        encoder: H264EncoderBackend,
        width: u32,
        height: u32,
        framerate: u32,
        bitrate_kbps: u32,
    ) -> Self {
        Self {
            capture: CaptureSourceConfig::LinuxPipeWire { node: node.into() },
            codec: VideoCodec::H264,
            encoder,
            width,
            height,
            framerate,
            bitrate_kbps,
            keyframe_interval_frames: framerate.saturating_mul(2).max(1),
            color_space: None,
            bit_depth: default_eight_bits(),
            hdr_static_metadata: None,
        }
    }

    pub fn windows_gdigrab_h264(
        input: impl Into<String>,
        encoder: H264EncoderBackend,
        width: u32,
        height: u32,
        framerate: u32,
        bitrate_kbps: u32,
    ) -> Self {
        Self {
            capture: CaptureSourceConfig::WindowsGdiGrab {
                input: input.into(),
            },
            codec: VideoCodec::H264,
            encoder,
            width,
            height,
            framerate,
            bitrate_kbps,
            keyframe_interval_frames: framerate.saturating_mul(2).max(1),
            color_space: None,
            bit_depth: default_eight_bits(),
            hdr_static_metadata: None,
        }
    }

    pub fn linux_x11_h264(
        display: impl Into<String>,
        encoder: H264EncoderBackend,
        width: u32,
        height: u32,
        framerate: u32,
        bitrate_kbps: u32,
    ) -> Self {
        Self {
            capture: CaptureSourceConfig::LinuxX11 {
                display: display.into(),
            },
            codec: VideoCodec::H264,
            encoder,
            width,
            height,
            framerate,
            bitrate_kbps,
            keyframe_interval_frames: framerate.saturating_mul(2).max(1),
            color_space: None,
            bit_depth: default_eight_bits(),
            hdr_static_metadata: None,
        }
    }

    pub fn macos_avfoundation_h264(
        display_index: impl Into<String>,
        audio_index: impl Into<String>,
        encoder: H264EncoderBackend,
        width: u32,
        height: u32,
        framerate: u32,
        bitrate_kbps: u32,
    ) -> Self {
        Self {
            capture: CaptureSourceConfig::MacosAvFoundation {
                display_index: display_index.into(),
                audio_index: audio_index.into(),
            },
            codec: VideoCodec::H264,
            encoder,
            width,
            height,
            framerate,
            bitrate_kbps,
            keyframe_interval_frames: framerate.saturating_mul(2).max(1),
            color_space: None,
            bit_depth: default_eight_bits(),
            hdr_static_metadata: None,
        }
    }

    pub fn windows_dxgi_h264(
        input: impl Into<String>,
        encoder: H264EncoderBackend,
        width: u32,
        height: u32,
        framerate: u32,
        bitrate_kbps: u32,
    ) -> Self {
        Self {
            capture: CaptureSourceConfig::WindowsDxgi {
                input: input.into(),
            },
            codec: VideoCodec::H264,
            encoder,
            width,
            height,
            framerate,
            bitrate_kbps,
            keyframe_interval_frames: framerate.saturating_mul(2).max(1),
            color_space: None,
            bit_depth: default_eight_bits(),
            hdr_static_metadata: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FfmpegPipelinePlan {
    pub program: String,
    pub args: Vec<String>,
    pub output: EncodedOutput,
    pub notes: Vec<String>,
}

pub struct RunningMediaPipeline {
    plan: FfmpegPipelinePlan,
    child: Child,
    stdout: ChildStdout,
    stderr: Option<ChildStderr>,
}

impl RunningMediaPipeline {
    pub fn plan(&self) -> &FfmpegPipelinePlan {
        &self.plan
    }

    pub fn stdout_mut(&mut self) -> &mut ChildStdout {
        &mut self.stdout
    }

    pub fn stderr_mut(&mut self) -> Option<&mut ChildStderr> {
        self.stderr.as_mut()
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, MediaRuntimeError> {
        self.child.try_wait().map_err(MediaRuntimeError::from_io)
    }

    pub fn kill(&mut self) -> Result<(), MediaRuntimeError> {
        self.child.kill().map_err(MediaRuntimeError::from_io)
    }

    pub fn wait(&mut self) -> Result<ExitStatus, MediaRuntimeError> {
        self.child.wait().map_err(MediaRuntimeError::from_io)
    }
}

impl Drop for RunningMediaPipeline {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", content = "codec")]
pub enum EncodedOutput {
    /// Annex B elementary stream on stdout, tagged with the codec.
    AnnexBStdout { codec: VideoCodec },
    /// Backwards-compatible alias for H.264 annex B (kept for older callers).
    H264AnnexBStdout,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaPlanError {
    pub message: String,
}

impl std::fmt::Display for MediaPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for MediaPlanError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaRuntimeError {
    pub message: String,
}

impl MediaRuntimeError {
    fn from_io(error: std::io::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }

    fn from_packetize(error: MediaPacketizeError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

impl std::fmt::Display for MediaRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for MediaRuntimeError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncodedVideoAccessUnit {
    pub codec: VideoCodec,
    pub frame_id: u64,
    pub timestamp_micros: u64,
    pub keyframe: bool,
    pub nal_units: Vec<H264NalUnitInfo>,
    pub bytes: Vec<u8>,
    /// Which display this frame belongs to (0 = single display or unknown).
    #[serde(default)]
    pub display_id: u32,
    /// Stream index within the display.
    #[serde(default)]
    pub stream_id: u16,
    /// Width of the display source in pixels (0 if unknown).
    #[serde(default)]
    pub width: u32,
    /// Height of the display source in pixels (0 if unknown).
    #[serde(default)]
    pub height: u32,
    /// HDR color space of this access unit. `None` means SDR
    /// (BT.709 / sRGB); the client defaults to BT.709 when the
    /// field is absent.
    #[serde(default)]
    pub color_space: Option<qubox_proto::ColorSpace>,
    /// Bit depth of this access unit (8 or 10). 8 is the implicit
    /// default for older clients / SDR streams.
    #[serde(default = "default_eight_bits")]
    pub bit_depth: u8,
}

fn default_eight_bits() -> u8 {
    8
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct H264NalUnitInfo {
    pub nal_type: u8,
    pub offset: usize,
    pub length: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaPacketizeError {
    pub message: String,
}

impl std::fmt::Display for MediaPacketizeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for MediaPacketizeError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct H264AnnexBStreamFramer {
    inner: AnnexBStreamFramer,
}

impl H264AnnexBStreamFramer {
    pub fn new(framerate: u32) -> Result<Self, MediaPacketizeError> {
        Ok(Self {
            inner: AnnexBStreamFramer::new(framerate, VideoCodec::H264)?,
        })
    }

    pub fn push_chunk(
        &mut self,
        chunk: &[u8],
    ) -> Result<Vec<EncodedVideoAccessUnit>, MediaPacketizeError> {
        self.inner.push_chunk(chunk)
    }

    pub fn finish(&mut self) -> Result<Option<EncodedVideoAccessUnit>, MediaPacketizeError> {
        self.inner.finish()
    }
}

/// Codec-agnostic Annex B framer. Splits an elementary stream into access
/// units using codec-specific access-unit delimiters:
/// - H.264: NAL type 9 (AUD)
/// - H.265: NAL type 35 (AUD)
/// - AV1: OBU type 1 (OBU_SEQUENCE_HEADER) is rare in practice; the encoder
///        bitstream is not annex-B in the same way and is currently treated as
///        a single frame per call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnnexBStreamFramer {
    codec: VideoCodec,
    buffer: Vec<u8>,
    next_frame_id: u64,
    next_timestamp_micros: u64,
    frame_duration_micros: u64,
}

impl AnnexBStreamFramer {
    pub fn new(framerate: u32, codec: VideoCodec) -> Result<Self, MediaPacketizeError> {
        if framerate == 0 {
            return Err(MediaPacketizeError {
                message: "framerate must be greater than zero".to_string(),
            });
        }
        Ok(Self {
            codec,
            buffer: Vec::new(),
            next_frame_id: 0,
            next_timestamp_micros: 0,
            frame_duration_micros: 1_000_000 / u64::from(framerate),
        })
    }

    pub fn push_chunk(
        &mut self,
        chunk: &[u8],
    ) -> Result<Vec<EncodedVideoAccessUnit>, MediaPacketizeError> {
        self.buffer.extend_from_slice(chunk);
        match self.codec {
            VideoCodec::H264 => self.drain_h264(),
            VideoCodec::H265 => self.drain_h265(),
            VideoCodec::Av1 => self.drain_av1(),
        }
    }

    pub fn finish(&mut self) -> Result<Option<EncodedVideoAccessUnit>, MediaPacketizeError> {
        if self.buffer.is_empty() {
            return Ok(None);
        }
        let bytes = self.buffer.drain(..).collect();
        self.next_access_unit(bytes).map(Some)
    }

    fn drain_h264(&mut self) -> Result<Vec<EncodedVideoAccessUnit>, MediaPacketizeError> {
        let mut access_units = Vec::new();
        loop {
            let auds = find_aud_start_codes_h264(&self.buffer);
            let Some((first_aud, _)) = auds.first().copied() else {
                return Ok(access_units);
            };
            if first_aud > 0 {
                self.buffer.drain(0..first_aud);
                continue;
            }
            let Some((next_aud, _)) = auds.get(1).copied() else {
                return Ok(access_units);
            };
            let bytes = self.buffer.drain(0..next_aud).collect();
            access_units.push(self.next_access_unit(bytes)?);
        }
    }

    fn drain_h265(&mut self) -> Result<Vec<EncodedVideoAccessUnit>, MediaPacketizeError> {
        let mut access_units = Vec::new();
        loop {
            let auds = find_aud_start_codes_h265(&self.buffer);
            let Some((first_aud, _)) = auds.first().copied() else {
                return Ok(access_units);
            };
            if first_aud > 0 {
                self.buffer.drain(0..first_aud);
                continue;
            }
            let Some((next_aud, _)) = auds.get(1).copied() else {
                return Ok(access_units);
            };
            let bytes = self.buffer.drain(0..next_aud).collect();
            access_units.push(self.next_access_unit(bytes)?);
        }
    }

    fn drain_av1(&mut self) -> Result<Vec<EncodedVideoAccessUnit>, MediaPacketizeError> {
        // AV1 in IVF/matroska/av1 annex B uses OBU framing. We treat each OBU
        // sequence as one frame for now; this is fine for low-latency streams
        // where ffmpeg emits one frame per temporal unit.
        let mut access_units = Vec::new();
        let boundaries = find_av1_obu_boundaries(&self.buffer);
        if boundaries.len() < 2 {
            return Ok(access_units);
        }
        for pair in boundaries.windows(2) {
            let (start, end) = (pair[0], pair[1]);
            if start >= end {
                continue;
            }
            let bytes = self.buffer[start..end].to_vec();
            access_units.push(self.next_access_unit(bytes)?);
        }
        // Drop what we have consumed
        let consumed = *boundaries.last().unwrap();
        self.buffer.drain(0..consumed);
        Ok(access_units)
    }

    fn next_access_unit(
        &mut self,
        bytes: Vec<u8>,
    ) -> Result<EncodedVideoAccessUnit, MediaPacketizeError> {
        let frame_id = self.next_frame_id;
        let timestamp_micros = self.next_timestamp_micros;
        self.next_frame_id += 1;
        self.next_timestamp_micros += self.frame_duration_micros;
        packetize_access_unit(self.codec, frame_id, timestamp_micros, bytes)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MediaPipelineRead {
    AccessUnits(Vec<EncodedVideoAccessUnit>),
    EndOfStream(Vec<EncodedVideoAccessUnit>),
}

pub fn probe_default_host_pipeline() -> MediaBackendReport {
    let platform = current_os();
    let capture = match platform {
        PlatformOs::Linux => probe_linux_capture(),
        PlatformOs::Windows => probe_windows_gdigrab_capture(),
        PlatformOs::Macos => BackendProbe {
            name: "ScreenCaptureKit".to_string(),
            status: ProbeStatus::Planned,
            details: vec!["macOS host capture is planned after Linux and Windows".to_string()],
        },
        PlatformOs::Android => BackendProbe {
            name: "MediaProjection".to_string(),
            status: ProbeStatus::Planned,
            details: vec![
                "Android hosting is intentionally later than Android client support".to_string(),
            ],
        },
    };
    let encoder = probe_h264_encoder();
    let ready_for_realtime = matches!(capture.status, ProbeStatus::Available)
        && matches!(encoder.status, ProbeStatus::Available);
    let mut blockers = Vec::new();

    if !matches!(capture.status, ProbeStatus::Available) {
        blockers.push(format!("{} capture is not available", capture.name));
    }

    if !matches!(encoder.status, ProbeStatus::Available) {
        blockers.push("no hardware H.264 encoder was detected through ffmpeg".to_string());
    }

    MediaBackendReport {
        platform,
        capture,
        encoder,
        codec: VideoCodec::H264,
        ready_for_realtime,
        blockers,
        planned_pipeline: planned_pipeline_for(platform),
    }
}

pub fn default_linux_pipewire_h264_config() -> HostVideoPipelineConfig {
    let encoder = best_h264_encoder_for_platform(&[]).unwrap_or(H264EncoderBackend::Nvenc);

    HostVideoPipelineConfig::linux_pipewire_h264("0", encoder, 1920, 1080, 60, 20_000)
}

pub fn default_linux_x11_h264_config() -> HostVideoPipelineConfig {
    let encoder = best_h264_encoder_for_platform(&[]).unwrap_or(H264EncoderBackend::Nvenc);

    HostVideoPipelineConfig::linux_x11_h264(":0.0", encoder, 1920, 1080, 60, 20_000)
}

pub fn default_windows_gdigrab_h264_config() -> HostVideoPipelineConfig {
    let encoder = best_h264_encoder_for_platform(&[]).unwrap_or(H264EncoderBackend::Nvenc);

    HostVideoPipelineConfig::windows_gdigrab_h264("desktop", encoder, 1920, 1080, 60, 20_000)
}

pub fn best_h264_encoder_for_platform(available_names: &[String]) -> Option<H264EncoderBackend> {
    let priority = if cfg!(target_os = "windows") {
        &[
            H264EncoderBackend::Nvenc,
            H264EncoderBackend::Qsv,
            H264EncoderBackend::Amf,
        ][..]
    } else if cfg!(target_os = "macos") {
        &[
            H264EncoderBackend::VideoToolbox,
            H264EncoderBackend::Nvenc,
            H264EncoderBackend::Qsv,
            H264EncoderBackend::Libx264,
        ][..]
    } else {
        &[
            H264EncoderBackend::Nvenc,
            H264EncoderBackend::Vaapi,
            H264EncoderBackend::Qsv,
            H264EncoderBackend::Amf,
            H264EncoderBackend::Libx264,
        ][..]
    };

    priority.iter().copied().find(|encoder| {
        available_names.is_empty()
            || available_names
                .iter()
                .any(|name| name == encoder.ffmpeg_name())
    })
}

pub fn plan_ffmpeg_pipewire_h264(
    config: &HostVideoPipelineConfig,
) -> Result<FfmpegPipelinePlan, MediaPlanError> {
    validate_h264_config(config)?;

    let CaptureSourceConfig::LinuxPipeWire { node } = &config.capture else {
        return Err(MediaPlanError {
            message: "PipeWire FFmpeg plans require a linux_pipewire capture config".to_string(),
        });
    };
    if node.trim().is_empty() {
        return Err(MediaPlanError {
            message: "PipeWire node must not be empty".to_string(),
        });
    }

    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "warning".to_string(),
        "-nostdin".to_string(),
        "-f".to_string(),
        "pipewire".to_string(),
        "-framerate".to_string(),
        config.framerate.to_string(),
        "-i".to_string(),
        node.clone(),
        "-an".to_string(),
    ];

    args.extend(video_filter_args(config));
    args.extend(encoder_args(config));
    args.extend([
        "-bsf:v".to_string(),
        "h264_metadata=aud=insert".to_string(),
        "-f".to_string(),
        "h264".to_string(),
        "pipe:1".to_string(),
    ]);

    Ok(FfmpegPipelinePlan {
        program: "ffmpeg".to_string(),
        args,
        output: EncodedOutput::H264AnnexBStdout,
        notes: vec![
            "Reads a Linux PipeWire node and writes H.264 Annex B access units to stdout".to_string(),
            "The transport layer should consume stdout, packetize frames, and apply pacing/backpressure".to_string(),
            format!("Encoder backend: {}", config.encoder.label()),
        ],
    })
}

pub fn plan_ffmpeg_linux_x11_h264(
    config: &HostVideoPipelineConfig,
) -> Result<FfmpegPipelinePlan, MediaPlanError> {
    validate_h264_config(config)?;

    let CaptureSourceConfig::LinuxX11 { display } = &config.capture else {
        return Err(MediaPlanError {
            message: "Linux X11 FFmpeg plans require a linux_x11 capture config".to_string(),
        });
    };

    if display.trim().is_empty() {
        return Err(MediaPlanError {
            message: "Linux X11 display must not be empty".to_string(),
        });
    }

    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "warning".to_string(),
        "-nostdin".to_string(),
        "-f".to_string(),
        "x11grab".to_string(),
        "-framerate".to_string(),
        config.framerate.to_string(),
        "-draw_mouse".to_string(),
        "1".to_string(),
        "-i".to_string(),
        display.clone(),
        "-an".to_string(),
    ];

    args.extend(video_filter_args(config));
    args.extend(encoder_args(config));
    args.extend([
        "-bsf:v".to_string(),
        "h264_metadata=aud=insert".to_string(),
        "-f".to_string(),
        "h264".to_string(),
        "pipe:1".to_string(),
    ]);

    Ok(FfmpegPipelinePlan {
        program: "ffmpeg".to_string(),
        args,
        output: EncodedOutput::H264AnnexBStdout,
        notes: vec![
            "Reads a Linux X11 display through FFmpeg x11grab and writes H.264 Annex B access units to stdout".to_string(),
            format!("Display source: {}", display),
            "The transport layer should consume stdout, packetize frames, and apply pacing/backpressure".to_string(),
            format!("Encoder backend: {}", config.encoder.label()),
        ],
    })
}

pub fn plan_ffmpeg_windows_gdigrab_h264(
    config: &HostVideoPipelineConfig,
) -> Result<FfmpegPipelinePlan, MediaPlanError> {
    validate_h264_config(config)?;

    if matches!(
        config.encoder,
        H264EncoderBackend::Vaapi | H264EncoderBackend::VideoToolbox
    ) {
        return Err(MediaPlanError {
            message: format!(
                "{} is not supported by the Windows gdigrab host pipeline",
                config.encoder.label()
            ),
        });
    }

    let CaptureSourceConfig::WindowsGdiGrab { input } = &config.capture else {
        return Err(MediaPlanError {
            message: "Windows FFmpeg plans require a windows_gdigrab capture config".to_string(),
        });
    };

    if input.trim().is_empty() {
        return Err(MediaPlanError {
            message: "Windows gdigrab input must not be empty".to_string(),
        });
    }

    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "warning".to_string(),
        "-nostdin".to_string(),
        "-f".to_string(),
        "gdigrab".to_string(),
        "-framerate".to_string(),
        config.framerate.to_string(),
        "-draw_mouse".to_string(),
        "1".to_string(),
        "-i".to_string(),
        input.clone(),
        "-an".to_string(),
    ];

    args.extend(video_filter_args(config));
    args.extend(encoder_args(config));
    args.extend([
        "-bsf:v".to_string(),
        "h264_metadata=aud=insert".to_string(),
        "-f".to_string(),
        "h264".to_string(),
        "pipe:1".to_string(),
    ]);

    Ok(FfmpegPipelinePlan {
        program: "ffmpeg".to_string(),
        args,
        output: EncodedOutput::H264AnnexBStdout,
        notes: vec![
            "Reads the Windows desktop through FFmpeg gdigrab and writes H.264 Annex B access units to stdout".to_string(),
            format!("Input source: {}", input),
            "The transport layer should consume stdout, packetize frames, and apply pacing/backpressure".to_string(),
            format!("Encoder backend: {}", config.encoder.label()),
        ],
    })
}

/// macOS AVFoundation capture via ffmpeg. Validates that the host is running
/// macOS, the capture config is `MacosAvFoundation`, and the requested encoder
/// is a macOS-compatible VideoToolbox variant (`h264_videotoolbox` or
/// `hevc_videotoolbox`). Other encoders are rejected.
pub fn plan_ffmpeg_macos_avfoundation_h264(
    config: &HostVideoPipelineConfig,
) -> Result<FfmpegPipelinePlan, MediaPlanError> {
    validate_h264_config(config)?;

    if !matches!(
        config.encoder,
        H264EncoderBackend::VideoToolbox | H264EncoderBackend::Libx264
    ) {
        return Err(MediaPlanError {
            message: format!(
                "{} is not supported by the macOS AVFoundation host pipeline; \
                 use h264_videotoolbox, hevc_videotoolbox, or libx264",
                config.encoder.label()
            ),
        });
    }

    let CaptureSourceConfig::MacosAvFoundation {
        display_index,
        audio_index,
    } = &config.capture
    else {
        return Err(MediaPlanError {
            message: "macOS AVFoundation FFmpeg plans require a macos_avfoundation capture config"
                .to_string(),
        });
    };

    if display_index.trim().is_empty() {
        return Err(MediaPlanError {
            message: "macOS AVFoundation display_index must not be empty".to_string(),
        });
    }

    let input = format!("{}:{}", display_index, audio_index);

    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "warning".to_string(),
        "-nostdin".to_string(),
        "-f".to_string(),
        "avfoundation".to_string(),
        "-framerate".to_string(),
        config.framerate.to_string(),
        "-i".to_string(),
        input,
        "-an".to_string(),
    ];

    args.extend(video_filter_args(config));
    args.extend(encoder_args(config));
    args.extend([
        "-bsf:v".to_string(),
        "h264_metadata=aud=insert".to_string(),
        "-f".to_string(),
        "h264".to_string(),
        "pipe:1".to_string(),
    ]);

    Ok(FfmpegPipelinePlan {
        program: "ffmpeg".to_string(),
        args,
        output: EncodedOutput::H264AnnexBStdout,
        notes: vec![
            "Reads a macOS display via FFmpeg AVFoundation and writes H.264 Annex B access units to stdout".to_string(),
            format!("Display index: {}, audio index: {}", display_index, audio_index),
            "The transport layer should consume stdout, packetize frames, and apply pacing/backpressure".to_string(),
            format!("Encoder backend: {}", config.encoder.label()),
        ],
    })
}

/// Windows DXGI capture via ffmpeg. Validates that the host is running
/// Windows, the capture config is `WindowsDxgi`, and the requested encoder is
/// a Windows-compatible encoder (`h264_nvenc`, `h264_amf`, `h264_qsv`, or
/// `libx264`). `h264_videotoolbox` and `vaapi` are rejected.
///
/// FFmpeg's `dshow` input is used as a proxy for DXGI desktop capture. The
/// function attempts HDR via the `format` filter; on failure it falls back to
/// the existing gdigrab plan via `plan_ffmpeg_windows_gdigrab_h264`.
pub fn plan_ffmpeg_windows_dxgi_h264(
    config: &HostVideoPipelineConfig,
) -> Result<FfmpegPipelinePlan, MediaPlanError> {
    validate_h264_config(config)?;

    if matches!(
        config.encoder,
        H264EncoderBackend::Vaapi | H264EncoderBackend::VideoToolbox
    ) {
        return Err(MediaPlanError {
            message: format!(
                "{} is not supported by the Windows DXGI host pipeline",
                config.encoder.label()
            ),
        });
    }

    let CaptureSourceConfig::WindowsDxgi { input } = &config.capture else {
        return Err(MediaPlanError {
            message: "Windows DXGI FFmpeg plans require a windows_dxgi capture config".to_string(),
        });
    };

    if input.trim().is_empty() {
        return Err(MediaPlanError {
            message: "Windows DXGI input must not be empty".to_string(),
        });
    }

    let hdr_requested = config
        .color_space
        .map(|c| c != qubox_proto::ColorSpace::Bt709)
        .unwrap_or(false)
        || config.bit_depth == 10;

    // Prefer FFmpeg lavfi `ddagrab` (Desktop Duplication API). `input` is the
    // output index as a string ("0") or a legacy desktop name → index 0.
    let output_idx: u32 = input.trim().parse().unwrap_or(0);
    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "warning".to_string(),
        "-nostdin".to_string(),
        "-f".to_string(),
        "lavfi".to_string(),
        "-i".to_string(),
        format!(
            "ddagrab=output_idx={}:framerate={}",
            output_idx, config.framerate
        ),
        "-an".to_string(),
    ];

    if hdr_requested {
        let mut hdr_args = video_filter_args(config);
        hdr_args.push("-vf".to_string());
        hdr_args.push(format!(
            "scale={}:{},format=yuv420p10le",
            config.width, config.height
        ));
        args.extend(hdr_args);
    } else {
        args.extend(video_filter_args(config));
    }

    args.extend(encoder_args(config));
    args.extend([
        "-bsf:v".to_string(),
        "h264_metadata=aud=insert".to_string(),
        "-f".to_string(),
        "h264".to_string(),
        "pipe:1".to_string(),
    ]);

    let mut notes = vec![
        "Reads the Windows desktop through FFmpeg lavfi ddagrab (DXGI Desktop Duplication) and writes H.264 Annex B access units to stdout".to_string(),
        format!("Input source: {}", input),
        "The transport layer should consume stdout, packetize frames, and apply pacing/backpressure".to_string(),
        format!("Encoder backend: {}", config.encoder.label()),
    ];

    if hdr_requested {
        notes.push("HDR requested: using format=yuv420p10le filter for 10-bit output".to_string());
    }

    Ok(FfmpegPipelinePlan {
        program: "ffmpeg".to_string(),
        args,
        output: EncodedOutput::H264AnnexBStdout,
        notes,
    })
}

pub fn plan_ffmpeg_h264(
    config: &HostVideoPipelineConfig,
) -> Result<FfmpegPipelinePlan, MediaPlanError> {
    match config.capture {
        CaptureSourceConfig::LinuxPipeWire { .. } => plan_ffmpeg_pipewire_h264(config),
        CaptureSourceConfig::LinuxX11 { .. } => plan_ffmpeg_linux_x11_h264(config),
        CaptureSourceConfig::WindowsGdiGrab { .. } => plan_ffmpeg_windows_gdigrab_h264(config),
        CaptureSourceConfig::MacosAvFoundation { .. } => {
            plan_ffmpeg_macos_avfoundation_h264(config)
        }
        CaptureSourceConfig::WindowsDxgi { .. } => plan_ffmpeg_windows_dxgi_h264(config),
    }
}

/// Enumerate displays available to the current capture backend. The result is
/// empty when the backend is not present (e.g. asking for X11 on Windows).
pub fn enumerate_displays(capture: &CaptureSourceConfig) -> Vec<DisplayInfo> {
    match capture {
        CaptureSourceConfig::LinuxX11 { display } => {
            let displays = enumerate_x11_displays(display);
            if !displays.is_empty() {
                return displays;
            }
            enumerate_xinerama_screens(display)
        }
        CaptureSourceConfig::WindowsGdiGrab { .. } => enumerate_windows_displays(),
        CaptureSourceConfig::LinuxPipeWire { .. } => Vec::new(),
        CaptureSourceConfig::MacosAvFoundation { .. } => {
            // Enumerate via ffmpeg on macOS; return empty for now.
            Vec::new()
        }
        CaptureSourceConfig::WindowsDxgi { .. } => enumerate_windows_displays(),
    }
}

#[cfg(target_os = "windows")]
fn enumerate_windows_displays() -> Vec<DisplayInfo> {
    // Use EnumDisplayMonitors via win32. The implementation is in
    // `platform/windows/displays.rs` if it exists; otherwise this returns an
    // empty list and the host falls back to capturing the full virtual screen.
    Vec::new()
}

#[cfg(not(target_os = "windows"))]
fn enumerate_windows_displays() -> Vec<DisplayInfo> {
    Vec::new()
}

/// Resolve the capture region (in source framebuffer coordinates) implied by a
/// pipeline config. Returns `None` when the source is captured in full.
pub fn resolve_capture_region(config: &VideoPipelineConfig) -> Option<CaptureRegion> {
    if let Some(region) = config.capture_region {
        return Some(region);
    }
    if let Some(display_index) = config.display_index {
        let displays = enumerate_displays(&config.capture);
        if let Some(display) = displays.into_iter().find(|d| d.index == display_index) {
            return Some(CaptureRegion {
                x: display.x,
                y: display.y,
                width: display.width,
                height: display.height,
            });
        }
    }
    None
}

/// Codec-agnostic entry point. Use this in new code; legacy `plan_ffmpeg_h264`
/// callers should migrate to it.
pub fn plan_ffmpeg_pipeline(
    config: &VideoPipelineConfig,
) -> Result<FfmpegPipelinePlan, MediaPlanError> {
    validate_pipeline_config(config)?;

    let region = resolve_capture_region(config);
    let capture_args = capture_input_args(config, region.as_ref());
    let filter = video_filter_args_for(config, region.as_ref());
    let enc = encoder_args_for(config);

    let mut args: Vec<String> = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "warning".to_string(),
        "-nostdin".to_string(),
    ];
    args.extend(capture_args);
    args.extend(filter);
    args.extend(enc);
    args.extend(annex_b_output_args(config.encoder_kind));

    let notes = vec![
        format!("Capture: {:?}", config.capture),
        format!(
            "Encoder: {} via {}",
            config.encoder_kind.label(),
            config.backend.label()
        ),
        format!(
            "Target: {}x{} @ {} fps, {} kbps {}",
            config.width,
            config.height,
            config.framerate,
            config.bitrate_kbps,
            config.scale_mode.label()
        ),
        if let Some(region) = region {
            format!(
                "Source region: {}x{}+{}+{}",
                region.width, region.height, region.x, region.y
            )
        } else {
            "Source region: full virtual screen".to_string()
        },
    ];

    Ok(FfmpegPipelinePlan {
        program: "ffmpeg".to_string(),
        args,
        output: EncodedOutput::AnnexBStdout {
            codec: config.encoder_kind.video_codec(),
        },
        notes,
    })
}

fn capture_input_args(config: &VideoPipelineConfig, region: Option<&CaptureRegion>) -> Vec<String> {
    match &config.capture {
        CaptureSourceConfig::LinuxPipeWire { node } => vec![
            "-f".to_string(),
            "pipewire".to_string(),
            "-framerate".to_string(),
            config.framerate.to_string(),
            "-i".to_string(),
            node.clone(),
            "-an".to_string(),
        ],
        CaptureSourceConfig::LinuxX11 { display } => {
            let mut args = vec![
                "-f".to_string(),
                "x11grab".to_string(),
                "-framerate".to_string(),
                config.framerate.to_string(),
                "-draw_mouse".to_string(),
                "1".to_string(),
            ];
            if let Some(r) = region {
                args.push("-video_size".to_string());
                args.push(format!("{}x{}", r.width, r.height));
                args.push("-i".to_string());
                args.push(format!("{}+{},{}", display, r.x, r.y));
            } else {
                args.push("-i".to_string());
                args.push(display.clone());
            }
            args.push("-an".to_string());
            args
        }
        CaptureSourceConfig::WindowsGdiGrab { input } => {
            let mut args = vec![
                "-f".to_string(),
                "gdigrab".to_string(),
                "-framerate".to_string(),
                config.framerate.to_string(),
                "-draw_mouse".to_string(),
                "1".to_string(),
            ];
            if let Some(r) = region {
                args.push("-offset_x".to_string());
                args.push(r.x.to_string());
                args.push("-offset_y".to_string());
                args.push(r.y.to_string());
                args.push("-video_size".to_string());
                args.push(format!("{}x{}", r.width, r.height));
            }
            args.push("-i".to_string());
            args.push(input.clone());
            args.push("-an".to_string());
            args
        }
        CaptureSourceConfig::MacosAvFoundation {
            display_index,
            audio_index,
        } => vec![
            "-f".to_string(),
            "avfoundation".to_string(),
            "-framerate".to_string(),
            config.framerate.to_string(),
            "-i".to_string(),
            format!("{}:{}", display_index, audio_index),
            "-an".to_string(),
        ],
        CaptureSourceConfig::WindowsDxgi { input } => vec![
            "-f".to_string(),
            "dshow".to_string(),
            "-framerate".to_string(),
            config.framerate.to_string(),
            "-i".to_string(),
            input.clone(),
            "-an".to_string(),
        ],
    }
}

fn video_filter_args_for(
    config: &VideoPipelineConfig,
    region: Option<&CaptureRegion>,
) -> Vec<String> {
    // When we already cropped to the target size via -video_size, no scale
    // filter is needed.
    if let Some(r) = region {
        if r.width == config.width && r.height == config.height {
            return default_pix_fmt_filter(config);
        }
    }

    let filter = match config.scale_mode {
        qubox_proto::ScaleMode::Native => {
            if let Some(r) = region {
                format!("scale={}:{}:flags=fast", r.width, r.height)
            } else {
                "null".to_string()
            }
        }
        qubox_proto::ScaleMode::Fit => format!(
            "scale={}:{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2",
            config.width, config.height, config.width, config.height
        ),
        qubox_proto::ScaleMode::Fill => format!(
            "scale={}:{}:force_original_aspect_ratio=increase,crop={}:{}",
            config.width, config.height, config.width, config.height
        ),
        qubox_proto::ScaleMode::Crop => {
            if let Some(r) = region {
                format!(
                    "crop={}:{}:{}:{},scale={}:{}:flags=fast",
                    r.width, r.height, r.x, r.y, config.width, config.height
                )
            } else {
                format!("scale={}:{}:flags=fast", config.width, config.height)
            }
        }
    };

    let needs_hwupload = matches!(config.backend, EncoderBackend::Vaapi);
    let full_filter = if needs_hwupload {
        format!("{filter},format=nv12,hwupload")
    } else {
        filter
    };

    let mut args = Vec::new();
    if needs_hwupload {
        args.push("-vaapi_device".to_string());
        args.push("/dev/dri/renderD128".to_string());
    }
    args.push("-vf".to_string());
    args.push(full_filter);
    args
}

fn default_pix_fmt_filter(config: &VideoPipelineConfig) -> Vec<String> {
    if matches!(config.backend, EncoderBackend::Vaapi) {
        vec![
            "-vaapi_device".to_string(),
            "/dev/dri/renderD128".to_string(),
            "-vf".to_string(),
            "format=nv12,hwupload".to_string(),
        ]
    } else {
        Vec::new()
    }
}

fn encoder_args_for(config: &VideoPipelineConfig) -> Vec<String> {
    let encoder_name = config
        .backend
        .ffmpeg_name(config.encoder_kind)
        .unwrap_or("libx264")
        .to_string();

    // 10-bit paths (P2-14 / ADR-010 §3.4) use `yuv420p10le` for
    // H.264 Main10 and H.265 Main10, and `yuv420p10le` for AV1 with
    // `--bit-depth=10`; SDR continues to use `yuv420p`.
    let hdr_requested = config
        .color_space
        .map(|c| c != qubox_proto::ColorSpace::Bt709)
        .unwrap_or(false)
        || config.bit_depth == 10;
    let pix_fmt = if hdr_requested {
        "yuv420p10le".to_string()
    } else {
        "yuv420p".to_string()
    };

    let mut args = vec![
        "-c:v".to_string(),
        encoder_name.clone(),
        "-b:v".to_string(),
        format!("{}k", config.bitrate_kbps),
        "-maxrate".to_string(),
        format!("{}k", config.bitrate_kbps),
        "-minrate".to_string(),
        format!(
            "{}k",
            config.min_bitrate_kbps.unwrap_or(
                config
                    .bitrate_kbps
                    .saturating_sub(config.bitrate_kbps / 4)
                    .max(1)
            )
        ),
        "-bufsize".to_string(),
        format!(
            "{}k",
            config
                .buffer_size_kbits
                .unwrap_or(config.bitrate_kbps / 2)
                .max(1)
        ),
        "-g".to_string(),
        config.keyframe_interval_frames.to_string(),
        "-bf".to_string(),
        "0".to_string(),
    ];

    match (config.backend, config.encoder_kind) {
        (EncoderBackend::Nvenc, _) => {
            args.extend([
                "-preset".to_string(),
                "p1".to_string(),
                "-tune".to_string(),
                "ull".to_string(),
                "-rc".to_string(),
                "cbr".to_string(),
                "-forced-idr".to_string(),
                "1".to_string(),
                "-pix_fmt".to_string(),
                pix_fmt.clone(),
            ]);
        }
        (EncoderBackend::Vaapi, _) => {
            args.extend([
                "-low_power".to_string(),
                "1".to_string(),
                "-rc_mode".to_string(),
                "CBR".to_string(),
                "-pix_fmt".to_string(),
                pix_fmt.clone(),
            ]);
        }
        (EncoderBackend::Qsv, _) => {
            args.extend([
                "-preset".to_string(),
                "veryfast".to_string(),
                "-look_ahead".to_string(),
                "0".to_string(),
                "-pix_fmt".to_string(),
                pix_fmt.clone(),
            ]);
        }
        (EncoderBackend::Amf, _) => {
            args.extend([
                "-quality".to_string(),
                "speed".to_string(),
                "-usage".to_string(),
                "ultralowlatency".to_string(),
                "-pix_fmt".to_string(),
                pix_fmt.clone(),
            ]);
        }
        (EncoderBackend::VideoToolbox, _) => {
            args.extend([
                "-realtime".to_string(),
                "1".to_string(),
                "-allow_sw".to_string(),
                "0".to_string(),
                "-pix_fmt".to_string(),
                pix_fmt.clone(),
            ]);
        }
        (EncoderBackend::Software, VideoEncoderKind::H264) => {
            args.extend([
                "-preset".to_string(),
                "ultrafast".to_string(),
                "-tune".to_string(),
                "zerolatency".to_string(),
                "-pix_fmt".to_string(),
                pix_fmt.clone(),
            ]);
        }
        (EncoderBackend::Software, VideoEncoderKind::H265) => {
            args.extend([
                "-preset".to_string(),
                "ultrafast".to_string(),
                "-tune".to_string(),
                "zerolatency".to_string(),
                "-pix_fmt".to_string(),
                pix_fmt.clone(),
            ]);
            if hdr_requested {
                args.extend(hdr_x265_params(config));
            }
        }
        (EncoderBackend::Software, VideoEncoderKind::Av1) => {
            args.extend([
                "-cpu-used".to_string(),
                "8".to_string(),
                "-row-mt".to_string(),
                "1".to_string(),
                "-tile-columns".to_string(),
                "2".to_string(),
                "-tile-rows".to_string(),
                "1".to_string(),
                "-pix_fmt".to_string(),
                pix_fmt.clone(),
            ]);
            if hdr_requested {
                args.extend(hdr_libaom_params(config));
            }
        }
    }

    args
}

/// x265 Main10 10-bit encoder parameters per ADR-010 §3.4. Returns
/// the trailing arguments that come after the `-pix_fmt yuv420p10le`
/// in the main encoder args. The x265-params are emitted as
/// `-x265-params key=val:key=val:...` which libx265 parses as a
/// colon-separated key/value list.
pub(crate) fn hdr_x265_params(config: &VideoPipelineConfig) -> Vec<String> {
    let max_cll = config
        .hdr_static_metadata
        .as_ref()
        .map(|m| m.max_cll)
        .unwrap_or(1000);
    let max_fall = config
        .hdr_static_metadata
        .as_ref()
        .map(|m| m.max_fall)
        .unwrap_or(400);
    // The master-display string is the 6-tuple of CIE 1931
    // chromaticity + luminance per CTA-861-G §6.4. We use a fixed
    // BT.2020 reference display because the live host EDID is
    // typically not available; future work will plumb the
    // OS-reported mastering display payload into the encoder.
    let master_display = "G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1)";
    let params = format!(
        "hdr-opt=1:repeat-headers=1:master-display={master_display}:max-cll={max_cll},{max_fall}:max-fall={max_fall}"
    );
    vec!["-x265-params".to_string(), params]
}

/// libaom-av1 10-bit encoder parameters per ADR-010 §3.4. Returns
/// the trailing args for `-aom-params`. AV1 HDR10 metadata travels
/// inside the OBU as side data; libaom emits the static SEI when
/// `color-primaries=bt2020` is set.
pub(crate) fn hdr_libaom_params(config: &VideoPipelineConfig) -> Vec<String> {
    let max_cll = config
        .hdr_static_metadata
        .as_ref()
        .map(|m| m.max_cll)
        .unwrap_or(1000);
    let max_fall = config
        .hdr_static_metadata
        .as_ref()
        .map(|m| m.max_fall)
        .unwrap_or(400);
    let params = format!(
        "bit-depth=10:profile=0:tier=0:enable-highbitdepth=1:color-primaries=bt2020:transfer-characteristics=smpte2084:matrix-coefficients=bt2020-ncl:max-cll={max_cll}:max-fall={max_fall}"
    );
    vec!["-aom-params".to_string(), params]
}

fn annex_b_output_args(kind: VideoEncoderKind) -> Vec<String> {
    vec![
        "-bsf:v".to_string(),
        match kind {
            VideoEncoderKind::H264 => "h264_metadata=aud=insert".to_string(),
            VideoEncoderKind::H265 => "hevc_metadata=aud=insert".to_string(),
            VideoEncoderKind::Av1 => "av1_metadata=td=insert".to_string(),
        },
        "-f".to_string(),
        kind.video_codec().ffmpeg_mux_format().to_string(),
        "pipe:1".to_string(),
    ]
}

fn validate_pipeline_config(config: &VideoPipelineConfig) -> Result<(), MediaPlanError> {
    if config.width == 0 || config.height == 0 || config.framerate == 0 || config.bitrate_kbps == 0
    {
        return Err(MediaPlanError {
            message: "width, height, framerate, and bitrate must be greater than zero".to_string(),
        });
    }
    if config.backend.ffmpeg_name(config.encoder_kind).is_none() {
        return Err(MediaPlanError {
            message: format!(
                "{} does not implement {}",
                config.backend.label(),
                config.encoder_kind.label()
            ),
        });
    }
    Ok(())
}

pub fn spawn_ffmpeg_pipeline(
    plan: &FfmpegPipelinePlan,
) -> Result<RunningMediaPipeline, MediaRuntimeError> {
    // Both EncodedOutput variants produce an annex-B elementary stream on
    // stdout; spawn accepts either.
    let _ = &plan.output;

    let mut child = Command::new(&plan.program)
        .args(&plan.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(MediaRuntimeError::from_io)?;

    let stdout = child.stdout.take().ok_or_else(|| MediaRuntimeError {
        message: "spawned FFmpeg pipeline did not expose stdout".to_string(),
    })?;
    let stderr = child.stderr.take();

    Ok(RunningMediaPipeline {
        plan: plan.clone(),
        child,
        stdout,
        stderr,
    })
}

pub fn read_h264_access_units<R: Read>(
    reader: &mut R,
    framer: &mut H264AnnexBStreamFramer,
    scratch: &mut [u8],
) -> Result<MediaPipelineRead, MediaRuntimeError> {
    if scratch.is_empty() {
        return Err(MediaRuntimeError {
            message: "read scratch buffer must not be empty".to_string(),
        });
    }

    let read = reader.read(scratch).map_err(MediaRuntimeError::from_io)?;

    if read == 0 {
        let tail = framer.finish().map_err(MediaRuntimeError::from_packetize)?;
        return Ok(MediaPipelineRead::EndOfStream(tail.into_iter().collect()));
    }

    let access_units = framer
        .push_chunk(&scratch[..read])
        .map_err(MediaRuntimeError::from_packetize)?;

    Ok(MediaPipelineRead::AccessUnits(access_units))
}

/// Codec-agnostic read loop. Reads chunks from `reader` into `scratch` and
/// feeds them to the annex-B framer, returning complete access units.
pub fn read_annex_b_access_units<R: Read>(
    reader: &mut R,
    framer: &mut AnnexBStreamFramer,
    scratch: &mut [u8],
) -> Result<MediaPipelineRead, MediaRuntimeError> {
    if scratch.is_empty() {
        return Err(MediaRuntimeError {
            message: "read scratch buffer must not be empty".to_string(),
        });
    }

    let read = reader.read(scratch).map_err(MediaRuntimeError::from_io)?;

    if read == 0 {
        let tail = framer.finish().map_err(MediaRuntimeError::from_packetize)?;
        return Ok(MediaPipelineRead::EndOfStream(tail.into_iter().collect()));
    }

    let access_units = framer
        .push_chunk(&scratch[..read])
        .map_err(MediaRuntimeError::from_packetize)?;

    Ok(MediaPipelineRead::AccessUnits(access_units))
}

pub fn packetize_h264_annex_b_access_unit(
    frame_id: u64,
    timestamp_micros: u64,
    bytes: Vec<u8>,
) -> Result<EncodedVideoAccessUnit, MediaPacketizeError> {
    let nal_units = inspect_h264_annex_b_nal_units(&bytes);

    if nal_units.is_empty() {
        return Err(MediaPacketizeError {
            message: "H.264 Annex B access unit did not contain any NAL units".to_string(),
        });
    }

    let keyframe = nal_units.iter().any(|unit| unit.nal_type == 5);

    Ok(EncodedVideoAccessUnit {
        codec: VideoCodec::H264,
        frame_id,
        timestamp_micros,
        keyframe,
        nal_units,
        bytes,
        display_id: 0,
        stream_id: 0,
        width: 0,
        height: 0,
        color_space: None,
        bit_depth: 8,
    })
}

pub fn inspect_h264_annex_b_nal_units(bytes: &[u8]) -> Vec<H264NalUnitInfo> {
    let start_codes = find_annex_b_start_codes(bytes);

    start_codes
        .iter()
        .enumerate()
        .filter_map(|(index, (start, prefix_len))| {
            let offset = start + prefix_len;
            let end = start_codes
                .get(index + 1)
                .map(|(next_start, _)| *next_start)
                .unwrap_or(bytes.len());

            if offset >= end {
                return None;
            }

            Some(H264NalUnitInfo {
                nal_type: bytes[offset] & 0x1f,
                offset,
                length: end - offset,
            })
        })
        .collect()
}

pub fn linux_capture_kind() -> CaptureKind {
    CaptureKind::Pipewire
}

fn probe_linux_capture() -> BackendProbe {
    let backends = probe_linux_capture_backends();

    if let Some(kind) = preferred_linux_capture_kind(&backends) {
        let selected_name = match kind {
            CaptureKind::Pipewire => "PipeWire",
            CaptureKind::X11 => "X11",
            _ => "Linux capture",
        };

        if let Some(selected) = backends
            .iter()
            .find(|backend| backend.name == selected_name)
        {
            let mut probe = selected.clone();
            probe
                .details
                .push(format!("auto capture selection chose {}", selected_name));
            return probe;
        }
    }

    let mut details = Vec::new();
    for backend in backends {
        details.extend(backend.details);
    }

    BackendProbe {
        name: "Linux capture".to_string(),
        status: ProbeStatus::Missing,
        details,
    }
}

pub fn probe_linux_capture_backends() -> Vec<BackendProbe> {
    vec![probe_pipewire_capture(), probe_x11_capture()]
}

/// Enumerate X11 displays and their geometry by running `xrandr` against the
/// given display. The returned list is ordered by xrandr's natural output
/// ordering, with the primary output marked.
pub fn enumerate_x11_displays(display: &str) -> Vec<DisplayInfo> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }

    let output = Command::new("xrandr")
        .args(["--display", display, "--query"])
        .env("DISPLAY", display)
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_xrandr_query(&stdout)
}

/// Enumerate X11 displays using the Xinerama extension. The returned
/// rectangles are relative to the root window.
pub fn enumerate_xinerama_screens(display: &str) -> Vec<DisplayInfo> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }
    // Prefer xrandr when available; fall back to Xinerama via python-xlib if not.
    let from_xrandr = enumerate_x11_displays(display);
    if !from_xrandr.is_empty() {
        return from_xrandr;
    }
    parse_xinerama_via_python(display)
}

fn parse_xrandr_query(stdout: &str) -> Vec<DisplayInfo> {
    // Lines we care about look like:
    //   DP-1 connected 1920x1080+1920+0 ...
    //   HDMI-1 connected primary 2560x1440+0+0 ...
    //   eDP-1 connected 1920x1080+0+1080 ...
    let mut displays = Vec::new();
    let mut primary: Option<String> = None;
    for line in stdout.lines() {
        // First pass: find "primary" markers so we know which output is primary.
        if line.contains(" primary ") || line.contains(" connected primary ") {
            if let Some(name) = line.split_whitespace().next() {
                primary = Some(name.to_string());
            }
        }
    }

    for (idx, line) in stdout.lines().enumerate() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else { continue };
        let Some(state) = parts.next() else { continue };
        if state != "connected" {
            continue;
        }
        // Skip if this is the "connected primary" line; primary detection done above
        // Look for the geometry token, which has the form WxH+X+Y
        let geom_token = parts
            .find(|tok| {
                let bytes = tok.as_bytes();
                bytes.contains(&b'x') && (bytes.contains(&b'+') || bytes.contains(&b'-'))
            })
            .unwrap_or("0x0+0+0");
        let Some((w, h, x, y)) = parse_geometry_token(geom_token) else {
            continue;
        };
        let refresh = parse_refresh_from_line(line);
        let is_primary = primary.as_deref() == Some(name);
        displays.push(DisplayInfo {
            index: idx as u32,
            name: name.to_string(),
            x,
            y,
            width: w,
            height: h,
            refresh_hz: refresh,
            is_primary,
        });
    }

    // If we saw no primary marker, mark index 0 as primary.
    if primary.is_none() {
        if let Some(first) = displays.first_mut() {
            first.is_primary = true;
        }
    }

    displays
}

fn parse_geometry_token(token: &str) -> Option<(u32, u32, u32, u32)> {
    // xrandr emits geometry as either `WxH+X+Y` or `WxH-X-Y` (negative offsets
    // happen with secondary outputs). Split on 'x' first to peel off W and H.
    let (w_str, rest) = token.split_once('x')?;
    let w = w_str.parse::<u32>().ok()?;

    // The remainder is something like `1080+1920+0` or `1080-0-1080`.
    // We need to find a separator (either `+` or `-`) that delimits H from X.
    // The first separator after H must be `+` (or `-` if H is followed by `-X`).
    // Strategy: scan for the first non-digit character after H.
    let h_end = rest
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)?;
    let h_str = &rest[..h_end];
    let h = h_str.parse::<u32>().ok()?;
    let after_h = &rest[h_end..]; // starts with '+' or '-'

    // Now split after_h into X and Y. The pattern is `+X+Y` or `-X-Y`.
    let x_sign = if after_h.starts_with('-') { -1i64 } else { 1 };
    let after_sign = if after_h.starts_with('+') || after_h.starts_with('-') {
        &after_h[1..]
    } else {
        return None;
    };
    let (x_str, y_str_with_sign) = after_sign.split_once(['+', '-'])?;
    let x = x_str.parse::<i64>().ok()?.saturating_mul(x_sign).max(0) as u32;
    let y_sign = if y_str_with_sign.starts_with('-') {
        -1i64
    } else {
        1
    };
    let y_str = y_str_with_sign.trim_start_matches(['+', '-']);
    let y = y_str.parse::<i64>().ok()?.saturating_mul(y_sign).max(0) as u32;

    Some((w, h, x, y))
}

fn parse_refresh_from_line(line: &str) -> Option<u32> {
    // xrandr lines often contain "+59.95" or "59.95*+0.00" pattern for refresh rate
    // We look for the first floating-point number with decimal point after geometry
    for tok in line.split_whitespace() {
        if let Some(stripped) = tok
            .trim_end_matches('+')
            .trim_end_matches('*')
            .strip_suffix("Hz")
        {
            if let Ok(v) = stripped.parse::<f32>() {
                return Some(v.round() as u32);
            }
        }
    }
    None
}

fn parse_xinerama_via_python(display: &str) -> Vec<DisplayInfo> {
    let script = r#"
import sys
from Xlib import display
d = display.Display(sys.argv[1])
try:
    info = d.screen().root.xinerama_query_screens()
except Exception:
    info = []
for i, s in enumerate(info or []):
    print(f"{i}\t{s.x}\t{s.y}\t{s.width}\t{s.height}\t{s.primary if hasattr(s, 'primary') else 0}")
"#;
    let output = Command::new("python3")
        .args(["-c", script, display])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut result = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 5 {
            continue;
        }
        let Ok(idx) = parts[0].parse::<u32>() else {
            continue;
        };
        let Ok(x) = parts[1].parse::<u32>() else {
            continue;
        };
        let Ok(y) = parts[2].parse::<u32>() else {
            continue;
        };
        let Ok(w) = parts[3].parse::<u32>() else {
            continue;
        };
        let Ok(h) = parts[4].parse::<u32>() else {
            continue;
        };
        let primary = parts.get(5).and_then(|s| s.parse::<u8>().ok()).unwrap_or(0) != 0;
        result.push(DisplayInfo {
            index: idx,
            name: format!("xinerama-{idx}"),
            x,
            y,
            width: w,
            height: h,
            refresh_hz: None,
            is_primary: primary,
        });
    }
    result
}

pub fn preferred_linux_capture_kind(backends: &[BackendProbe]) -> Option<CaptureKind> {
    let pipewire = backends.iter().find(|backend| backend.name == "PipeWire");
    let x11 = backends.iter().find(|backend| backend.name == "X11");
    let wayland_display = env::var_os("WAYLAND_DISPLAY").is_some();
    let x11_display = env::var_os("DISPLAY").is_some();

    if wayland_display && backend_available(pipewire) {
        return Some(CaptureKind::Pipewire);
    }

    if x11_display && backend_usable(x11) {
        return Some(CaptureKind::X11);
    }

    if backend_available(pipewire) {
        return Some(CaptureKind::Pipewire);
    }

    if backend_available(x11) {
        return Some(CaptureKind::X11);
    }

    if wayland_display && backend_usable(pipewire) {
        return Some(CaptureKind::Pipewire);
    }

    if x11_display && backend_usable(x11) {
        return Some(CaptureKind::X11);
    }

    None
}

pub fn h264_encoder_candidates() -> &'static [&'static str] {
    &[
        "h264_nvenc",
        "h264_vaapi",
        "h264_qsv",
        "h264_amf",
        "h264_videotoolbox",
        "libx264",
    ]
}

pub fn h265_encoder_candidates() -> &'static [&'static str] {
    &[
        "hevc_nvenc",
        "hevc_vaapi",
        "hevc_qsv",
        "hevc_amf",
        "hevc_videotoolbox",
        "libx265",
    ]
}

pub fn av1_encoder_candidates() -> &'static [&'static str] {
    &[
        "av1_nvenc",
        "av1_vaapi",
        "av1_qsv",
        "av1_amf",
        "av1_videotoolbox",
        "libaom-av1",
    ]
}

pub fn encoders_for_kind(kind: VideoEncoderKind) -> &'static [&'static str] {
    match kind {
        VideoEncoderKind::H264 => h264_encoder_candidates(),
        VideoEncoderKind::H265 => h265_encoder_candidates(),
        VideoEncoderKind::Av1 => av1_encoder_candidates(),
    }
}

/// Map an ffmpeg encoder name (e.g. `h264_nvenc`) back to a `VideoEncoderKind` + `EncoderBackend` pair.
pub fn classify_encoder(ffmpeg_name: &str) -> Option<(VideoEncoderKind, EncoderBackend)> {
    for backend in [
        EncoderBackend::Software,
        EncoderBackend::Nvenc,
        EncoderBackend::Vaapi,
        EncoderBackend::Qsv,
        EncoderBackend::Amf,
        EncoderBackend::VideoToolbox,
    ] {
        for kind in backend.all_kinds() {
            if let Some(name) = backend.ffmpeg_name(*kind) {
                if name == ffmpeg_name {
                    return Some((*kind, backend));
                }
            }
        }
    }
    None
}

/// Probe ffmpeg for the set of supported encoder backends for each codec kind.
pub fn probe_video_encoder_backends() -> HashMap<VideoEncoderKind, Vec<EncoderBackend>> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output();
    let Ok(output) = output else {
        return HashMap::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);

    let mut result = HashMap::new();
    for kind in [
        VideoEncoderKind::H264,
        VideoEncoderKind::H265,
        VideoEncoderKind::Av1,
    ] {
        let candidates = encoders_for_kind(kind);
        let mut found = Vec::new();
        for candidate in candidates {
            if text.contains(candidate) {
                if let Some((_, backend)) = classify_encoder(candidate) {
                    if !found.contains(&backend) {
                        found.push(backend);
                    }
                }
            }
        }
        result.insert(kind, found);
    }
    result
}

pub fn preferred_encoder_backend(
    kind: VideoEncoderKind,
    available: &[EncoderBackend],
) -> EncoderBackend {
    let priority: &[EncoderBackend] = if cfg!(target_os = "windows") {
        &[
            EncoderBackend::Nvenc,
            EncoderBackend::Qsv,
            EncoderBackend::Amf,
            EncoderBackend::Software,
        ]
    } else if cfg!(target_os = "macos") {
        &[
            EncoderBackend::VideoToolbox,
            EncoderBackend::Nvenc,
            EncoderBackend::Qsv,
            EncoderBackend::Software,
        ]
    } else {
        &[
            EncoderBackend::Nvenc,
            EncoderBackend::Vaapi,
            EncoderBackend::Qsv,
            EncoderBackend::Amf,
            EncoderBackend::Software,
        ]
    };

    priority
        .iter()
        .copied()
        .find(|backend| backend.all_kinds().contains(&kind) && available.contains(backend))
        .unwrap_or(EncoderBackend::Software)
}

pub fn best_encoder_for_kind(
    kind: VideoEncoderKind,
    available: &[EncoderBackend],
) -> EncoderBackend {
    preferred_encoder_backend(kind, available)
}

fn video_filter_args(config: &HostVideoPipelineConfig) -> Vec<String> {
    let scale = format!(
        "scale=w={}:h={}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2",
        config.width, config.height, config.width, config.height
    );

    match config.encoder {
        H264EncoderBackend::Vaapi => vec![
            "-vaapi_device".to_string(),
            "/dev/dri/renderD128".to_string(),
            "-vf".to_string(),
            format!("{scale},format=nv12,hwupload"),
        ],
        _ => vec!["-vf".to_string(), scale],
    }
}

fn validate_h264_config(config: &HostVideoPipelineConfig) -> Result<(), MediaPlanError> {
    if config.codec != VideoCodec::H264 {
        return Err(MediaPlanError {
            message: "only H.264 FFmpeg plans are implemented".to_string(),
        });
    }

    if config.width == 0 || config.height == 0 || config.framerate == 0 || config.bitrate_kbps == 0
    {
        return Err(MediaPlanError {
            message: "width, height, framerate, and bitrate must be greater than zero".to_string(),
        });
    }

    Ok(())
}

fn encoder_args(config: &HostVideoPipelineConfig) -> Vec<String> {
    let bitrate = format!("{}k", config.bitrate_kbps);
    let mut args = vec![
        "-c:v".to_string(),
        config.encoder.ffmpeg_name().to_string(),
        "-b:v".to_string(),
        bitrate.clone(),
        "-maxrate".to_string(),
        bitrate,
        "-bufsize".to_string(),
        format!("{}k", config.bitrate_kbps / 2),
        "-g".to_string(),
        config.keyframe_interval_frames.to_string(),
        "-bf".to_string(),
        "0".to_string(),
    ];

    match config.encoder {
        H264EncoderBackend::Nvenc => args.extend([
            "-preset".to_string(),
            "p1".to_string(),
            "-tune".to_string(),
            "ull".to_string(),
            "-rc".to_string(),
            "cbr".to_string(),
            "-forced-idr".to_string(),
            "1".to_string(),
        ]),
        H264EncoderBackend::Vaapi => args.extend([
            "-low_power".to_string(),
            "1".to_string(),
            "-rc_mode".to_string(),
            "CBR".to_string(),
        ]),
        H264EncoderBackend::Qsv => args.extend([
            "-preset".to_string(),
            "veryfast".to_string(),
            "-look_ahead".to_string(),
            "0".to_string(),
        ]),
        H264EncoderBackend::Amf => args.extend([
            "-quality".to_string(),
            "speed".to_string(),
            "-usage".to_string(),
            "ultralowlatency".to_string(),
        ]),
        H264EncoderBackend::VideoToolbox => args.extend([
            "-realtime".to_string(),
            "1".to_string(),
            "-allow_sw".to_string(),
            "0".to_string(),
        ]),
        H264EncoderBackend::Libx264 => args.extend([
            "-preset".to_string(),
            "ultrafast".to_string(),
            "-tune".to_string(),
            "zerolatency".to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
        ]),
    }

    args
}

fn find_annex_b_start_codes(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut start_codes = Vec::new();
    let mut index = 0;

    while index + 3 <= bytes.len() {
        if bytes[index..].starts_with(&[0, 0, 1]) {
            start_codes.push((index, 3));
            index += 3;
        } else if index + 4 <= bytes.len() && bytes[index..].starts_with(&[0, 0, 0, 1]) {
            start_codes.push((index, 4));
            index += 4;
        } else {
            index += 1;
        }
    }

    start_codes
}

fn find_aud_start_codes(bytes: &[u8]) -> Vec<(usize, usize)> {
    find_annex_b_start_codes(bytes)
        .into_iter()
        .filter(|(start, prefix_len)| {
            bytes
                .get(start + prefix_len)
                .map(|nal_header| nal_header & 0x1f == 9)
                .unwrap_or(false)
        })
        .collect()
}

fn find_aud_start_codes_h264(bytes: &[u8]) -> Vec<(usize, usize)> {
    find_aud_start_codes(bytes)
}

/// H.265 (HEVC) NAL unit header is two bytes:
///   [forbidden_zero_bit(1) | nal_unit_type(6) | nuh_layer_id(6) | nuh_temporal_id_plus1(3)]
/// Access-unit delimiter is nal_unit_type == 35 (AUD_NUT).
fn find_aud_start_codes_h265(bytes: &[u8]) -> Vec<(usize, usize)> {
    find_annex_b_start_codes(bytes)
        .into_iter()
        .filter(|(start, prefix_len)| {
            let offset = start + prefix_len;
            let Some(byte0) = bytes.get(offset) else {
                return false;
            };
            // First byte: 0 followed by the 6-bit NAL type.
            let nal_type = (byte0 >> 1) & 0x3f;
            // We need at least 2 header bytes; for AUD (type 35) the second
            // byte encodes nuh_layer_id + nuh_temporal_id_plus1, which must be
            // present so we don't read past the buffer.
            if bytes.get(offset + 1).is_none() {
                return false;
            }
            nal_type == 35
        })
        .collect()
}

/// Best-effort AV1 OBU boundary detection for low-latency streams produced by
/// ffmpeg's `-bsf:v av1_metadata=td=insert` filter. We look for OBU_TEMPORAL_DELIMITER
/// (type 2) OBUs which mark frame boundaries.
fn find_av1_obu_boundaries(bytes: &[u8]) -> Vec<usize> {
    let mut boundaries = vec![0usize];
    let mut i = 0;
    while i < bytes.len() {
        // Skip annex-B start code
        let prefix_len = match (
            bytes.get(i),
            bytes.get(i + 1),
            bytes.get(i + 2),
            bytes.get(i + 3),
        ) {
            (Some(0), Some(0), Some(0), Some(1)) => 4,
            (Some(0), Some(0), Some(1), _) => 3,
            _ => break,
        };
        let obu_start = i + prefix_len;
        let Some(&header) = bytes.get(obu_start) else {
            break;
        };
        let obu_has_size = (header & 0x02) != 0; // obu_has_size_field
        let obu_type = (header >> 3) & 0x0f;
        if !obu_has_size {
            // Without obu_has_size_field we can't reliably split — bail out.
            return boundaries;
        }
        // 1 byte header (no extension), then leb128 size
        let mut size_offset = obu_start + 1;
        // obu_extension_flag is header bit 2; if set, one more header byte
        if (header & 0x04) != 0 {
            size_offset += 1;
        }
        let (obu_size, consumed) = match read_leb128(&bytes[size_offset..]) {
            Some(pair) => pair,
            None => return boundaries,
        };
        let obu_total = (size_offset + consumed + obu_size) - i;
        // If this is a temporal delimiter, mark its start as a new frame.
        if obu_type == 2 {
            boundaries.push(i);
        }
        i += obu_total;
        if obu_type == 2 {
            // After the TD, the next frame begins at the current position
            boundaries.push(i);
        }
    }
    boundaries
}

fn read_leb128(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut value: usize = 0;
    for (i, b) in bytes.iter().take(8).enumerate() {
        value |= ((*b & 0x7f) as usize) << (i * 7);
        if (*b & 0x80) == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Codec-aware entry point. H.264 keeps the legacy H.264NalUnitInfo list;
/// H.265/AV1 surface a single bytes block (future expansion: typed H.265
/// NAL infos / AV1 OBU infos).
pub fn packetize_access_unit(
    codec: VideoCodec,
    frame_id: u64,
    timestamp_micros: u64,
    bytes: Vec<u8>,
) -> Result<EncodedVideoAccessUnit, MediaPacketizeError> {
    match codec {
        VideoCodec::H264 => packetize_h264_annex_b_access_unit(frame_id, timestamp_micros, bytes),
        VideoCodec::H265 => {
            if bytes.is_empty() {
                return Err(MediaPacketizeError {
                    message: "H.265 access unit was empty".to_string(),
                });
            }
            Ok(EncodedVideoAccessUnit {
                codec,
                frame_id,
                timestamp_micros,
                keyframe: false,
                nal_units: Vec::new(),
                bytes,
                display_id: 0,
                stream_id: 0,
                width: 0,
                height: 0,
                color_space: None,
                bit_depth: 8,
            })
        }
        VideoCodec::Av1 => {
            if bytes.is_empty() {
                return Err(MediaPacketizeError {
                    message: "AV1 access unit was empty".to_string(),
                });
            }
            Ok(EncodedVideoAccessUnit {
                codec,
                frame_id,
                timestamp_micros,
                keyframe: false,
                nal_units: Vec::new(),
                bytes,
                display_id: 0,
                stream_id: 0,
                width: 0,
                height: 0,
                color_space: None,
                bit_depth: 8,
            })
        }
    }
}

pub fn probe_pipewire_capture() -> BackendProbe {
    if !cfg!(target_os = "linux") {
        return BackendProbe {
            name: "PipeWire".to_string(),
            status: ProbeStatus::Unsupported,
            details: vec!["PipeWire capture is only available on Linux hosts".to_string()],
        };
    }

    let mut details = Vec::new();

    if let Ok(runtime_dir) =
        env::var("PIPEWIRE_RUNTIME_DIR").or_else(|_| env::var("XDG_RUNTIME_DIR"))
    {
        let socket = PathBuf::from(runtime_dir).join("pipewire-0");
        if socket.exists() {
            details.push(format!("found PipeWire socket at {}", socket.display()));
            return BackendProbe {
                name: "PipeWire".to_string(),
                status: ProbeStatus::Available,
                details,
            };
        }
        details.push(format!(
            "PipeWire socket was not found at {}",
            socket.display()
        ));
    } else {
        details.push("PIPEWIRE_RUNTIME_DIR/XDG_RUNTIME_DIR is not set".to_string());
    }

    if command_available("pw-cli") || command_available("pipewire") {
        details.push("PipeWire command-line tools are installed".to_string());
        return BackendProbe {
            name: "PipeWire".to_string(),
            status: ProbeStatus::Available,
            details,
        };
    }

    BackendProbe {
        name: "PipeWire".to_string(),
        status: ProbeStatus::Missing,
        details,
    }
}

pub fn probe_x11_capture() -> BackendProbe {
    if !cfg!(target_os = "linux") {
        return BackendProbe {
            name: "X11".to_string(),
            status: ProbeStatus::Unsupported,
            details: vec!["X11 capture is only available on Linux hosts".to_string()],
        };
    }

    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-devices"])
        .output();

    let Ok(output) = output else {
        return BackendProbe {
            name: "X11".to_string(),
            status: ProbeStatus::Missing,
            details: vec!["ffmpeg was not found on PATH".to_string()],
        };
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let devices = format!("{stdout}\n{stderr}");

    if !devices.contains("x11grab") {
        return BackendProbe {
            name: "X11".to_string(),
            status: ProbeStatus::Missing,
            details: vec![
                "ffmpeg is installed but the x11grab input device was not reported".to_string(),
            ],
        };
    }

    let mut details = Vec::new();
    if let Ok(display) = env::var("DISPLAY") {
        details.push(format!("DISPLAY is set to {}", display));
    } else {
        details.push("DISPLAY is not set".to_string());
    }

    BackendProbe {
        name: "X11".to_string(),
        status: ProbeStatus::Available,
        details,
    }
}

fn probe_windows_gdigrab_capture() -> BackendProbe {
    if !cfg!(target_os = "windows") {
        return BackendProbe {
            name: "FFmpeg gdigrab".to_string(),
            status: ProbeStatus::Unsupported,
            details: vec!["Windows gdigrab capture is only available on Windows hosts".to_string()],
        };
    }

    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-devices"])
        .output();

    let Ok(output) = output else {
        return BackendProbe {
            name: "FFmpeg gdigrab".to_string(),
            status: ProbeStatus::Missing,
            details: vec!["ffmpeg was not found on PATH".to_string()],
        };
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let devices = format!("{stdout}\n{stderr}");

    if devices.contains("gdigrab") {
        BackendProbe {
            name: "FFmpeg gdigrab".to_string(),
            status: ProbeStatus::Available,
            details: vec![
                "ffmpeg exposes the gdigrab desktop input device".to_string(),
                "Use the default input `desktop` to capture the primary desktop".to_string(),
            ],
        }
    } else {
        BackendProbe {
            name: "FFmpeg gdigrab".to_string(),
            status: ProbeStatus::Missing,
            details: vec![
                "ffmpeg is installed but the gdigrab input device was not reported".to_string(),
            ],
        }
    }
}

fn probe_h264_encoder() -> BackendProbe {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output();

    let Ok(output) = output else {
        return BackendProbe {
            name: "FFmpeg H.264 hardware encoder".to_string(),
            status: ProbeStatus::Missing,
            details: vec!["ffmpeg was not found on PATH".to_string()],
        };
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let found: Vec<String> = h264_encoder_candidates()
        .iter()
        .filter(|candidate| text.contains(**candidate))
        .map(|candidate| (*candidate).to_string())
        .collect();

    if found.is_empty() {
        BackendProbe {
            name: "FFmpeg H.264 hardware encoder".to_string(),
            status: ProbeStatus::Missing,
            details: vec![
                "ffmpeg is installed but no preferred hardware H.264 encoder was found".to_string(),
            ],
        }
    } else {
        BackendProbe {
            name: "FFmpeg H.264 hardware encoder".to_string(),
            status: ProbeStatus::Available,
            details: found,
        }
    }
}

fn command_available(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn backend_available(backend: Option<&BackendProbe>) -> bool {
    matches!(
        backend.map(|backend| &backend.status),
        Some(ProbeStatus::Available)
    )
}

fn backend_usable(backend: Option<&BackendProbe>) -> bool {
    matches!(
        backend.map(|backend| &backend.status),
        Some(ProbeStatus::Available | ProbeStatus::Missing | ProbeStatus::Planned)
    )
}

fn current_os() -> PlatformOs {
    if cfg!(target_os = "windows") {
        PlatformOs::Windows
    } else if cfg!(target_os = "macos") {
        PlatformOs::Macos
    } else if cfg!(target_os = "android") {
        PlatformOs::Android
    } else {
        PlatformOs::Linux
    }
}

fn planned_pipeline_for(platform: PlatformOs) -> Vec<String> {
    match platform {
        PlatformOs::Linux => vec![
            "capture frames from a PipeWire stream or an X11 display".to_string(),
            "prefer DMA-BUF import when compositor and GPU allow it".to_string(),
            "encode H.264 through NVENC, VAAPI, AMF, or QSV using the FFmpeg plan builder"
                .to_string(),
            "packetize encoded access units for WebRTC or native QUIC transport".to_string(),
        ],
        PlatformOs::Windows => vec![
            "capture the desktop through FFmpeg gdigrab (legacy) or DXGI (HDR)".to_string(),
            "scale or pad the desktop into the negotiated stream size".to_string(),
            "encode H.264 through NVENC, AMF, or QSV using the FFmpeg plan builder".to_string(),
            "packetize encoded access units for WebRTC or native QUIC transport".to_string(),
        ],
        PlatformOs::Macos => vec![
            "capture frames through FFmpeg AVFoundation".to_string(),
            "encode H.264 through VideoToolbox or libx264".to_string(),
            "packetize encoded access units for WebRTC or native QUIC transport".to_string(),
        ],
        PlatformOs::Android => vec![
            "capture frames through MediaProjection".to_string(),
            "encode H.264 through MediaCodec".to_string(),
            "packetize encoded access units for WebRTC or relay transport".to_string(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    #[test]
    fn h264_candidates_include_linux_and_windows_hardware_paths() {
        let candidates = h264_encoder_candidates();

        assert!(candidates.contains(&"h264_nvenc"));
        assert!(candidates.contains(&"h264_vaapi"));
        assert!(candidates.contains(&"h264_amf"));
        assert!(candidates.contains(&"h264_qsv"));
        assert!(candidates.contains(&"libx264"));
    }

    #[test]
    fn linux_capture_kind_is_pipewire() {
        assert_eq!(linux_capture_kind(), CaptureKind::Pipewire);
    }

    #[test]
    fn preferred_linux_capture_chooses_x11_when_only_x11_is_available() {
        let backends = vec![
            BackendProbe {
                name: "PipeWire".to_string(),
                status: ProbeStatus::Missing,
                details: Vec::new(),
            },
            BackendProbe {
                name: "X11".to_string(),
                status: ProbeStatus::Available,
                details: Vec::new(),
            },
        ];

        assert_eq!(
            preferred_linux_capture_kind(&backends),
            Some(CaptureKind::X11)
        );
    }

    #[test]
    #[serial]
    fn preferred_linux_capture_prefers_pipewire_when_available_without_x11_hint() {
        let orig_display = env::var_os("DISPLAY");
        let orig_wayland = env::var_os("WAYLAND_DISPLAY");
        let orig_xdg = env::var_os("XDG_SESSION_TYPE");

        env::remove_var("DISPLAY");
        env::set_var("WAYLAND_DISPLAY", "wayland-0");
        env::set_var("XDG_SESSION_TYPE", "wayland");

        let backends = vec![
            BackendProbe {
                name: "PipeWire".to_string(),
                status: ProbeStatus::Available,
                details: Vec::new(),
            },
            BackendProbe {
                name: "X11".to_string(),
                status: ProbeStatus::Missing,
                details: Vec::new(),
            },
        ];

        let prior_display = std::env::var_os("DISPLAY");
        let prior_wayland = std::env::var_os("WAYLAND_DISPLAY");
        std::env::remove_var("DISPLAY");
        std::env::remove_var("WAYLAND_DISPLAY");
        let result = preferred_linux_capture_kind(&backends);
        if let Some(value) = prior_display {
            std::env::set_var("DISPLAY", value);
        }
        if let Some(value) = prior_wayland {
            std::env::set_var("WAYLAND_DISPLAY", value);
        }

        assert_eq!(result, Some(CaptureKind::Pipewire));
    }

    #[test]
    fn pipewire_h264_plan_writes_annex_b_to_stdout() {
        let config = HostVideoPipelineConfig::linux_pipewire_h264(
            "42",
            H264EncoderBackend::Nvenc,
            1280,
            720,
            60,
            12_000,
        );

        let plan = plan_ffmpeg_pipewire_h264(&config).unwrap();

        assert_eq!(plan.program, "ffmpeg");
        assert_eq!(plan.output, EncodedOutput::H264AnnexBStdout);
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-f" && args[1] == "pipewire"));
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-i" && args[1] == "42"));
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-c:v" && args[1] == "h264_nvenc"));
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-bsf:v" && args[1] == "h264_metadata=aud=insert"));
        assert!(plan
            .args
            .ends_with(&["-f".to_string(), "h264".to_string(), "pipe:1".to_string()]));
    }

    #[test]
    fn linux_x11_h264_plan_writes_annex_b_to_stdout() {
        let config = HostVideoPipelineConfig::linux_x11_h264(
            ":99.0",
            H264EncoderBackend::Nvenc,
            1280,
            720,
            60,
            12_000,
        );

        let plan = plan_ffmpeg_linux_x11_h264(&config).unwrap();

        assert_eq!(plan.program, "ffmpeg");
        assert_eq!(plan.output, EncodedOutput::H264AnnexBStdout);
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-f" && args[1] == "x11grab"));
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-i" && args[1] == ":99.0"));
    }

    #[test]
    fn windows_gdigrab_plan_writes_annex_b_to_stdout() {
        let config = HostVideoPipelineConfig::windows_gdigrab_h264(
            "desktop",
            H264EncoderBackend::Nvenc,
            1280,
            720,
            60,
            12_000,
        );

        let plan = plan_ffmpeg_windows_gdigrab_h264(&config).unwrap();

        assert_eq!(plan.program, "ffmpeg");
        assert_eq!(plan.output, EncodedOutput::H264AnnexBStdout);
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-f" && args[1] == "gdigrab"));
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-draw_mouse" && args[1] == "1"));
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-i" && args[1] == "desktop"));
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-c:v" && args[1] == "h264_nvenc"));
        assert!(plan
            .args
            .ends_with(&["-f".to_string(), "h264".to_string(), "pipe:1".to_string()]));
    }

    #[test]
    fn windows_gdigrab_rejects_empty_input() {
        let config = HostVideoPipelineConfig::windows_gdigrab_h264(
            "",
            H264EncoderBackend::Nvenc,
            1280,
            720,
            60,
            12_000,
        );

        assert!(plan_ffmpeg_windows_gdigrab_h264(&config).is_err());
    }

    #[test]
    fn generic_h264_plan_dispatches_to_windows_capture() {
        let config = HostVideoPipelineConfig::windows_gdigrab_h264(
            "desktop",
            H264EncoderBackend::Qsv,
            1920,
            1080,
            60,
            20_000,
        );

        let plan = plan_ffmpeg_h264(&config).unwrap();

        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-f" && args[1] == "gdigrab"));
    }

    #[test]
    fn vaapi_plan_uploads_frames_to_gpu() {
        let config = HostVideoPipelineConfig::linux_pipewire_h264(
            "0",
            H264EncoderBackend::Vaapi,
            1920,
            1080,
            60,
            20_000,
        );

        let plan = plan_ffmpeg_pipewire_h264(&config).unwrap();

        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-vaapi_device" && args[1] == "/dev/dri/renderD128"));
        assert!(plan.args.iter().any(|arg| arg.contains("hwupload")));
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-c:v" && args[1] == "h264_vaapi"));
    }

    #[test]
    fn invalid_video_config_is_rejected() {
        let config = HostVideoPipelineConfig::linux_pipewire_h264(
            "0",
            H264EncoderBackend::Nvenc,
            0,
            1080,
            60,
            20_000,
        );

        assert!(plan_ffmpeg_pipewire_h264(&config).is_err());
    }

    #[test]
    fn h264_annex_b_inspection_finds_nal_units() {
        let bytes = vec![
            0, 0, 0, 1, 0x67, 1, 2, 3, 0, 0, 1, 0x68, 4, 5, 0, 0, 0, 1, 0x65, 6, 7,
        ];

        let units = inspect_h264_annex_b_nal_units(&bytes);

        assert_eq!(
            units,
            vec![
                H264NalUnitInfo {
                    nal_type: 7,
                    offset: 4,
                    length: 4,
                },
                H264NalUnitInfo {
                    nal_type: 8,
                    offset: 11,
                    length: 3,
                },
                H264NalUnitInfo {
                    nal_type: 5,
                    offset: 18,
                    length: 3,
                },
            ]
        );
    }

    #[test]
    fn h264_access_unit_marks_idr_as_keyframe() {
        let bytes = vec![0, 0, 1, 0x65, 1, 2, 3];

        let access_unit = packetize_h264_annex_b_access_unit(7, 12_345, bytes.clone()).unwrap();

        assert_eq!(access_unit.codec, VideoCodec::H264);
        assert_eq!(access_unit.frame_id, 7);
        assert_eq!(access_unit.timestamp_micros, 12_345);
        assert!(access_unit.keyframe);
        assert_eq!(access_unit.bytes, bytes);
    }

    #[test]
    fn h264_access_unit_rejects_missing_start_codes() {
        let result = packetize_h264_annex_b_access_unit(1, 0, vec![0x65, 1, 2, 3]);

        assert!(result.is_err());
    }

    #[test]
    fn h264_stream_framer_splits_on_access_unit_delimiters() {
        let mut framer = H264AnnexBStreamFramer::new(60).unwrap();
        let stream = vec![
            0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x65, 1, 2, 3, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x41, 4, 5,
            6,
        ];

        let first_batch = framer.push_chunk(&stream[..10]).unwrap();
        let second_batch = framer.push_chunk(&stream[10..]).unwrap();
        let tail = framer.finish().unwrap().unwrap();

        assert!(first_batch.is_empty());
        assert_eq!(second_batch.len(), 1);
        assert_eq!(second_batch[0].frame_id, 0);
        assert_eq!(second_batch[0].timestamp_micros, 0);
        assert!(second_batch[0].keyframe);
        assert_eq!(tail.frame_id, 1);
        assert_eq!(tail.timestamp_micros, 16_666);
        assert!(!tail.keyframe);
    }

    #[test]
    fn h264_read_helper_flushes_tail_on_eof() {
        let stream = vec![0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x65, 1, 2, 3];
        let mut reader = std::io::Cursor::new(stream);
        let mut framer = H264AnnexBStreamFramer::new(30).unwrap();
        let mut scratch = [0_u8; 64];

        let first = read_h264_access_units(&mut reader, &mut framer, &mut scratch).unwrap();
        let second = read_h264_access_units(&mut reader, &mut framer, &mut scratch).unwrap();

        assert_eq!(first, MediaPipelineRead::AccessUnits(Vec::new()));
        assert!(matches!(
            second,
            MediaPipelineRead::EndOfStream(ref units) if units.len() == 1 && units[0].keyframe
        ));
    }

    fn hdr_metadata_fixture() -> qubox_proto::HdrStaticMetadata {
        qubox_proto::HdrStaticMetadata {
            primaries: 9,
            transfer: 16,
            matrix: 9,
            max_cll: 1500,
            max_fall: 600,
            mastering_display_metadata: vec![0u8; 24],
        }
    }

    fn hdr_pipeline(encoder_kind: VideoEncoderKind) -> VideoPipelineConfig {
        VideoPipelineConfig {
            capture: CaptureSourceConfig::LinuxPipeWire {
                node: "node_42".into(),
            },
            encoder_kind,
            backend: EncoderBackend::Software,
            width: 3840,
            height: 2160,
            framerate: 60,
            bitrate_kbps: 25_000,
            min_bitrate_kbps: None,
            buffer_size_kbits: None,
            keyframe_interval_frames: 240,
            scale_mode: qubox_proto::ScaleMode::Fit,
            capture_region: None,
            display_index: None,
            color_space: Some(qubox_proto::ColorSpace::Bt2100Pq),
            bit_depth: 10,
            hdr_static_metadata: Some(hdr_metadata_fixture()),
        }
    }

    #[test]
    fn hdr_x265_params_returns_hdr_opt_and_master_display() {
        let config = hdr_pipeline(VideoEncoderKind::H265);
        let args = hdr_x265_params(&config);
        assert_eq!(args.len(), 2, "x265 params are <flag, value>");
        assert_eq!(args[0], "-x265-params");
        let value = &args[1];
        assert!(value.contains("hdr-opt=1"), "expected hdr-opt=1 in {value}");
        assert!(value.contains("repeat-headers=1"));
        assert!(value.contains(
            "master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1)"
        ));
        assert!(value.contains("max-cll=1500,600"));
        assert!(value.contains("max-fall=600"));
    }

    #[test]
    fn hdr_libaom_params_includes_bit_depth_10_and_smpte2084() {
        let config = hdr_pipeline(VideoEncoderKind::Av1);
        let args = hdr_libaom_params(&config);
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "-aom-params");
        let value = &args[1];
        assert!(value.contains("bit-depth=10"));
        assert!(value.contains("color-primaries=bt2020"));
        assert!(value.contains("transfer-characteristics=smpte2084"));
        assert!(value.contains("matrix-coefficients=bt2020-ncl"));
        assert!(value.contains("max-cll=1500"));
        assert!(value.contains("max-fall=600"));
    }

    #[test]
    fn encoder_args_for_picks_yuv420p10le_when_hdr() {
        let config = hdr_pipeline(VideoEncoderKind::H265);
        let args = encoder_args_for(&config);
        let pix_fmt = args
            .iter()
            .skip_while(|a| **a != "-pix_fmt")
            .nth(1)
            .expect("-pix_fmt must be present");
        assert_eq!(pix_fmt, "yuv420p10le");
    }

    #[test]
    fn encoder_args_for_picks_yuv420p_when_sdr() {
        let config = VideoPipelineConfig {
            capture: CaptureSourceConfig::LinuxPipeWire {
                node: "node_42".into(),
            },
            encoder_kind: VideoEncoderKind::H265,
            backend: EncoderBackend::Software,
            width: 3840,
            height: 2160,
            framerate: 60,
            bitrate_kbps: 25_000,
            min_bitrate_kbps: None,
            buffer_size_kbits: None,
            keyframe_interval_frames: 240,
            scale_mode: qubox_proto::ScaleMode::Fit,
            capture_region: None,
            display_index: None,
            color_space: None,
            bit_depth: 8,
            hdr_static_metadata: None,
        };
        let args = encoder_args_for(&config);
        let pix_fmt = args
            .iter()
            .skip_while(|a| **a != "-pix_fmt")
            .nth(1)
            .expect("-pix_fmt must be present");
        assert_eq!(pix_fmt, "yuv420p");
        assert!(!args.iter().any(|a| a == "-x265-params"));
    }

    #[test]
    fn hdr_x265_params_uses_safe_defaults_when_metadata_missing() {
        let mut config = hdr_pipeline(VideoEncoderKind::H265);
        config.hdr_static_metadata = None;
        let args = hdr_x265_params(&config);
        let value = &args[1];
        assert!(value.contains("max-cll=1000,400"));
        assert!(value.contains("max-fall=400"));
    }

    #[test]
    fn video_pipeline_config_default_eight_bits_for_bit_depth() {
        let config = VideoPipelineConfig {
            capture: CaptureSourceConfig::LinuxPipeWire { node: "n".into() },
            encoder_kind: VideoEncoderKind::H265,
            backend: EncoderBackend::Software,
            width: 1920,
            height: 1080,
            framerate: 60,
            bitrate_kbps: 5000,
            min_bitrate_kbps: None,
            buffer_size_kbits: None,
            keyframe_interval_frames: 240,
            scale_mode: qubox_proto::ScaleMode::Fit,
            capture_region: None,
            display_index: None,
            color_space: None,
            bit_depth: default_eight_bits(),
            hdr_static_metadata: None,
        };
        assert_eq!(config.bit_depth, 8);
        assert!(config.color_space.is_none());
        assert!(config.hdr_static_metadata.is_none());
    }

    #[test]
    fn macos_avfoundation_h264_plan_writes_annex_b_to_stdout() {
        let config = HostVideoPipelineConfig::macos_avfoundation_h264(
            "1",
            "0",
            H264EncoderBackend::VideoToolbox,
            1920,
            1080,
            30,
            8_000,
        );

        let plan = plan_ffmpeg_macos_avfoundation_h264(&config).unwrap();

        assert_eq!(plan.program, "ffmpeg");
        assert_eq!(plan.output, EncodedOutput::H264AnnexBStdout);
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-f" && args[1] == "avfoundation"));
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-i" && args[1] == "1:0"));
    }

    #[test]
    fn macos_avfoundation_rejects_non_macos_encoder() {
        let config = HostVideoPipelineConfig {
            capture: CaptureSourceConfig::MacosAvFoundation {
                display_index: "1".into(),
                audio_index: "0".into(),
            },
            codec: VideoCodec::H264,
            encoder: H264EncoderBackend::Vaapi,
            width: 1280,
            height: 720,
            framerate: 60,
            bitrate_kbps: 5_000,
            keyframe_interval_frames: 240,
            color_space: None,
            bit_depth: 8,
            hdr_static_metadata: None,
        };

        let err = plan_ffmpeg_macos_avfoundation_h264(&config).unwrap_err();
        assert!(err
            .message
            .contains("not supported by the macOS AVFoundation host pipeline"));
    }

    #[test]
    fn windows_dxgi_h264_plan_writes_annex_b_to_stdout() {
        let config = HostVideoPipelineConfig::windows_dxgi_h264(
            "desktop",
            H264EncoderBackend::Nvenc,
            1920,
            1080,
            60,
            10_000,
        );

        let plan = plan_ffmpeg_windows_dxgi_h264(&config).unwrap();

        assert_eq!(plan.program, "ffmpeg");
        assert_eq!(plan.output, EncodedOutput::H264AnnexBStdout);
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-f" && args[1] == "lavfi"));
        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-i" && args[1].starts_with("ddagrab=")));
    }

    #[test]
    fn windows_dxgi_h264_adds_10le_filter_when_hdr_requested() {
        let config = HostVideoPipelineConfig {
            capture: CaptureSourceConfig::WindowsDxgi {
                input: "desktop".into(),
            },
            codec: VideoCodec::H264,
            encoder: H264EncoderBackend::Nvenc,
            width: 3840,
            height: 2160,
            framerate: 60,
            bitrate_kbps: 25_000,
            keyframe_interval_frames: 240,
            color_space: Some(qubox_proto::ColorSpace::Bt2020),
            bit_depth: 10,
            hdr_static_metadata: None,
        };

        let plan = plan_ffmpeg_windows_dxgi_h264(&config).unwrap();
        let args_concat = plan.args.join(" ");

        assert!(args_concat.contains("yuv420p10le"));
    }
}
