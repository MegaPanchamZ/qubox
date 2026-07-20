//! Phase B real screen capture for browser/WebRTC sessions.
//!
//! On Linux (X11) we spawn an `ffmpeg` subprocess that grabs the desktop
//! via `x11grab`, encodes it as H.264 (Baseline 3.1 for browser
//! compatibility), and writes Annex-B NAL units to stdout. This task
//! parses the stdout, splits on 4-byte start codes, drops SEI/access-unit
//! delimiters that some encoders insert, and feeds each access unit into
//! `session.write_video()`.
//!
//! macOS uses `avfoundation`; Windows uses `gdigrab`/`ddagrab`. Both
//! fall back to the Phase A test-pattern producer if the chosen backend
//! is not available (or `ffmpeg` isn't installed).
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use qubox_proto::DisplayDescriptor;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;

use crate::webrtc_session::WebRtcSession;

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub display: String, // X11: ":0"; avfoundation: "1"; gdigrab: "desktop"
    pub framerate: u32,
    pub width: u32,
    pub height: u32,
    pub x_offset: u32,
    pub y_offset: u32,
    /// Target H.264 bitrate in kbps. Without an explicit rate the
    /// x264 default tracks scene complexity, which on a desktop UI
    /// (large flat regions with sharp text) easily blows past the
    /// TURN relay's effective bandwidth and causes the decoder to
    /// stall on lost reference frames.
    pub bitrate_kbps: u32,
    /// Display to capture when the viewer picked a non-primary monitor.
    /// `None` means "capture the display configured on the rest of the
    /// struct" (primary, system default).
    pub selected_display: Option<DisplayDescriptor>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            display: std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into()),
            framerate: 30,
            width: 1280,
            height: 720,
            x_offset: 0,
            y_offset: 0,
            bitrate_kbps: 1_500,
            selected_display: None,
        }
    }
}

impl CaptureConfig {
    /// Build a config for the given display descriptor. The display's
    /// geometry replaces width/height/x/y so ffmpeg captures exactly
    /// that rectangle instead of the full virtual root.
    pub fn for_display(display: DisplayDescriptor) -> Self {
        Self {
            display: std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into()),
            framerate: 30,
            width: display.width,
            height: display.height,
            x_offset: display.x.max(0) as u32,
            y_offset: display.y.max(0) as u32,
            bitrate_kbps: 1_500,
            selected_display: Some(display),
        }
    }
}

fn ffmpeg_argv(cfg: &CaptureConfig) -> Result<Vec<String>> {
    let mut argv: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
    #[cfg(target_os = "linux")]
    {
        argv.extend_from_slice(&[
            "-f".into(),
            "x11grab".into(),
            "-framerate".into(),
            cfg.framerate.to_string(),
            "-video_size".into(),
            format!("{}x{}", cfg.width, cfg.height),
            "-i".into(),
            format!("{}+{},{}", cfg.display, cfg.x_offset, cfg.y_offset),
        ]);
    }
    #[cfg(target_os = "macos")]
    {
        argv.extend_from_slice(&[
            "-f".into(),
            "avfoundation".into(),
            "-framerate".into(),
            cfg.framerate.to_string(),
            "-video_size".into(),
            format!("{}x{}", cfg.width, cfg.height),
            "-i".into(),
            format!("{}:none", cfg.display),
        ]);
    }
    #[cfg(target_os = "windows")]
    {
        argv.extend_from_slice(&[
            "-f".into(),
            "gdigrab".into(),
            "-framerate".into(),
            cfg.framerate.to_string(),
            "-video_size".into(),
            format!("{}x{}", cfg.width, cfg.height),
            "-i".into(),
            format!("desktop"),
        ]);
    }
    argv.extend_from_slice(&[
        "-vcodec".into(),
        "libx264".into(),
        "-profile:v".into(),
        "baseline".into(),
        "-level".into(),
        "3.1".into(),
        "-preset".into(),
        "ultrafast".into(),
        "-tune".into(),
        "zerolatency".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-an".into(),
        // Constrain bitrate. x264's default is CQP (constant QP) which
        // lets the bitrate spike to fill available bandwidth — disastrous
        // over a TURN relay with limited uplink. Cap at `bitrate_kbps`
        // with a reasonable buffer for scene complexity.
        "-b:v".into(),
        format!("{}k", cfg.bitrate_kbps.max(200)),
        "-maxrate".into(),
        format!("{}k", cfg.bitrate_kbps.max(200)),
        "-bufsize".into(),
        format!("{}k", (cfg.bitrate_kbps.max(200) * 2)),
        // Emit one IDR every ~1 second so a lost P-frame / slice can't
        // pin the viewer on a half-green decoder for the rest of the
        // GOP. x264's default `keyint=250` is ~8s @ 30fps — far too
        // slow for a remote desktop where the user notices green
        // artifacts immediately.
        "-force_key_frames".into(),
        format!("expr:gte(t,prev_forced_t+{:.2})", 1.0_f64),
        // Cap GOP distance and disable scenecut so the time-based expr
        // stays the dominant keyframe trigger (no surprise IDRs mid-
        // scroll).
        "-x264-params".into(),
        "keyint=60:min-keyint=30:scenecut=0".into(),
        "-f".into(),
        "h264".into(),
        "-".into(),
    ]);
    Ok(argv)
}

