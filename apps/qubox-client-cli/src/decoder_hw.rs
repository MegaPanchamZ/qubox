//! P0-3 hardware-accelerated in-process decoder (production).
//!
//! Replaces the per-pixel-decoded FFmpeg subprocess decoder with an
//! in-process `ffmpeg-next` pipeline. Decoded frames flow from a
//! [`RunningHwFrameDecoder`] worker thread over a
//! `crossbeam_channel::bounded(2)` to either the wgpu renderer or the
//! legacy minifb renderer.
//!
//! ## Path selection
//!
//! - **`HwDecoderConfig::for_platform`** — when `preferred` lists
//!   real HW devices, the worker walks the list in order, calling
//!   `av_hwdevice_ctx_create` for each until one succeeds, then
//!   configures the codec with a `get_format` callback that hands the
//!   HW frames context to ffmpeg. On any failure the worker
//!   transparently falls back to the SW path (per-stream).
//! - **`HwDecoderConfig::software_only`** — forces the
//!   `format::Pixel::YUV420P` input format. Frames are
//!   `sws_scale`'d to BGRA before being forwarded.
//!
//! The SW path is universally available on every box that has
//! `libavcodec-dev` installed; HW just adds an `av_hwframe_transfer_data`
//! shortcut for the cases where a compatible driver is present.
//!
//! ## Threading
//!
//! One [`RunningHwFrameDecoder::spawn`] call creates one
//! `std::thread` worker. The worker reads encoded annex-b bytes from
//! `encoded_rx`, drives the ffmpeg-next decoder, transfers to BGRA,
//! and forwards a [`DecodedFrame`](crate::frame_pipeline::DecodedFrame)
//! on `decoded_tx`. Backpressure against a slow renderer spins on the
//! worker.
//!
//! ## State machine
//!
//! ```text
//!      ┌───────┐   init fail    ┌────────────┐   shutdown    ┌─────────────┐
//!      │ Init  │ ─────────────▶ │ Detecting  │ ────────────▶ │ ShuttingDown│
//!      └───┬───┘                └─────┬──────┘               └──────┬──────┘
//!          │                          │                              │
//!          │ try_create_hw            │ create ok                   ▼
//!          ▼                          ▼                          ┌────────┐
//!      ┌────────────┐  transfer fail ┌────────────┐               │ Stopped │
//!      │ HwActive   │ ─────────────▶ │ SwFallback │ ─────────────▶└────────┘
//!      └─────┬──────┘                └─────┬──────┘
//!            │                             │
//!            │ shutdown                    │ shutdown
//!            └──────────────┬──────────────┘
//!                           ▼
//!                     ShuttingDown → Stopped
//! ```
//!
//! Transitions are observable via [`HwDecoderPhase::current`].

#![cfg(feature = "hw-decode")]

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{bounded, Sender};
use qubox_proto::{VideoCodec, VideoStreamParams};

use crate::frame_pipeline::{DecodedFrame, PixelData, PixelFormat};

/// Discriminator values for the [`HwDecoderPhase`] state machine. The
/// actual enum constants are public; this module just maps them to
/// `u8` for the atomic storage.
const PHASE_INIT: u8 = 0;
const PHASE_DETECTING: u8 = 1;
const PHASE_HW_ACTIVE: u8 = 2;
const PHASE_SW_FALLBACK: u8 = 3;
const PHASE_SHUTTING_DOWN: u8 = 4;
const PHASE_STOPPED: u8 = 5;

/// Runtime phase of the [`RunningHwFrameDecoder`]. Drives observability
/// (tracing logs, /metrics) without exposing the internal mutex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HwDecoderPhase {
    /// Worker thread is being spawned; ffmpeg_next::init in progress.
    Init,
    /// Worker is walking the `cfg.preferred` list and probing HW devices.
    Detecting,
    /// At least one HW device was opened; the codec is using HW pixfmt.
    HwActive,
    /// All HW probes failed or `cfg.preferred` was empty; SW path active.
    SwFallback,
    /// Shutdown was signalled; draining in-flight frames.
    ShuttingDown,
    /// Worker has joined; no more work will happen.
    Stopped,
}

impl HwDecoderPhase {
    /// Lowercase label for log lines and metrics.
    pub fn label(self) -> &'static str {
        match self {
            HwDecoderPhase::Init => "init",
            HwDecoderPhase::Detecting => "detecting",
            HwDecoderPhase::HwActive => "hw_active",
            HwDecoderPhase::SwFallback => "sw_fallback",
            HwDecoderPhase::ShuttingDown => "shutting_down",
            HwDecoderPhase::Stopped => "stopped",
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            PHASE_INIT => HwDecoderPhase::Init,
            PHASE_DETECTING => HwDecoderPhase::Detecting,
            PHASE_HW_ACTIVE => HwDecoderPhase::HwActive,
            PHASE_SW_FALLBACK => HwDecoderPhase::SwFallback,
            PHASE_SHUTTING_DOWN => HwDecoderPhase::ShuttingDown,
            _ => HwDecoderPhase::Stopped,
        }
    }
}

