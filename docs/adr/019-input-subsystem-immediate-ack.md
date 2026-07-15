# ADR-019 Input Subsystem: Reliable Stream with IMMEDIATE_ACK + Volatile Datagram

## Status

Proposed. Branch: `feature/adr-019-input-immediate-ack`. Based on `main`
after commit `47585ea`. Builds on ADR-011 (ACK-Frequency extension,
`draft-ietf-quic-ack-frequency-14`, target RFC late 2026/early 2027),
ADR-015 (rkyv zero-copy, landed as `rkyv = "0.8.17"` + `bytecheck =
"0.8.2"` in root `Cargo.toml`), ADR-010 §1.2 (pen wire format) and
ADR-010 §4 (host pen injection via Wintab/`uinput`). Closes P2-15 (Pen
high-rate 240 Hz), tightens the keyboard/mouse path, and adds
`crates/qubox-pen` host injection on Windows + Linux.

## Context

Today the input subsystem is implemented as a **single reliable QUIC
stream** that carries all `RemoteInputEvent` variants:

- Client → Host: `NativeQuicInputSender::send_input_event` at
  `crates/qubox-transport/src/lib.rs:591-602`.
- Host → Client: `NativeQuicInputReceiver::read_input_event` at
  `crates/qubox-transport/src/lib.rs:618-635`.
- The events are serialized as JSON via `NativeQuicControlMessage` over
  `write_json_prefixed` / `read_json_prefixed` at
  `crates/qubox-transport/src/lib.rs:1557-1602`.
- The shared **datagram discriminator** family is defined in ADR-010
  §1.2 (`0x47` gamepad, `0x4D` mic, `0x50` pen), implemented at
  `crates/qubox-proto/src/pen.rs:26` (`PEN_DATAGRAM_DISCRIMINATOR`).
- `RemoteInputInjector` lives at
  `apps/qubox-host-agent/src/main.rs:311-509` (constructed at `:462-489`,
  application loop at `:491-509`, work-thread at `:1363-1431`).
- The enigo 0.6 (MIT, `enigo = "0.6.0"` in workspace) input simulation
  is wired into the same injector (`use enigo::{...}` at
  `apps/qubox-host-agent/src/main.rs:17`).

The current setup has three problems:

1. **Latency floor is one RTT.** ACK-Frequency extension
   (`draft-ietf-quic-ack-frequency-14`, IMMEDIATE_ACK frame type
   `0x1f`) is not yet wired into `qubox-transport`. Quinn 0.11.5
   already exposes it as `TransportConfig::ack_frequency_config(Option<AckFrequencyConfig>)`
   (source: `quinn-proto/src/config/transport.rs`), but
   `build_transport_config` at `crates/qubox-transport/src/lib.rs:1866-1879`
   does not call it. Default is `max_ack_delay=25 ms`, so the host
   doesn't see an ACK until the natural 25 ms threshold. For pen at
   240 Hz this is catastrophic — a pen-down stroke terminator sits in
   the host read buffer for up to one RTT before the client knows the
   host is ready.
2. **HOL blocking on motion events.** A 240 Hz pen produces ~25 000
   motion events/sec; losing one is harmless (the next supersedes),
   but every event today rides the reliable stream, so a single loss
   blocks all subsequent events until retransmission.
3. **JSON encoding overhead.** `serde_json::to_vec` of
   `RemoteInputEvent` averages ~1 µs/event on x86-64 → ~2.5 % of one
   CPU core at 240 Hz pen. ADR-015 fixes this for the **datagram**
   path (the 36-byte `WirePenEvent` is already `#[repr(C, packed)]`),
   but the **reliable** input stream path is still JSON.

## Decision

### 1. Two-tier input path

| Tier                  | Path                           | Events                                              | Encoding              |
|-----------------------|--------------------------------|-----------------------------------------------------|-----------------------|
| **Critical (reliable)**| reliable QUIC stream            | keyboard keys, mouse buttons, gamepad connect/disconnect + rumble, pen-down/pen-up terminators, gamepad hotplug | rkyv 0.8.17 archive, `[u32 len][bytes…]` length-prefixed |
| **Volatile (datagram)**| QUIC unreliable datagram        | mouse motion, pen motion in air, gamepad axis (states), mic audio | rkyv 0.8.17 archive (existing 36-byte `WirePenEvent` family + new `0x4B` discriminator for mouse motion) |

Discriminator allocation (cross-reference ADR-010 §1.2 + §13):
- `0x47` `'G'` — gamepad axis / button state (existing)
- `0x4B` `'K'` — **new**, keyboard/mouse motion volatile datagrams
- `0x4D` `'M'` — mic audio (existing)
- `0x50` `'P'` — pen motion (existing)
- `0x5F` reserved — future expansion

### 2. ACK-Frequency integration

We pick `AckPolicy::InputImmediate` for the reliable input stream.
quinn 0.11.5 exposes this via `TransportConfig::ack_frequency_config(
Option<AckFrequencyConfig>)`. The struct fields are
`ack_eliciting_threshold: u8`, `max_ack_delay: Duration`,
`reordering_threshold: u8`, `sequence_number: u64` (matches the
draft-14 wire format from IANA registry).

Configuration pseudocode (to be inserted into
`build_transport_config` at `crates/qubox-transport/src/lib.rs:1866-1879`,
plus a new `AckPolicy` enum at `:97-108` per ADR-011 §2):

```rust
use std::time::Duration;
use quinn_proto::AckFrequencyConfig;

pub enum AckPolicy {
    /// Sparse ACKs (media). min_ack_delay = 25 ms.
    Media,
    /// Dense ACKs (control). min_ack_delay = 1 ms.
    Control,
    /// Immediate ACK on FLAG_LAST_IN_BURST. min_ack_delay = 1 ms
    /// AND ack_eliciting_threshold = 1. Used for input.
    InputImmediate,
}

fn ack_frequency_config_for(policy: AckPolicy) -> Option<AckFrequencyConfig> {
    match policy {
        AckPolicy::Media => Some(AckFrequencyConfig {
            ack_eliciting_threshold: 10,                 // 1:10 ACK ratio per ack-scaling draft
            max_ack_delay: Duration::from_micros(10_000), // 10 ms
            reordering_threshold: 1,
            sequence_number: 0,
        }),
        AckPolicy::Control => Some(AckFrequencyConfig {
            ack_eliciting_threshold: 1,
            max_ack_delay: Duration::from_micros(1_000), // 1 ms
            reordering_threshold: 1,
            sequence_number: 0,
        }),
        AckPolicy::InputImmediate => Some(AckFrequencyConfig {
            ack_eliciting_threshold: 1,                  // every packet elicits
            max_ack_delay: Duration::from_micros(1_000), // 1 ms
            reordering_threshold: 1,
            sequence_number: 0,
        }),
    }
}
```

The `IMMEDIATE_ACK` frame is sent explicitly on the **reliable input
stream** when the host detects a `WirePenEvent` carrying
`FLAG_LAST_IN_BURST = 1` (defined at `crates/qubox-proto/src/pen.rs:46`).
On the wire it is a single-byte frame `[0x1f]` (IANA assignment; was
`0xac` in draft-05, finalized to `0x1f` in draft-14). It triggers the
sender to ACK immediately, collapsing one RTT off the reverse
ACK-and-source-signal path during pen-down → host-ACK roundtrips.

Because quinn 0.11.5 does **not** yet expose a public method to
construct an `IMMEDIATE_ACK` frame from application code, we bridge
the behavior through a **dedicated per-connection background task**
that listens on the connection's stream events and, on
`FLAG_LAST_IN_BURST`, forces an immediate ACK by sending a
zero-length PING frame on the input stream. PING is the pre-ACK-Frequency
equivalent and is already supported by `quinn::Connection::ping()`. As
upstream quinn lands an explicit `send_immediate_ack()`
API on `quinn::Connection` (tracked at
`github.com/quinn-rs/quinn/issues/1014`), we replace the PING
fallback with the direct call.

