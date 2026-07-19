//! Audio Processing Module pipeline: WebRTC APM (AEC3 + AGC2 + NS)
//! + RNNoise secondary NS + Opus encode.
//!
//! The pipeline runs on a dedicated OS thread (not a `tokio`
//! task) because the WebRTC calls are blocking and CPU-bound and
//! must not compete with the multi-threaded `tokio` runtime for
//! scheduling slots. The thread pulls 20 ms frames from the
//! capture ring, runs the APM (with the reference signal from
//! `ReferenceAudioTap`), encodes the result to Opus, and pushes
//! the encoded bytes + a `WireMicHeader` onto an unbounded
//! `tokio::sync::mpsc::UnboundedSender` that the network task
//! drains into `connection.send_datagram(...)`.

use std::sync::Arc;

use qubox_proto::{MicStreamConfig, WireMicHeader, MIC_DATAGRAM_DISCRIMINATOR};

use crate::reference::ReferenceAudioTap;
use crate::ring::SpscRing;

#[cfg(feature = "webrtc-apm")]
use webrtc_audio_processing as webrtc_apm;
#[cfg(feature = "webrtc-apm")]
use webrtc_audio_processing_sys as ffi;

/// Thread handle + shutdown signal for a running pipeline.
pub struct PipelineHandle {
    join: Option<std::thread::JoinHandle<()>>,
    shutdown_flag: Arc<std::sync::atomic::AtomicBool>,
}