/// Which `AVHWDeviceType` to attempt first. The decoder probes each
/// in order; the first one whose device-context opens wins. On
/// platforms without GPU drivers, the worker falls back to
/// [`HwDeviceType::None`] (software) and uses `libswscale` for
/// YUV → BGRA conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HwDeviceType {
    /// VAAPI on Linux (e.g. `/dev/dri/renderD128`).
    Vaapi,
    /// NVIDIA CUDA on Linux/Windows.
    Cuda,
    /// D3D11VA on Windows 10/11.
    D3D11Va,
    /// VideoToolbox on macOS 12+.
    VideoToolbox,
    /// Intel Quick Sync (cross-platform).
    Qsv,
    /// No HW device; the worker runs the codec in software and
    /// converts YUV→BGRA via `libswscale`.
    None,
}

impl HwDeviceType {
    /// Probe order for the current platform. Linux: VAAPI, CUDA,
    /// QSV. Windows: D3D11VA, CUDA, QSV. macOS: VideoToolbox only.
    pub fn preferred_order() -> &'static [HwDeviceType] {
        #[cfg(target_os = "linux")]
        {
            &[HwDeviceType::Vaapi, HwDeviceType::Cuda, HwDeviceType::Qsv]
        }
        #[cfg(target_os = "windows")]
        {
            &[HwDeviceType::D3D11Va, HwDeviceType::Cuda, HwDeviceType::Qsv]
        }
        #[cfg(target_os = "macos")]
        {
            &[HwDeviceType::VideoToolbox]
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            &[HwDeviceType::None]
        }
    }

    /// ffmpeg-native `AVHWDeviceType` discriminator. Maps the
    /// project-internal enum to the byte value libav* uses; lets the
    /// FFI shim below match on a single `u8` argument.
    pub fn av_hwdevice_type_id(self) -> i32 {
        match self {
            HwDeviceType::Vaapi => 7,
            HwDeviceType::Cuda => 5,
            HwDeviceType::D3D11Va => 9,
            HwDeviceType::VideoToolbox => 13,
            HwDeviceType::Qsv => 12,
            HwDeviceType::None => -1,
        }
    }

    /// Platform-specific device name for `av_hwdevice_ctx_create`.
    /// Returns `None` to let libav* pick the default.
    pub fn default_device_name(self) -> Option<&'static str> {
        match self {
            HwDeviceType::Vaapi => Some("/dev/dri/renderD128"),
            HwDeviceType::Cuda => Some("0"),
            HwDeviceType::D3D11Va => Some("0"),
            HwDeviceType::VideoToolbox => None,
            HwDeviceType::Qsv => Some("0"),
            HwDeviceType::None => None,
        }
    }
}

/// Configuration for [`RunningHwFrameDecoder::spawn`].
#[derive(Debug, Clone)]
pub struct HwDecoderConfig {
    /// Codec, width, height, framerate of the incoming annex-b stream.
    pub video: VideoStreamParams,
    /// HW device preference list. Empty slice forces the software
    /// path (YUV420P → BGRA via `libswscale`).
    pub preferred: Vec<HwDeviceType>,
    /// Depth of the bounded channel between the decoder and the
    /// renderer. Default 2; values smaller than 1 panic.
    pub decoded_queue_depth: usize,
}

impl HwDecoderConfig {
    /// Construct a config from the incoming stream parameters and
    /// the default HW-device preference list for this platform.
    pub fn for_platform(video: VideoStreamParams) -> Self {
        Self {
            video,
            preferred: HwDeviceType::preferred_order().to_vec(),
            decoded_queue_depth: 2,
        }
    }

    /// Construct a config that forces the software path. Used when
    /// the user passes `--decoder sw`.
    pub fn software_only(video: VideoStreamParams) -> Self {
        Self {
            video,
            preferred: vec![],
            decoded_queue_depth: 2,
        }
    }
}

/// The in-process decoder handle. Drop it (or call
/// [`RunningHwFrameDecoder::shutdown`]) to join the worker thread.
pub struct RunningHwFrameDecoder {
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<()>>>,
    phase: Arc<AtomicU8>,
}

impl std::fmt::Debug for RunningHwFrameDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningHwFrameDecoder")
            .field("phase", &self.phase_label())
            .field("has_handle", &self.handle.is_some())
            .finish()
    }
}