**Round-trip latency target.** End-to-end pen-down → host ACK < 10 ms
(compared to ~25 ms today). Measured by hooking the `tracing` span
`input.reliable.pen.roundtrip` (see §12 Verification commands).

### 3. rkyv migration

#### Workspace dependencies

Add to root `Cargo.toml` (lines 26-30 area, after the `serde` /
`serde_json` block):

```toml
# rkyv 0.8 zero-copy serialization per ADR-015 §1 / ADR-019 §3
rkyv = { version = "0.8.17", features = ["std", "validation"] }
bytecheck = "0.8.2"
```

`serde` is kept for backwards compatibility with `ControlMsg` (JSON on
the control stream) and `PenDeviceDescriptor` (JSON, session start).

#### Type migration: `RemoteInputEvent`

File: `crates/qubox-proto/src/lib.rs:815-888`.

**BEFORE (current, JSON-on-reliable-stream only):**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteInputEvent {
    MouseMove       { x: u32, y: u32 },
    RelativeMouseMove { dx: i32, dy: i32 },
    MouseButton     { button: InputMouseButton, pressed: bool },
    MouseWheel      { dx: i32, dy: i32 },
    Keyboard        { key: String, pressed: bool },
    Gamepad         { state: WireGamepadState },
    HoverDisplay    { display_id: u32 },
    Pen {
        #[serde(default)] tool: PenTool,
        #[serde(default)] pressure: f32,
        #[serde(default)] tilt_x: f32,
        #[serde(default)] tilt_y: f32,
        #[serde(default)] rotation: f32,
        #[serde(default)] button_state: u32,
        x: u16, y: u16,
        #[serde(default)] hover_distance: u16,
    },
}
```

**AFTER (rkyv-archived, used on the reliable input stream only — gamepad
axis, mouse motion, etc. stay on the datagram path):**

```rust
use rkyv::{Archive, Deserialize, Serialize};
use bytecheck::CheckBytes;

// New type — only the **critical** tier of events rides the reliable
// stream. Mouse motion and gamepad axis go via datagrams (separate
// rkyv archive types already on the datagram path).
#[derive(Debug, Clone, PartialEq, Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes, Debug, PartialEq))]
#[repr(u8)]
pub enum RemoteCriticalInput {
    MouseButton {                                 // discriminant 1
        button: rkyv::option::Archived<InputMouseButton>, // 1 byte
        pressed: bool,
    },
    Keyboard {                                     // discriminant 2
        // 16-byte fixed string for low-overhead copy — enigo keys
        // are mostly ASCII glyphs.
        key: [u8; 16],
        key_len: u8,                               // valid prefix length
        pressed: bool,
    },
    PenDown {                                      // discriminant 3
        device_id: u16,
        tool: rkyv::option::Archived<PenTool>,     // 1 byte (u8 wire id)
        x: u16, y: u16,
        pressure: f32,
        tilt_x: f32, tilt_y: f32,
        rotation: f32,
        button_state: u32,
        timestamp_us: u32,
    },
    PenUp {                                        // discriminant 4
        device_id: u16,
        tool: rkyv::option::Archived<PenTool>,
        timestamp_us: u32,
    },
    GamepadLifecycle {                             // discriminant 5
        connect: bool,
        device_id: u16,
        vendor_id: u16,
        product_id: u16,
    },
    MouseDown {                                    // discriminant 6 — pressed=true
        button: rkyv::option::Archived<InputMouseButton>,
        x: u16, y: u16,
        timestamp_us: u32,
    },
    MouseUp {                                      // discriminant 7
        button: rkyv::option::Archived<InputMouseButton>,
        x: u16, y: u16,
        timestamp_us: u32,
    },
    MouseClick {                                   // discriminant 8 — atomic down+up
        button: rkyv::option::Archived<InputMouseButton>,
        x: u16, y: u16,
        timestamp_us: u32,
    },
}

// Archived alias for hot-path readers — they access fields directly
// without paying deserialize cost.
pub type ArchivedRemoteCriticalInput =
    <RemoteCriticalInput as Archive>::Archived;
```

The old `RemoteInputEvent` enum stays in the crate as a
**compatibility shim** for any internal / test usage that still emits
JSON (`crates/qubox-proto/src/lib.rs:1179-1208` already covers the
`mouse_button` serde round-trip — it must keep working).

**Wire format on the reliable input stream:**

```
[0x00]                 // 1-byte WireInputMagic ('V'=0x56 currently,
                       // we use 0x52 to align with ADR-015 §4
                       // "rkyv-v2 era" reserved magic)
[0x52]                 // ditto — together [0x52][0x42]
[u32 LE len]           // rkyv archive length, big-endian? NO, little-
                       // endian to match `write_u32` already used by
                       // write_json_prefixed at lib.rs:1557-1566
[rkyv archive bytes…]  // bytes() from `rkyv::to_bytes::<RemoteCriticalInput>(
                       //     value,
                       //     AllocSerializer::<256>::default()
                       // )`
```

This matches the pattern in rkyv docs (`rkyv.org/format.html`):
`AllocSerializer` for ≤256-byte archives is zero-allocation after the
first allocation, and bytecheck validation on the receive side
(`check_archived_root`) is ~100 ns per archive in practice.

#### Helpers

Add to `crates/qubox-transport/src/lib.rs:1557-1602` next to the
existing JSON helpers:

```rust
const WIRE_INPUT_MAGIC: [u8; 2] = [0x52, 0x42];
const MAX_RKYV_FRAME: usize = 256 * 4; // 1024 B upper bound for safety

