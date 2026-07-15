# ADR-015 Zero-Copy Serialization Migration to rkyv

## Status

Proposed. Replace the previous draft in full. This ADR is based on the
current repository paths under `crates/qubox-*` and the current hot-path
wire code at:

- `crates/qubox-proto/src/pen.rs:120` for `WirePenEvent`.
- `crates/qubox-proto/src/lib.rs:28` for `WireGamepadState`.
- `crates/qubox-proto/src/lib.rs:1086` for `WireMicHeader`.
- `crates/qubox-transport/src/media/mod.rs:58` for `MediaDatagramHeader`.
- `crates/qubox-transport/src/lib.rs:1512` for `WireAccessUnitHeader`.

## Research findings

- Current crates: `rkyv 0.8.17`, `bytecheck 0.8.2`, `rancor 0.1.2`; all require Rust 1.81+.
- rkyv 0.8 default features are `std` and `bytecheck`; the ADR pins `little_endian`, `unaligned`, and `pointer_width_32` to make the wire format explicit.
- rkyv 0.8 replaced the old mental model of `check_archived_root` / `check_archived_value` with `rkyv::access::<ArchivedT, rancor::Error>(buf)` for checked zero-copy access.
- rkyv 0.8 is not compatible with archives produced by rkyv 0.7 or earlier; 0.8 archives are guaranteed only for semver-compatible 0.8 releases and unchanged schemas/features.
- rkyv's safe API uses `bytecheck::CheckBytes` through `rkyv::access`; validation walks the archive once and is much cheaper than full deserialize for fixed-size structs.
- The public `rkyv-js` project exists, but it is niche and version-layout-coupled; ADR-017 must not depend on direct browser rkyv decoding without a separate review.
- Benchmarks support rkyv as one of the fastest Rust serializers and show nanosecond zero-copy access, but a blanket 100x claim is not guaranteed; Qubox must prove the 100x pen-path claim with `cargo bench -p qubox-proto --bench rkyv_vs_json`.
- `bitflags` has no rkyv feature; keep bitflags out of archived structs and archive their `.bits()` value as `u8`.
- A cold probe build of these exact deps compiled 42 crates in 5.03s on this workstation; expect the first CI build to grow by roughly that amount before cache.

Sources checked: rkyv docs for `to_bytes`, `access`, features, and compatibility; bytecheck 0.8 docs; rkyv 0.8 release notes; `rust_serialization_benchmark`; `rkyv-js`; bincode/postcard/capnp/flatbuffers docs and ecosystem notes.

## Decision

Use rkyv 0.8 only for native Rust hot-path binary frames. Keep JSON for control messages and as the v1 compatibility path. Do not expose rkyv as the browser-facing protocol for ADR-017 yet.

### Cargo additions

Add to root `Cargo.toml:34` under `[workspace.dependencies]`:

```toml
rkyv = { version = "0.8.17", default-features = false, features = ["std", "bytecheck", "little_endian", "unaligned", "pointer_width_32", "uuid-1"] }
bytecheck = { version = "0.8.2", features = ["uuid-1"] }
rancor = { version = "0.1.2", default-features = false, features = ["alloc"] }
criterion = { version = "0.8.2", default-features = false, features = ["cargo_bench_support"] }
```

Add to `crates/qubox-proto/Cargo.toml:6`:

```toml
[features]
default = ["wire-json-v1", "wire-rkyv-v2"]
wire-json-v1 = []
wire-rkyv-v2 = ["dep:rkyv", "dep:bytecheck", "dep:rancor"]

[dependencies]
rkyv = { workspace = true, optional = true }
bytecheck = { workspace = true, optional = true }
rancor = { workspace = true, optional = true }

[dev-dependencies]
criterion.workspace = true
```

Add to `crates/qubox-proto/Cargo.toml:18`:

```toml
[[bench]]
name = "rkyv_vs_json"
harness = false
```

Add to `crates/qubox-transport/Cargo.toml:6`:

```toml
[features]
default = ["wire-json-v1", "wire-rkyv-v2"]
wire-json-v1 = ["qubox-proto/wire-json-v1"]
wire-rkyv-v2 = ["qubox-proto/wire-rkyv-v2", "dep:rkyv", "dep:bytecheck", "dep:rancor"]

[dependencies]
rkyv = { workspace = true, optional = true }
bytecheck = { workspace = true, optional = true }
rancor = { workspace = true, optional = true }
```

