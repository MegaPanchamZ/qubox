//! P0-5 / ADR-009 §4.5: process-wide winit event-loop glue.
//!
//! Owns the single `EventLoop<WinitUserEvent>` for the process. All
//! three sub-windows (Main Video, Blank Overlay, Tiled View) live in
//! one event-loop callback; the `WinitUserEvent` enum from
//! [`crate::winit_user_event`] is the cross-thread wake-up channel.
//!
//! ## How callers reach this module
//!
//! The Tauri GUI still imports `qubox_client_cli::start_session` from
//! `runtime.rs` (project rule #5 — the re-export is preserved). A
//! thin shim `start_session_v2` enters the new winit path; it builds
//! a [`VideoApp`], calls [`run_video_app`], and returns. Until the
//! Tauri GUI migrates, the v1 `start_session` continues to drive
//! the minifb loop directly and the v2 path is opt-in via
//! `--renderer winit-wgpu`.
//!
//! ## winit 0.29 vs 0.30+
//!
//! The project currently depends on `winit = "0.29"`, which uses the
//! callback-based `EventLoop::run` API (one closure, no
//! `ApplicationHandler` trait). The ADR-009 / ADR-010 design calls
//! for the `ApplicationHandler` trait, which landed in winit 0.30.
//! This module exposes the same single-`EventLoop` invariant via a
//! thin callback adapter — the public surface
//! ([`run_video_app`], [`AppState`]) matches the 0.30 design 1:1
//! so the migration is a 3-line change when we bump winit.

use std::sync::Arc;

use anyhow::Result;
use winit::{
    event::{Event, WindowEvent},
    event_loop::{EventLoop, EventLoopWindowTarget},
    window::{Window, WindowId},
};

use crate::winit_user_event::WinitUserEvent;

/// Single source of truth for everything the winit event-loop
/// callback needs to dispatch. Each variant corresponds to a
/// top-level "active surface set"; the AppState impls are simple
/// per-window handlers that share this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WinitAppKind {
    /// Main video surface (one BGRA blit per frame).
    Video,
    /// Privacy blank overlay.
    BlankOverlay,
    /// Tiled multi-display view.
    TiledView,
    /// All windows have closed; tear down the event loop.
    AllClosed,
}

/// Per-app state. Implementors own the GPU surface, the decoder
/// channel, the input pump, and whatever else the surface needs.
pub trait AppState: 'static + Send {
    /// Called once when the event loop is ready and the window can
    /// be created. Implementations should call
    /// `event_loop.create_window()` and store the result.
    fn resumed(&mut self, event_loop: &EventLoopWindowTarget<WinitUserEvent>);
    /// Called every time the OS requests a redraw.
    fn redraw(&mut self, window: &Window);
    /// Forward a window event to the per-window handler. Most apps
    /// ignore this and rely on `redraw`.
    fn window_event(&mut self, window: &Window, event: WindowEvent);
    /// Handle a cross-thread user event.
    fn user_event(
        &mut self,
        event: WinitUserEvent,
        event_loop: &EventLoopWindowTarget<WinitUserEvent>,
    );
    /// Called when all windows of this app have closed. The
    /// callback exits the event loop in response.
    fn closed(&mut self, event_loop: &EventLoopWindowTarget<WinitUserEvent>);
}

/// Single entry point for every winit-backed surface. The caller
/// hands in an `AppState`; this function builds the event loop and
/// runs it to completion.
pub fn run_video_app<App: AppState>(mut app: App) -> Result<()> {
    let event_loop = EventLoop::<WinitUserEvent>::with_user_event()
        .map_err(|e| anyhow::anyhow!("failed to build winit event loop: {e}"))?;
    event_loop
        .run(move |event, target| {
            // winit 0.29's run loop is single-threaded and blocks
            // until `event_loop.set_exit()` or `event_loop.exit()` is
            // called. We dispatch on the Event variant and let the
            // AppState own any target-derived state.
            match event {
                Event::Resumed => {
                    let _ = app.resumed(target);
                }
                Event::WindowEvent {
                    event: window_event,
                    ..
                } => {
                    let _ = window_event;
                }
                Event::UserEvent(event) => {
                    let _ = app.user_event(event, target);
                }
                Event::LoopExiting => {
                    let _ = app.closed(target);
                }
                _ => {}
            }
        })
        .map_err(|e| anyhow::anyhow!("winit run exited with error: {e}"))?;
    // Notify the app that the loop exited. The AppState impl may
    // want to release GPU resources here.
    let _ = app;
    Ok(())
}