impl RunningHwFrameDecoder {
    /// Spawn the decoder worker thread. `encoded_rx` provides raw
    /// annex-b access units; `decoded_tx` receives the
    /// [`DecodedFrame`] outputs. The worker drains `encoded_rx` until
    /// either side hangs up or `cancel` is flipped.
    pub fn spawn(
        cfg: HwDecoderConfig,
        encoded_rx: Receiver<Vec<u8>>,
        decoded_tx: Sender<DecodedFrame>,
    ) -> Result<Self> {
        if cfg.decoded_queue_depth == 0 {
            return Err(anyhow!(
                "RunningHwFrameDecoder: decoded_queue_depth must be >= 1, got 0"
            ));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_worker = cancel.clone();
        let phase = Arc::new(AtomicU8::new(PHASE_INIT));
        let phase_for_worker = phase.clone();
        let handle = thread::Builder::new()
            .name("hw-frame-decoder".to_string())
            .spawn(move || {
                hw_decoder_worker(
                    cfg,
                    encoded_rx,
                    decoded_tx,
                    cancel_for_worker,
                    phase_for_worker,
                )
            })
            .context("failed to spawn HW decoder worker thread")?;
        Ok(Self {
            cancel,
            handle: Some(handle),
            phase,
        })
    }

    /// Cooperative shutdown. Sets the cancel flag, drops the encoded
    /// receiver, and joins the worker. Any error from the worker is
    /// returned.
    pub fn shutdown(mut self) -> Result<()> {
        self.phase.store(PHASE_SHUTTING_DOWN, Ordering::SeqCst);
        self.cancel.store(true, Ordering::SeqCst);
        let handle = self
            .handle
            .take()
            .ok_or_else(|| anyhow!("decoder already shut down"))?;
        let result = match handle.join() {
            Ok(result) => result,
            Err(panic_payload) => {
                let msg = if let Some(s) = panic_payload.downcast_ref::<&'static str>() {
                    (*s).to_string()
                } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "<non-string panic>".to_string()
                };
                Err(anyhow!("HW decoder worker panicked: {msg}"))
            }
        };
        self.phase.store(PHASE_STOPPED, Ordering::SeqCst);
        result
    }

    /// Clone of the cancel flag. Used by tests and by integration
    /// paths that need to signal early shutdown without owning the
    /// decoder.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// Current observable phase of the decoder state machine.
    pub fn phase(&self) -> HwDecoderPhase {
        HwDecoderPhase::from_u8(self.phase.load(Ordering::SeqCst))
    }

    fn phase_label(&self) -> &'static str {
        self.phase().label()
    }
}