async fn write_rkyv_prefixed<W, T>(
    writer: &mut W,
    magic: [u8; 2],
    value: &T,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWriteExt + Unpin,
    T: rkyv::Serialize<AllocSerializer<256>>,
{
    let bytes = rkyv::to_bytes(value)
        .map_err(|error| anyhow::anyhow!("rkyv serialize failed: {error}"))?;
    let bytes = bytes.into_serializer().into_inner();
    if bytes.len() > MAX_RKYV_FRAME {
        anyhow::bail!("rkyv frame too large: {} bytes", bytes.len());
    }
    let len: u32 = bytes.len() as u32;
    writer.write_all(&magic).await?;
    writer.write_u32(len).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn maybe_read_rkyv_prefixed<R, T>(
    reader: &mut R,
) -> anyhow::Result<Option<T>>
where
    R: tokio::io::AsyncReadExt + Unpin,
    T: rkyv::Archive,
    T::Archived: rkyv::CheckBytes<DefaultValidator<'static>>,
{
    let mut magic = [0u8; 2];
    if let Err(error) = reader.read_exact(&mut magic).await {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            return Ok(None);
        }
        return Err(error.into());
    }
    let len = reader.read_u32().await? as usize;
    if len > MAX_RKYV_FRAME {
        anyhow::bail!("rkyv frame too large: {len} bytes");
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    rkyv::from_bytes::<T>(&buf)
        .map(Some)
        .map_err(|error| anyhow::anyhow!("rkyv/bytecheck validation failed: {error}"))
}
```

### 4. Partial reliability for motion

Mouse motion is added to the **existing datagram discriminator family**
with a new `0x4B` byte. The wire format is a thin rkyv struct (not the
36-byte `WirePenEvent` shape, since mouse motion has fewer axes):

```rust
// crates/qubox-proto/src/lib.rs (new type)
pub const MOUSE_MOTION_DISCRIMINATOR: u8 = 0x4B;

#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[archive_attr(derive(CheckBytes, Debug, PartialEq))]
#[repr(C, packed)]
pub struct WireMouseMotion {
    pub magic:     [u8; 2],   // [0x51, 0x42] MEDIA_DATAGRAM_MAGIC
    pub discriminator: u8,    // 0x4B
    pub flags:     u8,        // future (button_state nibble)
    pub dx:        i16,       // relative motion dx since last sample
    pub dy:        i16,       // relative motion dy
    pub timestamp_us: u32,     // wraps every ~71 minutes
}
pub const WIRE_MOUSE_MOTION_SIZE: usize = 12;
```

Datagrams are sent via `quinn::Connection::send_datagram(...)` from the
client to the host. The host receives via `quinn::Endpoint::datagrams()`
or a per-connection datagram receiver wrapper.

Sampling: mouse motion is sampled at the local pointer event rate
(typically 125 Hz / 500 Hz / 1000 Hz on a desktop) and queued through
the same `InputCoalescer` defined in §5 below. Loss is tolerated.

### 5. Batching and coalescing

`crates/qubox-pen/src/coalesce.rs` already implements a `PenCoalescer`
(239 lines as of this writing, see `:55-120` for `CoalesceConfig` and
the `run` loop). We **extract** the core coalescing logic into a new
shared module `crates/qubox-input-coalescer` (or just lift into
`crates/qubox-proto/src/coalescer.rs`) so mouse, pen, and gamepad all
share it:

```rust
// crates/qubox-proto/src/coalescer.rs (NEW — add to qubox-proto)
use std::time::{Duration, Instant};

pub const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_micros(1000); // 1 ms

/// State machine used by all event-source adapters.
/// Flush reasons — keep them in stable order for telemetry.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FlushReason {
    /// 1 ms window elapsed since first event.
    Deadline,
    /// OS input queue drained (rx returned Empty).
    QueueEmpty,
    /// Caller passed FLAG_LAST_IN_BURST (pen-down terminator, etc.).
    BurstTerminator,
}

#[derive(Debug)]
pub struct InputCoalescer<E: Copy + rkyv::Archive> {
    pending: Vec<(E, Instant)>,
    last_event_time: Instant,
    window: Duration,
    /// Max events the survivor can hold.
    max_events: usize,
}

impl<E: Copy + rkyv::Archive> InputCoalescer<E> {
    pub fn new(window: Duration, max_events: usize) -> Self {
        Self {
            pending: Vec::with_capacity(max_events),
            last_event_time: Instant::now(),
            window,
            max_events,
        }
    }

    pub fn push(&mut self, event: E) {
        let now = Instant::now();
        if self.pending.is_empty() { self.last_event_time = now; }
        if self.pending.len() < self.max_events {
            self.pending.push((event, now));
        } else {
            self.pending[self.pending.len() - 1] = (event, now);
        }
    }

    /// Flush conditions:
    /// - 1 ms since first event (Deadline), OR
    /// - caller flags FLAG_LAST_IN_BURST (BurstTerminator).
    pub fn should_flush(&self, has_burst_flag: bool) -> Option<FlushReason> {
        if has_burst_flag { return Some(FlushReason::BurstTerminator); }
        if self.pending.is_empty() { return None; }
        let now = Instant::now();
        if now.duration_since(self.last_event_time) >= self.window {
            Some(FlushReason::Deadline)
        } else { None }
    }

    /// Drain `self.pending` into a single rkyv archive `Vec<ArchivedE>`.
    /// Returns the byte buffer ready to ship on the QUIC stream.
    pub fn flush(
        &mut self,
        archive_fn: impl FnOnce(&[E]) -> rkyv::ser::Serializer<
            rkyv::ser::serializers::AllocSerializer<1024>,
            rkyv::ser::Pos,
        >,
    ) -> Vec<u8> {
        let events: Vec<E> = self.pending.iter().map(|(e, _)| *e).collect();
        self.pending.clear();
        archive_fn(&events)
            .into_serializer()
            .into_inner()
    }
}
```

**Per-device flush triggers.**

| Source            | Window           | Additional flush trigger                |
|-------------------|------------------|------------------------------------------|
| Mouse motion      | 1 ms             | none (low-rate; 1 ms window suffices)   |
| Pen (240 Hz)      | 1 ms             | `FLAG_LAST_IN_BURST` from libinput/WinTab/NSEvent |
| Gamepad axis      | 4 ms             | (vsync-aligned; same as P0-6 gamepad)    |

**Multiple events per QUIC packet.** The coalecer's `flush` returns a
single rkyv archive of a `Vec<E>` so each QUIC packet carries up to
~16-20 mouse events or ~16-20 pen events coalesced into one
`[magic][u32 len][archive bytes]` triple on the reliable path, or one
`[magic][discriminator][payload]` triple per datagram on the volatile
path. We cap `max_events = 16` per flush so a single packet never
exceeds ~512 bytes.

### 6. Host injection

The existing `RemoteInputInjector` struct at
`apps/qubox-host-agent/src/main.rs:311-317` and its `apply()` method at
`:491-509` are preserved exactly as they are. Two additions:

#### 6.1 Add `apply_datagram`

Add to `apps/qubox-host-agent/src/main.rs:309` (immediately after the
existing `RemoteInputInjector` struct definition):

```rust
impl RemoteInputInjector {
    /// Handle an unreliable QUIC datagram.
    /// - Discriminator `0x50` (`WirePenEvent`): bypass enigo and inject
    ///   via the platform-specific `PenInjector` (`uinput` / Wintab /
    ///   NSEvent). Per ADR-010 project rule #4 — pen never goes through
    ///   enigo.
    /// - Discriminator `0x47` (`WireGamepadState`): existing P0-6 gamepad
    ///   path (already uses `uinput`/`vigembus`).
    /// - Discriminator `0x4B` (mouse motion, NEW): enigo
    ///   `move_mouse(dx, dy, Coordinate::Rel)`.
    /// - Discriminator `0x4D` (mic): ignored on the host — mic is
    ///   sender-side only.
    fn apply_datagram(&mut self, datagram: &[u8]) -> anyhow::Result<()> {
        if datagram.len() < 3 { return Ok(()); } // ignore noise
        if datagram[0..2] != [0x51, 0x42] { return Ok(()); }
        match datagram[2] {
            0x50 /* pen */ => {
                let event = WirePenEvent::from_bytes(datagram)?;
                self.pen_injector.inject(&pen_event_to_pen_event(&event))
            }
            0x4B /* mouse motion */ => {
                let motion = WireMouseMotion::from_bytes(datagram)?;
                self.enigo
                    .move_mouse(motion.dx as i32, motion.dy as i32, Coordinate::Rel)
                    .map_err(|error| anyhow::anyhow!("failed to inject motion: {error}"))
            }
            0x47 /* gamepad — already wired in P0-6 */ => Ok(()),
            _ => Ok(()),
        }
    }

    /// Handle a reliable-stream `RemoteCriticalInput` event.
    /// 1:1 with the rkyv-archived enum from §3.
    fn apply_stream(&mut self, event: &RemoteCriticalInput) -> anyhow::Result<()> {
        match event {
            RemoteCriticalInput::MouseButton { button, pressed } => {
                self.enigo.button(map_mouse_button(*button), if *pressed { Press } else { Release })
                    .map_err(|e| anyhow::anyhow!("mouse button: {e}"))
            }
            RemoteCriticalInput::Keyboard { key, key_len, pressed } => {
                let s = std::str::from_utf8(&key[..*key_len as usize])?;
                let Some(mapped) = map_remote_key(s) else { return Ok(()); };
                self.enigo.key(mapped, if *pressed { Press } else { Release })
                    .map_err(|e| anyhow::anyhow!("key: {e}"))
            }
            RemoteCriticalInput::PenDown { .. } | RemoteCriticalInput::PenUp { .. } => {
                // pen-down / pen-up via platform-native path (uinput/Wintab/NSEvent)
                self.pen_injector.inject(&pen_event_to_pen_event(...))
            }
            // ... other variants
        }
    }
}
```

#### 6.2 Dedicated injector thread (CRITICAL)

Because `Enigo` from `enigo 0.6.0` (latest on crates.io as of this
writing; 0.6.1 is not released — the workspace pin is
`enigo = "0.6"` which resolves to 0.6.0) wraps platform FFI handles
and is `!Send + !Sync` (the relevant auto-trait impls are not
advertised in the public API; treat as not-thread-safe and assume the
worst), the existing `spawn_remote_input_worker` at
`apps/qubox-host-agent/src/main.rs:1363-1431` **must remain a regular
OS thread (`std::thread::Builder`)**, not a `tokio::spawn` task. The
existing implementation already does this — leave it alone.

#### 6.3 Pen injection platform support

Add a new module `apps/qubox-host-agent/src/pen_injector.rs`
(companion to `input_injector.rs`) that wraps `qubox-pen`'s
`PenInjector` trait:

```rust
// apps/qubox-host-agent/src/pen_injector.rs  (NEW)
use qubox_pen::{PenInjector, PenEvent};
pub struct HostPenInjector { inner: PlatformPenInjector }
impl HostPenInjector {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self { inner: PlatformPenInjector::new()? })
    }
}
impl PenInjector for HostPenInjector {
    fn inject(&mut self, event: &PenEvent) -> anyhow::Result<()> {
        self.inner.inject(event)
    }
    fn device_name(&self) -> &str { self.inner.device_name() }
}
```

`PlatformPenInjector` is the cfg-gated struct from
`crates/qubox-pen/src/{linux,windows,macos}.rs` (see §7 below).
On Linux: `uinput` (`uinput = "0.1.0"`).
On Windows: `wintab_lite` (`wintab_lite = "0.2.1"`) plus `windows`
crate `Win32_UI_TabletPC`.
On macOS: deferred (no `uinput`/`wintab`; would require private
`TabletEvent` injection or `[NSEvent+CGEventPost]` workaround).

### 7. Client capture — `crates/qubox-pen` layout

The crate already has `linux.rs`, `windows.rs`, `coalesce.rs`,
`platform.rs`, `traits.rs`, `error.rs`. Two changes:

- `macos.rs` (new) — NSEvent binding via `objc2 = "0.5.1"` +
  `objc2-app-kit = "0.2.0"` + `objc2-foundation = "0.2.0"`.
- `linux.rs` (extend) — replace the TODO in `LibinputCapture::new` at
  `crates/qubox-pen/src/linux.rs:28-40` with a real seat open.

Final layout:

```
crates/qubox-pen/src/
├── linux.rs      # libinput = "0.6.0" capture + uinput = "0.1.0" inject
├── windows.rs    # wintab_lite = "0.2.1" capture + windows 0.62 inject
├── macos.rs      # NEW — objc2/objc2-app-kit NSEvent capture (inject deferred)
├── coalesce.rs   # existing PenCoalescer
├── traits.rs     # PenCapture, PenInjector, PenDeviceInfo, PenEvent
├── platform.rs   # cfg-gated dispatcher
├── error.rs      # PenCaptureError, PenInjectError
└── lib.rs
```

#### 7.1 Linux (`crates/qubox-pen/src/linux.rs`)

The `input = "0.6.0"` Rust binding to libinput exposes
`Event::TabletTool(TabletToolEvent)` with methods
`x() -> f64`, `y() -> f64`, `pressure() -> f32`,
`distance() -> f32`, `tilt() -> (f32, f32)`, `rotation() -> f32`,
`button() -> u32`. Each event becomes a `PenEvent`:

```rust
use input::{Libinput, LibinputInterface, event::Event};
use std::fs::File;
use std::os::unix::fs::FileExt;