### Common rkyv helper API

Create `crates/qubox-proto/src/rkyv_wire.rs` and export it from `crates/qubox-proto/src/lib.rs:4` behind `wire-rkyv-v2`.

```rust
#[cfg(feature = "wire-rkyv-v2")]
pub type CheckTypeError = rkyv::rancor::Error;

#[cfg(feature = "wire-rkyv-v2")]
pub type RkyvBytes = rkyv::util::AlignedVec;

#[cfg(feature = "wire-rkyv-v2")]
pub const WIRE_FORMAT_VERSION_RKYV_V2: u8 = 0x02;

#[cfg(feature = "wire-rkyv-v2")]
pub const MEDIA_DATAGRAM_MAGIC_V2: [u8; 2] = [0x52, 0x42];

#[cfg(feature = "wire-rkyv-v2")]
pub fn to_rkyv_bytes<T>(value: &T) -> Result<RkyvBytes, CheckTypeError>
where
    T: for<'a> rkyv::Serialize<
        rkyv::api::high::HighSerializer<
            rkyv::util::AlignedVec,
            rkyv::ser::allocator::ArenaHandle<'a>,
            CheckTypeError,
        >,
    >,
{
    rkyv::to_bytes::<CheckTypeError>(value)
}

#[cfg(feature = "wire-rkyv-v2")]
pub fn wire_error(message: &'static str) -> CheckTypeError {
    use rkyv::rancor::Source;
    CheckTypeError::new(std::io::Error::new(std::io::ErrorKind::InvalidData, message))
}
```

Each migrated module must expose a typed checked decode. The signature pattern is:

```rust
#[cfg(feature = "wire-rkyv-v2")]
pub fn from_rkyv_bytes<'a>(
    buf: &'a [u8],
) -> Result<&'a ArchivedWirePenEventRkyv, CheckTypeError> {
    let archived = rkyv::access::<ArchivedWirePenEventRkyv, CheckTypeError>(buf)?;
    validate_wire_pen_event_rkyv(archived)?;
    Ok(archived)
}
```

`rkyv::access` is the bytecheck validation step. The extra `validate_*` function must check semantic fields that bytecheck cannot know: magic, version, discriminator, reserved-zero bytes, lengths, and enum ranges.

## Wire format v2

`MEDIA_DATAGRAM_MAGIC` changes from `[0x51, 0x42]` to `[0x52, 0x42]` for rkyv v2.

Version byte position is fixed:

| Frame family | Byte offsets | Meaning |
| --- | --- | --- |
| Pen datagram | `0..2`, `2`, `3` | magic `[0x52,0x42]`, version `0x02`, discriminator `0x50` |
| Gamepad datagram | `0..2`, `2`, `3` | magic `[0x52,0x42]`, version `0x02`, discriminator `0x47` |
| Mic datagram | `0..2`, `2`, `3` | magic `[0x52,0x42]`, version `0x02`, discriminator `0x4D` |
| Media/FEC datagram | `0..2`, `2`, `3` | magic `[0x52,0x42]`, version `0x02`, flags byte; parity still uses `FLAG_PARITY` |
| Access-unit stream header | archive offsets `0..2`, `2`, `3`; stream offsets `4..6`, `6`, `7` after the `u32_le archive_len` prefix | magic `[0x52,0x42]`, version `0x02`, discriminator `0x41` |

Receivers dispatch this way:

```rust
match buf.get(0..2) {
    Some([0x51, 0x42]) => decode_v1_json_or_manual(buf),
    Some([0x52, 0x42]) if buf.get(2) == Some(&0x02) => decode_v2_rkyv(buf),
    _ => Err(WireFormatError::BadMagic),
}
```

## Migration code by type

### 1. Pen: `crates/qubox-proto/src/pen.rs:120`

Before:

```rust
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WirePenEvent {
    pub magic: [u8; 2],
    pub discriminator: u8,
    pub flags: u8,
    pub device_id: [u8; 2],
    pub tool_id: u8,
    pub hover_distance: u8,
    pub x: [u8; 4],
    pub y: [u8; 4],
    pub pressure: [u8; 4],
    pub tilt_x: [u8; 4],
    pub tilt_y: [u8; 4],
    pub rotation: [u8; 4],
    pub timestamp_us: [u8; 4],
}
```