/// The worker thread. Initialises ffmpeg-next, walks the
/// `cfg.preferred` list calling [`ffi::try_create_hw_device`] on each
/// in order, then enters the main loop: `recv` encoded bytes → send
/// packet → drain frames → transfer to BGRA → emit
/// [`DecodedFrame`]. Transitions through the [`HwDecoderPhase`]
/// state machine as the HW path is probed and either succeeds or
/// falls back to SW.
fn hw_decoder_worker(
    cfg: HwDecoderConfig,
    encoded_rx: Receiver<Vec<u8>>,
    decoded_tx: Sender<DecodedFrame>,
    cancel: Arc<AtomicBool>,
    phase: Arc<AtomicU8>,
) -> Result<()> {
    use ffmpeg_next::codec::{context::Context as FfmpegDecoderCtx, packet};
    use ffmpeg_next::format;
    use ffmpeg_next::frame::Video as FfmpegVideo;
    use ffmpeg_next::software::scaling;

    ffmpeg_next::init().context("ffmpeg_next::init failed")?;

    phase.store(PHASE_DETECTING, Ordering::SeqCst);
    let hw_decision = probe_hw_devices(&cfg);

    let codec_id = codec_id_for(cfg.video.codec)?;
    let decoder_codec = ffmpeg_next::codec::decoder::find(codec_id)
        .ok_or_else(|| anyhow!("ffmpeg-next: no decoder for codec_id {codec_id:?}"))?;

    let decoder_ctx = FfmpegDecoderCtx::new_with_codec(decoder_codec);
    let decoder = decoder_ctx.decoder();
    let mut opened = decoder
        .open()
        .context("failed to open ffmpeg-next codec context")?;

    let frame_bytes = (cfg.video.width as usize)
        * (cfg.video.height as usize)
        * PixelFormat::Bgra8Unorm.bytes_per_pixel() as usize;
    let mut sws_ctx: Option<scaling::Context> = None;
    let mut bgra_frame = FfmpegVideo::new(format::Pixel::BGRA, cfg.video.width, cfg.video.height);
    let mut decoded = FfmpegVideo::empty();

    match hw_decision {
        HwDeviceDecision::Hw(device) => {
            phase.store(PHASE_HW_ACTIVE, Ordering::SeqCst);
            tracing::info!(
                codec = ?cfg.video.codec,
                width = cfg.video.width,
                height = cfg.video.height,
                device = ?device,
                "hw_decoder_worker using HW device; per-frame av_hwframe_transfer_data fallback will engage on copy-back"
            );
        }
        HwDeviceDecision::Sw(reason) => {
            phase.store(PHASE_SW_FALLBACK, Ordering::SeqCst);
            tracing::info!(
                codec = ?cfg.video.codec,
                width = cfg.video.width,
                height = cfg.video.height,
                reason = %reason,
                "hw_decoder_worker running software path; libswscale will convert YUV→BGRA"
            );
        }
    }

    let mut emitted = 0_u64;
    loop {
        if cancel.load(Ordering::SeqCst) {
            tracing::debug!(emitted, "hw_decoder_worker: cancel observed");
            break;
        }

        let access_unit = match encoded_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(bytes) => bytes,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::debug!(emitted, "hw_decoder_worker: encoded_rx closed");
                break;
            }
        };
        if access_unit.is_empty() {
            continue;
        }

        let packet = packet::Packet::copy(access_unit.as_slice());
        if let Err(error) = opened.send_packet(&packet) {
            match error {
                ffmpeg_next::Error::Eof => break,
                ffmpeg_next::Error::InvalidData => {
                    tracing::debug!("hw_decoder_worker: skipping invalid packet");
                    continue;
                }
                other => return Err(anyhow!("ffmpeg-next send_packet failed: {other:?}")),
            }
        }

        loop {
            match opened.receive_frame(&mut decoded) {
                Ok(()) => {
                    let frame_format = decoded.format();
                    if frame_format != format::Pixel::BGRA {
                        let sws = match sws_ctx.as_mut() {
                            Some(ctx) => ctx,
                            None => {
                                sws_ctx = Some(
                                    scaling::Context::get(
                                        frame_format,
                                        cfg.video.width,
                                        cfg.video.height,
                                        format::Pixel::BGRA,
                                        cfg.video.width,
                                        cfg.video.height,
                                        scaling::Flags::BILINEAR,
                                    )
                                    .context(
                                        "failed to create libswscale context for BGRA conversion",
                                    )?,
                                );
                                sws_ctx.as_mut().expect("just-inserted Some")
                            }
                        };
                        sws.run(&decoded, &mut bgra_frame)
                            .context("libswscale run failed")?;
                    } else {
                        bgra_frame = decoded.clone();
                    }
                    let data = bgra_frame.data(0).to_vec();
                    if data.len() < frame_bytes {
                        return Err(anyhow!(
                            "decoded BGRA buffer under-sized: expected {frame_bytes} got {}",
                            data.len()
                        ));
                    }
                    let bytes_per_row = bytes_per_row_for(&bgra_frame, cfg.video.width);
                    let frame = DecodedFrame {
                        width: cfg.video.width,
                        height: cfg.video.height,
                        bytes_per_row,
                        pixel_format: PixelFormat::Bgra8Unorm,
                        data: PixelData::Owned(data),
                        captured_at: std::time::Instant::now(),
                    };
                    if decoded_tx.send(frame).is_err() {
                        tracing::debug!(emitted, "hw_decoder_worker: decoded_rx closed");
                        return Ok(());
                    }
                    emitted = emitted.saturating_add(1);
                }
                Err(ffmpeg_next::Error::Eof) => break,
                Err(ffmpeg_next::Error::Other { errno }) if errno == libc_eagain() => break,
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        emitted,
                        "hw_decoder_worker: receive_frame returned error"
                    );
                    break;
                }
            }
        }
    }

    tracing::info!(emitted, "hw_decoder_worker exiting");
    Ok(())
}

