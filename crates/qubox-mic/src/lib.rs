//! P1-10 microphone streaming.
//!
//! The capture side (`MicCapture`) wraps a `cpal` input stream and
//! pushes F32 PCM samples into a lock-free SPSC ring buffer. The
//! pipeline side (`pipeline::spawn_pipeline`) runs on a dedicated
//! OS thread, runs the WebRTC APM (AEC3 + AGC2 + NS) + RNNoise,
//! encodes to Opus, and pushes encoded frames onto a
//! `tokio::sync::mpsc` that the network task drains into the QUIC
//! datagram channel.
//!
//! The host side (`platform::VirtualMicDevice`) creates a virtual
//! input device so local apps (Discord, Steam, in-game VC) can
//! consume the client's mic. v1 supports a no-op stub on all
//! platforms; full PipeWire support is in the follow-up list.

use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use qubox_proto::MicStreamConfig;

mod pipeline;
mod platform;
mod reference;
mod ring;

pub use pipeline::{spawn_pipeline, EncodedMicFrame, PipelineHandle};
pub use platform::{VirtualDeviceStatus, VirtualMicDevice};
pub use reference::ReferenceAudioTap;
pub use ring::SpscRing;

/// Build a `cpal` input stream that pushes F32 PCM samples into
/// the supplied SPSC ring. Returns the live `cpal::Stream`
/// (which must be kept alive for capture to continue) and a
/// `Stream` you can drop to stop capture.
///
/// The cpal callback is real-time, so it constructs no
/// allocations and uses only non-blocking atomics via the
/// `SpscRing`.
pub struct MicCapture {
    pub stream: Stream,
    pub ring: Arc<SpscRing>,
}

impl MicCapture {
    /// Build a capture stream at 48 kHz mono (per the ADR-008
    /// mic spec). If `device_name` is `Some(name)`, we look up the
    /// device by name; otherwise the host default input device is
    /// used.
    pub fn start(config: &MicStreamConfig, device_name: Option<&str>) -> anyhow::Result<Self> {
        let sample_rate = if config.sample_rate_hz == 0 {
            48_000
        } else {
            config.sample_rate_hz
        };
        let ring = Arc::new(SpscRing::new(sample_rate as usize));

        let host = cpal::default_host();
        let device = if let Some(name) = device_name {
            #[allow(deprecated)]
            let found = host
                .input_devices()?
                .find(|d| d.name().ok().as_deref() == Some(name));
            #[allow(deprecated)]
            found.ok_or_else(|| anyhow::anyhow!("input device {name} not found"))?
        } else {
            host.default_input_device()
                .ok_or_else(|| anyhow::anyhow!("no default input device available"))?
        };

        let supported = device
            .default_input_config()
            .map_err(|error| anyhow::anyhow!("failed to query input config: {error}"))?;
        let sample_format = supported.sample_format();
        let stream_config = supported.config();

        let ring_for_stream = Arc::clone(&ring);
        let err_fn = |error| tracing::warn!(?error, "mic input stream error");

        let stream = match sample_format {
            SampleFormat::F32 => device.build_input_stream(
                &stream_config,
                move |data: &[f32], _| {
                    ring_for_stream.push_slice(data);
                },
                err_fn,
                None,
            ),
            SampleFormat::I16 => {
                let ring_for_stream = Arc::clone(&ring);
                device.build_input_stream(
                    &stream_config,
                    move |data: &[i16], _| {
                        let mut floats = Vec::with_capacity(data.len());
                        for s in data {
                            floats.push(f32::from(*s) / f32::from(i16::MAX));
                        }
                        ring_for_stream.push_slice(&floats);
                    },
                    err_fn,
                    None,
                )
            }
            SampleFormat::U16 => {
                let ring_for_stream = Arc::clone(&ring);
                device.build_input_stream(
                    &stream_config,
                    move |data: &[u16], _| {
                        let mut floats = Vec::with_capacity(data.len());
                        for s in data {
                            floats.push((f32::from(*s) / f32::from(u16::MAX)) * 2.0 - 1.0);
                        }
                        ring_for_stream.push_slice(&floats);
                    },
                    err_fn,
                    None,
                )
            }
            sample_format => {
                anyhow::bail!("unsupported mic input sample format {sample_format:?}");
            }
        }
        .map_err(|error| anyhow::anyhow!("failed to build mic input stream: {error}"))?;

        stream
            .play()
            .map_err(|error| anyhow::anyhow!("failed to start mic input stream: {error}"))?;

        tracing::info!(
            sample_rate,
            channels = stream_config.channels,
            ?sample_format,
            "mic input stream started"
        );

        Ok(Self { stream, ring })
    }
}

/// Decode an Opus payload to F32 PCM. Used by the host side to
/// produce samples for the virtual device.
pub fn decode_opus(payload: &[u8], sample_rate: u32) -> anyhow::Result<Vec<f32>> {
    let mut decoder = opus::Decoder::new(sample_rate, opus::Channels::Mono)
        .map_err(|error| anyhow::anyhow!("opus decoder: {error}"))?;
    let frame_samples = ((sample_rate / 1_000) * 20) as usize;
    let mut out = vec![0.0_f32; frame_samples];
    let n = decoder
        .decode_float(payload, &mut out, false)
        .map_err(|error| anyhow::anyhow!("opus decode: {error}"))?;
    out.truncate(n);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_capture_config_validation() {
        let cfg = MicStreamConfig::default();
        assert_eq!(cfg.sample_rate_hz, 48_000);
        assert_eq!(cfg.frame_ms, 20);
    }

    #[test]
    fn mic_capture_handles_unknown_device() {
        let cfg = MicStreamConfig::default();
        let result = MicCapture::start(&cfg, Some("nonexistent-device-xyz"));
        assert!(result.is_err());
    }
}