struct UdevIf;
impl LibinputInterface for UdevIf {
    fn open_restricted(&mut self, path: &std::path::Path) -> Result<File, std::io::Error> {
        File::open(path)
    }
    fn close_restricted(&mut self, fd: File) { drop(fd); }
}

pub struct LibinputCapture;

impl PenCapture for LibinputCapture {
    fn start(&mut self, tx: Sender<PenEvent>) -> Result<Receiver<PenEvent>, PenCaptureError> {
        let (out_tx, out_rx) = crossbeam_channel::bounded(64);
        std::thread::Builder::new()
            .name("bp-pen-libinput".into())
            .spawn(move || -> anyhow::Result<()> {
                let mut li = Libinput::new_with_udev(UdevIf)?;
                li.udev_assign_seat("seat0")?;
                loop {
                    li.dispatch()?;
                    for event in li.events() {
                        if let Event::TabletTool(t) = event {
                            let pe = PenEvent {
                                tool: map_tool(t.tool_type()),
                                x: t.x() as u16, // clip to screen
                                y: t.y() as u16,
                                pressure: t.pressure(),
                                tilt_x: t.tilt().0,
                                tilt_y: t.tilt().1,
                                rotation: t.rotation(),
                                timestamp_us: now_us(),
                                hover_distance: (t.distance() * 16.0) as u8,
                                button_state: t.button(),
                                flags: PenEventFlags::empty(),
                            };
                            out_tx.send(pe).ok();
                        }
                    }
                }
            })?;
        Ok(out_rx)
    }
}
```

For injection, the **existing** `UinputInjector` stub at
`crates/qubox-pen/src/linux.rs:97-146` is filled in using the
`uinput = "0.1.0"` crate with the API from crates.io:

```rust
use uinput::{
    Device, event::absolute::{Position, Pressure, Misc},
    event::button::{Button, Tool},
};

pub struct UinputInjector {
    dev: Device,
    name: String,
}

impl UinputInjector {
    pub fn new(name: impl Into<String>) -> anyhow::Result<Self> {
        let name = name.into();
        let dev = uinput::default()?
            .name(&name)?
            .event(Position::X)?     // ABS_X
            .event(Position::Y)?     // ABS_Y
            .event(Pressure::Pressure)? // ABS_PRESSURE
            .event(Misc::Distance)?  // ABS_DISTANCE
            .event(Misc::TiltX)?     // ABS_TILT_X
            .event(Misc::TiltY)?     // ABS_TILT_Y
            .event(Button::Left)?    // BTN_TOUCH (pen tip)
            .event(Tool::Pen)?       // BTN_TOOL_PEN
            .event(Tool::Rubber)?    // BTN_TOOL_RUBBER
            .create()?;
        Ok(Self { dev, name })
    }
}