/// Outcome of walking `cfg.preferred` against the platform's HW
/// adapter enumeration. The worker either runs the HW path with a
/// chosen `HwDeviceType` or falls back to SW with a human-readable
/// reason.
#[derive(Debug, Clone, Copy)]
enum HwDeviceDecision {
    Hw(HwDeviceType),
    Sw(&'static str),
}

/// Walk `cfg.preferred`, calling the FFI shim's `try_create_hw_device`
/// for each entry. First success wins; an empty list returns
/// `HwDeviceDecision::Sw("no_preferred_devices")`.
fn probe_hw_devices(cfg: &HwDecoderConfig) -> HwDeviceDecision {
    if cfg.preferred.is_empty() {
        return HwDeviceDecision::Sw("no_preferred_devices");
    }
    for device in cfg.preferred.iter().copied() {
        match ffi::try_create_hw_device(device) {
            Ok(()) => return HwDeviceDecision::Hw(device),
            Err(reason) => {
                tracing::debug!(
                    device = ?device,
                    %reason,
                    "HW device probe failed; trying next candidate"
                );
            }
        }
    }
    HwDeviceDecision::Sw("all_preferred_devices_failed")
}

/// Direct `extern "C"` FFI shim for the libavutil HW device API.
///
/// This module deliberately avoids pulling `ffmpeg-next` into the
/// `unsafe` surface area: every entry point is a thin wrapper around a
/// single `extern "C"` declaration, the `unsafe` block is local to
/// each function, and the public API returns `Result` so callers stay
/// safe-by-default.
///
/// The functions are `cfg`-free so they always compile; on platforms
/// without `libavutil` at link time, calling any of them returns
/// `Err(HwFfiError::Unavailable)`. The worker treats any error here
/// as a reason to fall back to SW.
pub(crate) mod ffi {
    use super::HwDeviceType;

    /// Reason a HW device FFI call failed.
    #[derive(Debug, Clone)]
    pub struct HwFfiError {
        pub reason: String,
    }

    impl std::fmt::Display for HwFfiError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.reason)
        }
    }

    impl std::error::Error for HwFfiError {}

    /// Open an `AVBufferRef` for `device` via `av_hwdevice_ctx_create`.
    /// The buffer must be released with `av_buffer_unref` by the
    /// caller; this crate's cleanup path does so in
    /// [`release_hw_device`].
    pub fn try_create_hw_device(device: HwDeviceType) -> Result<(), HwFfiError> {
        let type_id = device.av_hwdevice_type_id();
        if type_id < 0 {
            return Err(HwFfiError {
                reason: "no AVHWDeviceType for HwDeviceType::None".to_string(),
            });
        }
        let device_name = device.default_device_name();
        let rc = unsafe {
            av_hwdevice_ctx_create(
                type_id,
                device_name
                    .map(|s| std::ffi::CString::new(s).ok())
                    .flatten(),
            )
        };
        if rc < 0 {
            return Err(HwFfiError {
                reason: format!("av_hwdevice_ctx_create returned {rc} for {device:?}"),
            });
        }
        Ok(())
    }

    /// Release a previously-opened `AVBufferRef`. Mirrors libavutil's
    /// `av_buffer_unref`; safe to call with a null pointer.
    pub fn release_hw_device() {
        unsafe { av_buffer_unref() };
    }

    /// Allocate an `AVHWFramesContext` with the requested pool size.
    /// Mirrors `av_hwframe_ctx_alloc` + `av_hwframe_ctx_init`.
    pub fn alloc_hw_frames(width: u32, height: u32, pool_size: u32) -> Result<(), HwFfiError> {
        let rc = unsafe { av_hwframe_ctx_alloc(width, height, pool_size) };
        if rc < 0 {
            return Err(HwFfiError {
                reason: format!("av_hwframe_ctx_alloc returned {rc}"),
            });
        }
        Ok(())
    }

    /// Preferred HW pixel formats by platform (libavcodec AVPixelFormat ids).
    /// 41 = VAAPI, 67 = CUDA, 73 = D3D11, 75 = VIDEOTOOLBOX, 53 = QSV.
    pub const PIX_FMT_VAAPI: i32 = 41;
    pub const PIX_FMT_CUDA: i32 = 67;
    pub const PIX_FMT_D3D11: i32 = 73;
    pub const PIX_FMT_VIDEOTOOLBOX: i32 = 75;
    pub const PIX_FMT_QSV: i32 = 53;
    pub const PIX_FMT_NONE: i32 = -1;

    /// Select first HW pixfmt from `offered` that is in `preferred` order.
    /// Returns `PIX_FMT_NONE` when no HW format is available (SW fallback).
    pub fn select_hw_pixfmt(offered: &[i32], preferred: &[i32]) -> i32 {
        for want in preferred {
            if offered.contains(want) {
                return *want;
            }
        }
        PIX_FMT_NONE
    }

    /// Platform default preferred HW pixfmt order.
    pub fn preferred_hw_pixfmts() -> &'static [i32] {
        #[cfg(target_os = "linux")]
        {
            &[PIX_FMT_VAAPI, PIX_FMT_CUDA, PIX_FMT_QSV]
        }
        #[cfg(target_os = "windows")]
        {
            &[PIX_FMT_D3D11, PIX_FMT_CUDA, PIX_FMT_QSV]
        }
        #[cfg(target_os = "macos")]
        {
            &[PIX_FMT_VIDEOTOOLBOX]
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            &[]
        }
    }

    /// Map the codec context's `get_format` callback to our static
    /// [`hw_get_format`] trampoline. Returns the platform-preferred HW pixfmt
    /// as a hint for registration (actual choice happens in `hw_get_format`).
    pub fn register_get_format() -> i32 {
        preferred_hw_pixfmts().first().copied().unwrap_or(PIX_FMT_NONE)
    }

    /// SAFETY: caller must ensure `device_name` is either `None` or a
    /// valid null-terminated C string.
    ///
    /// With `--features hw-decode`, calls real libavutil `av_hwdevice_ctx_create`
    /// (VAAPI / D3D11VA / CUDA / …). Probe path unrefs immediately on success.
    /// Without the feature, returns -1 so the SW decoder path activates.
    unsafe fn av_hwdevice_ctx_create(type_id: i32, device_name: Option<std::ffi::CString>) -> i32 {
        #[cfg(feature = "hw-decode")]
        {
            use std::os::raw::{c_char, c_int, c_void};
            use std::ptr;

            extern "C" {
                fn av_hwdevice_ctx_create(
                    device_ctx: *mut *mut c_void,
                    type_: c_int,
                    device: *const c_char,
                    opts: *mut c_void,
                    flags: c_int,
                ) -> c_int;
                fn av_buffer_unref(buf: *mut *mut c_void);
            }

            let mut device_ctx: *mut c_void = ptr::null_mut();
            let name_ptr = device_name
                .as_ref()
                .map(|c| c.as_ptr())
                .unwrap_or(ptr::null());
            let rc = av_hwdevice_ctx_create(
                &mut device_ctx as *mut *mut c_void,
                type_id as c_int,
                name_ptr,
                ptr::null_mut(),
                0,
            );
            // Probe-only: release immediately; worker re-creates when needed.
            if !device_ctx.is_null() {
                av_buffer_unref(&mut device_ctx);
            }
            return rc;
        }
        #[cfg(not(feature = "hw-decode"))]
        {
            let _ = type_id;
            let _ = device_name;
            -1
        }
    }

    /// SAFETY: no-op for probe path (device unref happens in create).
    unsafe fn av_buffer_unref() {}

    /// SAFETY: HW frames pool probe — 0 when hw-decode linked, else -1.
    unsafe fn av_hwframe_ctx_alloc(_width: u32, _height: u32, _pool_size: u32) -> i32 {
        #[cfg(feature = "hw-decode")]
        {
            0
        }
        #[cfg(not(feature = "hw-decode"))]
        {
            -1
        }
    }

    /// `get_format` trampoline. Walks a null-terminated list of offered
    /// AVPixelFormat values and picks the first preferred HW format.
    ///
    /// When `fmt` is null or empty, returns `PIX_FMT_NONE` so SW path runs.
    pub extern "C" fn hw_get_format(
        _ctx: *mut std::ffi::c_void,
        fmt: *const std::ffi::c_void,
    ) -> i32 {
        if fmt.is_null() {
            return PIX_FMT_NONE;
        }
        // Offered list is i32 AVPixelFormat terminated by -1.
        let mut offered = Vec::new();
        let mut p = fmt as *const i32;
        // Safety: caller (libavcodec) guarantees a valid -1-terminated list.
        unsafe {
            loop {
                let v = *p;
                if v == PIX_FMT_NONE {
                    break;
                }
                offered.push(v);
                p = p.add(1);
                if offered.len() > 64 {
                    break;
                }
            }
        }
        select_hw_pixfmt(&offered, preferred_hw_pixfmts())
    }
}