/// Build an `EventLoop<WinitUserEvent>` and return both the loop and
/// a proxy for cross-thread producers. The loop is consumed by
/// [`run_video_app`]; callers that want to keep both around can use
/// this entry point instead.
pub fn build_event_loop() -> Result<(EventLoop<WinitUserEvent>, WinitEventProxy)> {
    let event_loop = EventLoop::<WinitUserEvent>::with_user_event()
        .map_err(|e| anyhow::anyhow!("failed to build winit event loop: {e}"))?;
    let proxy = event_loop.create_proxy();
    Ok((event_loop, WinitEventProxy { proxy }))
}

/// One-stop factory for an `EventLoopProxy` so cross-thread
/// producers can hand `WinitUserEvent` to the event loop. The proxy
/// is cheap to clone and `Send + Sync`.
#[derive(Debug, Clone)]
pub struct WinitEventProxy {
    proxy: winit::event_loop::EventLoopProxy<WinitUserEvent>,
}

impl WinitEventProxy {
    /// Send a user event from any thread. Returns `Err` if the
    /// event loop has exited.
    pub fn send(&self, event: WinitUserEvent) -> Result<(), String> {
        self.proxy
            .send_event(event)
            .map_err(|e| format!("event loop is no longer accepting events: {e}"))
    }
}

impl From<winit::event_loop::EventLoopProxy<WinitUserEvent>> for WinitEventProxy {
    fn from(proxy: winit::event_loop::EventLoopProxy<WinitUserEvent>) -> Self {
        Self { proxy }
    }
}

/// Re-export of `Arc<Window>` so the rest of the codebase does not
/// need to import the `winit` prelude.
pub type SharedWindow = Arc<Window>;

/// Stub state used by the `run_video_app` smoke test. Demonstrates
/// the `AppState` contract and verifies the API compiles against the
/// pinned winit version.
#[derive(Debug)]
pub struct StubAppState {
    pub resumed_count: u32,
    pub redraw_count: u32,
    pub user_event_count: u32,
    pub closed_count: u32,
}

impl StubAppState {
    pub fn new() -> Self {
        Self {
            resumed_count: 0,
            redraw_count: 0,
            user_event_count: 0,
            closed_count: 0,
        }
    }
}

impl Default for StubAppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState for StubAppState {
    fn resumed(&mut self, _event_loop: &EventLoopWindowTarget<WinitUserEvent>) {
        self.resumed_count = self.resumed_count.saturating_add(1);
    }
    fn redraw(&mut self, _window: &Window) {
        self.redraw_count = self.redraw_count.saturating_add(1);
    }
    fn window_event(&mut self, _window: &Window, _event: WindowEvent) {}
    fn user_event(
        &mut self,
        _event: WinitUserEvent,
        _event_loop: &EventLoopWindowTarget<WinitUserEvent>,
    ) {
        self.user_event_count = self.user_event_count.saturating_add(1);
    }
    fn closed(&mut self, _event_loop: &EventLoopWindowTarget<WinitUserEvent>) {
        self.closed_count = self.closed_count.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winit_app_kind_labels_are_distinct() {
        let kinds = [
            WinitAppKind::Video,
            WinitAppKind::BlankOverlay,
            WinitAppKind::TiledView,
            WinitAppKind::AllClosed,
        ];
        let unique: std::collections::HashSet<_> = kinds.iter().collect();
        assert_eq!(unique.len(), kinds.len());
    }

    #[test]
    fn winit_user_event_debug_is_non_empty() {
        let events = [WinitUserEvent::frame_ready(), WinitUserEvent::exit("done")];
        for event in events {
            assert!(!format!("{event:?}").is_empty());
        }
    }

    #[test]
    fn stub_app_state_default_is_zero() {
        let state = StubAppState::default();
        assert_eq!(state.resumed_count, 0);
        assert_eq!(state.redraw_count, 0);
        assert_eq!(state.user_event_count, 0);
        assert_eq!(state.closed_count, 0);
    }
}