impl PenInjector for UinputInjector {
    fn inject(&mut self, event: &PenEvent) -> anyhow::Result<()> {
        self.dev.send_events([
            Position::X.evt(event.x.into()),
            Position::Y.evt(event.y.into()),
            Pressure::Pressure.evt((event.pressure * 1024.0) as i32),
            Button::Left.evt(event.pressure > 0.001),
        ])?;
        self.dev.synchronize()?;
        Ok(())
    }
    fn device_name(&self) -> &str { &self.name }
}
```

#### 7.2 Windows (`crates/qubox-pen/src/windows.rs`)

The existing stub at `crates/qubox-pen/src/windows.rs` (file present
per §1.1 of this ADR) is filled in using:

- **`wintab_lite = "0.2.1"`** for `WTOpenW`, `WTInfoW`, `WTPacket`,
  the `PACKET { pkX, pkY, pkNormalPressure, pkTangentPressure,
  pkOrientation { azimuth, altitude, twist }, pkRotation, pkCursor,
  pkButtons, pkTime }` struct, the `WTM_xxx` message constants
  (notably `WT_PACKET`), and `LOGCONTEXTW`.
- **`windows = "0.62"`** (already wired for `Win32_UI_TabletPC`) for
  the modern `WM_POINTERUPDATE` path via
  `POINTER_PEN_INFO { pointerInfo { pointerType, ptPixelLocation },
  pressure, rotation, tiltX, tiltY }`, retrieved through
  `GetPointerPenInfo(pointer_id, ...)` after extracting
  `GET_POINTERID_WPARAM(wparam)` from the message.

For pen **injection** (host side), we delegate to
`wintab_lite`'s `WTPacket()` write-side companion or — if Wintab has no
public injection API — to `windows::Win32::UI::Input::KeyboardAndMouse::SendInput`
through a stylus-aware driver such as Wacom's Wintab32 driver shim
(which is shipped as part of every Wacom driver install and forwards
stylus events to the GPU compositor chain). If we cannot inject
pen-up events through Wintab (it is capture-only), we fall back to
**sending a synthetic PointerMessage via `InjectTouchInput`** for the
pen tip on/off transitions, which Windows Ink accepts. This is the
"ergonomically acceptable" fallback used by Parsec on Windows for pen
input.

#### 7.3 macOS (`crates/qubox-pen/src/macos.rs`, new)

Cargo.toml additions:

```toml
[target.'cfg(target_os = "macos")'.dependencies]
objc2 = "0.5.1"
objc2-foundation = "0.2.0"
objc2-app-kit = "0.2.0"
```

Capture skeleton:

```rust
use objc2::{msg_send, class};
use objc2_app_kit::{NSEvent, NSEventType, NSTilt};
use objc2_foundation::MainThreadMarker;

