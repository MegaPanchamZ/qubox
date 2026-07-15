# P1-9: Clipboard Sync (arboard)

Status: research complete, implementation pending.
Owner: `apps/host-agent` (host watcher) and `apps/client-cli` (client applier), with a new `clipboard` module in each.
Depends on: the existing reliable control stream (P0-2 already has one for NACK/rate feedback).
Blockers: none. arboard 3.4+ is mature and supports all our target platforms.

## Goal

Sync the host's clipboard to the client and vice versa, so when the user copies text or an image on one side, the other side's clipboard reflects the same content. Bidirectional, opt-in, and throttled. Latency target: <300 ms for a copy to propagate. Format support: text (UTF-8) and images (PNG round-trip). HTML is a follow-up.

## Research Summary

### arboard crate (3.4+ as of 2024-2026)

`arboard` is the de-facto cross-platform clipboard crate in Rust, a maintained fork of the older `rust-clipboard`.

- **Current version**: 3.4.1 (3.x line is the actively maintained branch).
- **Platforms**: Linux (X11 and Wayland), Windows, macOS. Wayland support requires the `wayland-data-control` feature.
- **API**: `Clipboard::new()`, `get_text()`, `set_text()`, `get_image()`, `set_image()`.
- **Image format**: `ImageData { width, height, bytes: Cow<[u8]> }` — RGBA8 in row-major order.
- **Thread-safety**: `Clipboard` instances are thread-local; multiple instances are allowed but must be dropped before program exit.
- **Lifecycle**: on X11 and Wayland, the clipboard is "hosted" in the application that last set the data. If our process exits, the data may become unavailable. The clipboard poller must stay alive.

Alternatives considered:
- `x11-clipboard` (Linux only, low-level, more control over ICCCM / ClipboardManager).
- `wl-clipboard-rs` (Wayland only).
- `clipboard` (older, unmaintained).
- `clipboard-master` (HTML support; less maintained).

`arboard` is the right choice for our use case: cross-platform, image support, mature.

### Per-platform clipboard APIs (background)

| Platform | Mechanism | Notes |
|----------|-----------|-------|
| Linux X11 | Selections (`PRIMARY`, `SECONDARY`, `CLIPBOARD`); `XConvertSelection` / `XSetSelectionOwner`; ICCCM + freedesktop ClipboardManager | Most complex — 3 selections + async ownership + ClipboardManager persistence |
| Linux Wayland | `wl_data_device` (MIME-typed data offers); primary-selection protocol extension | Privacy-preserving — apps can't read without explicit user paste |
| Windows | `OpenClipboard` / `GetClipboardData` / `SetClipboardData`; many formats (CF_TEXT, CF_UNICODETEXT, CF_BITMAP, CF_DIB, CF_HDROP) | Single logical clipboard, multiple registered formats |
| macOS | `NSPasteboard` (AppKit) or `UIPasteboard` (UIKit); items + types | macOS 10.14+ has privacy restrictions; some types require user consent |

`arboard` hides all of this behind a single API.

### Image clipboard

`arboard::ImageData` holds RGBA8 bytes. The wire format sends PNG-encoded (lossless compression); the receiver decodes PNG back to RGBA and calls `arboard::set_image`. Round-trip is exact.

For PNG, use the `png` crate (smallest, fast). For broader image format support, use the `image` crate.

### HTML clipboard

`arboard` does not have a first-class HTML API. To support HTML, we need a separate code path:

- **Linux X11**: `text/html` target in selections.
- **Linux Wayland**: `text/html` MIME in data offers.
- **Windows**: registered `CF_HTML` format.
- **macOS**: `NSPasteboardTypeHTML`.

For the first release, **support text + images only**. HTML is a follow-up using a per-platform crate or FFI.

### Wire format

```rust
#[derive(Serialize, Deserialize)]
pub enum ClipboardPayload {
    Text { seq: u64, utf8: String },
    ImagePng { seq: u64, width: u32, height: u32, png: Vec<u8> },
    Html { seq: u64, utf8_html: String },  // v2
}
```

- `seq` is a monotonic counter per direction; receiver applies only if `seq > last_seq`.
- Length-prefixed framing: 4-byte big-endian length + bincode payload.
- Sent over the existing reliable control stream (P0-2).
- 250 ms polling on the source side; send only on content change (blake3 hash on UTF-8 / PNG bytes).

### Latency

- Polling interval: 250 ms.
- Network: depends on the QUIC RTT; typically 5-50 ms.
- Apply: <10 ms.
- **End-to-end**: <300 ms. Acceptable for productivity workflows.

### Privacy / security

Clipboard sync is **opt-in**:
- **Off by default** for both directions.
- Per-direction toggle (host → client, client → host).
- Per-format toggle (text only, image only, both).
- "Sync only when triggered" mode: the user presses a hotkey to push the current clipboard.

A "sensitive content filter" is not reliable; the right answer is opt-in.

### Conflict resolution

- Each side maintains a `last_seq` per type.
- Receiver applies only if incoming `seq > last_seq`. Last-write-wins.
- This avoids flip-flopping when both sides copy in quick succession.

### Rust crate recommendations (2024-2026)

- `arboard` 3.4+
- `png` 0.17+ (PNG encode/decode)
- `image` 0.25+ (broader image support; PNG is enough for v1)
- `blake3` 1.5+ (fast content hashing)
- `serde` + `bincode` (wire format)

### 2024-2026 status

