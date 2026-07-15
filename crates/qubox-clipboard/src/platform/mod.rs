//! Platform-specific clipboard backends.
//!
//! `arboard::Clipboard` is `!Send + !Sync`, so the cross-platform
//! pattern is to construct a fresh `Clipboard` per call inside a
//! single thread and drop it at the end of the scope. Each platform
//! module exposes `read_snapshot_inner` and `apply_payload_inner`
//! functions that internally do exactly that.

use qubox_proto::ClipboardPayload;

#[derive(Debug)]
pub enum ClipboardError {
    Handle(String),
    Read(String),
    Write(String),
    PngDecode(String),
    PngEncode(String),
    Empty,
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClipboardError::Handle(m) => write!(f, "arboard open/handle failed: {m}"),
            ClipboardError::Read(m) => write!(f, "clipboard read failed: {m}"),
            ClipboardError::Write(m) => write!(f, "clipboard write failed: {m}"),
            ClipboardError::PngDecode(m) => write!(f, "png decode failed: {m}"),
            ClipboardError::PngEncode(m) => write!(f, "png encode failed: {m}"),
            ClipboardError::Empty => write!(f, "image data is empty"),
        }
    }
}

impl std::error::Error for ClipboardError {}

/// Snapshot of the current clipboard contents. `None` for the
/// image means "no image on the clipboard right now".
#[derive(Debug, Clone, Default)]
pub struct ClipboardSnapshot {
    pub text: Option<String>,
    pub image: Option<ClipboardImage>,
}

#[derive(Debug, Clone)]
pub struct ClipboardImage {
    pub width: u32,
    pub height: u32,
    /// PNG-encoded bytes (so we don't have to think about endianness
    /// or channel order at the wire boundary).
    pub png: Vec<u8>,
}

impl ClipboardSnapshot {
    /// Empty snapshot — no text, no image. Distinct from `Clear`
    /// only at the watcher level; the wire `Clear` variant is
    /// sent explicitly when transitioning into this state.
    pub fn empty() -> Self {
        Self::default()
    }

    /// True when neither text nor image is set on the clipboard.
    pub fn is_empty(&self) -> bool {
        self.text.is_none() && self.image.is_none()
    }
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// Read a snapshot of the current clipboard. `arboard` is
/// constructed inside the function (and dropped at the end) so this
/// is safe to call from any single thread.
pub fn read_snapshot_with_formats(
    text_enabled: bool,
    image_enabled: bool,
) -> Result<ClipboardSnapshot, ClipboardError> {
    #[cfg(target_os = "linux")]
    {
        linux::read_snapshot_inner(text_enabled, image_enabled)
    }
    #[cfg(target_os = "windows")]
    {
        windows::read_snapshot_inner(text_enabled, image_enabled)
    }
    #[cfg(target_os = "macos")]
    {
        macos::read_snapshot_inner(text_enabled, image_enabled)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = (text_enabled, image_enabled);
        Err(ClipboardError::Handle(
            "clipboard not supported on this platform".to_string(),
        ))
    }
}

/// Apply a `ClipboardPayload` to the local clipboard. The
/// `Clipboard` is constructed inside the function.
pub fn apply_payload(payload: &ClipboardPayload) -> Result<(), ClipboardError> {
    #[cfg(target_os = "linux")]
    {
        linux::apply_payload_inner(payload)
    }
    #[cfg(target_os = "windows")]
    {
        windows::apply_payload_inner(payload)
    }
    #[cfg(target_os = "macos")]
    {
        macos::apply_payload_inner(payload)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = payload;
        Err(ClipboardError::Handle(
            "clipboard not supported on this platform".to_string(),
        ))
    }
}