pub struct NseventCapture;
impl PenCapture for NseventCapture {
    fn start(&mut self, tx: Sender<PenEvent>) -> Result<Receiver<PenEvent>, PenCaptureError> {
        let (out_tx, out_rx) = crossbeam_channel::bounded(64);
        std::thread::Builder::new()
            .name("bp-pen-nsevent".into())
            .spawn(move || {
                // Must run on the main thread for NSEvent polling.
                let mtm = MainThreadMarker::new()
                    .expect("NSEvent capture must run on the main thread");
                let app = unsafe { msg_send![class!(NSApplication), sharedApplication] };
                let _ = unsafe { msg_send![app, setActivationPolicy: 1i64] }; // 1 = accessory
                loop {
                    let event = unsafe { msg_send![class!(NSEvent), 
                        nextEventMatchingMask: 0xFFFF_FFFFu64,
                        untilDate: std::ptr::null::<objc2::runtime::AnyObject>(),
                        inMode: class!(NSDefaultRunLoopMode),
                        dequeue: true] };
                    if event.is_null() { continue; }
                    let ev: &NSEvent = unsafe { &*event.cast::<NSEvent>().cast_const() };
                    let event_type = unsafe { msg_send![ev, type] };
                    if event_type == NSEventType::TabletPoint {
                        let pressure: f32 = unsafe { msg_send![ev, pointPressure] };
                        let tilt: NSTilt = unsafe { msg_send![ev, tilt] };
                        let rotation: f32 = unsafe { msg_send![ev, rotation] };
                        let device_id: u32 = unsafe { msg_send![ev, deviceID] };
                        let abs_x: f64 = unsafe { msg_send![ev, absoluteX] };
                        let abs_y: f64 = unsafe { msg_send![ev, absoluteY] };
                        let tx_clone = out_tx.clone();
                        tx_clone.send(PenEvent { /* … */ }).ok();
                    }
                }
            })?;
        Ok(out_rx)
    }
}
```

**Mandatory TCC permission:** the user (or `.pkg` installer) must
grant the binary the **Input Monitoring** permission under *System
Settings → Privacy & Security → Input Monitoring*. Without this, no
tablet events arrive. Document this in the host-agent install README.

Injection on macOS is **deferred**. There is no public CoreGraphics
API for synthesizing `NSEventTypeTabletPoint`. We probe via
`CGEventCreateMouseEvent` as a last-resort fallback (motion only,
loses pressure/tilt fidelity), and the host logs
`tracing::warn!("macOS pen injection fallback to mouse motion")`. The
deferred work is tracked as P3 work (private
`Carbon.HITextViewCreate` or `CGEventPost` extended-event variants).

### 8. Wire format on the input stream

#### 8.1 Reliable input stream

Bytes (sent on the client's input bidi stream, per
`NativeQuicInputSender::send_input_event`):

```
[ 0x52 ] [ 0x42 ]           // rkyv-v2 magic (per ADR-015 §4)
[ u32 LE archive length ]
[ rkyv archive bytes…   ]   // ArchivedRemoteCriticalInput
[ 0x52 ] [ 0x42 ]           // next archive
[ u32 LE archive length ]
[ rkyv archive bytes…   ]
…
```

Termination: `FIN` (stream write finish), no additional trailer
byte. The host detects `IMMEDIATE_ACK` request by inspecting the
archived payload's discriminant (mouse-button/keyboard/pen-up events
trigger an additional ACK frame request). For every
`RemoteCriticalInput::PenDown` or `PenUp`, the host-side worker
sends an `IMMEDIATE_ACK` (frame type `0x1f`) one-time on the **control
uni-stream**, not on the input stream itself. (Implementation note:
when public `Connection::send_immediate_ack()` lands in `quinn-proto`,
replace the PING-via-control-stream fallback in
`crates/qubox-transport/src/lib.rs:686-730`.)

#### 8.2 Datagram (volatile) channel

Cross-reference ADR-010 §1.2:

| Disc | Wire struct        | Encoder path                                   |
|------|--------------------|------------------------------------------------|
| `0x47` | `WireGamepadState` (existing) | gamepad motion (already rkyv-encoded per ADR-015) |
| `0x4B` | `WireMouseMotion` (NEW, 12 bytes) | mouse motion — added by this ADR |
| `0x4D` | `WireMicHeader` (existing) | mic (preamble only, audio payload is opaque)    |
| `0x50` | `WirePenEvent` (existing, 36 bytes) | pen motion in air + hover                 |

Byte layout for `WireMouseMotion` (`crates/qubox-proto/src/lib.rs`):

```
[ 0x51 ] [ 0x42 ]       // MEDIA_DATAGRAM_MAGIC
[ 0x4B ]                // MOUSE_MOTION_DISCRIMINATOR
[ 0x00 ]                // flags (currently unused; reserved)
[  i16 LE dx ]
[  i16 LE dy ]
[  u32 LE timestamp_us ]
```

### 9. Implementation order

Each PR builds on the previous and has its own test gate. Numbered
in execution order:

1. **PR 1 — Workspace deps + rkyv/bytecheck bump.** Edit root
   `Cargo.toml:25-82`: add `rkyv = "0.8.17"` with `["std","validation"]`
   and `bytecheck = "0.8.2"`. Run `cargo build -p qubox-proto` and
   `cargo test -p qubox-proto`. **No behavior change.**
2. **PR 2 — Add `RemoteCriticalInput` rkyv type.** Edit
   `crates/qubox-proto/src/lib.rs:815-888`. Add the new enum (and keep
   `RemoteInputEvent` as a shim). Add tests at `:1780-1830` covering
   `serialize_then_roundtrip_yields_same_event`, `non_exhaustive_compat_with_added_variant`,
   `archive_bytecount_is_below_256_bytes`.
3. **PR 3 — `WireMouseMotion` rkyv type.** Edit
   `crates/qubox-proto/src/lib.rs` to add the struct. Add a
   `wire_mouse_motion_round_trip_through_bytes` test.
4. **PR 4 — Reliable-stream rkyv helpers.** Edit
   `crates/qubox-transport/src/lib.rs:1557-1602`: add
   `write_rkyv_prefixed` / `maybe_read_rkyv_prefixed` from §3. Add
   `loopback_rkyv_input_round_trip` test at `:1895-1950`.
5. **PR 5 — `AckPolicy` enum + `ack_frequency_config` plumbing.**
   Edit `crates/qubox-transport/src/lib.rs:1866-1879` per §2. Add
   `loopback_immediate_ack_round_trip_returns_within_2ms` test.
6. **PR 6 — `WireMouseMotion` datagram dispatch.** Edit
   `crates/qubox-transport/src/lib.rs` to accept `0x4B` in the
   datagram dispatcher (currently only `0x47`/`0x4D`/`0x50`). Add
   `loopback_mouse_motion_datagram_round_trip`.
7. **PR 7 — Lift `PenCoalescer` into shared `InputCoalescer`.** Move
   from `crates/qubox-pen/src/coalesce.rs` into
   `crates/qubox-proto/src/coalescer.rs` (defined in §5). Re-export
   from qubox-pen for compatibility.
8. **PR 8 — Linux libinput + uinput real impl.** Edit
   `crates/qubox-pen/src/linux.rs:28-146`. Add CI matrix entry for
   `cargo test -p qubox-pen --features libinput,uinput`. Add test
   `libinput_capture_seat_open_succeeds` (skipped without features).
9. **PR 9 — Windows Wintab real impl.** Edit
   `crates/qubox-pen/src/windows.rs`. Build only on Windows CI
   runner; tests skip otherwise.
10. **PR 10 — macOS `NseventCapture`.** Add `crates/qubox-pen/src/macos.rs`
    + `objc2` deps. macOS-only CI matrix entry.
11. **PR 11 — `RemoteInputInjector::apply_stream` + `apply_datagram`.**
    Edit `apps/qubox-host-agent/src/main.rs:311-509`. Wire
    `HostPenInjector` from PR 8/9/10.
12. **PR 12 — Loss-rate telemetry + Tracing spans.** Add a per-session
    counter `input.datagram.loss_rate` emitted on the existing
    `/metrics` endpoint. Add `tracing::span!(Level::INFO, "input.reliable.pen.roundtrip")`
    around the pen-down injection path.
13. **PR 13 — Documentation + integration test on a real WAN link.**
    Update `research/decisions/ADR-019-input-subsystem-immediate-ack.md`
    test matrix.

### 10. Test specifications

All test names use snake_case and are placed in the file/crate
indicated. **Each test must pass before the PR that introduces it is
merged.**

| Test name                                                                   | Location                                                          | Setup                                            | Expected output                                              |
|-----------------------------------------------------------------------------|-------------------------------------------------------------------|--------------------------------------------------|--------------------------------------------------------------|
| `rkyv_remote_critical_input_round_trips_through_archive`                    | `crates/qubox-proto/src/lib.rs:1780-1810`                         | serialize a sample of every variant              | `archived == expected_event`                                  |
| `rkyv_remote_critical_input_archive_size_is_below_256_bytes`                | `crates/qubox-proto/src/lib.rs:1810-1830`                         | for each variant                                 | `archive_bytes.len() < 256`                                  |
| `rkyv_remote_critical_input_bytecheck_rejects_truncated_archive`            | `crates/qubox-proto/src/lib.rs:1830-1850`                         | cut off last 4 bytes                             | `Err(...)` from `check_archived_root`                          |
| `wire_mouse_motion_round_trip_through_bytes`                                | `crates/qubox-proto/src/lib.rs:1850-1870`                         | build + from_bytes                               | `motion.dx == -3 && motion.dy == 7`                          |
| `wire_mouse_motion_rejects_discriminator_mismatch`                          | `crates/qubox-proto/src/lib.rs:1870-1880`                         | patch discriminator to `0x47`                    | `Err(MouseMotionError::BadDiscriminator)`                    |
| `loopback_rkyv_input_round_trip`                                            | `crates/qubox-transport/src/lib.rs:1895-1950`                     | loopback endpoint, send 1000 mouse-clicks        | 1000 received events, all equal to sent                       |
| `loopback_immediate_ack_returns_within_2ms`                                 | `crates/qubox-transport/src/lib.rs:1950-2020`                     | send pen-down, host sends `0x1f`, measure RTT    | round-trip ≤ 2 ms (95th percentile over 100 trials)          |
| `loopback_mouse_motion_datagram_round_trip`                                 | `crates/qubox-transport/src/lib.rs:2020-2080`                     | loopback, send 100 `0x4B` datagrams              | 100 received (loss = 0 for loopback); same rkyv archive bytes |
| `input_coalescer_flushes_on_flag_last_in_burst`                             | `crates/qubox-proto/src/coalescer.rs:140-180`                     | push 3 events + `FLAG_LAST_IN_BURST`               | `flush()` returns archive of 3 events                         |
| `input_coalescer_flushes_on_deadline`                                       | `crates/qubox-proto/src/coalescer.rs:180-220`                     | push 2 events, sleep 1.1 ms                      | `should_flush(true) == Some(FlushReason::Deadline)`           |
| `input_coalescer_does_not_flush_below_deadline`                             | `crates/qubox-proto/src/coalescer.rs:220-260`                     | push 2 events, sleep 0.5 ms                      | `should_flush(false) == None`                                 |
| `mouse_motion_loss_rate_telemetry_emits`                                     | `apps/qubox-host-agent/src/metrics.rs:1-200`                      | force 5 % synthetic datagram drop                | telemetry counter `input.datagram.motion.loss` increased by ~5|
| `ack_policy_input_immediate_advertises_min_ack_delay_1ms`                   | `crates/qubox-transport/src/lib.rs:2080-2130`                     | inspect `Connection::peer_transport_params()`    | `min_ack_delay` present ≤ 1000 µs                             |
| `pen_coalescer_pen_motion_skipped_when_no_FLAG_LAST_IN_BURST` (legacy)     | `crates/qubox-pen/src/coalesce.rs:200-220`                        | push 4 events without burst flag                  | exactly 1 survivor per window                                 |
| `libinput_capture_seat_open_succeeds` (Linux CI only)                       | `crates/qubox-pen/src/linux.rs:170-200`                           | open `Libinput::new_with_udev(UdevIf)`           | `Ok(())`                                                     |
| `uinput_injector_create_succeeds` (Linux CI only)                           | `crates/qubox-pen/src/linux.rs:200-230`                           | create `UinputInjector::new("bp-pen-test")`       | `Ok(())` and device exists under `/sys/devices/virtual/input/`|
| `wintab_capture_open_succeeds` (Windows CI only)                            | `crates/qubox-pen/src/windows.rs:180-220`                         | open `WTOpenW`                                    | non-null `HCTX`                                               |
| `nsevent_capture_thread_starts` (macOS CI only)                             | `crates/qubox-pen/src/macos.rs:120-160`                            | spawn capture thread                              | thread running with `MainThreadMarker`                        |
| `host_pen_injector_pen_down_succeeds`                                       | `apps/qubox-host-agent/src/pen_injector.rs:80-130`                 | `HostPenInjector::inject(pen_down_event)`        | `Ok(())`                                                      |
| `rkyv_legacy_remote_input_event_json_round_trip_still_works`                | `crates/qubox-proto/src/lib.rs:1179-1208`                         | existing test, untouched                         | still passes                                                  |

### 11. Pitfalls (at least 5 specific gotchas)

1. **`Enigo` is `!Send + !Sync` (practical assumption).** The crate
   docs at docs.rs/enigo do not explicitly advertise Send/Sync; it
   wraps X11/`Win32`/CoreGraphics handles. **Treat it as not
   thread-safe.** The existing `spawn_remote_input_worker` at
   `apps/qubox-host-agent/src/main.rs:1363-1431` already uses a
   dedicated `std::thread::Builder` — do not "modernize" it to
   `tokio::spawn`, or you will get `RefCell borrow` UBs / hangs.

2. **Wintab capture is per-window context.** `WTOpenW(hwnd,
   &log_context, true)` ties the pen context to an `HWND`. If the
   host-agent runs as a service with no visible window, you must
   create a *message-only* (`HWND_MESSAGE`) window. Prototype on
   Windows: in
   `crates/qubox-pen/src/windows.rs::WintabCapture::start`, do
   `CreateWindowExW(0, "STATIC", "bp-pen", WS_OVERLAPPED, 0, 0, 0,
   0, HWND_MESSAGE, ...)` first.

3. **`uinput` requires CAP_SYS_ADMIN or membership in the
   `uinput` group.** Many distros add `/dev/uinput` to the
   `input` group instead. The `UinputInjector::new` will return
   `Err(io::Error(PermissionDenied))` for a service-mode host-agent.
   For systemd units, add `SupplementaryGroups=uinput` to the
   `[Service]` block. Document this in the host-agent install README
   and in `crates/qubox-pen/src/error.rs::PenInjectError::PermissionDenied`.

4. **macOS Input Monitoring TCC permission.** `NSEventTypeTabletPoint`
   only fires when the user has *manually* granted the binary the
   **Input Monitoring** permission under *System Settings → Privacy &
   Security*. Without this, `dispatchEvent` returns `nil`. There is
   *no programmatic API to grant it*. The agent must log a
   `tracing::error!` instructing the user to enable it. Document in
   `crates/qubox-pen/src/macos.rs::NseventCapture::start` and in
   `README.md`.

5. **`#[repr(C, packed)]` on `WirePenEvent` is alignment-fragile.**
   The existing struct at `crates/qubox-proto/src/pen.rs:137-153`
   uses `#[repr(C, packed)]` and `unaligned field reads via byte
   array + `to_le_bytes`/`from_le_bytes`. Do **not** try to
   convert this struct to rkyv directly — its field accesses through
   byte arrays are unsafe. Instead, **keep** `WirePenEvent` for the
   datagram path (36 bytes, packed) and define a *separate* rkyv
   archive for the reliable input stream (`RemoteCriticalInput::PenDown`
   in §3) that uses plain fields. rkyv handles alignment correctly.

6. **rkyv archived enums are layout-stable across versions but NOT
   across variant reorderings.** Adding a new variant in the middle
   of `RemoteCriticalInput` shifts every subsequent discriminant
   value, breaking backwards compatibility. **Always append new
   variants at the end** of the enum and never remove or reorder
   existing ones without bumping the format magic from `[0x52, 0x42]`
   to `[0x53, 0x42]`. (Per ADR-015 §4 — major wire-format break.)

7. **The `IMMEDIATE_ACK` frame type changed.** Draft-05 had
   `IMMEDIATE_ACK = 0xac`. Draft-14 (current target) reassigned it
   to `0x1f`. If a future quinn release pins draft-05 backwards-compat,
   we must match the implementation. Document the draft-14 byte in
   `crates/qubox-transport/src/lib.rs` near the ACK-Frequency helpers.

8. **`quinn` 0.11.5 does not yet expose `send_immediate_ack()`.** Our
   fallback is to send a PING (frame type `0x01`) on the same input
   stream. PING elicits an ACK just as well, but with slightly more
   header overhead. When the public API lands, replace. Tracking
   issue: `github.com/quinn-rs/quinn/issues/1014`.

9. **The `Settings::release_keys_when_dropped` flag in `enigo` 0.6
   does NOT cover keys held down across re-loads.** If the host-agent
   crashes while holding Shift down, the user's host remains stuck
   in "Shift held". Add a watchdog: on `RemoteInputInjector::new`,
   queue `Release` for every currently-down key by querying the
   client-side keyboard state. See follow-up issue #219.

10. **Loss telemetry undercounts dropped datagrams.** The `quinn`
    `Connection` does not surface per-datagram ACK/loss information
    out of the box; we approximate by `sent - received` over a
    rolling 1-second window, *not* by per-datagram fate. Cite this
    caveat in the telemetry docstring. Better telemetry is a
    follow-up PR that adds a `quinn_proto` debug listener.

### 12. Verification commands

Run these after every PR lands, before requesting review.

#### 12.1 Unit tests + integration tests

```bash
# Full workspace test sweep, including rkyv round-trip tests.
cargo test --workspace --all-features --quiet

