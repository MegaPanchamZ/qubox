//! Client-side stream registry — tracks all active display streams.
//!
//! Shared via `Arc` between the QUIC session thread (writer) and the
//! window event loop (reader). Lock holds are short (HashMap operations),
//! so `std::sync::RwLock` is used even in an async context.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use qubox_display::{ColorSpaceId, DisplayId, DisplayState};

/// Holds the runtime state of all active display streams.
#[derive(Clone)]
pub struct StreamRegistry {
    pub(crate) streams: Arc<std::sync::RwLock<HashMap<DisplayId, StreamEntry>>>,
    pub(crate) selected_stream: Arc<std::sync::RwLock<Option<DisplayId>>>,
    pub(crate) tile_mode: Arc<std::sync::RwLock<bool>>,
    pub(crate) show_privacy_indicator: Arc<std::sync::RwLock<bool>>,
}

/// Per-display stream metadata.
#[derive(Debug, Clone)]
pub struct StreamEntry {
    pub display_id: DisplayId,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f32,
    pub color_space: ColorSpaceId,
    /// Running exponential-moving-average of measured FPS.
    pub fps: f32,
    /// Current privacy state of the display.
    pub privacy_state: DisplayState,
    pub first_frame_at: Instant,
    pub last_frame_at: Instant,
    pub frame_count: u64,
}

impl Default for StreamEntry {
    fn default() -> Self {
        Self {
            display_id: DisplayId::primary(),
            width: 0,
            height: 0,
            refresh_hz: 0.0,
            color_space: ColorSpaceId::Srgb,
            fps: 0.0,
            privacy_state: DisplayState::Active,
            first_frame_at: Instant::now(),
            last_frame_at: Instant::now(),
            frame_count: 0,
        }
    }
}

impl StreamRegistry {
    pub fn new() -> Self {
        Self {
            streams: Arc::new(std::sync::RwLock::new(HashMap::new())),
            selected_stream: Arc::new(std::sync::RwLock::new(None)),
            tile_mode: Arc::new(std::sync::RwLock::new(false)),
            show_privacy_indicator: Arc::new(std::sync::RwLock::new(true)),
        }
    }

    pub fn add_stream(&self, entry: StreamEntry) {
        let mut streams = self.streams.write().unwrap();
        streams.insert(entry.display_id, entry);
    }

    pub fn remove_stream(&self, display_id: DisplayId) -> Option<StreamEntry> {
        let mut streams = self.streams.write().unwrap();
        streams.remove(&display_id)
    }

    pub fn get_stream(&self, display_id: DisplayId) -> Option<StreamEntry> {
        let streams = self.streams.read().unwrap();
        streams.get(&display_id).cloned()
    }

    pub fn list_streams(&self) -> Vec<StreamEntry> {
        let streams = self.streams.read().unwrap();
        let mut entries: Vec<StreamEntry> = streams.values().cloned().collect();
        entries.sort_by_key(|e| e.display_id.0);
        entries
    }

    pub fn update_fps(&self, display_id: DisplayId, measured_fps: f32) {
        let mut streams = self.streams.write().unwrap();
        if let Some(entry) = streams.get_mut(&display_id) {
            // EMA: 0.9 * old + 0.1 * measured
            entry.fps = 0.9 * entry.fps + 0.1 * measured_fps;
            entry.last_frame_at = Instant::now();
            entry.frame_count += 1;
        }
    }

    pub fn update_privacy_state(&self, display_id: DisplayId, new_state: DisplayState) {
        let mut streams = self.streams.write().unwrap();
        if let Some(entry) = streams.get_mut(&display_id) {
            entry.privacy_state = new_state;
        }
    }

    pub fn set_selected_stream(&self, display_id: Option<DisplayId>) {
        let mut selected = self.selected_stream.write().unwrap();
        *selected = display_id;
    }

    pub fn get_selected_stream(&self) -> Option<DisplayId> {
        *self.selected_stream.read().unwrap()
    }

    pub fn set_tile_mode(&self, tile: bool) {
        let mut mode = self.tile_mode.write().unwrap();
        *mode = tile;
    }

    pub fn is_tile_mode(&self) -> bool {
        *self.tile_mode.read().unwrap()
    }

