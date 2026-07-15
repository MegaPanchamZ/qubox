//! P0-4 rate-feedback loop. Owns a `GccRateController`, receives
//! `ControlMsg::RateFeedback` from the client at ~4 Hz, and publishes
//! the new target encoder bitrate to a watch channel.
//!
//! Implements 1 Hz coalescing: `on_rate_change` is only called when
//! the bitrate changes *and* at least one second has elapsed since the
//! last call. This prevents thrashing the ffmpeg subprocess restart
//! during transient network jitter.

use std::time::{Duration, Instant};

use qubox_proto::{ControlMsg, RateFeedback};
use qubox_transport::media::ControlChannel;
use tokio::sync::watch;
use tracing::Instrument;
use uuid::Uuid;

use crate::rate_control::{GccConfig, GccRateController};

/// Minimum interval between ffmpeg subprocess restarts (1 Hz
/// coalescing to avoid thrash on transient network jitter).
const COALESCE_INTERVAL: Duration = Duration::from_secs(1);

/// Drive the `GccRateController` from the client's `RateFeedback`
/// stream. Returns the control channel error when the stream closes.
///
/// `bitrate_tx` is a watch sender; the read loop subscribes via the
/// receiver half and restarts the ffmpeg pipeline on change.
pub async fn rate_feedback_loop(
    session_id: Uuid,
    initial_bitrate_bps: u32,
    min_bitrate_bps: u32,
    max_bitrate_bps: u32,
    mut control: ControlChannel,
    bitrate_tx: watch::Sender<u32>,
    mut on_rate_change: impl FnMut(u32) + Send + 'static,
) -> anyhow::Result<()> {
    let cfg = GccConfig {
        min_bitrate_bps,
        max_bitrate_bps,
        start_bitrate_bps: initial_bitrate_bps,
        ..Default::default()
    };
    let mut controller = GccRateController::new(cfg);
    let mut last_emitted_bps: Option<u32> = None;
    let mut last_coalesce_at: Option<Instant> = None;

    tracing::info!(
        %session_id,
        initial_bitrate_bps,
        min_bitrate_bps,
        max_bitrate_bps,
        "P0-4 rate feedback loop started"
    );

    loop {
        let maybe_msg = control.recv().await?;
        let msg = match maybe_msg {
            Some(m) => m,
            None => {
                tracing::info!(%session_id, "P0-4 control channel closed");
                return Ok(());
            }
        };

        let fb: RateFeedback = match msg {
            ControlMsg::RateFeedback(fb) => fb,
            other => {
                tracing::trace!(%session_id, ?other, "P0-4 ignoring non-rate-feedback message");
                continue;
            }
        };

        let now = Instant::now();
        let new_bps = controller.on_observation(
            f64::from(fb.one_way_delay_ms),
            fb.loss_x1000,
            Duration::from_millis(u64::from(fb.rtt_ms)),
            now,
        );

        // 1 Hz coalesce: only emit if bitrate changed AND enough time
        // has passed since the last ffmpeg restart.
        let should_emit = match last_coalesce_at {
            Some(t) => now.duration_since(t) >= COALESCE_INTERVAL,
            None => true,
        };
        if should_emit {
            let emit = match last_emitted_bps {
                Some(prev) => prev != new_bps,
                None => true,
            };
            if emit {
                tracing::info!(
                    %session_id,
                    new_bitrate_bps = new_bps,
                    rtt_ms = fb.rtt_ms,
                    loss_x1000 = fb.loss_x1000,
                    owd_ms = fb.one_way_delay_ms,
                    "P0-4 bitrate change emitted"
                );
                last_emitted_bps = Some(new_bps);
                last_coalesce_at = Some(now);
                let _ = bitrate_tx.send(new_bps);
                on_rate_change(new_bps);
            }
        }
    }
}

/// Spawn the `rate_feedback_loop` on the tokio runtime and return the
/// watch receiver for the read loop to monitor.
pub fn spawn_rate_feedback(
    session_id: Uuid,
    initial_bitrate_bps: u32,
    min_bitrate_bps: u32,
    max_bitrate_bps: u32,
    control: ControlChannel,
) -> watch::Receiver<u32> {
    spawn_rate_feedback_with_hook(
        session_id,
        initial_bitrate_bps,
        min_bitrate_bps,
        max_bitrate_bps,
        control,
        None,
    )
}

/// Like [`spawn_rate_feedback`] but also invokes `on_bps` (e.g. FileSync congestion sample).
pub fn spawn_rate_feedback_with_hook(
    session_id: Uuid,
    initial_bitrate_bps: u32,
    min_bitrate_bps: u32,
    max_bitrate_bps: u32,
    control: ControlChannel,
    on_bps: Option<std::sync::Arc<dyn Fn(u32) + Send + Sync>>,
) -> watch::Receiver<u32> {
    let (tx, rx) = watch::channel(initial_bitrate_bps);
    let log_session_id = session_id;
    tokio::spawn(
        async move {
            let hook = on_bps;
            let on_change = move |bps: u32| {
                tracing::debug!(session_id = %log_session_id, bps, "rate feedback on_change callback");
                if let Some(ref h) = hook {
                    h(bps);
                }
            };
            if let Err(error) = rate_feedback_loop(
                log_session_id,
                initial_bitrate_bps,
                min_bitrate_bps,
                max_bitrate_bps,
                control,
                tx.clone(),
                on_change,
            )
            .await
            {
                tracing::warn!(
                    session_id = %log_session_id,
                    ?error,
                    "P0-4 rate feedback loop ended with error"
                );
            }
        }
        .in_current_span(),
    );
    rx
}