After, insert at `crates/qubox-proto/src/pen.rs:120` in a new `wire_pen_event_rkyv` module:

```rust
#[cfg(feature = "wire-rkyv-v2")]
pub mod wire_pen_event_rkyv {
    use rkyv::{Archive, Deserialize, Serialize};
    use crate::rkyv_wire::{wire_error, CheckTypeError, MEDIA_DATAGRAM_MAGIC_V2, WIRE_FORMAT_VERSION_RKYV_V2};

    pub const PEN_DATAGRAM_DISCRIMINATOR_V2: u8 = 0x50;
    pub const PEN_RKYV_WIRE_SIZE: usize = 37;

    #[repr(C)]
    #[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[rkyv(derive(Debug, Eq, PartialEq, Hash))]
    #[rkyv(attr(derive(serde::Serialize, serde::Deserialize)))]
    pub struct WirePenEventRkyv {
        pub magic: [u8; 2],
        pub version: u8,
        pub discriminator: u8,
        pub flags: u8,
        pub device_id: [u8; 2],
        pub tool_id: u8,
        pub hover_distance: u8,
        pub x: [u8; 4],
        pub y: [u8; 4],
        pub pressure: [u8; 4],
        pub tilt_x: [u8; 4],
        pub tilt_y: [u8; 4],
        pub rotation: [u8; 4],
        pub timestamp_us: [u8; 4],
    }

    pub fn to_rkyv_bytes(value: &WirePenEventRkyv) -> Result<rkyv::util::AlignedVec, CheckTypeError> {
        rkyv::to_bytes::<CheckTypeError>(value)
    }

    pub fn from_rkyv_bytes<'a>(buf: &'a [u8]) -> Result<&'a ArchivedWirePenEventRkyv, CheckTypeError> {
        let archived = rkyv::access::<ArchivedWirePenEventRkyv, CheckTypeError>(buf)?;
        validate_wire_pen_event_rkyv(archived)?;
        Ok(archived)
    }

    pub fn validate_wire_pen_event_rkyv(value: &ArchivedWirePenEventRkyv) -> Result<(), CheckTypeError> {
        if value.magic != MEDIA_DATAGRAM_MAGIC_V2 || value.version != WIRE_FORMAT_VERSION_RKYV_V2 || value.discriminator != PEN_DATAGRAM_DISCRIMINATOR_V2 {
            return Err(wire_error("bad pen rkyv header"));
        }
        Ok(())
    }
}
```

Keep the current v1 struct in `wire_pen_event_json` or `wire_pen_event_v1` until v1 is removed. Do not derive rkyv on `#[repr(C, packed)]`.

### 2. Gamepad: `crates/qubox-proto/src/lib.rs:28`

Before:

```rust
#[repr(C, packed)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireGamepadState {
    pub gamepad_id: u8,
    pub flags: u8,
    pub buttons_lo: u8,
    pub buttons_hi: u8,
    pub lt: u8,
    pub rt: u8,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    pub _pad: [u8; 2],
}
```

After, insert at `crates/qubox-proto/src/lib.rs:68` in `wire_gamepad_state_rkyv`:

```rust
#[cfg(feature = "wire-rkyv-v2")]
pub mod wire_gamepad_state_rkyv {
    use rkyv::{Archive, Deserialize, Serialize};
    use crate::rkyv_wire::{wire_error, CheckTypeError, MEDIA_DATAGRAM_MAGIC_V2, WIRE_FORMAT_VERSION_RKYV_V2};

    pub const GAMEPAD_DATAGRAM_DISCRIMINATOR_V2: u8 = 0x47;
    pub const GAMEPAD_RKYV_WIRE_SIZE: usize = 20;

    #[repr(C)]
    #[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[rkyv(derive(Debug, Eq, PartialEq, Hash))]
    #[rkyv(attr(derive(serde::Serialize, serde::Deserialize)))]
    pub struct WireGamepadStateRkyv {
        pub magic: [u8; 2],
        pub version: u8,
        pub discriminator: u8,
        pub gamepad_id: u8,
        pub flags: u8,
        pub buttons_lo: u8,
        pub buttons_hi: u8,
        pub lt: u8,
        pub rt: u8,
        pub lx: [u8; 2],
        pub ly: [u8; 2],
        pub rx: [u8; 2],
        pub ry: [u8; 2],
        pub _pad: [u8; 2],
    }

    pub fn to_rkyv_bytes(value: &WireGamepadStateRkyv) -> Result<rkyv::util::AlignedVec, CheckTypeError> {
        rkyv::to_bytes::<CheckTypeError>(value)
    }

    pub fn from_rkyv_bytes<'a>(buf: &'a [u8]) -> Result<&'a ArchivedWireGamepadStateRkyv, CheckTypeError> {
        let archived = rkyv::access::<ArchivedWireGamepadStateRkyv, CheckTypeError>(buf)?;
        validate_wire_gamepad_state_rkyv(archived)?;
        Ok(archived)
    }

    pub fn validate_wire_gamepad_state_rkyv(value: &ArchivedWireGamepadStateRkyv) -> Result<(), CheckTypeError> {
        if value.magic != MEDIA_DATAGRAM_MAGIC_V2 || value.version != WIRE_FORMAT_VERSION_RKYV_V2 || value.discriminator != GAMEPAD_DATAGRAM_DISCRIMINATOR_V2 || value._pad != [0, 0] {
            return Err(wire_error("bad gamepad rkyv header"));
        }
        Ok(())
    }
}
```

Use little-endian byte arrays for axes in v2. Convert with `i16::to_le_bytes()` and `i16::from_le_bytes()`.

### 3. Mic: `crates/qubox-proto/src/lib.rs:1086`

Before:

```rust
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireMicHeader {
    pub magic: [u8; 2],
    pub discriminator: u8,
    pub flags: u8,
    pub sequence: [u8; 2],
    pub _reserved: [u8; 2],
}
```

After, insert at `crates/qubox-proto/src/lib.rs:1149` in `wire_mic_header_rkyv`:

```rust
#[cfg(feature = "wire-rkyv-v2")]
pub mod wire_mic_header_rkyv {
    use rkyv::{Archive, Deserialize, Serialize};
    use crate::rkyv_wire::{wire_error, CheckTypeError, MEDIA_DATAGRAM_MAGIC_V2, WIRE_FORMAT_VERSION_RKYV_V2};

    pub const MIC_DATAGRAM_DISCRIMINATOR_V2: u8 = 0x4D;
    pub const MIC_RKYV_WIRE_SIZE: usize = 9;

    #[repr(C)]
    #[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[rkyv(derive(Debug, Eq, PartialEq, Hash))]
    #[rkyv(attr(derive(serde::Serialize, serde::Deserialize)))]
    pub struct WireMicHeaderRkyv {
        pub magic: [u8; 2],
        pub version: u8,
        pub discriminator: u8,
        pub flags: u8,
        pub sequence: [u8; 2],
        pub _reserved: [u8; 2],
    }

    pub fn to_rkyv_bytes(value: &WireMicHeaderRkyv) -> Result<rkyv::util::AlignedVec, CheckTypeError> {
        rkyv::to_bytes::<CheckTypeError>(value)
    }

    pub fn from_rkyv_bytes<'a>(buf: &'a [u8]) -> Result<&'a ArchivedWireMicHeaderRkyv, CheckTypeError> {
        let archived = rkyv::access::<ArchivedWireMicHeaderRkyv, CheckTypeError>(buf)?;
        validate_wire_mic_header_rkyv(archived)?;
        Ok(archived)
    }

    pub fn validate_wire_mic_header_rkyv(value: &ArchivedWireMicHeaderRkyv) -> Result<(), CheckTypeError> {
        if value.magic != MEDIA_DATAGRAM_MAGIC_V2 || value.version != WIRE_FORMAT_VERSION_RKYV_V2 || value.discriminator != MIC_DATAGRAM_DISCRIMINATOR_V2 || value._reserved != [0, 0] {
            return Err(wire_error("bad mic rkyv header"));
        }
        Ok(())
    }
}
```

### 4. FEC/media datagram header: `crates/qubox-transport/src/media/mod.rs:58`