    pub fn set_show_privacy_indicator(&self, show: bool) {
        let mut indicator = self.show_privacy_indicator.write().unwrap();
        *indicator = show;
    }

    pub fn should_show_privacy_indicator(&self) -> bool {
        *self.show_privacy_indicator.read().unwrap()
    }

    /// Check if a stream exists.
    pub fn has_stream(&self, display_id: DisplayId) -> bool {
        self.streams.read().unwrap().contains_key(&display_id)
    }

    /// Number of active streams.
    pub fn stream_count(&self) -> usize {
        self.streams.read().unwrap().len()
    }

    /// Cycle to the next stream in the registry (round-robin).
    pub fn cycle_selected_stream(&self) {
        let streams = self.streams.read().unwrap();
        if streams.is_empty() {
            return;
        }
        let current = *self.selected_stream.read().unwrap();
        let mut keys: Vec<DisplayId> = streams.keys().copied().collect();
        keys.sort_by_key(|k| k.0);

        let next = match current {
            Some(cur) => {
                let pos = keys.iter().position(|k| *k == cur).unwrap_or(0);
                let next_idx = (pos + 1) % keys.len();
                keys[next_idx]
            }
            None => keys[0],
        };
        drop(streams);
        self.set_selected_stream(Some(next));
    }
}

impl Default for StreamRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qubox_display::DisplayState;

    fn make_entry(id: u32) -> StreamEntry {
        StreamEntry {
            display_id: DisplayId(id),
            width: 1920,
            height: 1080,
            refresh_hz: 60.0,
            color_space: ColorSpaceId::Srgb,
            fps: 0.0,
            privacy_state: DisplayState::Active,
            first_frame_at: Instant::now(),
            last_frame_at: Instant::now(),
            frame_count: 0,
        }
    }

    #[test]
    fn add_stream_and_list_returns_in_order() {
        let reg = StreamRegistry::new();
        reg.add_stream(make_entry(2));
        reg.add_stream(make_entry(0));
        reg.add_stream(make_entry(1));

        let list = reg.list_streams();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].display_id.0, 0);
        assert_eq!(list[1].display_id.0, 1);
        assert_eq!(list[2].display_id.0, 2);
    }

    #[test]
    fn update_fps_applies_ema_correctly() {
        let reg = StreamRegistry::new();
        reg.add_stream(make_entry(0));

        reg.update_fps(DisplayId(0), 60.0);
        let entry = reg.get_stream(DisplayId(0)).unwrap();
        assert!(
            (entry.fps - 6.0).abs() < 0.001,
            "expected 6.0, got {}",
            entry.fps
        );

        reg.update_fps(DisplayId(0), 60.0);
        let entry = reg.get_stream(DisplayId(0)).unwrap();
        assert!(
            (entry.fps - 11.4).abs() < 0.001,
            "expected 11.4, got {}",
            entry.fps
        );
    }

    #[test]
    fn update_privacy_state_updates_entry() {
        let reg = StreamRegistry::new();
        reg.add_stream(make_entry(0));

        reg.update_privacy_state(DisplayId(0), DisplayState::Privacy);
        let entry = reg.get_stream(DisplayId(0)).unwrap();
        assert_eq!(entry.privacy_state, DisplayState::Privacy);

        reg.update_privacy_state(DisplayId(0), DisplayState::Active);
        let entry = reg.get_stream(DisplayId(0)).unwrap();
        assert_eq!(entry.privacy_state, DisplayState::Active);
    }

    #[test]
    fn cycle_selected_stream_round_robin() {
        let reg = StreamRegistry::new();
        reg.add_stream(make_entry(0));
        reg.add_stream(make_entry(1));
        reg.add_stream(make_entry(2));

        assert_eq!(reg.get_selected_stream(), None);
        reg.cycle_selected_stream();
        assert_eq!(reg.get_selected_stream(), Some(DisplayId(0)));

        reg.cycle_selected_stream();
        assert_eq!(reg.get_selected_stream(), Some(DisplayId(1)));

        reg.cycle_selected_stream();
        assert_eq!(reg.get_selected_stream(), Some(DisplayId(2)));

        reg.cycle_selected_stream();
        assert_eq!(reg.get_selected_stream(), Some(DisplayId(0)));
    }
}