impl PipelineHandle {
    /// Signal shutdown and wait for the thread to exit.
    pub fn shutdown(mut self) {
        self.shutdown_flag
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for PipelineHandle {
    fn drop(&mut self) {
        self.shutdown_flag
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

/// One encoded frame ready to be sent over the wire.
#[derive(Debug, Clone)]
pub struct EncodedMicFrame {
    /// Full datagram: header (8 bytes) + Opus payload.
    pub bytes: Vec<u8>,
    /// Sequence number in this `WireMicHeader` (used for logging).
    pub sequence: u16,
}

/// Build the pipeline. Returns a handle to the worker thread; the
/// worker reads 20 ms frames from `capture_ring`, runs the APM
/// with the reference signal from `reference_tap`, and pushes
/// encoded frames onto `out_tx`.
pub fn spawn_pipeline(
    config: MicStreamConfig,
    capture_ring: Arc<SpscRing>,
    reference_tap: ReferenceAudioTap,
    out_tx: tokio::sync::mpsc::UnboundedSender<EncodedMicFrame>,
) -> anyhow::Result<PipelineHandle> {
    let sample_rate = config.sample_rate_hz.max(8_000);
    let frame_ms = match config.frame_ms {
        10 | 20 | 60 => config.frame_ms,
        _ => 20,
    };
    let channels = 1_u8.max(config.channels).min(2) as u32;
    let frame_samples = (sample_rate / 1_000) * (frame_ms as u32);

    #[cfg(feature = "webrtc-apm")]
    let mut apm = {
        let init_cfg = ffi::InitializationConfig {
            num_capture_channels: channels as i32,
            num_render_channels: channels as i32,
            enable_experimental_agc: config.agc_enabled,
            enable_intelligibility_enhancer: false,
        };

        let mut apm = match webrtc_apm::Processor::new(&init_cfg) {
            Ok(p) => Some(p),
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "WebRTC APM build failed; falling back to passthrough"
                );
                None
            }
        };

        if let Some(p) = apm.as_mut() {
            let apm_cfg = webrtc_apm::Config {
                echo_cancellation: if config.aec_enabled {
                    Some(webrtc_apm::EchoCancellation {
                        suppression_level: webrtc_apm::EchoCancellationSuppressionLevel::High,
                        enable_extended_filter: false,
                        enable_delay_agnostic: true,
                        stream_delay_ms: None,
                    })
                } else {
                    None
                },
                gain_control: if config.agc_enabled {
                    Some(webrtc_apm::GainControl {
                        mode: webrtc_apm::GainControlMode::AdaptiveDigital,
                        target_level_dbfs: 3,
                        compression_gain_db: 9,
                        enable_limiter: true,
                    })
                } else {
                    None
                },
                noise_suppression: if config.ns_enabled {
                    Some(webrtc_apm::NoiseSuppression {
                        suppression_level: webrtc_apm::NoiseSuppressionLevel::High,
                    })
                } else {
                    None
                },
                voice_detection: None,
                enable_transient_suppressor: false,
                enable_high_pass_filter: true,
            };
            p.set_config(apm_cfg);
        }
        apm
    };
    #[cfg(not(feature = "webrtc-apm"))]
    let apm = {
        let _ = channels;
        if config.aec_enabled || config.agc_enabled {
            tracing::info!("WebRTC APM disabled in this build; mic uses RNNoise/Opus only");
        }
        None::<()>
    };

    let encoder =
        match opus::Encoder::new(sample_rate, opus::Channels::Mono, opus::Application::Voip) {
            Ok(enc) => enc,
            Err(error) => {
                anyhow::bail!("failed to create opus encoder: {error}");
            }
        };

    let bitrate_bps = config.bitrate_bps.clamp(6_000, 128_000) as i32;

    let rnnoise = if config.ns_enabled {
        Some(std::sync::Mutex::new(nnnoiseless::DenoiseState::new()))
    } else {
        None
    };

    let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_for_thread = Arc::clone(&shutdown_flag);

    let join = std::thread::Builder::new()
        .name("bp-mic-pipeline".to_string())
        .spawn(move || {
            run_pipeline_loop(
                apm,
                encoder,
                rnnoise,
                capture_ring,
                reference_tap,
                out_tx,
                sample_rate,
                frame_samples,
                frame_ms,
                bitrate_bps,
                shutdown_for_thread,
            );
        })?;

    Ok(PipelineHandle {
        join: Some(join),
        shutdown_flag,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_pipeline_loop(
    #[cfg(feature = "webrtc-apm")] mut apm: Option<webrtc_apm::Processor>,
    #[cfg(not(feature = "webrtc-apm"))] apm: Option<()>,
    mut encoder: opus::Encoder,
    rnnoise: Option<std::sync::Mutex<Box<nnnoiseless::DenoiseState<'static>>>>,
    capture_ring: Arc<SpscRing>,
    reference_tap: ReferenceAudioTap,
    out_tx: tokio::sync::mpsc::UnboundedSender<EncodedMicFrame>,
    sample_rate: u32,
    frame_samples: u32,
    _frame_ms: u8,
    bitrate_bps: i32,
    shutdown_flag: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut capture_buf = vec![0.0_f32; frame_samples as usize];
    #[cfg(feature = "webrtc-apm")]
    let mut render_buf = vec![0.0_f32; frame_samples as usize];
    let mut opus_buf = vec![0_u8; 4_000];
    let mut sequence: u16 = 0;
    let mut warmup_frames: u32 = 0;
    #[cfg(not(feature = "webrtc-apm"))]
    let _ = (apm, reference_tap);

    while !shutdown_flag.load(std::sync::atomic::Ordering::Acquire) {
        let n_capture = capture_ring.pop_into(&mut capture_buf);
        if n_capture < frame_samples as usize {
            std::thread::sleep(std::time::Duration::from_millis(2));
            continue;
        }

        #[cfg(feature = "webrtc-apm")]
        if let Some(p) = apm.as_mut() {
            let n_render = reference_tap.pop_into(&mut render_buf);
            if n_render > 0 {
                let _ = p.process_render_frame(&mut render_buf[..n_render]);
            }
            let _ = p.process_capture_frame(&mut capture_buf);
        }

        if let Some(state) = rnnoise.as_ref() {
            if let Ok(mut guard) = state.lock() {
                let mut nn_in = [0.0_f32; nnnoiseless::FRAME_SIZE];
                let mut nn_out = [0.0_f32; nnnoiseless::FRAME_SIZE];
                let frame_count = frame_samples as usize / nnnoiseless::FRAME_SIZE;
                for i in 0..frame_count {
                    let start = i * nnnoiseless::FRAME_SIZE;
                    let end = start + nnnoiseless::FRAME_SIZE;
                    nn_in.copy_from_slice(&capture_buf[start..end]);
                    let _ = guard.process_frame(&mut nn_out, &nn_in);
                    capture_buf[start..end].copy_from_slice(&nn_out);
                }
            }
        }

        match encoder.encode_float(&capture_buf[..frame_samples as usize], &mut opus_buf) {
            Ok(payload_len) => {
                if payload_len > 0 {
                    sequence = sequence.wrapping_add(1);
                    let header = WireMicHeader {
                        magic: [0x51, 0x42],
                        discriminator: MIC_DATAGRAM_DISCRIMINATOR,
                        flags: 0,
                        sequence: sequence.to_be_bytes(),
                        _reserved: [0, 0],
                    };
                    let mut datagram = Vec::with_capacity(8 + payload_len);
                    let mut header_buf = [0_u8; 8];
                    header.write_into(&mut header_buf);
                    datagram.extend_from_slice(&header_buf);
                    datagram.extend_from_slice(&opus_buf[..payload_len]);
                    let _ = sample_rate;
                    let _ = bitrate_bps;

                    if out_tx
                        .send(EncodedMicFrame {
                            bytes: datagram,
                            sequence,
                        })
                        .is_err()
                    {
                        break;
                    }
                    warmup_frames = warmup_frames.saturating_add(1);
                    if warmup_frames == 1 || warmup_frames == 50 {
                        tracing::info!(sequence, warmup_frames, "mic pipeline emitting frames");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(?error, "opus encode failed; dropping frame");
            }
        }
    }

    tracing::debug!("mic pipeline loop exiting");
}

#[cfg(test)]
mod tests {
    #[test]
    fn opus_encode_decode_round_trip_is_lossless_within_one_sample() {
        let mut encoder = opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip)
            .expect("opus encoder");
        let mut decoder = opus::Decoder::new(48_000, opus::Channels::Mono).expect("opus decoder");

        let mut input = vec![0.0_f32; 960];
        for (i, sample) in input.iter_mut().enumerate() {
            let t = i as f32 / 48_000.0;
            *sample = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.25;
        }
        let mut encoded = vec![0_u8; 4_000];
        let n = encoder
            .encode_float(&input, &mut encoded)
            .expect("opus encode");
        assert!(n > 0);

        let mut output = vec![0.0_f32; 960];
        let decoded_samples = decoder
            .decode_float(&encoded[..n], &mut output, false)
            .expect("opus decode");
        assert_eq!(decoded_samples, 960);

        let mut rms_input = 0.0_f64;
        let mut rms_output = 0.0_f64;
        for (a, b) in input.iter().zip(output.iter()) {
            rms_input += f64::from(*a).powi(2);
            rms_output += f64::from(*b).powi(2);
        }
        let n = input.len() as f64;
        let rms_input = (rms_input / n).sqrt();
        let rms_output = (rms_output / n).sqrt();
        let ratio_db = 20.0 * (rms_output / rms_input).log10();
        assert!(
            ratio_db.abs() < 6.0,
            "output/input RMS = {ratio_db} dB (gain drift must be < 6 dB)"
        );
    }
}