Before:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct MediaDatagramHeader {
    pub magic: [u8; 2],
    pub flags: u8,
    pub codec: u8,
    pub stream_id: u16,
    pub frame_id: u32,
    pub chunk_id: u16,
    pub chunk_count: u16,
}
```

After, insert at `crates/qubox-transport/src/media/mod.rs:52` in `media_datagram_header_rkyv`:

```rust
#[cfg(feature = "wire-rkyv-v2")]
pub mod media_datagram_header_rkyv {
    use rkyv::{Archive, Deserialize, Serialize};
    use qubox_proto::rkyv_wire::{wire_error, CheckTypeError, MEDIA_DATAGRAM_MAGIC_V2, WIRE_FORMAT_VERSION_RKYV_V2};

    pub const MEDIA_RKYV_HEADER_SIZE: usize = 15;

    #[repr(C)]
    #[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[rkyv(derive(Debug, Eq, PartialEq, Hash))]
    #[rkyv(attr(derive(serde::Serialize, serde::Deserialize)))]
    pub struct MediaDatagramHeaderRkyv {
        pub magic: [u8; 2],
        pub version: u8,
        pub flags: u8,
        pub codec: u8,
        pub stream_id: [u8; 2],
        pub frame_id: [u8; 4],
        pub chunk_id: [u8; 2],
        pub chunk_count: [u8; 2],
    }

    pub fn to_rkyv_bytes(value: &MediaDatagramHeaderRkyv) -> Result<rkyv::util::AlignedVec, CheckTypeError> {
        rkyv::to_bytes::<CheckTypeError>(value)
    }

    pub fn from_rkyv_bytes<'a>(buf: &'a [u8]) -> Result<&'a ArchivedMediaDatagramHeaderRkyv, CheckTypeError> {
        let archived = rkyv::access::<ArchivedMediaDatagramHeaderRkyv, CheckTypeError>(buf)?;
        validate_media_datagram_header_rkyv(archived)?;
        Ok(archived)
    }

    pub fn validate_media_datagram_header_rkyv(value: &ArchivedMediaDatagramHeaderRkyv) -> Result<(), CheckTypeError> {
        if value.magic != MEDIA_DATAGRAM_MAGIC_V2 || value.version != WIRE_FORMAT_VERSION_RKYV_V2 {
            return Err(wire_error("bad media rkyv header"));
        }
        Ok(())
    }
}
```

FEC parity datagrams continue to use this header with `flags & FLAG_PARITY != 0`. Do not add an independent FEC discriminator in this ADR.

### 5. Access-unit headers: `crates/qubox-transport/src/lib.rs:1512`

Before:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct WireAccessUnitHeader {
    session_id: Uuid,
    frame_id: u64,
    timestamp_micros: u64,
    keyframe: bool,
    byte_len: usize,
    #[serde(default)]
    codec: Option<VideoCodec>,
    #[serde(default)]
    stream_id: u16,
    #[serde(default)]
    display_id: u32,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    refresh_hz: f32,
    #[serde(default)]
    color_space_id: u8,
    #[serde(default)]
    hdr_static_metadata: Option<Vec<u8>>,
}
```

After, insert at `crates/qubox-transport/src/lib.rs:1536` in `wire_access_unit_header_rkyv`:

```rust
#[cfg(feature = "wire-rkyv-v2")]
mod wire_access_unit_header_rkyv {
    use rkyv::{Archive, Deserialize, Serialize};
    use qubox_proto::rkyv_wire::{wire_error, CheckTypeError, MEDIA_DATAGRAM_MAGIC_V2, WIRE_FORMAT_VERSION_RKYV_V2};

    pub const ACCESS_UNIT_DISCRIMINATOR_V2: u8 = 0x41;
    pub const ACCESS_UNIT_RKYV_HEADER_SIZE: usize = 63;

    #[repr(C)]
    #[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
    #[rkyv(derive(Debug, Eq, PartialEq, Hash))]
    #[rkyv(attr(derive(serde::Serialize, serde::Deserialize)))]
    pub struct WireAccessUnitHeaderRkyv {
        pub magic: [u8; 2],
        pub version: u8,
        pub discriminator: u8,
        pub session_id: [u8; 16],
        pub frame_id: [u8; 8],
        pub timestamp_micros: [u8; 8],
        pub keyframe: bool,
        pub byte_len: [u8; 4],
        pub codec: u8,
        pub stream_id: [u8; 2],
        pub display_id: [u8; 4],
        pub width: [u8; 4],
        pub height: [u8; 4],
        pub refresh_hz_millihz: [u8; 4],
        pub color_space_id: u8,
        pub hdr_static_metadata_len: [u8; 2],
    }

    pub fn to_rkyv_bytes(value: &WireAccessUnitHeaderRkyv) -> Result<rkyv::util::AlignedVec, CheckTypeError> {
        rkyv::to_bytes::<CheckTypeError>(value)
    }

    pub fn from_rkyv_bytes<'a>(buf: &'a [u8]) -> Result<&'a ArchivedWireAccessUnitHeaderRkyv, CheckTypeError> {
        let archived = rkyv::access::<ArchivedWireAccessUnitHeaderRkyv, CheckTypeError>(buf)?;
        validate_wire_access_unit_header_rkyv(archived)?;
        Ok(archived)
    }

    pub fn validate_wire_access_unit_header_rkyv(value: &ArchivedWireAccessUnitHeaderRkyv) -> Result<(), CheckTypeError> {
        if value.magic != MEDIA_DATAGRAM_MAGIC_V2 || value.version != WIRE_FORMAT_VERSION_RKYV_V2 || value.discriminator != ACCESS_UNIT_DISCRIMINATOR_V2 {
            return Err(wire_error("bad access-unit rkyv header"));
        }
        Ok(())
    }
}
```

