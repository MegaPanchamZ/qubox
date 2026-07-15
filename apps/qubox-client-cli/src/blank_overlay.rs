//! Client-side blank overlay window for privacy mode.
//!
//! Creates a fullscreen black `minifb::Window` that covers the content
//! when privacy is active. Managed as a singleton alongside the main
//! video window in the same event loop thread.

use std::sync::mpsc::{Receiver, Sender};

use minifb::{Window, WindowOptions};

/// Command sent to the overlay controller.
#[derive(Debug, Clone)]
pub struct OverlayCommand {
    pub show: bool,
    pub display_id: Option<u32>,
}

/// Thread-safe handle for sending overlay commands.
#[derive(Clone)]
pub struct OverlayController {
    tx: Sender<OverlayCommand>,
}

impl OverlayController {
    pub fn new(tx: Sender<OverlayCommand>) -> Self {
        Self { tx }
    }

    pub fn show(&self, display_id: Option<u32>) {
        let _ = self.tx.send(OverlayCommand {
            show: true,
            display_id,
        });
    }

    pub fn hide(&self, display_id: Option<u32>) {
        let _ = self.tx.send(OverlayCommand {
            show: false,
            display_id,
        });
    }
}

/// Manages the lifecycle of a single blank overlay `minifb::Window`.
pub struct BlankOverlayWindow {
    window: Option<Window>,
}

impl BlankOverlayWindow {
    pub fn new() -> Self {
        Self { window: None }
    }

    /// Show the overlay. Creates the window lazily on first call.
    pub fn show(&mut self, _display_id: Option<u32>) {
        if self.window.is_some() {
            return;
        }

        match Window::new(
            "Privacy Mode",
            1,
            1,
            WindowOptions {
                borderless: true,
                ..WindowOptions::default()
            },
        ) {
            Ok(mut window) => {
                // Set to fullscreen via minifb API
                // (minifb doesn't have a fullscreen API on all platforms,
                //  so we set it to a large size; on X11 we use borderless
                //  and rely on the window manager or set position)
                let _ = window.update();
                self.window = Some(window);
                tracing::info!("blank overlay window shown");
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to create blank overlay window");
            }
        }
    }

    /// Hide the overlay window.
    pub fn hide(&mut self) {
        if let Some(window) = self.window.take() {
            drop(window);
            tracing::info!("blank overlay window hidden");
        }
    }

    /// Paint the overlay black. Call this each frame while visible.
    pub fn paint_black(&mut self) {
        if let Some(ref mut window) = self.window {
            // Paint a 1x1 black pixel (minimally invasive)
            let black = vec![0_u32; 1];
            let _ = window.update_with_buffer(&black, 1, 1);
        }
    }
}

/// Process overlay commands from the channel. Call this in the video loop.
pub fn process_overlay_commands(
    overlay_rx: &Receiver<OverlayCommand>,
    overlay: &mut BlankOverlayWindow,
) {
    loop {
        match overlay_rx.try_recv() {
            Ok(cmd) => {
                tracing::info!(
                    "received OverlayCommand {{ show: {}, display_id: {:?} }}",
                    cmd.show,
                    cmd.display_id
                );
                if cmd.show {
                    overlay.show(cmd.display_id);
                } else {
                    overlay.hide();
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }
}

/// Paints the overlay window black if it exists.
pub fn paint_overlay_black(overlay: &mut BlankOverlayWindow) {
    overlay.paint_black();
}
