//! FFmpeg-backed raw BGRA capture sessions for platforms where native
//! capture is mid-port or delegated to FFmpeg demuxers (PipeWire, gdigrab).
//!
//! Produces the same [`CapturedFrame`] layout as X11/soft sessions so the
//! encode/QUIC path stays uniform.

use std::io::{BufReader, Read};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::CaptureError;
use crate::traits::CaptureSession;
use crate::types::{
    CapturedFrame, ColorSpaceId, DisplayId, PixelFormat, Point, Rect, Size,
};

/// Capture source for an FFmpeg rawvideo pipe.
#[derive(Debug, Clone)]
pub enum FfmpegRawSource {
    /// Linux X11: `-f x11grab -i DISPLAY+X,Y`
    X11 {
        display: String,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        fps: u32,
    },
    /// Linux PipeWire: `-f pipewire -i NODE`
    PipeWire {
        node: String,
        width: u32,
        height: u32,
        fps: u32,
    },
    /// Windows gdigrab: `-f gdigrab -i desktop`
    GdiGrab {
        input: String,
        width: u32,
        height: u32,
        fps: u32,
    },
    /// Windows Desktop Duplication via lavfi `ddagrab` (FFmpeg ≥ 6).
    DdaGrab {
        output_idx: u32,
        width: u32,
        height: u32,
        fps: u32,
    },
}

impl FfmpegRawSource {
    pub fn width(&self) -> u32 {
        match self {
            Self::X11 { width, .. }
            | Self::PipeWire { width, .. }
            | Self::GdiGrab { width, .. }
            | Self::DdaGrab { width, .. } => *width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Self::X11 { height, .. }
            | Self::PipeWire { height, .. }
            | Self::GdiGrab { height, .. }
            | Self::DdaGrab { height, .. } => *height,
        }
    }

    pub fn fps(&self) -> u32 {
        match self {
            Self::X11 { fps, .. }
            | Self::PipeWire { fps, .. }
            | Self::GdiGrab { fps, .. }
            | Self::DdaGrab { fps, .. } => *fps,
        }
    }

    /// Build argv for `ffmpeg … -f rawvideo -pix_fmt bgra pipe:1`.
    pub fn ffmpeg_args(&self) -> Vec<String> {
        let (w, h, fps) = (self.width(), self.height(), self.fps().max(1));
        let mut args = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-nostdin".into(),
        ];
        match self {
            Self::X11 {
                display, x, y, ..
            } => {
                args.extend([
                    "-f".into(),
                    "x11grab".into(),
                    "-framerate".into(),
                    fps.to_string(),
                    "-video_size".into(),
                    format!("{w}x{h}"),
                    "-draw_mouse".into(),
                    "1".into(),
                    "-i".into(),
                    format!("{display}+{x},{y}"),
                ]);
            }
            Self::PipeWire { node, .. } => {
                args.extend([
                    "-f".into(),
                    "pipewire".into(),
                    "-framerate".into(),
                    fps.to_string(),
                    "-i".into(),
                    node.clone(),
                ]);
            }
            Self::GdiGrab { input, .. } => {
                args.extend([
                    "-f".into(),
                    "gdigrab".into(),
                    "-framerate".into(),
                    fps.to_string(),
                    "-draw_mouse".into(),
                    "1".into(),
                    "-i".into(),
                    input.clone(),
                ]);
            }
            Self::DdaGrab { output_idx, .. } => {
                args.extend([
                    "-f".into(),
                    "lavfi".into(),
                    "-i".into(),
                    format!("ddagrab=output_idx={output_idx}:framerate={fps}"),
                ]);
            }
        }
        args.extend([
            "-an".into(),
            "-vf".into(),
            format!("scale={w}:{h}:flags=neighbor,format=bgra"),
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            "bgra".into(),
            "pipe:1".into(),
        ]);
        args
    }
}

/// Streaming BGRA frames from an FFmpeg child process.
pub struct FfmpegRawCaptureSession {
    display: DisplayId,
    width: u32,
    height: u32,
    fps: f32,
    frame_bytes: usize,
    child: Child,
    stdout: BufReader<ChildStdout>,
    frame_index: u64,
    closed: bool,
}

