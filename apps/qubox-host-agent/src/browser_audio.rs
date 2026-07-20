//! Browser-viewer audio capture for WebRTC sessions.
//!
//! Captures the host's system-audio (output / monitor device) using
//! `cpal`, resamples to 48 kHz mono if needed, Opus-encodes 20 ms
//! frames, and writes them to the WebRTC session's audio track.
//!
//! Currently routed **only** for browser (WebRTC) sessions; the native
//! QUIC transport still uses the PCM path in `open_host_audio_capture`
//! (see Phase B ADR-008).

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use futures::FutureExt;
use std::sync::Arc;
use tracing::{info, warn};

use crate::webrtc_session::WebRtcSession;

/// 20 ms @ 48 kHz mono = 960 samples per Opus frame.
const OPUS_FRAME_SAMPLES: usize = 960;
const OPUS_SAMPLE_RATE: u32 = 48_000;
const OPUS_MAX_PAYLOAD: usize = 4_000;

/// Spawn an audio capture thread that feeds Opus frames into
/// `session.write_audio(...)`. Returns once the stream has been built
/// successfully; the spawned `std::thread` runs until `shutdown` is
/// notified.
pub fn spawn_browser_audio_capture(
    session: Arc<WebRtcSession>,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<()> {
    let host = cpal::default_host();

    // Pick the output / monitor device (loopback-style capture of what
    // the host plays through its speakers). Fall back to the default
    // input device if no output device is available (some sandboxed
    // Linux setups hide the monitor source from cpal).
    let (device, config, source_label) = if let Some(d) = host.default_output_device() {
        let cfg = d
            .default_output_config()
            .map_err(|e| anyhow!("default output config: {e}"))?;
        (d, cfg, "default-output".to_string())
    } else if let Some(d) = host.default_input_device() {
        let cfg = d
            .default_input_config()
            .map_err(|e| anyhow!("default input config: {e}"))?;
        (d, cfg, "default-input-fallback".to_string())
    } else {
        anyhow::bail!("no audio device available for browser audio capture");
    };

    let capture_sample_rate = config.sample_rate();
    let capture_channels = config.channels() as usize;
    let sample_format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();

    info!(
        source = %source_label,
        capture_sample_rate,
        capture_channels,
        ?sample_format,
        opus_sample_rate = OPUS_SAMPLE_RATE,
        "browser audio capture starting (system monitor → Opus → WebRTC)"
    );

    // Shared buffer between cpal callback (real-time) and the encode
    // thread. `Mutex<Vec<f32>>` is fine here — cpal callbacks are
    // serialized per-stream and the encoder is single-threaded.
    let buffer: Arc<std::sync::Mutex<Vec<f32>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let buffer_for_callback = Arc::clone(&buffer);

    let err_fn = |err| warn!(?err, "browser audio cpal stream error");

    let stream: Stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], _| {
                let mut buf = buffer_for_callback
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if capture_channels == 1 {
                    buf.extend_from_slice(data);
                } else {
                    for frame in data.chunks_exact(capture_channels) {
                        let mut acc = 0.0_f32;
                        for &s in frame {
                            acc += s;
                        }
                        buf.push(acc / capture_channels as f32);
                    }
                }
            },
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            move |data: &[i16], _| {
                let mut buf = buffer_for_callback
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if capture_channels == 1 {
                    buf.extend(data.iter().map(|&s| s as f32 / 32_768.0));
                } else {
                    for frame in data.chunks_exact(capture_channels) {
                        let mut acc = 0.0_f32;
                        for &s in frame {
                            acc += s as f32 / 32_768.0;
                        }
                        buf.push(acc / capture_channels as f32);
                    }
                }
            },
            err_fn,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream(
            &stream_config,
            move |data: &[u16], _| {
                let mut buf = buffer_for_callback
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if capture_channels == 1 {
                    buf.extend(data.iter().map(|&s| (s as f32 - 32_768.0) / 32_768.0));
                } else {
                    for frame in data.chunks_exact(capture_channels) {
                        let mut acc = 0.0_f32;
                        for &s in frame {
                            acc += (s as f32 - 32_768.0) / 32_768.0;
                        }
                        buf.push(acc / capture_channels as f32);
                    }
                }
            },
            err_fn,
            None,
        ),
        other => anyhow::bail!("unsupported browser audio sample format {other:?}"),
    }
    .context("failed to build browser audio cpal input stream")?;

    stream
        .play()
        .context("failed to start browser audio cpal stream")?;

    // Encode thread.
    let shutdown_for_thread = Arc::clone(&shutdown);
    std::thread::Builder::new()
        .name("browser-audio-opus".to_string())
        .spawn(move || {
            let mut encoder = match opus::Encoder::new(
                OPUS_SAMPLE_RATE,
                opus::Channels::Mono,
                opus::Application::Audio,
            ) {
                Ok(e) => e,
                Err(err) => {
                    warn!(?err, "browser audio: failed to create opus encoder");
                    drop(stream);
                    return;
                }
            };
            // 64 kbps VBR — good for general desktop audio (voice + music).
            let _ = encoder.set_bitrate(opus::Bitrate::Bits(64_000));

            let mut opus_buf = vec![0_u8; OPUS_MAX_PAYLOAD];
            let mut resample_buf: Vec<f32> = Vec::with_capacity(OPUS_FRAME_SAMPLES * 4);
            let mut resampler = LinearResampler::new(capture_sample_rate, OPUS_SAMPLE_RATE);
            let mut leftover: Vec<f32> = Vec::with_capacity(OPUS_FRAME_SAMPLES * 4);

            loop {
                // Non-blocking shutdown check.
                if shutdown_for_thread.notified().now_or_never().is_some() {
                    break;
                }
                // Drain buffer.
                let drained: Vec<f32> = {
                    let mut buf = buffer.lock().unwrap_or_else(|p| p.into_inner());
                    std::mem::take(&mut *buf)
                };
                if !drained.is_empty() {
                    resample_buf.clear();
                    resampler.resample(&drained, &mut resample_buf);
                    leftover.extend_from_slice(&resample_buf);
                }

                // Encode whole 20 ms frames.
                while leftover.len() >= OPUS_FRAME_SAMPLES {
                    let frame = &leftover[..OPUS_FRAME_SAMPLES];
                    match encoder.encode_float(frame, &mut opus_buf) {
                        Ok(n) if n > 0 => {
                            let pkt = bytes::Bytes::copy_from_slice(&opus_buf[..n]);
                            let session_clone = Arc::clone(&session);
                            // Spawn short task so we don't block the
                            // encode loop on the webrtc-rs write mutex.
                            tokio::spawn(async move {
                                if let Err(err) = session_clone.write_audio(pkt).await {
                                    warn!(?err, "browser audio: write_audio failed");
                                }
                            });
                        }
                        Ok(_) => {}
                        Err(err) => {
                            warn!(?err, "browser audio opus encode failed");
                        }
                    }
                    // Drop the consumed samples; keep tail for next iter.
                    leftover.drain(..OPUS_FRAME_SAMPLES);
                }

                // Avoid spinning when there's nothing to do.
                std::thread::sleep(std::time::Duration::from_millis(2));
            }

            info!("browser audio encode thread exiting");
            drop(stream);
        })
        .context("failed to spawn browser audio encode thread")?;

    Ok(())
}

/// Lightweight linear resampler. Used to convert arbitrary cpal
/// capture rates to the fixed Opus 48 kHz without pulling in a full
/// resampling crate.
struct LinearResampler {
    from: u32,
    to: u32,
    /// Fractional position offset kept between consecutive chunks so
    /// audio stays in phase.
    carry: f64,
}

impl LinearResampler {
    fn new(from: u32, to: u32) -> Self {
        Self {
            from,
            to,
            carry: 0.0,
        }
    }

    fn resample(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.from == self.to {
            out.extend_from_slice(input);
            return;
        }
        let step = self.from as f64 / self.to as f64;
        let mut idx = self.carry;
        while (idx as usize) + 1 < input.len() {
            let i = idx as usize;
            let frac = idx - i as f64;
            let a = input[i];
            let b = input[i + 1];
            out.push((a as f64 + (b as f64 - a as f64) * frac) as f32);
            idx += step;
        }
        // Carry over the fractional part so the next chunk picks up
        // exactly where we left off.
        self.carry = idx - (idx as usize) as f64;
        if self.carry >= 1.0 {
            self.carry -= 1.0;
        }
    }
}
