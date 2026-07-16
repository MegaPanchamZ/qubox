//! P1-9 clipboard sync.
//!
//! Two main types, both safe to drive from a single `tokio` task:
//!
//! - [`ClipboardWatcher`] — polls the local clipboard at a fixed
//!   interval (250 ms default), hashes the contents with `blake3`,
//!   and pushes a [`ClipboardChanged`] `ControlMsg` on the outbound
//!   channel only when the hash actually changes.
//! - [`ClipboardApplier`] — receives `ClipboardChanged` messages
//!   and writes them to the local clipboard via `arboard`.
//!
//! Both types are platform-agnostic; the actual `arboard` calls live
//! in `platform::{linux,windows,macos}::read_snapshot_inner` /
//! `apply_payload_inner`.
//!
//! `arboard::Clipboard` is `!Send + !Sync`, so the cross-platform
//! pattern is to construct a fresh `Clipboard` per call inside a
//! single thread and drop it at the end of the scope.

use std::time::Duration;

use qubox_proto::{ClipboardPayload, ControlMsg};
use tokio::sync::mpsc;

mod hash;
mod platform;

pub use hash::{hash_payload, seq_advances};
pub use platform::{read_snapshot_with_formats, ClipboardError, ClipboardImage, ClipboardSnapshot};

/// Configuration for the watcher and applier.
#[derive(Debug, Clone)]
pub struct ClipboardSyncConfig {
    /// Whether to watch text. Default true.
    pub text_enabled: bool,
    /// Whether to watch images (encoded as PNG on the wire).
    /// Default true.
    pub image_enabled: bool,
    /// Poll interval. Default 250 ms.
    pub poll_interval: Duration,
}

impl Default for ClipboardSyncConfig {
    fn default() -> Self {
        Self {
            text_enabled: true,
            image_enabled: true,
            poll_interval: Duration::from_millis(250),
        }
    }
}

/// Direction the watcher runs in. Used internally to decide which
/// payload shape to emit and whether to consume empty-state
/// transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardDirection {
    /// Watch local → push to remote (host→client or client→host).
    Send,
    /// Receive from remote → apply to local.
    Receive,
}

/// Polls the local clipboard at a fixed interval and pushes
/// `ClipboardChanged` messages on `out_tx` whenever the content
/// hash changes.
///
/// Drop the watcher (or close the receiver of the control stream
/// that drains `out_tx`) to stop it. The polling loop yields via
/// `tokio::time::sleep` between iterations.
pub struct ClipboardWatcher {
    config: ClipboardSyncConfig,
    out_tx: mpsc::UnboundedSender<ControlMsg>,
    next_seq: u64,
    /// Last hash we emitted (covers text + image bytes). When the
    /// new snapshot's hash equals this, we skip the message.
    last_hash: Option<blake3::Hash>,
    /// Whether the last emitted message was a `Clear`. We always
    /// emit `Clear` on the transition non-empty → empty; we skip
    /// empty → empty transitions.
    last_was_clear: bool,
}

impl ClipboardWatcher {
    /// Build a new watcher.
    pub fn new(config: ClipboardSyncConfig, out_tx: mpsc::UnboundedSender<ControlMsg>) -> Self {
        Self {
            config,
            out_tx,
            next_seq: 0,
            last_hash: None,
            last_was_clear: false,
        }
    }