`hdr_static_metadata` is not stored inside the archive. For v2 stream frames, write:

```text
u32_le archive_len
rkyv archive bytes for WireAccessUnitHeaderRkyv
hdr_static_metadata bytes, length = hdr_static_metadata_len
encoded access-unit bytes, length = byte_len
```

Use `byte_len: [u8; 4]`; reject frames larger than `MAX_VIDEO_AU_BYTES` before allocation.

## Compatibility shim

For one release, compile both paths by default.

Module rule:

- Existing JSON/manual code becomes `*_json` or `*_v1`.
- New rkyv code becomes `*_rkyv` or `*_v2`.
- Public encode/decode functions dispatch on magic and version, not on Cargo features alone.

Transport insertion points:

- `crates/qubox-transport/src/media/mod.rs:35`: add `MEDIA_DATAGRAM_MAGIC_V1` and `MEDIA_DATAGRAM_MAGIC_V2`; keep `MEDIA_DATAGRAM_MAGIC` as an alias to v1 until PR 6 flips it.
- `crates/qubox-transport/src/media/mod.rs:1297`: make `encode_gamepad_datagram` call v1 or v2 based on negotiated `WireFormatVersion`.
- `crates/qubox-transport/src/media/mod.rs:1336`: make `decode_gamepad_datagram` accept v1 and v2.
- `crates/qubox-transport/src/media/mod.rs:1397`: make `encode_pen_datagram` stop prefixing magic/discriminator twice in v2.
- `crates/qubox-transport/src/media/mod.rs:1410`: make `decode_pen_datagram` accept v1 and v2.
- `crates/qubox-transport/src/lib.rs:1557`: keep `write_json_prefixed` and `read_json_prefixed` for control messages and v1 access-unit headers.
- `crates/qubox-transport/src/lib.rs:1604`: add `read_access_unit_header_rkyv` beside `read_access_unit_header`; do not replace it in the first PR.