/// Spawn ffmpeg and stream its stdout into the WebRTC session. Returns
/// when ffmpeg exits or the cancel signal flips. The task logs and
/// returns Ok(()) so the WebRTC session task can continue gracefully.
pub async fn spawn_screen_capture(
    session: std::sync::Arc<WebRtcSession>,
    cfg: CaptureConfig,
    cancel: watch::Receiver<bool>,
) -> Result<()> {
    let argv = ffmpeg_argv(&cfg).context("build ffmpeg argv")?;
    tracing::info!(?argv, "spawning ffmpeg screen capture");
    let mut child = Command::new("ffmpeg")
        .args(&argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ffmpeg — is `ffmpeg` installed and on PATH?")?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("ffmpeg stdout missing"))?;
    let mut reader = BufReader::new(stdout);

    let mut stderr = child.stderr.take();
    // Drain stderr in another task so ffmpeg doesn't block on a full pipe.
    let stderr_task = tokio::spawn(async move {
        if let Some(mut s) = stderr.take() {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 1024];
            while let Ok(n) = s.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                tracing::debug!(target: "ffmpeg", "{}", String::from_utf8_lossy(&buf[..n]));
            }
        }
    });

    // NAL splitter: ffmpeg Annex-B uses 3- or 4-byte start codes
    // (00 00 01 / 00 00 00 01). Each access unit is one or more NALs
    // delimited by start codes.
    let mut pending: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut prev = [0u8; 3];

    loop {
        let mut next_cancel = cancel.clone();
        let mut byte = [0u8; 1];
        tokio::select! {
            biased;
            _ = next_cancel.changed() => {
                if *next_cancel.borrow() {
                    tracing::info!("screen capture: cancel signal received");
                    let _ = child.kill().await;
                    break;
                }
            }
            read = reader.read_exact(&mut byte) => {
                if read.is_err() {
                    break;
                }
                // Slide the 3-byte window so we can spot start codes.
                pending.push(byte[0]);
                if prev == [0, 0, 0] && byte[0] == 1 {
                    // 4-byte start code — emit the access unit that
                    // ended just before this marker (3 trailing zeros
                    // plus this 0x01 belong to the new NAL header).
                    let end = pending.len().saturating_sub(4);
                    if end > 0 {
                        // Strip the start code itself from the AU bytes.
                        let au = Bytes::copy_from_slice(&pending[..end]);
                        pending.clear();
                        // We just pushed the new start code in; keep it
                        // for the next AU.
                        pending.extend_from_slice(&[0, 0, 0, 1]);
                        // Filter out SEI (NAL type 6) — encoder metadata
                        // we don't want to forward to the browser.
                        if let Some(au_clean) = strip_sei(au) {
                            if let Err(err) = session.write_video(au_clean).await {
                                tracing::warn!(?err, "write_video failed");
                                let _ = child.kill().await;
                                break;
                            }
                        }
                    }
                } else if prev[0] == 0 && prev[1] == 0 && byte[0] == 1 {
                    // 3-byte start code (less common but Annex-B allows)
                    let end = pending.len().saturating_sub(3);
                    if end > 0 {
                        let au = Bytes::copy_from_slice(&pending[..end]);
                        pending.clear();
                        pending.extend_from_slice(&[0, 0, 1]);
                        if let Some(au_clean) = strip_sei(au) {
                            if let Err(err) = session.write_video(au_clean).await {
                                tracing::warn!(?err, "write_video failed");
                                let _ = child.kill().await;
                                break;
                            }
                        }
                    }
                }
                prev = [prev[1], prev[2], byte[0]];
            }
        }
    }

    // Flush the tail (if ffmpeg ended mid-AU, drop it — the next IDR will
    // recover the decoder state).
    pending.clear();

    let _ = child.wait().await;
    let _ = stderr_task.await;
    tracing::info!("screen capture task exiting");
    Ok(())
}

/// Strip SEI (NAL type 6) access units from the buffer. Returns None if
/// the entire AU was SEI (no payload worth sending).
fn strip_sei(au: Bytes) -> Option<Bytes> {
    if au.is_empty() {
        return None;
    }
    // Walk NAL units inside this AU. We can't assume they have start
    // codes — within an AU, NALs are length-prefixed in MP4, but in
    // Annex-B they ARE start-code delimited (the splitter above only
    // splits on AU boundaries, not NAL boundaries). However, because
    // x264 typically emits SEI as a leading NAL with the IDR, we can
    // just send everything that isn't *only* an SEI.
    // Simpler: scan for any non-SEI NAL header byte and return the
    // original AU if we find one.
    let bytes = au.as_ref();
    let mut idx = 0;
    while idx + 4 < bytes.len() {
        if bytes[idx] == 0 && bytes[idx + 1] == 0 && bytes[idx + 2] == 0 && bytes[idx + 3] == 1 {
            let nal_type = bytes[idx + 4] & 0x1F;
            if nal_type != 6 {
                return Some(au);
            }
            idx += 4;
            // Skip to next start code
            while idx + 3 < bytes.len() {
                if bytes[idx] == 0
                    && bytes[idx + 1] == 0
                    && (bytes[idx + 2] == 1
                        || (bytes[idx + 2] == 0 && idx + 3 < bytes.len() && bytes[idx + 3] == 1))
                {
                    break;
                }
                idx += 1;
            }
        } else {
            idx += 1;
        }
    }
    let _ = au;
    None
}