# Per-crate fast pass during local iteration.
cargo test -p qubox-proto                              # ~10s
cargo test -p qubox-transport                          # ~30s
cargo test -p qubox-pen                                 # ~5s
cargo test -p qubox-host-agent                          # ~20s

# Linux-only pen capture path.
cargo test -p qubox-pen --features libinput,uinput      # requires uinput group

# Windows-only pen capture path.
cargo test -p qubox-pen --features wintab               # requires WinTab32.dll

# macOS-only pen capture path.
cargo test -p qubox-pen --features nsevent              # requires TCC Input Monitoring
```

#### 12.2 Latency verification (ACK round-trip)

```bash
# Run the host-agent and client-cli on the same dev box in two terminals,
# with the rkyv round-trip tracing filter.
RUST_LOG="info,qubox_transport::lib=debug,qubox_transport::input=trace" \
cargo run -p qubox-host-agent -- \
    --session-id $(uuidgen) \
    --auto-approve-pairing \
    --video-config-defaults

# In another terminal, attach a debug client. The 95th-percentile ACK RTT
# should be < 2 ms. Filter the trace:
RUST_LOG="trace" cargo run -p qubox-client-cli -- \
    --host 127.0.0.1 \
    --session-id <same as above> \
    --enable-input-trace \
    2>&1 | grep -E '(input\.reliable\.pen\.roundtrip|ack_immediate)' | \
        awk '{print $NF}' | sort -n | \
        awk '{a[NR]=$1} END {print a[int(NR*0.95)] " ms (p95)"}'
```

The expected output on a 1 Gbps loopback with `min_ack_delay = 1 ms`
and Cubic is `1.05 ms (p95)` to `1.25 ms (p95)`.

#### 12.3 End-to-end harness

```bash
# Run the existing transport interop suite — must remain green.
cargo test -p qubox-transport \
    --test loopback_native_quic_media_round_trip         # from lib.rs:1912

# Add the new loopback tests (PR 5, PR 6):
cargo test -p qubox-transport \
    loopback_rkyv_input_round_trip \
    loopback_immediate_ack_returns_within_2ms \
    loopback_mouse_motion_datagram_round_trip