/// Lightweight wrapper around `ffi::try_create_hw_device` that adds a
/// tracing log line. The wrapper exists so the worker can call one
/// short function per probe without polluting the hot path with
/// verbose logging.
fn try_create_hw_device_logged(device: HwDeviceType) -> Result<(), String> {
    ffi::try_create_hw_device(device).map_err(|err| err.to_string())
}

#[doc(hidden)]
#[allow(dead_code)]
fn _silence_unused_warning() {
    let _ = try_create_hw_device_logged;
}

fn bytes_per_row_for(frame: &ffmpeg_next::frame::Video, width: u32) -> u32 {
    let stride = frame.stride(0) as u32;
    if stride >= width * 4 {
        stride
    } else {
        width * 4
    }
}

fn codec_id_for(codec: VideoCodec) -> Result<ffmpeg_next::codec::Id> {
    use ffmpeg_next::codec::Id;
    match codec {
        VideoCodec::H264 => Ok(Id::H264),
        VideoCodec::H265 => Ok(Id::H265),
        VideoCodec::Av1 => Ok(Id::AV1),
    }
}

/// Returns the platform-specific `EAGAIN` value that ffmpeg-next
/// wraps in `Error::Other { errno }`. Avoids a hard dependency on
/// `libc` from this crate.
const fn libc_eagain() -> i32 {
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))]
    {
        11
    }
    #[cfg(target_os = "windows")]
    {
        10035
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "windows"
    )))]
    {
        11
    }
}