Negotiation rule:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormatVersion {
    JsonV1,
    RkyvV2,
}
```

Use `RkyvV2` only when both peers advertise `wire-rkyv-v2`. Otherwise send v1. Never send v2 to a v1-only peer; old receivers may interpret the first v2 stream bytes as a huge JSON length and disconnect.

## Implementation order

1. PR 0: add Cargo deps/features, `rkyv_wire.rs`, `WireFormatVersion`, and `benches/rkyv_vs_json.rs`; no behavior change.
2. PR 1: migrate pen rkyv v2 at `crates/qubox-proto/src/pen.rs:120` and `crates/qubox-transport/src/media/mod.rs:1397`.
3. PR 2: migrate gamepad rkyv v2 at `crates/qubox-proto/src/lib.rs:28` and `crates/qubox-transport/src/media/mod.rs:1297`.
4. PR 3: migrate mic rkyv v2 at `crates/qubox-proto/src/lib.rs:1086` and mic sender/receiver call sites in `crates/qubox-mic/src/pipeline.rs:225`.
5. PR 4: migrate media/FEC header at `crates/qubox-transport/src/media/mod.rs:58`; parity remains `FLAG_PARITY`.
6. PR 5: migrate access-unit headers at `crates/qubox-transport/src/lib.rs:1512`; keep control messages JSON.
7. PR 6: flip negotiated default to rkyv v2 for native QUIC peers; keep v1 decode path and tests.
8. PR 7: remove v1 send path only after two releases; keep v1 receive path indefinitely unless product says otherwise.

## Test specifications

Add tests at the nearest existing test modules:

| Test | File | Expected result |
| --- | --- | --- |
| `wire_pen_event_rkyv_round_trip` | `crates/qubox-proto/src/pen.rs:280` | archive length `37`; `rkyv::access` passes; fields match |
| `rkyv_pen_event_compatible_with_old_json_receiver_when_magic_v1` | `crates/qubox-transport/src/media/mod.rs:1478` | v1 magic `[0x51,0x42]` routes to old decoder and never calls rkyv |
| `rkyv_pen_event_rejects_truncated_archive` | `crates/qubox-proto/src/pen.rs:280` | first `36` bytes of a `37` byte archive returns `Err` from `rkyv::access` |
| `rkyv_pen_event_rejects_bad_magic_semantically` | `crates/qubox-proto/src/pen.rs:280` | bytecheck may pass; `validate_wire_pen_event_rkyv` must fail |
| `wire_gamepad_state_rkyv_round_trip` | `crates/qubox-proto/src/lib.rs:1326` | archive length `20`; `_pad == [0,0]`; axes use little-endian bytes |
| `wire_gamepad_state_rkyv_rejects_nonzero_pad` | `crates/qubox-proto/src/lib.rs:1326` | bytecheck passes; semantic validation fails |
| `wire_mic_header_rkyv_round_trip` | `crates/qubox-proto/src/lib.rs:1157` | archive length `9`; sequence round-trips |
| `media_datagram_header_rkyv_round_trip` | `crates/qubox-transport/src/media/mod.rs:1478` | archive length `15`; `chunk_count` round-trips |
| `fec_parity_rkyv_round_trip` | `crates/qubox-transport/src/media/rs_fec.rs:250` | rkyv header length `15`; `flags & FLAG_PARITY != 0`; parity shard reconstructs dropped data |
| `wire_access_unit_header_rkyv_round_trip` | `crates/qubox-transport/src/lib.rs:2360` | archive length `63`; metadata tail length equals `hdr_static_metadata_len` |
| `wire_access_unit_header_rkyv_rejects_invalid_bool` | `crates/qubox-transport/src/lib.rs:2360` | corrupt `keyframe` byte to `2`; bytecheck fails |
| `rkyv_access_unit_rejects_oversized_byte_len_before_alloc` | `crates/qubox-transport/src/lib.rs:2360` | `byte_len > MAX_VIDEO_AU_BYTES` returns error before allocating |

Byte-size assertions are mandatory and must use `to_rkyv_bytes(&value)?.len()`, not `std::mem::size_of`, because rkyv may add padding if features change.

## Benchmark specification

Create `crates/qubox-proto/benches/rkyv_vs_json.rs`.

Benchmarks:

- `pen_json_remote_input_event_decode`: `serde_json::from_slice::<RemoteInputEvent>` using `RemoteInputEvent::Pen`.
- `pen_rkyv_checked_access`: `wire_pen_event_rkyv::from_rkyv_bytes`.
- `pen_rkyv_unchecked_access_after_one_check`: call `from_rkyv_bytes` once outside the loop, then benchmark `unsafe { rkyv::access_unchecked::<ArchivedWirePenEventRkyv>(bytes) }`.
- `gamepad_json_decode`: current serde JSON state path.
- `gamepad_rkyv_checked_access`: rkyv v2 checked access.

Acceptance gates:

- `pen_rkyv_checked_access` must be at least 20x faster than `pen_json_remote_input_event_decode`.
- `pen_rkyv_unchecked_access_after_one_check` must be at least 100x faster than `pen_json_remote_input_event_decode` before the ADR summary may claim 100x.
- Archive size must be <= 25% of the JSON payload size for pen and gamepad.

Run:

```bash
cargo bench -p qubox-proto --bench rkyv_vs_json
```

## Verification commands

Run after each PR:

```bash
cargo test -p qubox-proto wire_pen_event_rkyv_round_trip --features wire-json-v1,wire-rkyv-v2
cargo test -p qubox-proto wire_gamepad_state_rkyv_round_trip --features wire-json-v1,wire-rkyv-v2
cargo test -p qubox-transport rkyv --features wire-json-v1,wire-rkyv-v2
cargo test -p qubox-transport fec_parity_rkyv_round_trip --features wire-json-v1,wire-rkyv-v2
cargo test --workspace --features wire-json-v1,wire-rkyv-v2
cargo clippy --workspace --all-targets --features wire-json-v1,wire-rkyv-v2 -- -D warnings
cargo bench -p qubox-proto --bench rkyv_vs_json
```

## Pitfalls

1. Do not derive rkyv directly on `#[repr(C, packed)]` structs. Remove `packed` and use byte-array fields, or use rkyv's `unaligned` feature and wrappers where unavoidable.
2. Do not use old rkyv 0.7 APIs in new code. Use `rkyv::to_bytes::<rancor::Error>` and `rkyv::access::<ArchivedT, rancor::Error>`.
3. Do not use `usize` in rkyv wire structs. Store lengths as `[u8; 4]` or `[u8; 8]` with explicit little-endian conversion.
4. Do not put `bitflags` types in archived structs. Store the underlying `u8`/`u16` bits.
5. bytecheck validates layout, enum tags, bools, and pointers; it does not validate magic bytes, reserved-zero fields, protocol ranges, or maximum payload lengths.
6. `#[serde(default)]` does not make rkyv archives forward-compatible. Add a new versioned rkyv type; for nested versioned payloads, serialize the nested value out-of-line with `#[rkyv(with = rkyv::with::AsBox)]` instead of embedding packed fields inline.
7. Do not change rkyv format-control features after shipping v2. Changing `unaligned`, endian, or pointer width is a wire break and requires v3 magic/version.
8. rkyv `to_bytes` allocates an `AlignedVec`. That is acceptable in PR 1; optimize later with a reusable serializer only if benchmarks show allocation cost matters.
9. Direct browser rkyv decode is not a product commitment. `rkyv-js` exists but is small and version-sensitive; ADR-017 should compare WASM, FlatBuffers, and Cap'n Proto again.
10. v2 must not duplicate magic/discriminator the way current pen encode does at `crates/qubox-transport/src/media/mod.rs:1397`; the rkyv archive is the whole v2 datagram.
11. The v1 gamepad path uses big-endian `i16`; v2 uses explicit little-endian byte arrays. Tests must cover both.
12. Adding `#[rkyv(attr(derive(serde::Serialize, serde::Deserialize)))]` is the compileable rkyv 0.8 form. Do not write `#[rkyv(attr(serde::Serialize, serde::Deserialize))]` without `derive(...)`.