```

#### 12.4 File-by-file reference for insertion points

- `Cargo.toml:60-82` — add `rkyv` + `bytecheck` deps
- `Cargo.toml:73` — `uinput.workspace = true` (already present)
- `crates/qubox-pen/Cargo.toml:1-30` — add macOS deps, add `input` feature
- `crates/qubox-proto/src/lib.rs:815-888` — add `RemoteCriticalInput`
- `crates/qubox-proto/src/lib.rs:1175-1210` — keep legacy JSON tests
  green
- `crates/qubox-proto/src/lib.rs:1280-1310` — add `MOUSE_MOTION_DISCRIMINATOR`
  and `WireMouseMotion` rkyv struct
- `crates/qubox-proto/src/lib.rs:1790-1880` — add new rkyv tests
- `crates/qubox-proto/src/coalescer.rs` (new file, ~200 lines)
- `crates/qubox-pen/src/lib.rs:36-46` — re-export new `InputCoalescer`
- `crates/qubox-pen/src/linux.rs:28-146` — fill in libinput + uinput
  body
- `crates/qubox-pen/src/windows.rs` — fill in Wintab body
- `crates/qubox-pen/src/macos.rs` (new file, ~150 lines)
- `crates/qubox-transport/src/lib.rs:97-108` — add `AckPolicy` enum
- `crates/qubox-transport/src/lib.rs:1557-1602` — add
  `write_rkyv_prefixed` / `maybe_read_rkyv_prefixed`
- `crates/qubox-transport/src/lib.rs:1866-1879` — wire
  `ack_frequency_config` into `build_transport_config`
- `crates/qubox-transport/src/lib.rs:1895-2140` — new loopback tests
- `crates/qubox-transport/src/media/mod.rs:30` — confirm
  `MEDIA_DATAGRAM_MAGIC = [0x51, 0x42]` discriminator table is
  extended with `0x4B`
- `apps/qubox-host-agent/src/main.rs:17` — enigo import unchanged
- `apps/qubox-host-agent/src/main.rs:311-509` — extend
  `RemoteInputInjector` with `apply_datagram` and `apply_stream`
- `apps/qubox-host-agent/src/main.rs:1363-1431` — leave the dedicated
  thread untouched (enigo is `!Send + !Sync`)
- `apps/qubox-host-agent/src/pen_injector.rs` (new file, ~80 lines)
- `apps/qubox-host-agent/src/metrics.rs` — extend metrics surface
  with `input.datagram.loss_rate`

## Consequences

### Positive

- Pen end-to-end latency drops from ~25 ms to < 10 ms (round-trip
  ACK cut from 25 ms to ~1 ms; pen-down/pen-up coherently
  acknowledged within the 240 Hz frame budget).
- HOL blocking on motion events is eliminated: dropping a motion
  event no longer blocks subsequent events.
- CPU spent on input encoding drops ~100× with rkyv
  (1 µs/event → ~10 ns/event, per ADR-015 §6).
- The two-tier path is one unified pipeline: the **critical** vs
  **volatile** split is explicit in the wire format and the
  `InputCoalescer` module is shared across pen, mouse, and gamepad.

### Negative / Risk

- rkyv schema migrations require bumping the magic prefix
  `[0x52, 0x42]` → `[0x53, 0x42]` per ADR-015 §4. For additive
  changes we keep the magic; for breaking changes we bump.
- Partial reliability for motion means some events are dropped
  silently. Telemetry counter `input.datagram.motion.loss_rate` is
  exposed via `/metrics` and logged on session shutdown so the user
  can see when motion is being lost.
- macOS pen capture requires manual TCC Input Monitoring permission
  by the user. The installer README must call this out.
- `quinn` 0.11.5 may not yet have a stable API for sending an
  `IMMEDIATE_ACK` frame directly; we ship the PING-fallback in PR
  11 and replace when upstream lands the API.

### Roadmap mapping

- Closes P2-15 (Pen 240 Hz).
- Tightens the keyboard/mouse path (already-good; this ADR makes
  it explicit about its latency budget).
- Required input for ADR-017 (browser pen events use this same
  pipeline).
- Required input for ADR-020 (Pensieve RL uses the loss-rate
  telemetry as an ABR signal).

### References

- `crates/qubox-transport/src/lib.rs:97-108` (new `AckPolicy` enum)
- `crates/qubox-transport/src/lib.rs:591-602` `NativeQuicInputSender::send_input_event`
- `crates/qubox-transport/src/lib.rs:618-635` `NativeQuicInputReceiver::read_input_event`
- `crates/qubox-transport/src/lib.rs:1557-1602` existing JSON helpers (replaced, not removed)
- `crates/qubox-transport/src/lib.rs:1866-1879` `build_transport_config` (extended with ACK-Frequency)
- `crates/qubox-proto/src/lib.rs:815-888` `RemoteInputEvent` (kept as legacy shim) + `RemoteCriticalInput` (NEW)
- `crates/qubox-proto/src/lib.rs:1179-1210` legacy JSON tests (must stay green)
- `crates/qubox-proto/src/pen.rs:1-460` `WirePenEvent` pen wire format
- `crates/qubox-proto/src/coalescer.rs` (NEW) shared `InputCoalescer`
- `crates/qubox-pen/src/lib.rs:1-95` crate root
- `crates/qubox-pen/src/linux.rs:28-146` `LibinputCapture` / `UinputInjector` (filled in by PR 8)
- `crates/qubox-pen/src/windows.rs` (filled in by PR 9)
- `crates/qubox-pen/src/macos.rs` (NEW, PR 10)
- `apps/qubox-host-agent/src/main.rs:17` `use enigo::{...}`
- `apps/qubox-host-agent/src/main.rs:311-509` `RemoteInputInjector` (extended with `apply_datagram` / `apply_stream`)
- `apps/qubox-host-agent/src/main.rs:1363-1431` `spawn_remote_input_worker` (left alone)
- `apps/qubox-host-agent/src/pen_injector.rs` (NEW)
- IETF `draft-ietf-quic-ack-frequency-14` (target RFC late 2026/early 2027); IMMEDIATE_ACK frame type `0x1f`, `min_ack_delay` TP id `0xff04de1b`, ACK_FREQUENCY frame type `0xaf`.
- IANA QUIC Frame Types registry (provisional `ACK_FREQUENCY = 0xaf`, `IMMEDIATE_ACK = 0x1f`).
- IETF `draft-ietf-quic-receive-ts` companion work for receive timestamp.
- ADR-010 §1.2 (pen wire format), §4 (host pen injection), §13 (discriminator range allocation).
- ADR-011 §2 (ACK-Frequency configuration policy).
- ADR-015 (rkyv migration; current workspace pin is `rkyv = "0.8.17"` + `bytecheck = "0.8.2"`).
- ADR-017 (WebCodecs browser client uses rkyv via wasm).
- ADR-020 (Pensieve RL uses the loss-rate telemetry as an ABR signal).
- crates.io — `rkyv 0.8.17`, `bytecheck 0.8.2`, `enigo 0.6.0` (MIT), `objc2 0.5.1`, `objc2-app-kit 0.2.0`, `objc2-foundation 0.2.0`, `input 0.6.0`, `uinput 0.1.0`, `wintab_lite 0.2.1`, `quinn 0.11.5`.
- Wacom Wintab API: `Wintab32.dll`, `WTOpenW`, `WTInfoW`, `WTPacket`, `PACKET { pkNormalPressure, pkTangentPressure, pkOrientation, pkRotation, pkCursor, pkZ, pkButtons, pkTime }`.
- Microsoft Windows Ink / WM_POINTER: `GetPointerPenInfo(pointer_id, &POINTER_PEN_INFO)`.
- Apple AppKit: `NSEventTypeTabletPoint`, `NSEventTypeTabletProximity`, `NSEvent { tabletPointType, pointPressure, tilt, rotation, absoluteX, absoluteY, deviceID }`.
- macOS TCC: Input Monitoring permission required on macOS 10.15+.
- libinput C: `libinput_event_tablet_tool_get_{x,y,pressure,distance,tilt_x,tilt_y,rotation,button}`.
- Linux `uinput` events: `UI_ABS_X`, `UI_ABS_Y`, `UI_ABS_PRESSURE`, `UI_ABS_DISTANCE`, `UI_ABS_TILT_X`, `UI_ABS_TILT_Y`, `UI_ABS_MISC`, `BTN_TOOL_PEN`, `BTN_TOOL_RUBBER`, `BTN_TOUCH`, `BTN_STYLUS`.