- `arboard` is actively maintained; Wayland support via `wayland-data-control` is stable.
- X11's `PRIMARY` vs `CLIPBOARD` selections: the convention is `CLIPBOARD` for explicit copy/paste, `PRIMARY` for mouse selection (middle-click paste). `arboard` defaults to `CLIPBOARD`.
- macOS 10.14+ has pasteboard privacy restrictions. Background clipboard polling may be limited. Some operations may trigger user prompts.

## Implementation Plan

### Step 1: Wire format

`crates/qubox-proto/src/lib.rs`:
- Add `ClipboardPayload` enum (Text, ImagePng, Html).
- Add to `ControlMsg` as a new variant: `ControlMsg::Clipboard { direction: ClipboardDirection, payload: ClipboardPayload }`.

### Step 2: Host-side watcher

`apps/host-agent/src/clipboard/mod.rs`:
- `pub struct ClipboardWatcher { clipboard: arboard::Clipboard, tx: tokio::sync::mpsc::Sender<ClipboardPayload>, last_text_hash: Option<[u8; 32]>, last_image_hash: Option<[u8; 32]>, seq: u64 }`.
- `pub fn new(tx: tokio::sync::mpsc::Sender<ClipboardPayload>) -> Result<Self>`.
- `pub fn run(self)` — spawns a thread that polls every 250 ms.
- On each poll, read text and image, hash, compare to last, send on change.

### Step 3: Client-side applier

`apps/client-cli/src/clipboard/mod.rs`:
- `pub struct ClipboardApplier { clipboard: arboard::Clipboard, last_seq: u64 }`.
- `pub fn apply(&mut self, payload: &ClipboardPayload) -> Result<()>` — applies text via `set_text`, image via `set_image` (PNG → RGBA decode).
- For each incoming `ControlMsg::Clipboard`, call `apply`.

### Step 4: Bidirectional sync

Both the host and the client run a watcher and an applier. The host's `ControlMsg::Clipboard` is sent host→client; the client's `ControlMsg::Clipboard` is sent client→host. Each side has its own `seq` counter.

### Step 5: Configuration

Add to `VideoStreamPreferences` (or a new `ClipboardConfig`):
- `clipboard_sync_enabled: bool` (default: false)
- `clipboard_direction: ClipboardDirection { HostToClient, ClientToHost, Both }`
- `clipboard_formats: ClipboardFormats { text: bool, image: bool, html: bool }`
- `clipboard_poll_interval_ms: u32` (default: 250)

CLI flag: `--clipboard-sync {off,host-to-client,client-to-host,both}` and `--clipboard-formats {text,image,both}`.

### Step 6: Tests

- Unit test: `ClipboardPayload` serde round-trip.
- Unit test: image round-trip (RGBA → PNG → RGBA) is lossless.
- Integration test: set text on the host, verify the client receives it within 500 ms.
- Integration test: set image on the host, verify the client receives it.
- Privacy test: when `clipboard_sync_enabled = false`, no `ControlMsg::Clipboard` is sent.

## Risks and Open Questions

- **Sensitive content**: opt-in is the right answer. Don't try to filter passwords / credit cards by regex — it's unreliable. Document the risk.
- **HTML support**: deferred. arboard doesn't have it. Per-platform code paths are substantial.
- **File clipboard**: Windows has `CF_HDROP` for file lists. We can sync file paths but not the file contents (security: don't send files). Defer.
- **Large images**: 4K screenshot is ~30 MB RGBA, ~10 MB PNG. Polling at 250 ms with 10 MB PNG sends ~40 MB/s if the user is taking screenshots. Add a size cap (e.g. 1 MB) and refuse to sync larger images.
- **Wayland privacy**: Wayland apps can't read the clipboard without the user performing a paste in the app. This is a security feature, not a bug. Our "read on the host" is fine because the host's compositor is the one our process is running under; we have access. But if a Wayland user copies something on the host, the arboard watcher on the host can read it (it's the host's compositor). On the client side, our app is in the client's session and can read the client's clipboard normally.
- **arboard and X11 clipboard managers**: arboard on X11 may not play well with `xclip` / `xsel` / Klipper. The clipboard manager reifies clipboard data; arboard's data may be stored in the manager or in the original owner. Test with Klipper and GNOME's clipboard.
- **macOS pasteboard privacy**: macOS 10.14+ may prompt the user the first time the app reads the clipboard. Document this.
- **Format conversion quality**: arboard's image bytes are RGBA, but the source app may have stored the image in a different format (e.g. YUV with an alpha channel). For game streaming use cases (PNG screenshots, color-picker output), RGBA is the common case. Document edge cases.
- **Per-clipboard "lock"**: when the user copies a 1 MB image and the network is slow, the next 250 ms poll may overwrite the in-flight image. Use sequence numbers to drop stale updates; receiver applies only if `seq > last_seq`.

## References

- arboard on GitHub: https://github.com/1Password/arboard
- arboard on crates.io: https://crates.io/crates/arboard/3.4.1
- arboard docs: https://docs.rs/arboard
- arboard Clipboard struct: https://docs.rs/arboard/latest/arboard/struct.Clipboard.html
- OpenRR arboard wrapper: https://openrr.github.io/openrr/arboard/struct.Clipboard.html
- X11 clipboard spec: https://www.freedesktop.org/wiki/Specifications/ClipboardsWiki/
- X11 clipboard management: https://jameshunt.us/writings/x11-clipboard-management-foibles/
- Cross-platform clipboard library writeup: https://jtanx.github.io/2016/08/19/a-cross-platform-clipboard-library/
- Arch wiki on Clipboard: https://wiki.archlinux.org/title/Clipboard
- Perplexity research, 2026-07-02: arboard API, per-platform clipboard, image/HTML, wire format, 2024-2026 status.