    /// Run the polling loop until `out_tx` is closed (the receiver
    /// was dropped). Returns on cancellation.
    pub async fn run(mut self) {
        let mut ticker = tokio::time::interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            if self.out_tx.is_closed() {
                return;
            }
            if let Err(error) = self.poll_once() {
                tracing::warn!(?error, "clipboard poll failed; will retry next tick");
            }
        }
    }

    /// One iteration of the polling loop. Public for unit tests.
    pub fn poll_once(&mut self) -> Result<(), ClipboardError> {
        let snapshot =
            read_snapshot_with_formats(self.config.text_enabled, self.config.image_enabled)?;
        let payload = snapshot_to_payload(&snapshot);
        let hash = hash_payload(&payload);
        let is_clear = matches!(payload, ClipboardPayload::Clear);

        if Some(hash) == self.last_hash {
            return Ok(());
        }

        if is_clear && self.last_was_clear {
            return Ok(());
        }

        self.last_hash = Some(hash);
        self.last_was_clear = is_clear;
        self.next_seq = self.next_seq.wrapping_add(1);
        let seq = self.next_seq;
        let msg = ControlMsg::ClipboardChanged { seq, payload };
        if self.out_tx.send(msg).is_err() {
            tracing::debug!("clipboard watcher out_tx closed; exiting");
        }
        Ok(())
    }
}

fn snapshot_to_payload(snapshot: &ClipboardSnapshot) -> ClipboardPayload {
    if let Some(image) = &snapshot.image {
        return ClipboardPayload::ImagePng {
            width: image.width,
            height: image.height,
            png: image.png.clone(),
        };
    }
    if let Some(text) = &snapshot.text {
        return ClipboardPayload::Text { utf8: text.clone() };
    }
    ClipboardPayload::Clear
}

/// Applies incoming `ClipboardChanged` messages to the local
/// clipboard. The applier is a stateless function over a
/// `ControlMsg`; the caller (typically the host's or client's
/// control-stream consumer task) drives it.
pub struct ClipboardApplier;

impl ClipboardApplier {
    pub fn new() -> Self {
        Self
    }

    /// Apply a `ClipboardChanged` message to the local clipboard.
    /// Drops stale messages (where `seq <= last_seq`).
    pub fn apply(&self, msg: &ControlMsg, last_seq: &mut u64) -> Result<(), ClipboardError> {
        let ControlMsg::ClipboardChanged { seq, payload } = msg else {
            return Ok(());
        };
        if !seq_advances(*last_seq, *seq) {
            return Ok(());
        }
        platform::apply_payload(payload)?;
        *last_seq = *seq;
        Ok(())
    }
}

impl Default for ClipboardApplier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_to_payload_prefers_image_over_text() {
        let snap = ClipboardSnapshot {
            text: Some("hello".to_string()),
            image: Some(ClipboardImage {
                width: 4,
                height: 4,
                png: vec![0xFF; 8],
            }),
        };
        match snapshot_to_payload(&snap) {
            ClipboardPayload::ImagePng { width, .. } => assert_eq!(width, 4),
            other => panic!("expected ImagePng, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_to_payload_returns_clear_when_empty() {
        let snap = ClipboardSnapshot::default();
        assert!(matches!(
            snapshot_to_payload(&snap),
            ClipboardPayload::Clear
        ));
    }

    #[test]
    fn snapshot_to_payload_text_only() {
        let snap = ClipboardSnapshot {
            text: Some("hi".to_string()),
            image: None,
        };
        match snapshot_to_payload(&snap) {
            ClipboardPayload::Text { utf8 } => assert_eq!(utf8, "hi"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_is_empty() {
        let empty = ClipboardSnapshot::default();
        assert!(empty.is_empty());
        let non_empty = ClipboardSnapshot {
            text: Some("x".to_string()),
            image: None,
        };
        assert!(!non_empty.is_empty());
    }

    #[test]
    fn applier_skips_stale_seq() {
        let applier = ClipboardApplier::new();
        let mut last_seq = 5_u64;
        let msg = ControlMsg::ClipboardChanged {
            seq: 5,
            payload: ClipboardPayload::Text {
                utf8: "stale".to_string(),
            },
        };
        let _ = applier.apply(&msg, &mut last_seq);
        assert_eq!(last_seq, 5);
    }

    #[test]
    fn applier_ignores_other_control_msgs() {
        let applier = ClipboardApplier::new();
        let mut last_seq = 0_u64;
        applier
            .apply(&ControlMsg::MicStop, &mut last_seq)
            .expect("non-clipboard messages are no-ops");
        assert_eq!(last_seq, 0);
    }
}