impl FfmpegRawCaptureSession {
    pub fn spawn(display: DisplayId, source: &FfmpegRawSource) -> Result<Self, CaptureError> {
        let width = source.width().max(16);
        let height = source.height().max(16);
        let args = source.ffmpeg_args();
        let mut child = Command::new("ffmpeg")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| CaptureError::Other(format!("ffmpeg spawn: {e}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CaptureError::Other("ffmpeg missing stdout".into()))?;
        Ok(Self {
            display,
            width,
            height,
            fps: source.fps().max(1) as f32,
            frame_bytes: (width as usize)
                .saturating_mul(height as usize)
                .saturating_mul(4),
            child,
            stdout: BufReader::new(stdout),
            frame_index: 0,
            closed: false,
        })
    }
}

impl CaptureSession for FfmpegRawCaptureSession {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        if self.closed {
            return Ok(None);
        }
        let _ = timeout;
        let mut buf = vec![0u8; self.frame_bytes];
        match self.stdout.read_exact(&mut buf) {
            Ok(()) => {
                self.frame_index = self.frame_index.saturating_add(1);
                Ok(Some(CapturedFrame {
                    display_id: self.display,
                    width: self.width,
                    height: self.height,
                    bytes: Arc::new(buf),
                    format: PixelFormat::Bgra8,
                    captured_at: Instant::now(),
                    frame_index: self.frame_index,
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(CaptureError::Other(format!("ffmpeg read: {e}"))),
        }
    }

    fn capture_region(&self) -> Rect {
        Rect {
            origin: Point { x: 0, y: 0 },
            size: Size {
                width: self.width,
                height: self.height,
            },
        }
    }

    fn display_id(&self) -> DisplayId {
        self.display
    }

    fn color_space(&self) -> ColorSpaceId {
        ColorSpaceId::Srgb
    }

    fn refresh_hz(&self) -> f32 {
        self.fps
    }

    fn close(&mut self) -> Result<(), CaptureError> {
        self.closed = true;
        let _ = self.child.kill();
        let _ = self.child.wait();
        Ok(())
    }
}

impl Drop for FfmpegRawCaptureSession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Prefer soft capture in CI / when `QUBOX_SOFT_CAPTURE=1`.
pub fn prefer_soft_capture() -> bool {
    crate::soft_capture::soft_capture_enabled()
}

/// Resolve PipeWire node name: env override → default.
pub fn resolve_pipewire_node() -> String {
    std::env::var("QUBOX_PIPEWIRE_NODE").unwrap_or_else(|_| "default".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_args_include_x11grab_and_bgra() {
        let src = FfmpegRawSource::X11 {
            display: ":0".into(),
            x: 10,
            y: 20,
            width: 640,
            height: 480,
            fps: 30,
        };
        let args = src.ffmpeg_args();
        let joined = args.join(" ");
        assert!(joined.contains("x11grab"));
        assert!(joined.contains(":0+10,20"));
        assert!(joined.contains("bgra"));
        assert!(joined.contains("640x480") || joined.contains("scale=640:480"));
    }

    #[test]
    fn pipewire_args_include_pipewire_node() {
        let src = FfmpegRawSource::PipeWire {
            node: "screen".into(),
            width: 1280,
            height: 720,
            fps: 60,
        };
        let args = src.ffmpeg_args();
        assert!(args.windows(2).any(|w| w[0] == "-f" && w[1] == "pipewire"));
        assert!(args.windows(2).any(|w| w[0] == "-i" && w[1] == "screen"));
    }

    #[test]
    fn ddagrab_args_use_lavfi() {
        let src = FfmpegRawSource::DdaGrab {
            output_idx: 0,
            width: 1920,
            height: 1080,
            fps: 60,
        };
        let joined = src.ffmpeg_args().join(" ");
        assert!(joined.contains("lavfi"));
        assert!(joined.contains("ddagrab=output_idx=0"));
    }

    #[test]
    fn gdigrab_args_include_desktop() {
        let src = FfmpegRawSource::GdiGrab {
            input: "desktop".into(),
            width: 800,
            height: 600,
            fps: 30,
        };
        let joined = src.ffmpeg_args().join(" ");
        assert!(joined.contains("gdigrab"));
        assert!(joined.contains("desktop"));
    }

    #[test]
    fn resolve_pipewire_node_default() {
        let prev = std::env::var_os("QUBOX_PIPEWIRE_NODE");
        std::env::remove_var("QUBOX_PIPEWIRE_NODE");
        assert_eq!(resolve_pipewire_node(), "default");
        if let Some(v) = prev {
            std::env::set_var("QUBOX_PIPEWIRE_NODE", v);
        }
    }
}