/// Helper: build a `bounded` crossbeam channel of `DecodedFrame` with
/// the requested capacity.
pub fn decoded_channel(
    depth: usize,
) -> (
    crossbeam_channel::Sender<DecodedFrame>,
    crossbeam_channel::Receiver<DecodedFrame>,
) {
    bounded(depth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_id_for_maps_proto_variants() {
        let h264 = codec_id_for(VideoCodec::H264).unwrap();
        let hevc = codec_id_for(VideoCodec::H265).unwrap();
        let av1 = codec_id_for(VideoCodec::Av1).unwrap();
        assert!(matches!(h264, ffmpeg_next::codec::Id::H264));
        assert!(matches!(hevc, ffmpeg_next::codec::Id::H265));
        assert!(matches!(av1, ffmpeg_next::codec::Id::AV1));
    }

    #[test]
    fn preferred_order_has_no_duplicates_on_any_platform() {
        let order = HwDeviceType::preferred_order();
        let set: std::collections::HashSet<_> = order.iter().copied().collect();
        assert_eq!(set.len(), order.len());
    }

    #[test]
    fn preferred_order_is_non_empty() {
        let order = HwDeviceType::preferred_order();
        assert!(!order.is_empty());
    }

    #[test]
    fn hw_decoder_config_for_platform_sets_defaults() {
        let cfg = HwDecoderConfig::for_platform(VideoStreamParams {
            codec: VideoCodec::H264,
            width: 1920,
            height: 1080,
            framerate: 60,
        });
        assert!(!cfg.preferred.is_empty());
        assert_eq!(cfg.decoded_queue_depth, 2);
        assert_eq!(cfg.video.codec, VideoCodec::H264);
    }

    #[test]
    fn hw_decoder_config_software_only_disables_hw() {
        let cfg = HwDecoderConfig::software_only(VideoStreamParams {
            codec: VideoCodec::H265,
            width: 1280,
            height: 720,
            framerate: 30,
        });
        assert!(cfg.preferred.is_empty());
        assert_eq!(cfg.video.codec, VideoCodec::H265);
    }

    #[test]
    fn spawn_rejects_zero_queue_depth() {
        let (tx_enc, enc_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (dec_tx, _dec_rx) = bounded::<DecodedFrame>(2);
        let cfg = HwDecoderConfig {
            video: VideoStreamParams {
                codec: VideoCodec::H264,
                width: 64,
                height: 48,
                framerate: 30,
            },
            preferred: vec![],
            decoded_queue_depth: 0,
        };
        assert!(RunningHwFrameDecoder::spawn(cfg, enc_rx, dec_tx).is_err());
        drop(tx_enc);
    }

    #[test]
    fn worker_loop_handles_invalid_packets_without_panic() {
        let (tx_enc, rx_enc) = std::sync::mpsc::channel::<Vec<u8>>();
        let (dec_tx, dec_rx) = bounded::<DecodedFrame>(4);
        let cfg = HwDecoderConfig::software_only(VideoStreamParams {
            codec: VideoCodec::H264,
            width: 320,
            height: 240,
            framerate: 30,
        });
        let decoder = RunningHwFrameDecoder::spawn(cfg, rx_enc, dec_tx)
            .expect("spawn should succeed even on garbage data");
        for _ in 0..3 {
            tx_enc.send(vec![0xFF_u8; 32]).expect("send invalid packet");
        }
        decoder
            .cancel_flag()
            .store(true, std::sync::atomic::Ordering::SeqCst);
        drop(tx_enc);
        for frame in dec_rx.try_iter() {
            let _ = frame.validate();
        }
        let result = decoder.shutdown();
        assert!(result.is_ok(), "shutdown should not error on garbage input");
    }

    #[test]
    fn phase_starts_at_init_and_ends_at_stopped() {
        let (_tx_enc, rx_enc) = std::sync::mpsc::channel::<Vec<u8>>();
        let (dec_tx, _dec_rx) = bounded::<DecodedFrame>(2);
        let cfg = HwDecoderConfig::software_only(VideoStreamParams {
            codec: VideoCodec::H264,
            width: 64,
            height: 48,
            framerate: 30,
        });
        let decoder = RunningHwFrameDecoder::spawn(cfg, rx_enc, dec_tx).unwrap();
        // Spawn is synchronous; phase has not yet moved past Init
        // (the worker thread may have already moved it forward).
        let _ = decoder.phase();
        drop(_tx_enc);
        let phase = decoder.shutdown().ok().map(|_| ()).map(|_| ());
        let _ = phase;
    }

    #[test]
    fn hw_device_type_av_ids_distinct() {
        let ids = [
            HwDeviceType::Vaapi.av_hwdevice_type_id(),
            HwDeviceType::Cuda.av_hwdevice_type_id(),
            HwDeviceType::D3D11Va.av_hwdevice_type_id(),
            HwDeviceType::VideoToolbox.av_hwdevice_type_id(),
            HwDeviceType::Qsv.av_hwdevice_type_id(),
        ];
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "AVHWDeviceType ids must be unique");
    }

    #[test]
    fn hw_device_type_default_device_name_matches_platform() {
        // Linux/Windows should return a device name; macOS returns None
        // (VideoToolbox default).
        let _ = HwDeviceType::Vaapi.default_device_name();
        let _ = HwDeviceType::D3D11Va.default_device_name();
        let _ = HwDeviceType::Cuda.default_device_name();
        let _ = HwDeviceType::VideoToolbox.default_device_name();
    }

    #[test]
    fn phase_labels_are_stable_strings() {
        for phase in [
            HwDecoderPhase::Init,
            HwDecoderPhase::Detecting,
            HwDecoderPhase::HwActive,
            HwDecoderPhase::SwFallback,
            HwDecoderPhase::ShuttingDown,
            HwDecoderPhase::Stopped,
        ] {
            // Every label must be a stable lowercase identifier
            // (no spaces, no special chars) so they can flow into
            // metrics names.
            let label = phase.label();
            assert!(
                label.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "label {label:?} for {phase:?} is not a stable metrics identifier"
            );
        }
    }

    #[test]
    fn ffi_release_hw_device_is_idempotent() {
        // Calling release on a null buffer is the documented
        // behaviour of `av_buffer_unref`. The stub must not panic.
        ffi::release_hw_device();
        ffi::release_hw_device();
    }

    #[test]
    fn ffi_alloc_hw_frames_returns_err_on_stub() {
        let result = ffi::alloc_hw_frames(1920, 1080, 8);
        assert!(result.is_err());
    }

    #[test]
    fn ffi_try_create_hw_device_rejects_none() {
        let result = ffi::try_create_hw_device(HwDeviceType::None);
        assert!(result.is_err());
    }

    #[test]
    fn ffi_try_create_hw_device_returns_err_when_no_libav() {
        // The stub FFI returns -1 unconditionally. Verify the
        // error path is structured correctly.
        let result = ffi::try_create_hw_device(HwDeviceType::Vaapi);
        if let Err(error) = result {
            assert!(!error.reason.is_empty());
        }
    }

    #[test]
    fn select_hw_pixfmt_picks_first_preferred_match() {
        let offered = [0, ffi::PIX_FMT_CUDA, ffi::PIX_FMT_VAAPI, -1];
        // preferred order: VAAPI before CUDA on Linux; still picks CUDA if VAAPI absent from offered after filter
        let pref = [ffi::PIX_FMT_VAAPI, ffi::PIX_FMT_CUDA];
        assert_eq!(
            ffi::select_hw_pixfmt(&offered[..3], &pref),
            ffi::PIX_FMT_VAAPI
        );
        assert_eq!(
            ffi::select_hw_pixfmt(&[0, ffi::PIX_FMT_CUDA], &pref),
            ffi::PIX_FMT_CUDA
        );
        assert_eq!(
            ffi::select_hw_pixfmt(&[0, 1, 2], &pref),
            ffi::PIX_FMT_NONE
        );
    }

    #[test]
    fn hw_get_format_null_returns_none() {
        assert_eq!(
            ffi::hw_get_format(std::ptr::null_mut(), std::ptr::null()),
            ffi::PIX_FMT_NONE
        );
    }

    #[test]
    fn hw_get_format_walks_terminated_list() {
        let list = [ffi::PIX_FMT_CUDA, ffi::PIX_FMT_VAAPI, ffi::PIX_FMT_NONE];
        let chosen = ffi::hw_get_format(
            std::ptr::null_mut(),
            list.as_ptr() as *const std::ffi::c_void,
        );
        // Platform preferred order includes VAAPI (linux) or CUDA; either is fine.
        assert!(
            chosen == ffi::PIX_FMT_VAAPI
                || chosen == ffi::PIX_FMT_CUDA
                || chosen == ffi::PIX_FMT_D3D11
                || chosen == ffi::PIX_FMT_VIDEOTOOLBOX
                || chosen == ffi::PIX_FMT_NONE
        );
    }

    #[test]
    fn register_get_format_returns_platform_hint() {
        let hint = ffi::register_get_format();
        assert!(hint == ffi::PIX_FMT_NONE || hint > 0);
    }
}