## Alternative comparison

| Format | Decision | Reason |
| --- | --- | --- |
| rkyv 0.8 | Use for native QUIC hot path | Fastest Rust-native zero-copy access, no schema compiler, no browser commitment |
| bincode 2 | Reject for hot path | Safe and simple but not zero-copy; still constructs Rust values |
| postcard | Reject for this path | Excellent embedded/no_std serde format, but not zero-copy and Rust-only |
| Cap'n Proto | Keep as future browser/protocol option | Schema evolution and cross-language story are stronger; Rust codegen/tooling is heavier |
| FlatBuffers | Keep as ADR-017 candidate | Best JS/TS interoperability; requires `.fbs` schema and `flatc` workflow |

## References

- rkyv docs: `https://docs.rs/rkyv/0.8.17/rkyv/`
- rkyv `to_bytes`: `https://docs.rs/rkyv/0.8.17/rkyv/fn.to_bytes.html`
- rkyv `access`: `https://docs.rs/rkyv/0.8.17/rkyv/fn.access.html`
- rkyv `access_unchecked`: `https://docs.rs/rkyv/0.8.17/rkyv/fn.access_unchecked.html`
- bytecheck docs: `https://docs.rs/bytecheck/0.8.2/bytecheck/`
- rkyv benchmark project: `https://github.com/djkoloski/rust_serialization_benchmark`
- rkyv backward compatibility example: `https://raw.githubusercontent.com/rkyv/rkyv/main/rkyv/examples/backwards_compat.rs`
- rkyv-js: `https://github.com/cometkim/rkyv-js`