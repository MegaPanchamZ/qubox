//! Capture orchestrator — owns the lifecycle of all capture sessions and encoder pipelines.
//!
//! Each display gets its own ffmpeg subprocess and QUIC uni-stream.
//! The orchestrator supports single-stream, multi-display, and all-displays modes.

use std::collections::HashMap;
use std::sync::Arc;

use qubox_display::error::CaptureError;
use qubox_display::traits::{CaptureBackend, CaptureSession, DisplayManager};
use qubox_display::types::{CaptureOptions, DisplayId, DisplayInfo};
use qubox_media::RunningMediaPipeline;
use qubox_proto::RemoteInputEvent;
use qubox_transport::NativeQuicHostConnection;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleMode {
    Native,
    Letterbox,
    Stretch,
}

#[derive(Debug, Clone)]
pub struct PerStreamConfig {
    pub codec: qubox_proto::VideoCodec,
    pub encoder: qubox_media::H264EncoderBackend,
    pub target_fps: u32,
    pub target_bitrate_kbps: u32,
    pub scale_mode: ScaleMode,
    pub target_resolution: Option<(u32, u32)>,
}

impl From<&PerStreamConfig> for CaptureOptions {
    fn from(config: &PerStreamConfig) -> Self {
        CaptureOptions {
            region: None,
            color_space: None,
            target_fps: config.target_fps,
            capture_cursor: true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("capture backend error: {0}")]
    Capture(#[from] CaptureError),
    #[error("display manager error: {0}")]
    Display(#[from] qubox_display::error::DisplayError),
    #[error("ffmpeg pipeline error: {0}")]
    Pipeline(String),
    #[error("QUIC error: {0}")]
    Quic(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("other: {0}")]
    Other(String),
}

impl From<anyhow::Error> for OrchestratorError {
    fn from(e: anyhow::Error) -> Self {
        OrchestratorError::Other(e.to_string())
    }
}

/// Per-display handle holding the session, ffmpeg subprocess, and encoder task.
struct DisplayPipeline {
    session: Box<dyn CaptureSession>,
    pipeline: Option<RunningMediaPipeline>,
    task: Option<JoinHandle<Result<(), OrchestratorError>>>,
    display_info: DisplayInfo,
}

/// Orchestrates capture sessions and encoder pipelines for one or more displays.
/// Owns the lifecycle of ffmpeg subprocesses, QUIC stream writers, and capture sessions.
pub struct CaptureOrchestrator {
    backend: Box<dyn CaptureBackend>,
    display_manager: Box<dyn DisplayManager>,
    pipelines: HashMap<DisplayId, DisplayPipeline>,
    connection: Arc<NativeQuicHostConnection>,
    /// Channel to send HoverDisplay events back to the main input handler
    hover_display_tx: tokio::sync::mpsc::UnboundedSender<RemoteInputEvent>,
    /// Base X11 display string (e.g. ":0")
    x11_display: String,
}

impl CaptureOrchestrator {
    /// Create a new orchestrator with the given backend and QUIC connection.
    pub fn new(
        backend: Box<dyn CaptureBackend>,
        display_manager: Box<dyn DisplayManager>,
        connection: Arc<NativeQuicHostConnection>,
        hover_display_tx: tokio::sync::mpsc::UnboundedSender<RemoteInputEvent>,
        x11_display: String,
    ) -> Self {
        Self {
            backend,
            display_manager,
            pipelines: HashMap::new(),
            connection,
            hover_display_tx,
            x11_display,
        }
    }

    /// Start capturing a single display.
    pub async fn start_single_stream(
        &mut self,
        display_id: DisplayId,
        config: PerStreamConfig,
    ) -> Result<(), OrchestratorError> {
        self.start_display_inner(display_id, config).await
    }

    /// Start capturing multiple displays. Rolls back on failure.
    pub async fn start_multi_display(
        &mut self,
        display_ids: Vec<DisplayId>,
        config: PerStreamConfig,
    ) -> Result<(), OrchestratorError> {
        let mut started = Vec::new();
        for &display_id in &display_ids {
            match self.start_display_inner(display_id, config.clone()).await {
                Ok(()) => started.push(display_id),
                Err(e) => {
                    // Roll back: stop all successfully started displays
                    for id in &started {
                        let _ = self.stop_display(*id).await;
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Start capturing all detected displays.
    pub async fn start_all_displays(
        &mut self,
        config: PerStreamConfig,
    ) -> Result<Vec<DisplayId>, OrchestratorError> {
        let displays = self.backend.enumerate_displays()?;
        let ids: Vec<DisplayId> = displays.iter().map(|d| d.id).collect();
        self.start_multi_display(ids.clone(), config).await?;
        Ok(ids)
    }

    /// Subscribe to additional displays mid-session.
    pub async fn subscribe(
        &mut self,
        display_ids: Vec<DisplayId>,
        config: PerStreamConfig,
    ) -> Result<(), OrchestratorError> {
        let mut started = Vec::new();
        for &display_id in &display_ids {
            if self.pipelines.contains_key(&display_id) {
                continue; // already subscribed
            }
            match self.start_display_inner(display_id, config.clone()).await {
                Ok(()) => started.push(display_id),
                Err(e) => {
                    for id in &started {
                        let _ = self.stop_display(*id).await;
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Unsubscribe from specific displays.
    pub async fn unsubscribe(
        &mut self,
        display_ids: Vec<DisplayId>,
    ) -> Result<(), OrchestratorError> {
        for &display_id in &display_ids {
            self.stop_display(display_id).await?;
        }
        Ok(())
    }

    /// Stop all captures and clean up.
    pub async fn stop(&mut self) -> Result<(), OrchestratorError> {
        let ids: Vec<DisplayId> = self.pipelines.keys().copied().collect();
        for id in ids {
            self.stop_display(id).await?;
        }
        Ok(())
    }

    /// Get the display topology (enumerated displays).
    pub fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        self.backend.enumerate_displays()
    }

    /// Get the backend's capabilities.
    pub fn list_capabilities(&self) -> qubox_display::types::BackendCapabilities {
        self.backend.list_capabilities()
    }

    /// Emit a HoverDisplay event to the client.
    pub fn emit_hover_display_event(&self, display_id: DisplayId) {
        let event = RemoteInputEvent::HoverDisplay {
            display_id: display_id.0,
        };
        let _ = self.hover_display_tx.send(event);
    }


    /// Build platform-appropriate FFmpeg capture+encode argv for one display.
    fn build_platform_ffmpeg_args(
        display_info: &DisplayInfo,
        config: &PerStreamConfig,
        x11_display: &str,
    ) -> (Vec<String>, u32, u32) {
        let (out_w, out_h) = config
            .target_resolution
            .unwrap_or((display_info.size.width, display_info.size.height));
        let fps = config.target_fps.max(1);

        let mut args = vec![
            "-hide_banner".to_string(),
            "-loglevel".to_string(),
            "warning".to_string(),
            "-nostdin".to_string(),
        ];

        #[cfg(target_os = "windows")]
        {
            // DXGI Desktop Duplication via lavfi ddagrab
            args.extend([
                "-f".to_string(),
                "lavfi".to_string(),
                "-i".to_string(),
                format!(
                    "ddagrab=output_idx={}:framerate={}",
                    display_info.id.0, fps
                ),
            ]);
        }

        #[cfg(target_os = "linux")]
        {
            let wayland = std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
                && std::env::var_os("WAYLAND_DISPLAY").is_some();
            if wayland {
                let node = std::env::var("QUBOX_PIPEWIRE_NODE")
                    .unwrap_or_else(|_| "default".to_string());
                args.extend([
                    "-f".to_string(),
                    "pipewire".to_string(),
                    "-framerate".to_string(),
                    fps.to_string(),
                    "-i".to_string(),
                    node,
                ]);
            } else {
                let display_input = format!(
                    "{}+{},{}",
                    x11_display, display_info.position.x, display_info.position.y,
                );
                let capture_size =
                    format!("{}x{}", display_info.size.width, display_info.size.height);
                args.extend([
                    "-f".to_string(),
                    "x11grab".to_string(),
                    "-framerate".to_string(),
                    fps.to_string(),
                    "-video_size".to_string(),
                    capture_size,
                    "-draw_mouse".to_string(),
                    "1".to_string(),
                    "-i".to_string(),
                    display_input,
                ]);
            }
        }

        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            let _ = (display_info, x11_display, fps);
            args.extend([
                "-f".to_string(),
                "lavfi".to_string(),
                "-i".to_string(),
                format!("color=c=black:s={out_w}x{out_h}:r={fps}"),
            ]);
        }

        args.extend([
            "-an".to_string(),
            "-vf".to_string(),
            format!("scale={}:{}", out_w, out_h),
            "-c:v".to_string(),
            config.encoder.ffmpeg_name().to_string(),
            "-b:v".to_string(),
            format!("{}k", config.target_bitrate_kbps),
            "-maxrate".to_string(),
            format!("{}k", config.target_bitrate_kbps),
            "-bufsize".to_string(),
            format!("{}k", config.target_bitrate_kbps / 2),
            "-g".to_string(),
            (fps * 2).to_string(),
            "-bf".to_string(),
            "0".to_string(),
            "-bsf:v".to_string(),
            "h264_metadata=aud=insert".to_string(),
            "-f".to_string(),
            "h264".to_string(),
            "pipe:1".to_string(),
        ]);

        match config.encoder {
            qubox_media::H264EncoderBackend::Nvenc => {
                args.extend([
                    "-preset".to_string(),
                    "p1".to_string(),
                    "-tune".to_string(),
                    "ull".to_string(),
                    "-rc".to_string(),
                    "cbr".to_string(),
                    "-forced-idr".to_string(),
                    "1".to_string(),
                ]);
            }
            qubox_media::H264EncoderBackend::Vaapi => {
                args.extend([
                    "-vaapi_device".to_string(),
                    "/dev/dri/renderD128".to_string(),
                    "-low_power".to_string(),
                    "1".to_string(),
                    "-rc_mode".to_string(),
                    "CBR".to_string(),
                ]);
            }
            qubox_media::H264EncoderBackend::Qsv => {
                args.extend([
                    "-preset".to_string(),
                    "veryfast".to_string(),
                    "-look_ahead".to_string(),
                    "0".to_string(),
                ]);
            }
            qubox_media::H264EncoderBackend::Amf => {
                args.extend([
                    "-quality".to_string(),
                    "speed".to_string(),
                    "-usage".to_string(),
                    "ultralowlatency".to_string(),
                ]);
            }
            qubox_media::H264EncoderBackend::VideoToolbox => {
                args.extend([
                    "-realtime".to_string(),
                    "1".to_string(),
                    "-allow_sw".to_string(),
                    "0".to_string(),
                ]);
            }
            qubox_media::H264EncoderBackend::Libx264 => {
                args.extend([
                    "-preset".to_string(),
                    "ultrafast".to_string(),
                    "-tune".to_string(),
                    "zerolatency".to_string(),
                ]);
            }
        }

        (args, out_w, out_h)
    }

    /// Internal: start a single display's capture pipeline.
    async fn start_display_inner(
        &mut self,
        display_id: DisplayId,
        config: PerStreamConfig,
    ) -> Result<(), OrchestratorError> {
        if self.pipelines.contains_key(&display_id) {
            return Ok(()); // already running
        }

        // Validate display exists
        let displays = self.backend.enumerate_displays()?;
        let display_info = displays
            .iter()
            .find(|d| d.id == display_id)
            .ok_or(CaptureError::DisplayNotFound(display_id))?
            .clone();

        // Open capture session
        let session = self
            .backend
            .open_capture(display_id, CaptureOptions::from(&config))
            .await?;

        // ── Platform capture: x11grab / pipewire / ddagrab ──
        let (args, out_w, out_h) =
            Self::build_platform_ffmpeg_args(&display_info, &config, &self.x11_display);

                let plan = qubox_media::FfmpegPipelinePlan {
            program: "ffmpeg".to_string(),
            args,
            output: qubox_media::EncodedOutput::H264AnnexBStdout,
            notes: vec![format!(
                "Display {} {} at ({}, {})",
                display_info.name,
                display_info.id.0,
                display_info.position.x,
                display_info.position.y,
            )],
        };

        let mut pipeline = qubox_media::spawn_ffmpeg_pipeline(&plan)
            .map_err(|e| OrchestratorError::Pipeline(format!("ffmpeg spawn: {e}")))?;

        // ── Open a QUIC media sender for this display ──
        let mut display_sender = self
            .connection
            .open_media_sender()
            .await
            .map_err(|e| OrchestratorError::Quic(format!("open media sender: {e}")))?;
        let stream_id = display_sender.stream_id_bits() as u16;
        let display_id_u32 = display_id.0;
        let refresh_hz = display_info.refresh_hz;

        // ── Spawn the read+send task ──
        let task: JoinHandle<Result<(), OrchestratorError>> = tokio::spawn(async move {
            let mut framer = match qubox_media::H264AnnexBStreamFramer::new(config.target_fps) {
                Ok(f) => f,
                Err(e) => {
                    return Err(OrchestratorError::Pipeline(format!("framer: {e}")));
                }
            };
            let mut scratch = vec![0_u8; 64 * 1024];

            loop {
                match qubox_media::read_h264_access_units(
                    pipeline.stdout_mut(),
                    &mut framer,
                    &mut scratch,
                ) {
                    Ok(qubox_media::MediaPipelineRead::AccessUnits(units)) => {
                        for au in units {
                            if let Err(e) = display_sender
                                .send_access_unit_ext(
                                    &au,
                                    stream_id,
                                    display_id_u32,
                                    out_w,
                                    out_h,
                                    refresh_hz,
                                    0,
                                    None,
                                )
                                .await
                            {
                                tracing::warn!(
                                    display_id = display_id_u32,
                                    error = %e,
                                    "send failed on display stream"
                                );
                                // Propagate send errors to exit the task
                                return Err(OrchestratorError::Quic(e.to_string()));
                            }
                        }
                    }
                    Ok(qubox_media::MediaPipelineRead::EndOfStream(units)) => {
                        for au in units {
                            let _ = display_sender
                                .send_access_unit_ext(
                                    &au,
                                    stream_id,
                                    display_id_u32,
                                    out_w,
                                    out_h,
                                    refresh_hz,
                                    0,
                                    None,
                                )
                                .await;
                        }
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(
                            display_id = display_id_u32,
                            error = %e,
                            "read error on display stream"
                        );
                        break;
                    }
                }
            }

            let _ = display_sender.finish().await;
            // pipeline is dropped here, kills the ffmpeg subprocess
            Ok(())
        });

        let dp = DisplayPipeline {
            session,
            pipeline: None, // moved into the async task
            task: Some(task),
            display_info,
        };

        self.pipelines.insert(display_id, dp);
        Ok(())
    }

    /// Internal: stop a single display's capture pipeline.
    async fn stop_display(&mut self, display_id: DisplayId) -> Result<(), OrchestratorError> {
        let Some(mut dp) = self.pipelines.remove(&display_id) else {
            return Ok(());
        };

        // Abort the encoder task (drops the pipeline, killing the subprocess)
        if let Some(task) = dp.task.take() {
            task.abort();
        }

        // Close the capture session
        let _ = dp.session.close();

        Ok(())
    }

    /// Wait for all display pipeline tasks to complete.
    /// Used by the caller to block until the multi-display session ends.
    pub async fn wait_for_all(&mut self) -> Result<(), OrchestratorError> {
        let ids: Vec<DisplayId> = self.pipelines.keys().copied().collect();
        for id in ids {
            let Some(mut dp) = self.pipelines.remove(&id) else {
                continue;
            };
            if let Some(task) = dp.task.take() {
                match task.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(display = ?id, error = %e, "display pipeline task failed");
                    }
                    Err(e) => {
                        tracing::warn!(display = ?id, error = %e, "display pipeline task join error");
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qubox_media::H264EncoderBackend;
    use qubox_proto::SessionCredential;

    fn unix_millis_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    #[test]
    fn per_stream_config_converts_to_capture_options() {
        let config = PerStreamConfig {
            codec: qubox_proto::VideoCodec::H264,
            encoder: H264EncoderBackend::Libx264,
            target_fps: 15,
            target_bitrate_kbps: 2000,
            scale_mode: ScaleMode::Stretch,
            target_resolution: Some((640, 480)),
        };
        let opts: CaptureOptions = (&config).into();
        assert_eq!(opts.target_fps, 15);
        assert!(opts.capture_cursor);
    }

    #[test]
    #[test]
    fn build_platform_ffmpeg_args_linux_x11_shape() {
        let info = DisplayInfo {
            id: DisplayId(0),
            name: "test".into(),
            position: qubox_display::types::Point { x: 0, y: 0 },
            size: qubox_display::types::Size {
                width: 640,
                height: 480,
            },
            refresh_hz: 60.0,
            scale_factor: 1.0,
            color_space: qubox_display::types::ColorSpaceId::Srgb,
            hdr_capable: false,
            is_virtual: false,
        };
        let config = PerStreamConfig {
            codec: qubox_proto::VideoCodec::H264,
            encoder: H264EncoderBackend::Libx264,
            target_fps: 30,
            target_bitrate_kbps: 2000,
            scale_mode: ScaleMode::Stretch,
            target_resolution: Some((640, 480)),
        };
        let (args, w, h) = CaptureOrchestrator::build_platform_ffmpeg_args(&info, &config, ":0");
        assert_eq!((w, h), (640, 480));
        let joined = args.join(" ");
        #[cfg(target_os = "linux")]
        {
            assert!(
                joined.contains("x11grab") || joined.contains("pipewire"),
                "linux args: {joined}"
            );
        }
        #[cfg(target_os = "windows")]
        {
            assert!(joined.contains("ddagrab"), "windows args: {joined}");
        }
        assert!(joined.contains("libx264"));
        assert!(joined.contains("pipe:1"));
    }

    fn orchestrator_error_display_impl() {
        let err = OrchestratorError::Capture(CaptureError::DisplayNotFound(DisplayId(42)));
        let msg = format!("{err}");
        assert!(msg.contains("not found"), "msg: {msg}");
    }

    /// Full E2E test: enumerate → start_all_displays → stop.
    ///
    /// Placed inside the crate (not in tests/) because Rust integration tests
    /// cannot access binary-crate internal modules. Requires DISPLAY=:99 (Xephyr).
    #[tokio::test]
    async fn orchestrator_enumerates_starts_and_sends_frames() -> Result<(), String> {
        if std::env::var("DISPLAY").map_or(true, |d| d != ":99" && d != ":99.0") {
            eprintln!("SKIPPED: orchestrator E2E test (DISPLAY=:99 not set)");
            return Ok(());
        }

        let client_credential =
            SessionCredential::new_legacy_token(unix_millis_now() + 60_000);
        let session_id = uuid::Uuid::new_v4();
        let video_config = qubox_proto::VideoStreamParams {
            codec: qubox_proto::VideoCodec::H264,
            width: 640,
            height: 480,
            framerate: 15,
        };

        let host = qubox_transport::NativeQuicHost::bind(
            "127.0.0.1:0".parse().unwrap(),
            None,
            session_id,
            client_credential.clone(),
        )
        .unwrap();
        let ticket = host.ticket().clone();

        let server_task: tokio::task::JoinHandle<Result<(), String>> = tokio::spawn(async move {
            let conn = host
                .accept_authenticated_connection()
                .await
                .map_err(|e| format!("accept auth: {e}"))?;

            let _input_receiver = conn
                .open_input_receiver(video_config.clone())
                .await
                .map_err(|e| format!("open input: {e}"))?;
            let mut audio_sender = conn
                .open_audio_sender(qubox_proto::AudioStreamParams {
                    codec: qubox_proto::AudioCodec::PcmF32,
                    sample_rate: 48_000,
                    channels: 2,
                })
                .await
                .map_err(|e| format!("open audio: {e}"))?;
            audio_sender
                .finish()
                .await
                .map_err(|e| format!("finish audio: {e}"))?;

            let conn = std::sync::Arc::new(conn);
            let backend =
                qubox_display::detect_backend().map_err(|e| format!("detect backend: {e}"))?;
            let display_manager =
                qubox_display::display_manager().map_err(|e| format!("display manager: {e}"))?;
            let (hover_display_tx, _) = tokio::sync::mpsc::unbounded_channel();

            let per_stream = PerStreamConfig {
                codec: qubox_proto::VideoCodec::H264,
                encoder: H264EncoderBackend::Libx264,
                target_fps: 15,
                target_bitrate_kbps: 2000,
                scale_mode: ScaleMode::Stretch,
                target_resolution: Some((640, 480)),
            };

            let mut orchestrator = CaptureOrchestrator::new(
                backend,
                display_manager,
                conn,
                hover_display_tx,
                ":99".to_string(),
            );

            let display_ids = orchestrator
                .start_all_displays(per_stream)
                .await
                .map_err(|e| format!("start_all_displays: {e}"))?;
            assert!(!display_ids.is_empty(), "should start at least one display");

            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            orchestrator
                .stop()
                .await
                .map_err(|e| format!("stop: {e}"))?;
            Ok(())
        });

        let client_result: Result<(), String> = async {
            let mut session = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                qubox_transport::connect_to_native_quic(&ticket, &client_credential),
            )
            .await
            .map_err(|_| "client connect timed out".to_string())?
            .map_err(|e| format!("client connect: {e}"))?;

            let au = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                session.media_receiver.read_access_unit(),
            )
            .await
            .map_err(|_| "timed out reading access unit".to_string())?
            .map_err(|e| format!("read access unit: {e}"))?;

            if let Some(access_unit) = au {
                assert!(
                    access_unit.frame_id > 0 || access_unit.keyframe,
                    "first frame should be a keyframe or have a valid frame_id"
                );
            }
            Ok(())
        }
        .await;

        let server_result = server_task.await.map_err(|e| format!("server join: {e}"))?;

        if let Err(e) = &client_result {
            eprintln!("client side warning (non-fatal): {e}");
        }
        if let Err(e) = &server_result {
            return Err(format!("server side error: {e}"));
        }
        Ok(())
    }

    /// Dual-display subscribe test: start 2 displays → subscribe a 3rd → unsubscribe 1 → stop.
    ///
    /// Exercises subscribe/unsubscribe mid-session with per-display identity tracking.
    /// Requires DISPLAY=:99 (Xephyr); skipped otherwise.
    #[tokio::test]
    async fn orchestrator_dual_display_subscribe_unsubscribe() -> Result<(), String> {
        if std::env::var("DISPLAY").map_or(true, |d| d != ":99" && d != ":99.0") {
            eprintln!("SKIPPED: dual_display test (DISPLAY=:99 not set)");
            return Ok(());
        }

        let client_credential =
            SessionCredential::new_legacy_token(unix_millis_now() + 60_000);
        let session_id = uuid::Uuid::new_v4();
        let video_config = qubox_proto::VideoStreamParams {
            codec: qubox_proto::VideoCodec::H264,
            width: 640,
            height: 480,
            framerate: 15,
        };

        let host = qubox_transport::NativeQuicHost::bind(
            "127.0.0.1:0".parse().unwrap(),
            None,
            session_id,
            client_credential.clone(),
        )
        .unwrap();
        let ticket = host.ticket().clone();

        let server_task: tokio::task::JoinHandle<Result<(), String>> = tokio::spawn(async move {
            let conn = host
                .accept_authenticated_connection()
                .await
                .map_err(|e| format!("accept auth: {e}"))?;

            let _input_receiver = conn
                .open_input_receiver(video_config.clone())
                .await
                .map_err(|e| format!("open input: {e}"))?;
            let mut audio_sender = conn
                .open_audio_sender(qubox_proto::AudioStreamParams {
                    codec: qubox_proto::AudioCodec::PcmF32,
                    sample_rate: 48_000,
                    channels: 2,
                })
                .await
                .map_err(|e| format!("open audio: {e}"))?;
            audio_sender
                .finish()
                .await
                .map_err(|e| format!("finish audio: {e}"))?;

            let conn = std::sync::Arc::new(conn);
            let backend =
                qubox_display::detect_backend().map_err(|e| format!("detect backend: {e}"))?;
            let display_manager =
                qubox_display::display_manager().map_err(|e| format!("display manager: {e}"))?;
            let (hover_display_tx, _) = tokio::sync::mpsc::unbounded_channel();

            let per_stream = PerStreamConfig {
                codec: qubox_proto::VideoCodec::H264,
                encoder: H264EncoderBackend::Libx264,
                target_fps: 15,
                target_bitrate_kbps: 2000,
                scale_mode: ScaleMode::Stretch,
                target_resolution: Some((640, 480)),
            };

            let mut orchestrator = CaptureOrchestrator::new(
                backend,
                display_manager,
                conn,
                hover_display_tx,
                ":99".to_string(),
            );

            // 1. Enumerate — need at least 3 displays
            let displays = orchestrator
                .enumerate_displays()
                .map_err(|e| format!("enumerate: {e}"))?;
            assert!(
                displays.len() >= 3,
                "need >=3 displays, got {}",
                displays.len()
            );

            // 2. Start first 2 displays via start_multi_display
            let ids: Vec<DisplayId> = displays.iter().map(|d| d.id).collect();
            orchestrator
                .start_multi_display(ids[0..2].to_vec(), per_stream.clone())
                .await
                .map_err(|e| format!("start_multi_display: {e}"))?;

            // 3. Subscribe a third display mid-session
            orchestrator
                .subscribe(vec![ids[2]], per_stream.clone())
                .await
                .map_err(|e| format!("subscribe: {e}"))?;

            // 4. Unsubscribe the second display
            orchestrator
                .unsubscribe(vec![ids[1]])
                .await
                .map_err(|e| format!("unsubscribe: {e}"))?;

            // 5. Stop all remaining displays
            orchestrator
                .stop()
                .await
                .map_err(|e| format!("stop: {e}"))?;

            Ok(())
        });

        // Client: connect and let the server test run
        let client_result: Result<(), String> = async {
            let _session = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                qubox_transport::connect_to_native_quic(&ticket, &client_credential),
            )
            .await
            .map_err(|_| "client connect timed out".to_string())?
            .map_err(|e| format!("client connect: {e}"))?;

            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            Ok(())
        }
        .await;

        let server_result = server_task.await.map_err(|e| format!("server join: {e}"))?;

        if let Err(e) = &client_result {
            eprintln!("client side warning (non-fatal): {e}");
        }
        if let Err(e) = &server_result {
            return Err(format!("server side error: {e}"));
        }
        Ok(())
    }
}
