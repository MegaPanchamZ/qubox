//! Privacy mode: BlankOverlayManager host-side stub.
//!
//! The `BlankOverlayManager` is the fallback path for privacy mode when
//! vkms is unavailable (no sudo, Secure Boot). It sends a
//! `ControlMsg::BlankOverlay` message to the client over the QUIC
//! control stream, instructing it to show/hide a fullscreen black
//! overlay window on a specific display.
//!
//! The actual window creation is on the **client side** (Phase D).
//! This module owns the host-side sending and state tracking.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use qubox_display::types::DisplayId;
use qubox_proto::ControlMsg;

/// Errors from privacy mode operations.
#[derive(Debug, thiserror::Error)]
pub enum PrivacyError {
    #[error("blank overlay channel closed")]
    ChannelClosed,
    #[error("blank overlay is already shown")]
    AlreadyShown,
    #[error("blank overlay is not shown")]
    NotShown,
}

/// Host-side blank overlay manager.
///
/// Sends `ControlMsg::BlankOverlay` messages to the client over a
/// channel (typically wired to a QUIC bidi stream by the orchestrator).
/// Tracks whether the overlay is currently visible.
pub struct BlankOverlayManager {
    control_tx: tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<ControlMsg>>>,
    visible: Arc<AtomicBool>,
}

impl BlankOverlayManager {
    /// Create a new BlankOverlayManager with no control channel.
    /// Call `set_control_channel` after establishing the QUIC connection.
    pub fn new() -> Self {
        Self {
            control_tx: tokio::sync::Mutex::new(None),
            visible: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Set the control channel sender for sending messages to the client.
    pub async fn set_control_channel(&self, tx: tokio::sync::mpsc::UnboundedSender<ControlMsg>) {
        *self.control_tx.lock().await = Some(tx);
    }

    /// Show the blank overlay on the given display.
    pub async fn show(&self, display_id: DisplayId) -> Result<(), PrivacyError> {
        if self.visible.load(Ordering::SeqCst) {
            return Err(PrivacyError::AlreadyShown);
        }

        let msg = ControlMsg::BlankOverlay {
            show: true,
            display_id: Some(display_id.0),
        };

        let tx_guard = self.control_tx.lock().await;
        if let Some(ref tx) = *tx_guard {
            tx.send(msg).map_err(|_| PrivacyError::ChannelClosed)?;
        } else {
            tracing::debug!(
                display_id = display_id.0,
                "BlankOverlayManager show (no control channel)"
            );
        }
        drop(tx_guard);

        self.visible.store(true, Ordering::SeqCst);
        tracing::info!(display_id = display_id.0, "blank overlay shown");
        Ok(())
    }

    /// Hide the blank overlay on the given display.
    pub async fn hide(&self, display_id: DisplayId) -> Result<(), PrivacyError> {
        if !self.visible.load(Ordering::SeqCst) {
            return Err(PrivacyError::NotShown);
        }

        let msg = ControlMsg::BlankOverlay {
            show: false,
            display_id: Some(display_id.0),
        };

        let tx_guard = self.control_tx.lock().await;
        if let Some(ref tx) = *tx_guard {
            tx.send(msg).map_err(|_| PrivacyError::ChannelClosed)?;
        } else {
            tracing::debug!(
                display_id = display_id.0,
                "BlankOverlayManager hide (no control channel)"
            );
        }
        drop(tx_guard);

        self.visible.store(false, Ordering::SeqCst);
        tracing::info!(display_id = display_id.0, "blank overlay hidden");
        Ok(())
    }

    /// Check whether the blank overlay is currently visible.
    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::SeqCst)
    }
}

impl Default for BlankOverlayManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_manager_starts_hidden() {
        let mgr = BlankOverlayManager::new();
        assert!(!mgr.is_visible());
    }

    #[tokio::test]
    async fn show_and_hide_round_trip() {
        let mgr = BlankOverlayManager::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        mgr.set_control_channel(tx).await;

        // Show
        mgr.show(DisplayId(0)).await.unwrap();
        assert!(mgr.is_visible());

        // Verify a message was sent
        let msg = rx.recv().await.unwrap();
        assert_eq!(
            msg,
            ControlMsg::BlankOverlay {
                show: true,
                display_id: Some(0),
            }
        );

        // Hide
        mgr.hide(DisplayId(0)).await.unwrap();
        assert!(!mgr.is_visible());

        let msg = rx.recv().await.unwrap();
        assert_eq!(
            msg,
            ControlMsg::BlankOverlay {
                show: false,
                display_id: Some(0),
            }
        );
    }

    #[tokio::test]
    async fn double_show_returns_already_shown() {
        let mgr = BlankOverlayManager::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<ControlMsg>();
        mgr.set_control_channel(tx).await;

        mgr.show(DisplayId(0)).await.unwrap();
        let err = mgr.show(DisplayId(0)).await.unwrap_err();
        assert!(matches!(err, PrivacyError::AlreadyShown));
    }

    #[tokio::test]
    async fn double_hide_returns_not_shown() {
        let mgr = BlankOverlayManager::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<ControlMsg>();
        mgr.set_control_channel(tx).await;

        let err = mgr.hide(DisplayId(0)).await.unwrap_err();
        assert!(matches!(err, PrivacyError::NotShown));
    }

    #[tokio::test]
    async fn without_control_channel_show_hide_works_and_logs() {
        // Without a control channel, show/hide still updates visibility state
        let mgr = BlankOverlayManager::new();
        mgr.show(DisplayId(0)).await.unwrap();
        assert!(mgr.is_visible());
        mgr.hide(DisplayId(0)).await.unwrap();
        assert!(!mgr.is_visible());
    }
}
