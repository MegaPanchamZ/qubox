use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod coalescer;
pub mod pen;
#[cfg(feature = "wire-rkyv-v2")]
pub mod rkyv_wire;
pub mod wire;

pub use coalescer::{FlushReason, InputCoalescer, COALESCE_MAX_EVENTS, DEFAULT_COALESCE_WINDOW};
pub use pen::{
    PenDeviceDescriptor, PenEventError, PenEventFlags, PenTool, WirePenEvent,
    PEN_DATAGRAM_DISCRIMINATOR, PEN_WIRE_HEADER_SIZE, PEN_WIRE_SIZE,
};

/// Discriminator byte for gamepad datagrams. ASCII `'G'`. Placed at
/// offset 2 immediately after `MEDIA_DATAGRAM_MAGIC`. Distinct from
/// pen (`0x50`) so the shared dispatch byte can route both families.
/// See `DatagramDispatcher` in `qubox-transport`.
pub const GAMEPAD_DATAGRAM_DISCRIMINATOR: u8 = 0x47;

/// 16-byte packed gamepad state. Sent over QUIC datagrams on change (delta
/// encoding). Layout matches `p0-06-gamepad.md` §"Wire format".
///
/// 0    1     2     3     4     5     6     7     8     9     10    11
/// ┌────┬────┬─────┬─────┬─────┬─────┬─────┬─────┬─────┬─────┬─────┬─────┐
/// │ id │ flg│ b_lo│ b_hi│ lt  │ rt  │ lx  │ lx  │ ly  │ ly  │ rx  │ rx  │
/// ├────┴────┴─────┴─────┴─────┴─────┴─────┴─────┴─────┴─────┴─────┴─────┤
/// │ ry  │ ry  │ _pad│ _pad│ ... pad to 16 bytes total                          │
/// └─────┴─────┴─────┴─────┴─────┴─────┴─────┴─────┴─────┴─────┴─────┴─────┘
#[repr(C, packed)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireGamepadState {
    pub gamepad_id: u8,
    /// bit 0: dpad-up, 1: dpad-down, 2: dpad-left, 3: dpad-right
    /// bit 4: connected, 5: repeat-last-frame
    pub flags: u8,
    /// bits 0-7: A,B,X,Y,LB,RB,Select,Start (Xbox 360 layout)
    pub buttons_lo: u8,
    /// bits 0-7: L3,R3,Guide,Reserved*5
    pub buttons_hi: u8,
    pub lt: u8,
    pub rt: u8,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    /// Two reserved bytes; intended for future motion-sensor / battery flags
    /// in v2 of the wire format. Must be zero in v1.
    pub _pad: [u8; 2],
}

impl WireGamepadState {
    pub const SIZE: usize = 16;
    pub const FLAG_DPAD_UP: u8 = 1 << 0;
    pub const FLAG_DPAD_DOWN: u8 = 1 << 1;
    pub const FLAG_DPAD_LEFT: u8 = 1 << 2;
    pub const FLAG_DPAD_RIGHT: u8 = 1 << 3;
    pub const FLAG_CONNECTED: u8 = 1 << 4;
    pub const FLAG_REPEAT_LAST: u8 = 1 << 5;
    pub const BTN_A: u8 = 1 << 0;
    pub const BTN_B: u8 = 1 << 1;
    pub const BTN_X: u8 = 1 << 2;
    pub const BTN_Y: u8 = 1 << 3;
    pub const BTN_LB: u8 = 1 << 4;
    pub const BTN_RB: u8 = 1 << 5;
    pub const BTN_SELECT: u8 = 1 << 6;
    pub const BTN_START: u8 = 1 << 7;
    pub const BTN_L3: u8 = 1 << 0;
    pub const BTN_R3: u8 = 1 << 1;
    pub const BTN_GUIDE: u8 = 1 << 2;
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GamepadKind {
    Xbox,
    DualShock4,
    DualSense,
    SwitchPro,
    Generic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PeerRole {
    Host,
    Client,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PlatformOs {
    Linux,
    Windows,
    Macos,
    Android,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    NativeQuic,
    WebRtc,
    RelayQuic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

impl VideoCodec {
    pub fn label(self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::H265 => "H.265 / HEVC",
            VideoCodec::Av1 => "AV1",
        }
    }

    /// `ffmpeg -f <input_format>` value when reading raw annex-b bytes from stdin.
    pub fn ffmpeg_demux_format(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::H265 => "hevc",
            VideoCodec::Av1 => "av1",
        }
    }

    /// Container format `ffmpeg -f <output_format>` for an annex-b byte stream.
    pub fn ffmpeg_mux_format(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::H265 => "hevc",
            VideoCodec::Av1 => "av1",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ScaleMode {
    /// Center source into target, letterbox if necessary.
    Fit,
    /// Crop source to fill target, may lose edges.
    Fill,
    /// Capture a region equal to the target with no scaling. Requires the source to
    /// be at least target-sized; smaller sources are rejected.
    Crop,
    /// Pass through source pixels without scaling. Target width/height are ignored.
    Native,
}

impl ScaleMode {
    pub fn label(self) -> &'static str {
        match self {
            ScaleMode::Fit => "fit (letterbox)",
            ScaleMode::Fill => "fill (crop edges)",
            ScaleMode::Crop => "crop exact",
            ScaleMode::Native => "native (no scaling)",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    PcmF32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    Pipewire,
    X11,
    DesktopDuplication,
    ScreenCaptureKit,
    MediaProjection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CapabilityProfile {
    pub transports: Vec<TransportKind>,
    pub capture: Vec<CaptureKind>,
    pub encoders: Vec<VideoCodec>,
    pub decoders: Vec<VideoCodec>,
    pub notes: Vec<String>,
}

impl CapabilityProfile {
    pub fn supports_transport(&self, transport: TransportKind) -> bool {
        self.transports.contains(&transport)
    }

    pub fn supports_codec(&self, codec: VideoCodec) -> bool {
        self.encoders.contains(&codec) || self.decoders.contains(&codec)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerDescriptor {
    pub device_id: Uuid,
    pub peer_id: Uuid,
    pub device_name: String,
    pub role: PeerRole,
    pub os: PlatformOs,
    pub capabilities: CapabilityProfile,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairingRequest {
    pub request_id: Uuid,
    pub host_peer_id: Uuid,
    pub client_label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairingDecision {
    pub request_id: Uuid,
    pub approved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairingRequested {
    pub request_id: Uuid,
    pub host_peer_id: Uuid,
    pub client: PeerDescriptor,
    pub client_label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairingGrant {
    pub host_peer_id: Uuid,
    pub client_peer_id: Uuid,
}

/// Per-session capability mask (input / clipboard / mic).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPermissions {
    #[serde(default = "default_true")]
    pub input: bool,
    #[serde(default = "default_true")]
    pub clipboard: bool,
    #[serde(default = "default_true")]
    pub mic: bool,
}

impl Default for SessionPermissions {
    fn default() -> Self {
        Self {
            input: true,
            clipboard: true,
            mic: true,
        }
    }
}

/// Short-lived share / pair link (self-host + managed UX).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShareLink {
    pub code: String,
    pub host_peer_id: Uuid,
    pub created_unix_ms: u64,
    pub expires_unix_ms: u64,
    pub permissions: SessionPermissions,
    /// Optional human label for the host ("Deb's PC").
    #[serde(default)]
    pub host_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VideoStreamPreferences {
    /// Preferred video codec. `None` lets the host choose.
    pub codec: Option<VideoCodec>,
    /// Target frame width in pixels. `None` means the host picks.
    pub width: Option<u32>,
    /// Target frame height in pixels. `None` means the host picks.
    pub height: Option<u32>,
    /// Target frame rate in frames per second. `None` means the host picks.
    pub framerate: Option<u32>,
    /// Target average bitrate in kilobits per second. `None` means the host picks.
    pub bitrate_kbps: Option<u32>,
    /// How the source framebuffer is mapped to the target resolution.
    pub scale_mode: Option<ScaleMode>,
    /// Display/monitor index on the host. `None` means primary or full root.
    pub display_index: Option<u32>,
    /// Optional capture region within the source, in pixels (x, y, width, height).
    pub capture_region: Option<CaptureRegion>,
    /// Preferred encoder backend name (e.g. "h264_nvenc"). `None` means the host picks.
    pub encoder: Option<String>,
    /// HDR color space (P2-14). `None` means SDR (BT.709 / sRGB).
    #[serde(default)]
    pub color_space: Option<ColorSpace>,
    /// 8 or 10. 8 is the implicit default. Drives encoder config
    /// (`Main` vs `Main10` for H.265, `profile 0 + bit-depth 10` for
    /// AV1, etc.).
    #[serde(default = "default_eight")]
    pub bit_depth: u8,
    /// Maximum framerate the host should drive the encoder at.
    /// Distinct from `framerate` (target). `None` = no explicit cap.
    #[serde(default)]
    pub max_framerate: Option<u32>,
    /// Target framerate the host should drive the encoder at.
    /// `None` lets the host pick (60 Hz default in practice).
    #[serde(default)]
    pub target_framerate: Option<u32>,
}

/// HDR color space advertised by the host (P2-14). `None` on
/// `VideoStreamPreferences::color_space` means the host should fall
/// back to BT.709 / sRGB SDR.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ColorSpace {
    /// ITU-R BT.709 (HD / SDR default).
    Bt709,
    /// ITU-R BT.2020 (UHD / WCG container for SDR and HDR).
    Bt2020,
    /// ITU-R BT.2100 PQ (HDR10, HDR10+, Dolby Vision profile 8).
    Bt2100Pq,
    /// ITU-R BT.2100 HLG (Hybrid Log-Gamma; broadcast HDR).
    Bt2100Hlg,
    /// scRGB (linear floating-point HDR; Windows only).
    ScRgb,
}

/// HDR static metadata advertised by the host at session start. Maps
/// directly to the HDR10 SEI / CTA-861-G static metadata block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HdrStaticMetadata {
    /// CIE 1931 chromaticity primaries per CTA-861-G §6.4.
    pub primaries: u8,
    /// Transfer characteristics. 16 = PQ (BT.2100), 18 = HLG.
    pub transfer: u8,
    /// Matrix coefficients. 9 = BT.2020 NCL, 10 = BT.2020 CL.
    pub matrix: u8,
    /// Max Content Light Level in 1 cd/m² units. 0 if unknown.
    pub max_cll: u16,
    /// Max Frame-Average Light Level in 1 cd/m² units. 0 if unknown.
    pub max_fall: u16,
    /// Mastering display chromaticity + luminance, encoded as a
    /// raw CTA-861-G SEI blob (24 bytes). Empty if unknown.
    #[serde(default)]
    pub mastering_display_metadata: Vec<u8>,
}

impl Default for HdrStaticMetadata {
    fn default() -> Self {
        Self {
            primaries: 9,
            transfer: 16,
            matrix: 9,
            max_cll: 0,
            max_fall: 0,
            mastering_display_metadata: Vec::new(),
        }
    }
}

#[inline]
fn default_eight() -> u8 {
    8
}

impl Default for VideoStreamPreferences {
    fn default() -> Self {
        Self {
            codec: None,
            width: None,
            height: None,
            framerate: None,
            bitrate_kbps: None,
            scale_mode: None,
            display_index: None,
            capture_region: None,
            encoder: None,
            color_space: None,
            bit_depth: default_eight(),
            max_framerate: None,
            target_framerate: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Per-frame rate feedback from the client to the host (P0-4). The host's
/// `GccRateController` consumes these at 4 Hz to drive the encoder's
/// bitrate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct RateFeedback {
    pub rtt_ms: u16,
    /// loss rate * 1000 (e.g. 12 = 1.2%). 0..=1000.
    pub loss_x1000: u16,
    /// inter-arrival jitter, milliseconds (0..=u16::MAX).
    pub jitter_ms: u16,
    /// Current one-way delay sample, milliseconds.
    pub one_way_delay_ms: f32,
    /// Minimum one-way delay observed in the session (path propagation, no
    /// queuing). milliseconds.
    pub one_way_delay_min_ms: f32,
}

impl Default for RateFeedback {
    fn default() -> Self {
        Self {
            rtt_ms: 0,
            loss_x1000: 0,
            jitter_ms: 0,
            one_way_delay_ms: 0.0,
            one_way_delay_min_ms: 0.0,
        }
    }
}

/// Control messages on the media-path's reliable stream (P0-2 §"Control
/// channel"). NACK + rate feedback (4 Hz) + keyframe request + gamepad
/// lifecycle + per-stream stats.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlMsg {
    Nack {
        stream_id: u16,
        frame_id: u32,
        missing_chunks: Vec<u16>,
    },
    RateFeedback(RateFeedback),
    KeyframeRequest {
        stream_id: u16,
    },
    StreamStats {
        stream_id: u16,
        frames_decoded: u32,
        frames_dropped: u32,
        frames_recovered: u32,
    },
    GamepadConnect {
        id: u8,
        name: String,
        kind: GamepadKind,
    },
    GamepadDisconnect {
        id: u8,
    },
    GamepadRumble {
        id: u8,
        low: u16,
        high: u16,
    },
    /// Client → Host: subscribe to specific display streams.
    /// If empty, subscribe to all active streams.
    StreamSubscribe {
        display_ids: Vec<u32>,
    },
    /// Client → Host: unsubscribe from specific display streams.
    /// display_ids must be a subset of currently subscribed streams.
    StreamUnsubscribe {
        display_ids: Vec<u32>,
    },
    /// Host → Client: show or hide a blank overlay on a specific display.
    /// Used as fallback when vkms is unavailable (BlankOverlayManager path).
    BlankOverlay {
        show: bool,
        display_id: Option<u32>,
    },
    /// Host → Client (or Host → Daemon): the privacy state of a display changed.
    DisplayStateChanged {
        display_id: u32,
        old_state: u8,
        new_state: u8,
    },
    /// Host↔Client (bidirectional): clipboard payload. Both directions use the
    /// same variant; the direction is implicit in the stream that carried it
    /// (host→client control uni-stream or client→host control uni-stream).
    ///
    /// `seq` is a monotonic counter per direction. Receivers apply the
    /// payload only if `seq > last_seen_seq_for_kind(kind)`, giving
    /// last-write-wins semantics and avoiding flip-flop when both sides
    /// copy in quick succession.
    ClipboardChanged {
        /// Monotonic per-direction counter (wraps at u64::MAX).
        seq: u64,
        /// What changed. Text and PNG image only in v1 (HTML deferred).
        payload: ClipboardPayload,
    },
    /// Client → Host: opt-in request to start streaming the microphone. The
    /// host replies with `MicConfigAck`. Idempotent: a second `MicStart`
    /// while a mic stream is already active is a no-op + a fresh `MicConfigAck`.
    MicStart {
        /// Negotiated audio parameters (sample rate, channels, frame size).
        /// All `#[serde(default)]` so an older client can omit fields.
        config: MicStreamConfig,
    },
    /// Client → Host: stop streaming the microphone.
    MicStop,
    /// Host → Client: acknowledge the latest `MicStart` with the actual
    /// parameters the host will use (may differ if the client requested
    /// something the host can't satisfy — e.g. 48 kHz vs 44.1 kHz).
    MicConfigAck {
        config: MicStreamConfig,
        /// True if the host successfully created the virtual input device.
        /// False means mic capture continues but the host app cannot hear
        /// it (e.g. PipeWire not available). Client surfaces a warning.
        virtual_device_ok: bool,
    },
    /// Host → Client: advertise what the host can deliver so the client
    /// can pick a compatible `VideoStreamPreferences` instead of probing
    /// blindly. Always sent once per session, immediately after
    /// `SessionEstablished`. `#[serde(default)]` on all fields.
    DisplayCapabilities {
        /// `None` means the host output is SDR (BT.709 / sRGB). `Some`
        /// means the host display(s) can deliver HDR static metadata.
        #[serde(default)]
        hdr_static_metadata: Option<HdrStaticMetadata>,
        /// Max pixel resolution supported by the capture pipeline (after
        /// any client-side downscaling request this maps to 1:1).
        max_resolution: [u16; 2],
        /// Max refresh rate the pipeline can sustain, in Hz.
        max_refresh_hz: u32,
    },
    /// Client → Host: enumerate tablet / pen devices connected to the
    /// client at session start. Host creates the matching `uinput` /
    /// `WinTab` virtual device so that injected events from the host
    /// appear to come from the matching physical tool.
    PenDeviceList {
        devices: Vec<PenDeviceDescriptor>,
    },
    /// Host → Client: a single high-level pen event (lifecycle, low
    /// frequency). High-frequency per-sample events ride the QUIC
    /// datagram path as `WirePenEvent`.
    PenEvent {
        device_id: u16,
        tool: PenTool,
        /// True when the tool made physical contact with the surface.
        #[serde(default)]
        contact: bool,
    },
}

impl ControlMsg {
    /// Borrow the [`ControlMsg::DisplayCapabilities`] fields. Returns
    /// `None` when `self` is a different variant.
    pub fn display_capabilities(&self) -> Option<DisplayCapabilitiesView> {
        match self {
            ControlMsg::DisplayCapabilities {
                hdr_static_metadata,
                max_resolution,
                max_refresh_hz,
            } => Some(DisplayCapabilitiesView {
                hdr_static_metadata: hdr_static_metadata.clone(),
                max_resolution: *max_resolution,
                max_refresh_hz: *max_refresh_hz,
            }),
            _ => None,
        }
    }
}

/// Read-only view of the [`ControlMsg::DisplayCapabilities`] payload
/// without exposing the `pub(crate)` enum fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayCapabilitiesView {
    pub hdr_static_metadata: Option<HdrStaticMetadata>,
    pub max_resolution: [u16; 2],
    pub max_refresh_hz: u32,
}

/// What is on the clipboard right now. Tagged enum so the wire format
/// carries a `kind` discriminator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClipboardPayload {
    /// UTF-8 text. PNG-free path; smallest payload.
    Text { utf8: String },
    /// PNG-encoded image. `width` and `height` are pre-encoding
    /// pixel dimensions; the receiver passes the PNG bytes to
    /// `arboard::Clipboard::set_image` (which decodes back to RGBA).
    ImagePng {
        width: u32,
        height: u32,
        png: Vec<u8>,
    },
    /// Empty clipboard (user selected "clear"). Sent on every
    /// transition from non-empty to empty so the receiver
    /// unconditionally drops its own cached content.
    Clear,
}

/// Negotiated microphone stream configuration. Every numeric field
/// defaults to 0 (and is validated downstream); bool fields default
/// to `true` via `default_true` so an older client can omit them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MicStreamConfig {
    /// Sample rate in Hz. 48_000 is the default; 16_000 also supported.
    #[serde(default)]
    pub sample_rate_hz: u32,
    /// Always 1 (mono) in v1.
    #[serde(default)]
    pub channels: u8,
    /// Frame size in milliseconds: 10, 20, or 60. Default 20.
    #[serde(default)]
    pub frame_ms: u8,
    /// Opus bitrate in bits per second. 32_000..=128_000. Default 64_000.
    #[serde(default)]
    pub bitrate_bps: u32,
    /// Whether the client should run AEC3 before encoding. Default true.
    #[serde(default = "default_true")]
    pub aec_enabled: bool,
    /// Whether the client should run NS (WebRTC + RNNoise) before encoding.
    #[serde(default = "default_true")]
    pub ns_enabled: bool,
    /// Whether the client should run AGC2 before encoding.
    #[serde(default = "default_true")]
    pub agc_enabled: bool,
}

impl Default for MicStreamConfig {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000,
            channels: 1,
            frame_ms: 20,
            bitrate_bps: 64_000,
            aec_enabled: true,
            ns_enabled: true,
            agc_enabled: true,
        }
    }
}

/// Returns `true`; used as `#[serde(default = "default_true")]` on
/// `MicStreamConfig` bool fields so a v1 client that omits the field
/// gets the safe (processing-on) default.
#[inline]
pub fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartSessionRequest {
    pub session_id: Uuid,
    pub target_host_id: Uuid,
    pub requested_transport: Option<TransportKind>,
    pub preferred_codec: Option<VideoCodec>,
    /// Granular video stream preferences. When set, supersedes `preferred_codec`.
    pub video: Option<VideoStreamPreferences>,
    /// Session capability mask (defaults: all allowed).
    /// On managed Cloud, signaling overwrites this from accounts authorize.
    #[serde(default)]
    pub permissions: SessionPermissions,
    /// ADR-022 Phase C: open QUIC for FileSync only (no video/audio media).
    #[serde(default)]
    pub sync_only: bool,
    /// After owner approves a pending consent, client retries with this id.
    #[serde(default)]
    pub consent_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionCredential {
    /// Legacy bearer token (UUID string). Old paths send this verbatim
    /// over the QUIC auth stream and the signaling token-gated routes.
    /// New bound credentials still carry it for log-friendly correlation
    /// and for transport-layer back-compat.
    #[serde(default)]
    pub token: String,
    pub expires_unix_millis: u64,
    /// New (P0 trust rewrite): when issued via
    /// [`SessionCredential::issue`], these fields are populated and an
    /// HMAC binds `(session_id, host_pubkey, client_pubkey, exp)` to a
    /// server-side secret.
    #[serde(default = "Uuid::nil")]
    pub session_id: Uuid,
    #[serde(default)]
    pub host_pubkey: [u8; 32],
    #[serde(default)]
    pub client_pubkey: [u8; 32],
    #[serde(default)]
    pub issued_unix_millis: u64,
    #[serde(default)]
    pub hmac: [u8; 32],
}

impl SessionCredential {
    /// Build a token-only legacy credential. New code should prefer
    /// [`SessionCredential::issue`] to produce a credential bound to two
    /// device pubkeys.
    pub fn new_legacy_token(expires_unix_millis: u64) -> Self {
        Self {
            token: Uuid::new_v4().to_string(),
            expires_unix_millis,
            session_id: Uuid::nil(),
            host_pubkey: [0u8; 32],
            client_pubkey: [0u8; 32],
            issued_unix_millis: 0,
            hmac: [0u8; 32],
        }
    }

    /// Issue a credential bound to `(session_id, host_pubkey,
    /// client_pubkey, expires_unix_millis)` under the server's HMAC
    /// secret.
    pub fn issue(
        server_secret: &[u8],
        session_id: Uuid,
        host_pubkey: [u8; 32],
        client_pubkey: [u8; 32],
        issued_unix_millis: u64,
        expires_unix_millis: u64,
    ) -> Self {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(server_secret)
            .expect("HMAC accepts any key length");
        mac.update(session_id.as_bytes());
        mac.update(&host_pubkey);
        mac.update(&client_pubkey);
        mac.update(&issued_unix_millis.to_be_bytes());
        mac.update(&expires_unix_millis.to_be_bytes());
        let tag = mac.finalize().into_bytes();
        let mut hmac = [0u8; 32];
        hmac.copy_from_slice(&tag);

        Self {
            token: session_id.to_string(),
            expires_unix_millis,
            session_id,
            host_pubkey,
            client_pubkey,
            issued_unix_millis,
            hmac,
        }
    }

    /// Returns `true` iff the credential carries a non-empty bound
    /// signature AND the signature verifies under `server_secret` AND
    /// `now_unix_millis < expires_unix_millis`.
    pub fn verify(&self, server_secret: &[u8], now_unix_millis: u64) -> bool {
        if self.host_pubkey == [0u8; 32] || self.client_pubkey == [0u8; 32] {
            return false;
        }
        if self.hmac == [0u8; 32] {
            return false;
        }
        if now_unix_millis >= self.expires_unix_millis {
            return false;
        }
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(server_secret)
            .expect("HMAC accepts any key length");
        mac.update(self.session_id.as_bytes());
        mac.update(&self.host_pubkey);
        mac.update(&self.client_pubkey);
        mac.update(&self.issued_unix_millis.to_be_bytes());
        mac.update(&self.expires_unix_millis.to_be_bytes());
        mac.verify_slice(&self.hmac).is_ok()
    }
}

/// Context tag prepended to the descriptor bytes before signing,
/// preventing cross-protocol signature reuse.
pub const SIGNED_HELLO_CONTEXT: &[u8] = b"qubox-hello-v1";

/// A `Hello` message whose `PeerDescriptor` has been signed with the
/// senders's Ed25519 signing key. `verify` checks the signature against
/// the embedded `public_key`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedHello {
    pub descriptor: PeerDescriptor,
    pub public_key: [u8; 32],
    pub signature: Vec<u8>,
}

impl SignedHello {
    /// Sign a `PeerDescriptor` with an Ed25519 `SigningKey`.
    pub fn sign(descriptor: &PeerDescriptor, signing_key: &ed25519_dalek::SigningKey) -> Self {
        use ed25519_dalek::Signer;
        let body = serde_json::to_vec(descriptor).expect("PeerDescriptor is serializable");
        let mut signed = Vec::with_capacity(SIGNED_HELLO_CONTEXT.len() + body.len());
        signed.extend_from_slice(SIGNED_HELLO_CONTEXT);
        signed.extend_from_slice(&body);
        let signature = signing_key.sign(&signed);
        Self {
            descriptor: descriptor.clone(),
            public_key: signing_key.verifying_key().to_bytes(),
            signature: signature.to_bytes().to_vec(),
        }
    }

    /// Returns `true` iff `self.signature` is a valid Ed25519 signature
    /// over `SIGNED_HELLO_CONTEXT || serde_json(descriptor)` signed by
    /// the key matching `self.public_key`.
    pub fn verify(&self) -> bool {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let Ok(vk) = VerifyingKey::from_bytes(&self.public_key) else {
            return false;
        };
        let sig_bytes: [u8; 64] = match self.signature.as_slice().try_into() {
            Ok(s) => s,
            Err(_) => return false,
        };
        let body = match serde_json::to_vec(&self.descriptor) {
            Ok(b) => b,
            Err(_) => return false,
        };
        let mut signed = Vec::with_capacity(SIGNED_HELLO_CONTEXT.len() + body.len());
        signed.extend_from_slice(SIGNED_HELLO_CONTEXT);
        signed.extend_from_slice(&body);
        vk.verify(&signed, &Signature::from_bytes(&sig_bytes))
            .is_ok()
    }
}

/// Generate a fresh Ed25519 signing key using the OS RNG.
pub fn generate_signing_key() -> ed25519_dalek::SigningKey {
    use rand_core::OsRng;
    ed25519_dalek::SigningKey::generate(&mut OsRng)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VideoStreamParams {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioStreamParams {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(
    feature = "wire-rkyv-v2",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(feature = "wire-rkyv-v2", rkyv(derive(Debug, PartialEq, Eq)))]
#[serde(rename_all = "snake_case")]
pub enum InputMouseButton {
    Left,
    Right,
    Middle,
}

// ── ADR-019: two-tier input path ──────────────────────────────────

/// Magic prefix for the reliable input stream (rkyv v2 era, ADR-015 §4).
pub const WIRE_INPUT_MAGIC: [u8; 2] = [0x52, 0x42];

/// QUIC IMMEDIATE_ACK frame type per draft-ietf-quic-ack-frequency-14.
/// IANA permanent assignment (was `0xac` in draft-05).
pub const IMMEDIATE_ACK_FRAME_TYPE: u8 = 0x1f;

/// ACK policy selector for the reliable input stream.
/// Full `AckFrequencyConfig` wiring lives in `qubox-transport` per ADR-011.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AckPolicy {
    /// Sparse ACKs (media). max_ack_delay = 10 ms.
    Media,
    /// Dense ACKs (control). max_ack_delay = 1 ms.
    Control,
    /// Immediate ACK on FLAG_LAST_IN_BURST. threshold=1, max_ack_delay=1 ms.
    InputImmediate,
}

impl AckPolicy {
    pub fn ack_eliciting_threshold(self) -> u8 {
        match self {
            AckPolicy::Media => 10,
            AckPolicy::Control => 1,
            AckPolicy::InputImmediate => 1,
        }
    }

    pub fn max_ack_delay_us(self) -> u64 {
        match self {
            AckPolicy::Media => 10_000,
            AckPolicy::Control => 1_000,
            AckPolicy::InputImmediate => 1_000,
        }
    }
}

// ── ADR-019 §4: WireMouseMotion datagram type ────────────────────

/// Discriminator byte for mouse motion datagrams. ASCII `'K'`.
/// Allocated from the 0x40-0x5F range per ADR-010 §13.
pub const MOUSE_MOTION_DISCRIMINATOR: u8 = 0x4B;

/// 12-byte packed mouse motion datagram. Sent over QUIC unreliable
/// datagrams; loss is tolerated (next sample supersedes).
///
/// Layout:
/// ```text
/// [0x51][0x42]  MEDIA_DATAGRAM_MAGIC (2 bytes)
/// [0x4B]        MOUSE_MOTION_DISCRIMINATOR (1 byte)
/// [0x00]        flags (reserved, 1 byte)
/// [i16 LE dx]   relative motion x (2 bytes)
/// [i16 LE dy]   relative motion y (2 bytes)
/// [u32 LE ts]   timestamp_us (4 bytes)
/// ```
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WireMouseMotion {
    pub magic: [u8; 2],
    pub discriminator: u8,
    pub flags: u8,
    pub dx: i16,
    pub dy: i16,
    pub timestamp_us: u32,
}

impl WireMouseMotion {
    pub const SIZE: usize = 12;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..2].copy_from_slice(&self.magic);
        out[2] = self.discriminator;
        out[3] = self.flags;
        out[4..6].copy_from_slice(&self.dx.to_le_bytes());
        out[6..8].copy_from_slice(&self.dy.to_le_bytes());
        out[8..12].copy_from_slice(&self.timestamp_us.to_le_bytes());
        out
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Self, MouseMotionError> {
        if buf.len() < Self::SIZE {
            return Err(MouseMotionError::Short);
        }
        if buf[0..2] != [0x51, 0x42] {
            return Err(MouseMotionError::BadMagic);
        }
        if buf[2] != MOUSE_MOTION_DISCRIMINATOR {
            return Err(MouseMotionError::BadDiscriminator);
        }
        Ok(Self {
            magic: [buf[0], buf[1]],
            discriminator: buf[2],
            flags: buf[3],
            dx: i16::from_le_bytes([buf[4], buf[5]]),
            dy: i16::from_le_bytes([buf[6], buf[7]]),
            timestamp_us: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
        })
    }

    /// Read `dx` without creating an unaligned reference. Safe because
    /// the packed repr means we must use `read_unaligned`.
    pub fn dx_value(&self) -> i16 {
        unsafe { std::ptr::addr_of!(self.dx).read_unaligned() }
    }

    /// Read `dy` without creating an unaligned reference.
    pub fn dy_value(&self) -> i16 {
        unsafe { std::ptr::addr_of!(self.dy).read_unaligned() }
    }

    /// Read `timestamp_us` without creating an unaligned reference.
    pub fn timestamp_us_value(&self) -> u32 {
        unsafe { std::ptr::addr_of!(self.timestamp_us).read_unaligned() }
    }
}

pub const WIRE_MOUSE_MOTION_SIZE: usize = WireMouseMotion::SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseMotionError {
    Short,
    BadMagic,
    BadDiscriminator,
}

impl std::fmt::Display for MouseMotionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MouseMotionError::Short => write!(f, "mouse motion datagram too short"),
            MouseMotionError::BadMagic => write!(f, "mouse motion datagram magic mismatch"),
            MouseMotionError::BadDiscriminator => {
                write!(f, "mouse motion datagram discriminator mismatch")
            }
        }
    }
}

impl std::error::Error for MouseMotionError {}

#[cfg(feature = "wire-rkyv-v2")]
pub use wire::RemoteCriticalInput;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteInputEvent {
    MouseMove {
        x: u32,
        y: u32,
    },
    /// Relative mouse motion in pixels, captured from the client's locked pointer
    /// (e.g. `winit::DeviceEvent::MouseMotion { delta }`). Host injects via
    /// `enigo::move_mouse(dx, dy, Coordinate::Rel)`. Independent of display
    /// resolution — required for FPS / TPS games.
    RelativeMouseMove {
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: InputMouseButton,
        pressed: bool,
    },
    /// Vertical (dy) and horizontal (dx) scroll. dy positive = scroll up.
    MouseWheel {
        dx: i32,
        dy: i32,
    },
    Keyboard {
        key: String,
        pressed: bool,
    },
    /// Gamepad state (P0-6). Sent on the data-plane datagram channel
    /// separately from the reliable input event stream — gamepad state is
    /// high-frequency and tolerates loss of intermediate samples. The
    /// connect/disconnect/rumble lifecycle rides on the reliable control
    /// stream via `ControlMsg::Gamepad*`.
    Gamepad {
        state: WireGamepadState,
    },
    /// Emitted by the host when the cursor crosses a display boundary.
    /// display_id identifies the display the cursor has entered.
    HoverDisplay {
        display_id: u32,
    },
    /// Pen / tablet event captured on the client. Routes over the
    /// same reliable control uni-stream as keyboard/mouse, NOT through
    /// the high-frequency datagram path; pen events tolerate loss but
    /// the connection/disconnection lifecycle requires reliability. The
    /// high-frequency per-sample stream rides `WirePenEvent` over the
    /// QUIC datagram channel (`PEN_DATAGRAM_DISCRIMINATOR` = `0x50`).
    Pen {
        /// Logical pen tool. `#[serde(default)]` for backward compat.
        #[serde(default)]
        tool: PenTool,
        /// 0.0 ..= 1.0; Pen only meaningful (else 0).
        #[serde(default)]
        pressure: f32,
        /// Degrees, -90 ..= 90; Pen only.
        #[serde(default)]
        tilt_x: f32,
        /// Degrees, -90 ..= 90; Pen only.
        #[serde(default)]
        tilt_y: f32,
        /// Degrees, 0 ..= 360.
        #[serde(default)]
        rotation: f32,
        /// Tool-specific button bitmask (Eraser tip, barrel buttons, etc.).
        #[serde(default)]
        button_state: u32,
        /// Pixels, screen-space, relative to the captured display.
        x: u16,
        y: u16,
        /// Hover distance in millimeters; 0 = contact; u16::MAX = "out of range".
        #[serde(default)]
        hover_distance: u16,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPlan {
    pub session_id: Uuid,
    pub target_host_id: Uuid,
    pub transport: TransportKind,
    pub codec: VideoCodec,
    pub client_credential: SessionCredential,
    pub ice_servers: Vec<IceServer>,
    #[serde(default)]
    pub permissions: SessionPermissions,
    /// ADR-022 Phase C: FileSync-only session (no media streams required).
    #[serde(default)]
    pub sync_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRequested {
    pub session_id: Uuid,
    pub client: PeerDescriptor,
    pub transport: TransportKind,
    pub codec: VideoCodec,
    pub host_credential: SessionCredential,
    pub client_credential: SessionCredential,
    pub ice_servers: Vec<IceServer>,
    /// Optional per-session video preferences negotiated with the client.
    /// `None` means "use the host's defaults".
    #[serde(default)]
    pub video: Option<VideoStreamPreferences>,
    #[serde(default)]
    pub permissions: SessionPermissions,
    /// ADR-022 Phase C: FileSync-only session.
    #[serde(default)]
    pub sync_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelaySignal {
    pub session_id: Uuid,
    pub from_peer_id: Uuid,
    pub to_peer_id: Uuid,
    pub signal: SessionSignal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionSignal {
    SdpOffer {
        sdp: String,
    },
    SdpAnswer {
        sdp: String,
    },
    IceCandidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    NativeQuicTicket {
        alpn: String,
        ticket_b64: String,
    },
    Ready,
}

// ── P2 session bundle (cloud-signed, audience-bound) ────────────────
//
// Per `docs/browser-viewer-identity-and-host-trust.md` Phase 2, the cloud
// issues a *mutual* session bundle to both peers instead of an opaque
// signaling-only token. Each half is Ed25519-signed by the cloud; both
// peers verify against the same JWKS.
//
// Wire JSON is `camelCase` (matches Stream A's TypeScript schema). The
// canonical signing payload is a JSON document with **sorted keys** to
// make signatures deterministic regardless of struct field order. Use
// `canonical_cbor_bytes` / canonicalize helpers below.

/// One TURN credential entry paired with an ICE server URL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnCreds {
    pub username: String,
    pub credential: String,
}

/// Per-session capability mask used inside the cloud-signed bundle.
/// Distinct from the existing `SessionPermissions` so the bundle stays
/// self-describing on the wire (Stream A does not need to depend on the
/// OSS crate hierarchy).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionCaps {
    #[serde(default)]
    pub input: bool,
    #[serde(default)]
    pub clipboard: bool,
    #[serde(default)]
    pub mic: bool,
    #[serde(default)]
    pub files: bool,
    #[serde(default)]
    pub audio: bool,
}

/// `viewer_to_host` half of the mutual session bundle.
///
/// Audience (`aud`) MUST equal the target host's `device_id`. The host
/// pins the DTLS fingerprint of the WebRTC peer against
/// `viewer_dtls_fp`; a mismatch aborts the session before any media
/// frames are decoded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ViewerToHost {
    /// Schema version of this bundle (`1`).
    pub v: u8,
    pub jti: String,
    pub sid: String,
    /// Account id (or `account_id`) of the human holding the viewer.
    pub sub: String,
    /// Target host `device_id`.
    pub aud: String,
    /// Issued-at (unix milliseconds).
    pub iat: i64,
    /// Expires-at (unix milliseconds).
    pub exp: i64,
    pub caps: SessionCaps,
    /// WebRTC DTLS fingerprint the viewer will present during handshake.
    #[serde(alias = "viewer_dtls_fp")]
    pub viewer_dtls_fp: String,
}

/// `host_to_viewer` half of the mutual session bundle.
///
/// `sid` is the canonical session id both peers track. The viewer
/// pins its peer DTLS fingerprint against `host_dtls_fp` so a
/// compromised signaling server cannot MITM the host side.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostToViewer {
    pub v: u8,
    pub jti: String,
    pub sid: String,
    /// Host device_id (issuing party).
    pub sub: String,
    /// Account id the viewer authenticated as.
    pub aud: String,
    pub iat: i64,
    pub exp: i64,
    /// WebRTC DTLS fingerprint the host will present during handshake.
    #[serde(alias = "host_dtls_fp")]
    pub host_dtls_fp: String,
    pub caps: SessionCaps,
}

/// Cloud-signed ICE / TURN allowlist. Both ends reject ICE servers
/// that are not in `urls`. Only `stun:` / `turn:` / `turns:` schemes
/// are accepted on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IceAllowlist {
    pub v: u8,
    pub jti: String,
    pub exp: i64,
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creds: Option<TurnCreds>,
}

/// Cloud-signed wire envelope. Shape (matches Stream A's TypeScript
/// schema):
///
/// ```text
/// {
///   "kid": "<jwk kid>",
///   "v": <payload schema version>,
///   "payload": "<base64url(canonical_json)>",
///   "sig": "<base64url(ed25519_signature)>"
/// }
/// ```
///
/// `payload` is the canonical-JSON serialization of the underlying
/// `ViewerToHost` / `HostToViewer` / `IceAllowlist` / `SignedKill`.
/// The verifier resolves `kid` → JWKS, fetches the public key, and
/// verifies the Ed25519 signature over the raw payload bytes before
/// JSON-decoding `payload`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SignedBundle {
    pub kid: String,
    /// Schema version (mirrors the embedded payload's `v`).
    pub v: u8,
    pub payload: String,
    pub sig: String,
}

impl SignedBundle {
    /// Build a wire envelope from an arbitrary payload.
    pub fn new<T: Serialize>(
        value: &T,
        kid: impl Into<String>,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> serde_json::Result<Self> {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use ed25519_dalek::Signer;
        use serde_json::Value;

        let payload = canonical_json_bytes(value)?;
        let sig = signing_key.sign(&payload);
        let payload_b64 = URL_SAFE_NO_PAD.encode(&payload);
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
        let version = serde_json::to_value(value)
            .ok()
            .and_then(|v| match v {
                Value::Object(map) => map.get("v").and_then(|x| x.as_u64()).map(|x| x as u8),
                _ => None,
            })
            .unwrap_or(0);
        Ok(SignedBundle {
            kid: kid.into(),
            v: version,
            payload: payload_b64,
            sig: sig_b64,
        })
    }

    pub fn from_compact(
        token: &str,
        kid: impl Into<String>,
    ) -> Result<Self, SignedBundleError> {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;

        let (payload, sig) = token
            .split_once('.')
            .ok_or(SignedBundleError::MalformedEnvelope)?;
        if payload.is_empty() || sig.is_empty() || sig.contains('.') {
            return Err(SignedBundleError::MalformedEnvelope);
        }
        let payload_bytes = URL_SAFE_NO_PAD
            .decode(payload.as_bytes())
            .map_err(|_| SignedBundleError::BadBase64)?;
        let sig_bytes = URL_SAFE_NO_PAD
            .decode(sig.as_bytes())
            .map_err(|_| SignedBundleError::BadBase64)?;
        if sig_bytes.len() != 64 {
            return Err(SignedBundleError::BadSignatureLength);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&payload_bytes).map_err(SignedBundleError::BadPayload)?;
        let version = value
            .get("v")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u8::try_from(v).ok())
            .unwrap_or(0);
        Ok(Self {
            kid: kid.into(),
            v: version,
            payload: payload.to_string(),
            sig: sig.to_string(),
        })
    }

    pub fn to_compact(&self) -> String {
        format!("{}.{}", self.payload, self.sig)
    }

    /// Verify the Ed25519 signature over the raw payload bytes using
    /// the supplied verifying key. Does NOT deserialize the payload.
    pub fn verify_signature(
        &self,
        verify_pk: &ed25519_dalek::VerifyingKey,
    ) -> Result<(), SignedBundleError> {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use ed25519_dalek::{Signature, Verifier};

        let payload = URL_SAFE_NO_PAD
            .decode(self.payload.as_bytes())
            .map_err(|_| SignedBundleError::BadBase64)?;
        let sig_bytes = URL_SAFE_NO_PAD
            .decode(self.sig.as_bytes())
            .map_err(|_| SignedBundleError::BadBase64)?;
        let sig_array: [u8; 64] = sig_bytes
            .as_slice()
            .try_into()
            .map_err(|_| SignedBundleError::BadSignatureLength)?;
        verify_pk
            .verify(&payload, &Signature::from_bytes(&sig_array))
            .map_err(|_| SignedBundleError::SignatureMismatch)
    }

    /// Verify + decode the payload as `T`.
    pub fn decode<T: for<'de> Deserialize<'de>>(
        &self,
        verify_pk: &ed25519_dalek::VerifyingKey,
    ) -> Result<T, SignedBundleError> {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;

        self.verify_signature(verify_pk)?;
        let payload = URL_SAFE_NO_PAD
            .decode(self.payload.as_bytes())
            .map_err(|_| SignedBundleError::BadBase64)?;
        serde_json::from_slice(&payload).map_err(SignedBundleError::BadPayload)
    }
}

/// Cloud-signed mid-session kill.
///
/// Same envelope shape as the session bundles. The host verifies
/// `aud == host.device_id` and that the `sid` matches an active
/// session before tearing down P2P.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SignedKill {
    pub v: u8,
    pub jti: String,
    pub sid: String,
    /// Host device_id whose session must be terminated.
    pub aud: String,
    /// Account id the operator acted as.
    #[serde(default)]
    pub sub: String,
    pub iat: i64,
    pub exp: i64,
    pub reason: String,
}

/// Compute the deterministic, sorted-keys JSON bytes for a payload
/// that will be fed to the Ed25519 signer.
///
/// This matches the JCS-ish convention used by the cloud: objects
/// re-emit with keys sorted by their UTF-8 byte order, no whitespace,
/// numbers in their natural Rust `Display` form (which is what
/// `serde_json::Number::to_string` produces for finite values).
pub fn canonical_json_bytes<T: Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    let v = serde_json::to_value(value)?;
    Ok(canonicalize_value(&v).into_bytes())
}

fn canonicalize_value(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()),
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(canonicalize_value).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let parts: Vec<String> = entries
                .into_iter()
                .map(|(k, v)| format!("{}:{}", serde_json::to_string(k).unwrap_or_else(|_| "\"\"".to_string()), canonicalize_value(v)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

/// Encode a bundle + Ed25519 signature into the canonical wire form:
/// `<base64url(payload)>.<base64url(signature)>`. Base64url = URL-safe
/// no-padding (RFC 4648 §5). The `kid` is intentionally NOT part of the
/// signed payload — the JWKS-resolver layer wraps the wire string in
/// a higher-level envelope (see `SignedBundle`) that carries `kid`
/// alongside it.
pub fn encode_signed_bundle<T: Serialize>(
    value: &T,
    signing_key: &ed25519_dalek::SigningKey,
) -> serde_json::Result<String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use ed25519_dalek::Signer;

    let payload = canonical_json_bytes(value)?;
    let sig = signing_key.sign(&payload);
    let payload_b64 = URL_SAFE_NO_PAD.encode(&payload);
    let sig_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
    let parts = [payload_b64, sig_b64];
    Ok(format!("{}.{}", parts[0], parts[1]))
}

/// Decode + verify an Ed25519-signed wire envelope. Returns the parsed
/// payload on success.
///
/// `verify_pk` is the Ed25519 verifying key the JWKS resolver returned
/// for the envelope's `kid`. The caller is responsible for the JWKS
/// lookup (and any clock-skew tolerance) — this helper only does the
/// signature math + payload deserialization.
pub fn decode_signed_bundle<T: for<'de> Deserialize<'de>>(
    envelope_b64: &str,
    verify_pk: &ed25519_dalek::VerifyingKey,
) -> Result<T, SignedBundleError> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use ed25519_dalek::{Signature, Verifier};

    let (payload_b64, sig_b64) = envelope_b64
        .split_once('.')
        .ok_or(SignedBundleError::MalformedEnvelope)?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64.as_bytes())
        .map_err(|_| SignedBundleError::BadBase64)?;
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64.as_bytes())
        .map_err(|_| SignedBundleError::BadBase64)?;
    let sig_array: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| SignedBundleError::BadSignatureLength)?;
    verify_pk
        .verify(&payload, &Signature::from_bytes(&sig_array))
        .map_err(|_| SignedBundleError::SignatureMismatch)?;
    serde_json::from_slice(&payload).map_err(SignedBundleError::BadPayload)
}

#[derive(Debug)]
pub enum SignedBundleError {
    MalformedEnvelope,
    BadBase64,
    BadSignatureLength,
    SignatureMismatch,
    BadPayload(serde_json::Error),
}

impl std::fmt::Display for SignedBundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignedBundleError::MalformedEnvelope => write!(f, "envelope missing '.' separator"),
            SignedBundleError::BadBase64 => write!(f, "envelope base64url decode failed"),
            SignedBundleError::BadSignatureLength => {
                write!(f, "Ed25519 signature is not 64 bytes")
            }
            SignedBundleError::SignatureMismatch => write!(f, "signature did not verify"),
            SignedBundleError::BadPayload(e) => write!(f, "payload deserialize failed: {e}"),
        }
    }
}

impl std::error::Error for SignedBundleError {}

/// Scheme validation for ICE server URLs. Per spec, only `stun:`,
/// `turn:`, and `turns:` are acceptable on the bundle wire. Anything
/// else is rejected so a compromised signaling server cannot inject
/// arbitrary relay hosts.
pub fn ice_url_is_valid(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("stun:") || lower.starts_with("turn:") || lower.starts_with("turns:")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Welcome {
    pub self_id: Uuid,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresenceEvent {
    pub peer: PeerDescriptor,
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorMessage {
    pub code: String,
    pub message: String,
}

impl ErrorMessage {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello(PeerDescriptor),
    /// Preferred: hello that proves ownership of an Ed25519 keypair.
    /// New builds should send this; legacy `Hello` is still accepted
    /// but the peer is registered without a pubkey.
    SignedHello(SignedHello),
    ListHosts,
    RequestPairing(PairingRequest),
    PairingDecision(PairingDecision),
    StartSession(StartSessionRequest),
    /// P2-2: Client presents a cloud-signed `ViewerToHost` bundle
    /// alongside the session request. The signaling relay verifies
    /// JWKS / audience / exp / jti before passing the request on to
    /// the host. Optional — clients that still use the HMAC-bound
    /// `SessionCredential` path can omit it.
    StartSessionWithBundle(StartSessionBundleRequest),
    RelaySignal(RelaySignal),
    Heartbeat,
    /// Host revokes an existing pair grant.
    RevokePairing {
        host_peer_id: Uuid,
        client_peer_id: Uuid,
    },
    /// Either participant ends an active session.
    KickSession {
        session_id: Uuid,
        #[serde(default)]
        reason: String,
    },
    /// Host creates a share/pair link (code redeemable by client).
    CreateShareLink {
        #[serde(default)]
        ttl_secs: u64,
        #[serde(default)]
        permissions: SessionPermissions,
    },
    /// Client redeems a share link to request pairing with the host.
    RedeemShareLink {
        code: String,
        #[serde(default)]
        client_label: String,
    },
    /// P2-2: Cloud pushes a signed kill for an active session.
    /// Operator-side message; only the JWKS-verifying relay should
    /// accept it (managed cloud admins, not LAN self-host). The host
    /// independently re-verifies the envelope before tearing down
    /// P2P.
    SignedKill(SignedBundle),
}

/// Wrapper that pairs a legacy `StartSessionRequest` with an optional
/// cloud-signed `ViewerToHost` bundle. Either field is enough to drive
/// a session on its own; the relay prefers the bundle when present
/// because it carries the `aud` / `jti` / DTLS-fp binding the host
/// needs to enforce.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StartSessionBundleRequest {
    #[serde(flatten)]
    pub request: StartSessionRequest,
    pub viewer_bundle: SignedBundle,
    /// Optional signed ICE allowlist. When present, the relay
    /// filters the candidate `ice_servers` against it before
    /// forwarding the request to the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ice_allowlist: Option<SignedBundle>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome(Welcome),
    Hosts {
        hosts: Vec<PeerDescriptor>,
    },
    PairingRequested(PairingRequested),
    PairingEstablished(PairingGrant),
    PairingRejected {
        request_id: Uuid,
        reason: String,
    },
    SessionPlanned(SessionPlan),
    SessionRequested(Box<SessionRequested>),
    Signal(RelaySignal),
    Presence(PresenceEvent),
    HeartbeatAck,
    Error(ErrorMessage),
    ShareLinkCreated {
        code: String,
        expires_unix_ms: u64,
        /// Suggested deep-link / URL hint for QR (may be relative).
        url_hint: String,
    },
    SessionKicked {
        session_id: Uuid,
        reason: String,
    },
    PairingRevoked {
        host_peer_id: Uuid,
        client_peer_id: Uuid,
    },
    /// Managed Cloud: session needs owner approval (dashboard / host UI).
    SessionConsentPending {
        consent_id: Uuid,
        client_peer_id: Uuid,
        host_peer_id: Uuid,
        expires_at_unix_ms: u64,
        #[serde(default)]
        client_label: String,
    },
    /// P2-2: Cloud-signed `ViewerToHost` bundle was accepted by the
    /// relay and the host is being notified. Forwarded to the host
    /// so it can independently compare the actual WebRTC DTLS
    /// fingerprint against `viewer_dtls_fp` from the bundle.
    SessionBundleAccepted(SessionBundleInfo),
    /// P2-2: Cloud-signed kill for an active session. The host
    /// verifies the envelope itself against JWKS before tearing
    /// down P2P, but the relay pre-applies the local denylist so
    /// concurrent bundle replays are rejected even before the host
    /// observes the kill.
    SignedKillReceived(SignedKillEnvelope),
}

/// Wire-friendly wrapper that pairs a `SignedKill` payload with its
/// `SignedBundle` envelope, so the host can re-verify the kill
/// without having to rebuild the envelope from scratch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SignedKillEnvelope {
    pub payload: SignedKill,
    pub envelope: SignedBundle,
}

/// Trimmed `ViewerToHost` payload that the relay forwards to the
/// host alongside `SessionRequested`. Keeps the DTLS-fp + jti
/// binding available for the host's own media-time check without
/// re-issuing the JWKS request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionBundleInfo {
    pub jti: String,
    pub viewer_dtls_fp: String,
    pub exp_unix_ms: u64,
    pub caps: SessionCaps,
    pub sub: String,
}

/// Events emitted by the daemon to subscribed clients.
/// Wire format: serde_json with `#[serde(tag = "kind")]` for forward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IpcEvent {
    /// A pairing request was initiated by a client.
    PairingRequest {
        host_id: uuid::Uuid,
        client_id: uuid::Uuid,
        code: String,
    },
    /// The host's online/offline state changed.
    HostStateChanged { host_id: uuid::Uuid, online: bool },
    /// A display's privacy state changed.
    /// `old_state` and `new_state` are u8 (0=Active, 1=Privacy, 2=Blanked)
    /// for forward compatibility with future state values.
    DisplayStateChanged {
        display_id: u32,
        old_state: u8,
        new_state: u8,
    },
    /// Emitted by the daemon when the active session state changes.
    /// Subscribers (host-agent, qubox-client-cli) use this to gate clipboard
    /// sync and microphone streaming. Sensitive data must not flow when
    /// `active == false`.
    SessionStateChanged {
        /// True while a host↔client media session is established.
        active: bool,
        /// Optional session id (None when `active == false`).
        #[serde(default)]
        session_id: Option<Uuid>,
        /// Why the state changed (for UI / audit log).
        reason: SessionStateReason,
    },
}

/// Reason a session-state transition happened. Serialized as snake_case
/// to keep the wire format stable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStateReason {
    SessionEstablished,
    SessionEnded,
    DaemonShuttingDown,
    PairingRevoked,
}

impl IpcEvent {
    /// Create a DisplayStateChanged event from a display ID and state transition.
    pub fn display_state_changed(display_id: u32, old_state: u8, new_state: u8) -> Self {
        IpcEvent::DisplayStateChanged {
            display_id,
            old_state,
            new_state,
        }
    }
}

/// Convert a `DisplayState` (from qubox-display) to its wire-representation u8.
#[inline]
pub fn display_state_to_u8(state: u8) -> u8 {
    state
}

/// Mic datagram discriminator byte. Placed at offset 2 immediately
/// after the 2-byte `MEDIA_DATAGRAM_MAGIC` (`[0x51, 0x42]`). Distinct
/// from gamepad (0x47) so a single shared dispatch byte checks both
/// kinds.
pub const MIC_DATAGRAM_DISCRIMINATOR: u8 = 0x4D;

/// Total mic datagram wire header size in bytes.
pub const MIC_WIRE_HEADER_SIZE: usize = 8;

/// 8-byte mic datagram header. Packed for zero-copy deserialization.
/// Layout (big-endian for multi-byte fields):
///   [0..2]  magic = [0x51, 0x42]
///   [2]     discriminator = `MIC_DATAGRAM_DISCRIMINATOR` (0x4D)
///   [3]     flags (bit 0 = last packet in burst, future bits reserved)
///   [4..6]  sequence (u16, per-stream, wraps)
///   [6..8]  reserved (zero in v1)
/// Followed by the Opus payload (typically 50-200 bytes; max ~400).
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireMicHeader {
    pub magic: [u8; 2],
    pub discriminator: u8,
    pub flags: u8,
    pub sequence: [u8; 2],
    pub _reserved: [u8; 2],
}

impl WireMicHeader {
    /// Total wire size on the wire.
    pub const SIZE: usize = MIC_WIRE_HEADER_SIZE;

    /// Write the 8-byte header into the start of `buf`. The caller is
    /// expected to have allocated at least `MIC_WIRE_HEADER_SIZE` bytes.
    pub fn write_into(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= MIC_WIRE_HEADER_SIZE);
        buf[0..2].copy_from_slice(&self.magic);
        buf[2] = self.discriminator;
        buf[3] = self.flags;
        buf[4..6].copy_from_slice(&self.sequence);
        buf[6..8].copy_from_slice(&self._reserved);
    }

    /// Parse an 8-byte header. Returns `Err` if the buffer is too short
    /// or the magic prefix doesn't match `MEDIA_DATAGRAM_MAGIC` (`[0x51, 0x42]`).
    pub fn from_bytes(buf: &[u8]) -> Result<Self, MicHeaderError> {
        if buf.len() < MIC_WIRE_HEADER_SIZE {
            return Err(MicHeaderError::Short);
        }
        if buf[0..2] != [0x51, 0x42] {
            return Err(MicHeaderError::BadMagic);
        }
        Ok(Self {
            magic: [buf[0], buf[1]],
            discriminator: buf[2],
            flags: buf[3],
            sequence: [buf[4], buf[5]],
            _reserved: [buf[6], buf[7]],
        })
    }

    /// Reconstruct the `u16` sequence counter from the big-endian pair.
    pub fn sequence_value(&self) -> u16 {
        u16::from_be_bytes(self.sequence)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicHeaderError {
    Short,
    BadMagic,
}

impl std::fmt::Display for MicHeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MicHeaderError::Short => write!(f, "mic datagram too short for header"),
            MicHeaderError::BadMagic => write!(f, "mic datagram magic prefix mismatch"),
        }
    }
}

impl std::error::Error for MicHeaderError {}

/// Convert a wire-format u8 back to a `DisplayState` value.
/// Maps 0→Active, 1→Privacy, 2→Blanked; clamps unknown values to Blanked.
#[inline]
pub fn u8_to_display_state(value: u8) -> u8 {
    value.min(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webrtc_ice_signal_round_trips_through_json() {
        let signal = ClientMessage::RelaySignal(RelaySignal {
            session_id: Uuid::new_v4(),
            from_peer_id: Uuid::new_v4(),
            to_peer_id: Uuid::new_v4(),
            signal: SessionSignal::IceCandidate {
                candidate: "candidate:foundation 1 udp 2122260223 192.0.2.1 54321 typ host"
                    .to_string(),
                sdp_mid: Some("0".to_string()),
                sdp_mline_index: Some(0),
            },
        });

        let encoded = serde_json::to_string(&signal).unwrap();
        let decoded: ClientMessage = serde_json::from_str(&encoded).unwrap();

        assert_eq!(signal, decoded);
    }

    #[test]
    fn remote_input_event_round_trips_through_json() {
        let event = RemoteInputEvent::MouseButton {
            button: InputMouseButton::Left,
            pressed: true,
        };

        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: RemoteInputEvent = serde_json::from_str(&encoded).unwrap();

        assert_eq!(event, decoded);
    }

    #[test]
    fn audio_stream_params_round_trip_through_json() {
        let params = AudioStreamParams {
            codec: AudioCodec::PcmF32,
            sample_rate: 48_000,
            channels: 2,
        };

        let encoded = serde_json::to_string(&params).unwrap();
        let decoded: AudioStreamParams = serde_json::from_str(&encoded).unwrap();

        assert_eq!(params, decoded);
    }

    #[test]
    fn control_msg_blank_overlay_round_trips_through_json() {
        let msg = ControlMsg::BlankOverlay {
            show: true,
            display_id: Some(0),
        };

        let encoded = serde_json::to_string(&msg).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&encoded).unwrap();

        assert_eq!(msg, decoded);
        // Verify tag-based serde format
        assert!(
            encoded.contains(r#""op":"blank_overlay""#),
            "encoded: {encoded}"
        );
        assert!(encoded.contains(r#""show":true"#), "encoded: {encoded}");
    }

    #[test]
    fn control_msg_display_state_changed_round_trips_through_json() {
        let msg = ControlMsg::DisplayStateChanged {
            display_id: 42,
            old_state: 0,
            new_state: 1,
        };

        let encoded = serde_json::to_string(&msg).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&encoded).unwrap();

        assert_eq!(msg, decoded);
        assert!(
            encoded.contains(r#""op":"display_state_changed""#),
            "encoded: {encoded}"
        );
    }

    #[test]
    fn ipc_event_display_state_changed_round_trips_through_json() {
        let event = IpcEvent::display_state_changed(1, 0, 1);

        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: IpcEvent = serde_json::from_str(&encoded).unwrap();

        assert_eq!(event, decoded);
        assert!(
            encoded.contains(r#""kind":"display_state_changed""#),
            "encoded: {encoded}"
        );
    }

    #[test]
    fn u8_display_state_conversion_works() {
        assert_eq!(u8_to_display_state(0), 0);
        assert_eq!(u8_to_display_state(1), 1);
        assert_eq!(u8_to_display_state(2), 2);
        assert_eq!(u8_to_display_state(3), 2);
        assert_eq!(u8_to_display_state(255), 2);
    }

    #[test]
    fn video_stream_preferences_round_trip_through_json() {
        let prefs = VideoStreamPreferences {
            codec: Some(VideoCodec::H265),
            width: Some(2560),
            height: Some(1440),
            framerate: Some(120),
            bitrate_kbps: Some(25_000),
            scale_mode: Some(ScaleMode::Fill),
            display_index: Some(1),
            capture_region: Some(CaptureRegion {
                x: 1920,
                y: 0,
                width: 2560,
                height: 1440,
            }),
            encoder: Some("hevc_nvenc".to_string()),
            color_space: Some(ColorSpace::Bt2100Pq),
            bit_depth: 10,
            max_framerate: Some(120),
            target_framerate: Some(120),
        };

        let encoded = serde_json::to_string(&prefs).unwrap();
        let decoded: VideoStreamPreferences = serde_json::from_str(&encoded).unwrap();
        assert_eq!(prefs, decoded);
    }

    #[test]
    fn start_session_with_video_preferences_round_trip() {
        let request = StartSessionRequest {
            session_id: Uuid::new_v4(),
            target_host_id: Uuid::new_v4(),
            requested_transport: Some(TransportKind::NativeQuic),
            preferred_codec: Some(VideoCodec::H264),
            video: Some(VideoStreamPreferences {
                codec: None,
                width: Some(1280),
                height: Some(720),
                framerate: Some(60),
                bitrate_kbps: Some(8_000),
                scale_mode: Some(ScaleMode::Crop),
                display_index: Some(0),
                capture_region: None,
                encoder: Some("h264_nvenc".to_string()),
                color_space: None,
                bit_depth: 8,
                max_framerate: None,
                target_framerate: None,
            }),
            permissions: SessionPermissions::default(),
            sync_only: false,
            consent_id: None,
        };

        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: StartSessionRequest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(request, decoded);
    }

    #[test]
    fn wire_gamepad_state_is_sixteen_bytes_and_round_trips() {
        assert_eq!(
            std::mem::size_of::<WireGamepadState>(),
            WireGamepadState::SIZE
        );
        let state = WireGamepadState {
            gamepad_id: 0,
            flags: WireGamepadState::FLAG_DPAD_UP | WireGamepadState::FLAG_CONNECTED,
            buttons_lo: WireGamepadState::BTN_A | WireGamepadState::BTN_START,
            buttons_hi: WireGamepadState::BTN_L3,
            lt: 128,
            rt: 64,
            lx: 12345,
            ly: -12000,
            rx: 0,
            ry: 0,
            _pad: [0, 0],
        };
        let json = serde_json::to_string(&state).unwrap();
        let decoded: WireGamepadState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, decoded);
    }

    #[test]
    fn remote_input_event_gamepad_variant_round_trips() {
        let event = RemoteInputEvent::Gamepad {
            state: WireGamepadState {
                gamepad_id: 2,
                flags: 0,
                buttons_lo: 0,
                buttons_hi: 0,
                lt: 255,
                rt: 0,
                lx: -32768,
                ly: 32767,
                rx: 0,
                ry: 0,
                _pad: [0, 0],
            },
        };
        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: RemoteInputEvent = serde_json::from_str(&encoded).unwrap();
        assert_eq!(event, decoded);
    }

    #[test]
    fn control_msg_rate_feedback_round_trips() {
        let msg = ControlMsg::RateFeedback(RateFeedback {
            rtt_ms: 18,
            loss_x1000: 7,
            jitter_ms: 4,
            one_way_delay_ms: 12.0,
            one_way_delay_min_ms: 7.5,
        });
        let encoded = serde_json::to_string(&msg).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&encoded).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn control_msg_nack_round_trips() {
        let msg = ControlMsg::Nack {
            stream_id: 1,
            frame_id: 42,
            missing_chunks: vec![2, 3],
        };
        let encoded = serde_json::to_string(&msg).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&encoded).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn control_msg_gamepad_lifecycle_round_trips() {
        let connect = ControlMsg::GamepadConnect {
            id: 0,
            name: "Xbox Wireless Controller".to_string(),
            kind: GamepadKind::Xbox,
        };
        let encoded = serde_json::to_string(&connect).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&encoded).unwrap();
        assert_eq!(connect, decoded);

        let rumble = ControlMsg::GamepadRumble {
            id: 0,
            low: 32768,
            high: 0,
        };
        let encoded = serde_json::to_string(&rumble).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&encoded).unwrap();
        assert_eq!(rumble, decoded);
    }

    #[test]
    fn control_msg_clipboard_text_round_trips_through_json() {
        let msg = ControlMsg::ClipboardChanged {
            seq: 7,
            payload: ClipboardPayload::Text {
                utf8: "hello, world".to_string(),
            },
        };
        let encoded = serde_json::to_string(&msg).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&encoded).unwrap();
        assert_eq!(msg, decoded);
        assert!(
            encoded.contains(r#""op":"clipboard_changed""#),
            "encoded: {encoded}"
        );
        assert!(encoded.contains(r#""kind":"text""#), "encoded: {encoded}");
    }

    #[test]
    fn control_msg_clipboard_image_round_trips_through_json() {
        let msg = ControlMsg::ClipboardChanged {
            seq: 42,
            payload: ClipboardPayload::ImagePng {
                width: 16,
                height: 8,
                png: vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
            },
        };
        let encoded = serde_json::to_string(&msg).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&encoded).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn control_msg_clipboard_clear_round_trips_through_json() {
        let msg = ControlMsg::ClipboardChanged {
            seq: 1,
            payload: ClipboardPayload::Clear,
        };
        let encoded = serde_json::to_string(&msg).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&encoded).unwrap();
        assert_eq!(msg, decoded);
        assert!(encoded.contains(r#""kind":"clear""#), "encoded: {encoded}");
    }

    #[test]
    fn control_msg_mic_start_round_trips_through_json() {
        let msg = ControlMsg::MicStart {
            config: MicStreamConfig::default(),
        };
        let encoded = serde_json::to_string(&msg).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&encoded).unwrap();
        assert_eq!(msg, decoded);
        assert!(
            encoded.contains(r#""op":"mic_start""#),
            "encoded: {encoded}"
        );
    }

    #[test]
    fn control_msg_mic_start_with_partial_config_is_backward_compatible() {
        let json = r#"{"op":"mic_start","config":{}}"#;
        let decoded: ControlMsg = serde_json::from_str(json).unwrap();
        match decoded {
            ControlMsg::MicStart { config } => {
                assert!(config.aec_enabled);
                assert!(config.ns_enabled);
                assert!(config.agc_enabled);
                assert_eq!(config.sample_rate_hz, 0);
            }
            other => panic!("expected MicStart, got {other:?}"),
        }
    }

    #[test]
    fn control_msg_mic_config_ack_round_trips_through_json() {
        let msg = ControlMsg::MicConfigAck {
            config: MicStreamConfig::default(),
            virtual_device_ok: false,
        };
        let encoded = serde_json::to_string(&msg).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&encoded).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn control_msg_mic_stop_round_trips_through_json() {
        let msg = ControlMsg::MicStop;
        let encoded = serde_json::to_string(&msg).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&encoded).unwrap();
        assert_eq!(msg, decoded);
        assert!(encoded.contains(r#""op":"mic_stop""#), "encoded: {encoded}");
    }

    #[test]
    fn wire_mic_header_is_eight_bytes_and_round_trips() {
        assert_eq!(std::mem::size_of::<WireMicHeader>(), WireMicHeader::SIZE);
        let h = WireMicHeader {
            magic: [0x51, 0x42],
            discriminator: MIC_DATAGRAM_DISCRIMINATOR,
            flags: 0x01,
            sequence: [0x00, 0x2A],
            _reserved: [0, 0],
        };
        let mut buf = [0_u8; 8];
        h.write_into(&mut buf);
        let decoded = WireMicHeader::from_bytes(&buf).unwrap();
        assert_eq!(decoded.magic, [0x51, 0x42]);
        assert_eq!(decoded.discriminator, MIC_DATAGRAM_DISCRIMINATOR);
        assert_eq!(decoded.flags, 0x01);
        assert_eq!(decoded.sequence_value(), 42);
    }

    #[test]
    fn wire_mic_header_rejects_bad_magic() {
        let mut buf = [0_u8; 8];
        buf[0] = 0xDE;
        buf[1] = 0xAD;
        let err = WireMicHeader::from_bytes(&buf).unwrap_err();
        assert_eq!(err, MicHeaderError::BadMagic);
    }

    #[test]
    fn wire_mic_header_rejects_short_buffer() {
        let buf = [0xB2_u8, 0x16, 0x4D];
        let err = WireMicHeader::from_bytes(&buf).unwrap_err();
        assert_eq!(err, MicHeaderError::Short);
    }

    #[test]
    fn ipc_event_session_state_changed_round_trips_through_json() {
        let event = IpcEvent::SessionStateChanged {
            active: true,
            session_id: Some(Uuid::new_v4()),
            reason: SessionStateReason::SessionEstablished,
        };
        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: IpcEvent = serde_json::from_str(&encoded).unwrap();
        assert_eq!(event, decoded);
        assert!(
            encoded.contains(r#""kind":"session_state_changed""#),
            "encoded: {encoded}"
        );
    }

    #[test]
    fn ipc_event_session_state_ended_serializes_with_snake_case_reason() {
        let event = IpcEvent::SessionStateChanged {
            active: false,
            session_id: None,
            reason: SessionStateReason::SessionEnded,
        };
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(
            encoded.contains(r#""reason":"session_ended""#),
            "encoded: {encoded}"
        );
    }

    #[test]
    fn mic_stream_config_default_is_full_processing() {
        let cfg = MicStreamConfig::default();
        assert_eq!(cfg.sample_rate_hz, 48_000);
        assert_eq!(cfg.channels, 1);
        assert_eq!(cfg.frame_ms, 20);
        assert_eq!(cfg.bitrate_bps, 64_000);
        assert!(cfg.aec_enabled);
        assert!(cfg.ns_enabled);
        assert!(cfg.agc_enabled);
    }

    #[test]
    fn default_true_returns_true() {
        assert!(default_true());
    }

    #[test]
    fn color_space_round_trips_through_json() {
        for space in [
            ColorSpace::Bt709,
            ColorSpace::Bt2020,
            ColorSpace::Bt2100Pq,
            ColorSpace::Bt2100Hlg,
            ColorSpace::ScRgb,
        ] {
            let json = serde_json::to_string(&space).unwrap();
            let decoded: ColorSpace = serde_json::from_str(&json).unwrap();
            assert_eq!(space, decoded);
        }
    }

    #[test]
    fn hdr_static_metadata_round_trips_through_json() {
        let meta = HdrStaticMetadata {
            primaries: 9,
            transfer: 16,
            matrix: 9,
            max_cll: 1000,
            max_fall: 400,
            mastering_display_metadata: vec![0xAA; 24],
        };
        let json = serde_json::to_string(&meta).unwrap();
        let decoded: HdrStaticMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, decoded);
    }

    #[test]
    fn hdr_static_metadata_backward_compat_with_omitted_mastering() {
        // A v1 client emits metadata without the mastering block.
        let json = r#"{"primaries":9,"transfer":16,"matrix":9,"max_cll":1000,"max_fall":400}"#;
        let decoded: HdrStaticMetadata = serde_json::from_str(json).unwrap();
        assert!(decoded.mastering_display_metadata.is_empty());
    }

    #[test]
    fn control_msg_display_capabilities_round_trips() {
        let msg = ControlMsg::DisplayCapabilities {
            hdr_static_metadata: Some(HdrStaticMetadata {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                max_cll: 1000,
                max_fall: 400,
                mastering_display_metadata: vec![0; 24],
            }),
            max_resolution: [3840, 2160],
            max_refresh_hz: 144,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, decoded);
        assert!(
            json.contains(r#""op":"display_capabilities""#),
            "encoded: {json}"
        );
    }

    #[test]
    fn control_msg_display_capabilities_view_exposes_payload() {
        let msg = ControlMsg::DisplayCapabilities {
            hdr_static_metadata: Some(HdrStaticMetadata::default()),
            max_resolution: [1920, 1080],
            max_refresh_hz: 60,
        };
        let view = msg
            .display_capabilities()
            .expect("expected DisplayCapabilities view");
        assert_eq!(view.max_resolution, [1920, 1080]);
        assert_eq!(view.max_refresh_hz, 60);
        assert!(view.hdr_static_metadata.is_some());
        assert!(ControlMsg::Nack {
            stream_id: 0,
            frame_id: 0,
            missing_chunks: Vec::new()
        }
        .display_capabilities()
        .is_none());
    }

    #[test]
    fn control_msg_pen_device_list_round_trips() {
        let msg = ControlMsg::PenDeviceList {
            devices: vec![PenDeviceDescriptor {
                device_id: 0,
                name: "Wacom Intuos".to_string(),
                tools: vec![PenTool::Pen, PenTool::Eraser],
                max_pressure: 8192,
                max_tilt_degrees: 60,
                rotation_supported: true,
            }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, decoded);
        assert!(
            json.contains(r#""op":"pen_device_list""#),
            "encoded: {json}"
        );
    }

    #[test]
    fn control_msg_pen_event_round_trips() {
        let msg = ControlMsg::PenEvent {
            device_id: 0,
            tool: PenTool::Pen,
            contact: true,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: ControlMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, decoded);
        assert!(json.contains(r#""op":"pen_event""#), "encoded: {json}");
    }

    #[test]
    fn remote_input_event_pen_variant_round_trips() {
        let event = RemoteInputEvent::Pen {
            tool: PenTool::Pen,
            pressure: 0.42,
            tilt_x: 15.0,
            tilt_y: -3.0,
            rotation: 90.0,
            button_state: 0b0010,
            x: 1920,
            y: 1080,
            hover_distance: 0,
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: RemoteInputEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, decoded);
    }

    #[test]
    fn video_stream_preferences_default_eight_is_eight() {
        assert_eq!(default_eight(), 8);
        let prefs = VideoStreamPreferences::default();
        assert_eq!(prefs.bit_depth, 8);
        assert!(prefs.color_space.is_none());
        assert!(prefs.max_framerate.is_none());
        assert!(prefs.target_framerate.is_none());
    }

    #[test]
    fn video_stream_preferences_new_fields_are_backward_compatible() {
        // A v1 client emits preferences without the new HDR / 10-bit
        // fields; the new server must still deserialize.
        let json = r#"{"codec":"h264","width":1280,"height":720,"framerate":60}"#;
        let decoded: VideoStreamPreferences = serde_json::from_str(json).unwrap();
        assert_eq!(decoded.bit_depth, 8);
        assert!(decoded.color_space.is_none());
    }

    // ── ADR-019 discriminator & constant tests ───────────────────

    #[test]
    fn mouse_motion_discriminator_is_0x4b() {
        assert_eq!(MOUSE_MOTION_DISCRIMINATOR, 0x4B);
        assert_ne!(MOUSE_MOTION_DISCRIMINATOR, PEN_DATAGRAM_DISCRIMINATOR);
        assert_ne!(MOUSE_MOTION_DISCRIMINATOR, GAMEPAD_DATAGRAM_DISCRIMINATOR);
        assert_ne!(MOUSE_MOTION_DISCRIMINATOR, MIC_DATAGRAM_DISCRIMINATOR);
    }

    #[test]
    fn immediate_ack_frame_type_is_0x1f() {
        assert_eq!(IMMEDIATE_ACK_FRAME_TYPE, 0x1f);
    }

    #[test]
    fn wire_input_magic_is_0x52_0x42() {
        assert_eq!(WIRE_INPUT_MAGIC, [0x52, 0x42]);
    }

    #[test]
    fn wire_mouse_motion_size_is_twelve() {
        assert_eq!(
            std::mem::size_of::<WireMouseMotion>(),
            WireMouseMotion::SIZE
        );
        assert_eq!(WireMouseMotion::SIZE, WIRE_MOUSE_MOTION_SIZE);
        assert_eq!(WIRE_MOUSE_MOTION_SIZE, 12);
    }

    #[test]
    fn wire_mouse_motion_round_trip_through_bytes() {
        let motion = WireMouseMotion {
            magic: [0x51, 0x42],
            discriminator: MOUSE_MOTION_DISCRIMINATOR,
            flags: 0,
            dx: -3,
            dy: 7,
            timestamp_us: 1_234_567,
        };
        let bytes = motion.to_bytes();
        assert_eq!(bytes.len(), WireMouseMotion::SIZE);
        let decoded = WireMouseMotion::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.magic, [0x51, 0x42]);
        assert_eq!(decoded.discriminator, MOUSE_MOTION_DISCRIMINATOR);
        assert_eq!(decoded.dx_value(), -3);
        assert_eq!(decoded.dy_value(), 7);
        assert_eq!(decoded.timestamp_us_value(), 1_234_567);
    }

    #[test]
    fn wire_mouse_motion_rejects_discriminator_mismatch() {
        let mut buf = [0u8; WireMouseMotion::SIZE];
        buf[0..2].copy_from_slice(&[0x51, 0x42]);
        buf[2] = 0x47; // gamepad discriminator, not 0x4B
        let err = WireMouseMotion::from_bytes(&buf).unwrap_err();
        assert_eq!(err, MouseMotionError::BadDiscriminator);
    }

    #[test]
    fn wire_mouse_motion_rejects_short_buffer() {
        let buf = [0x51, 0x42, 0x4B];
        let err = WireMouseMotion::from_bytes(&buf).unwrap_err();
        assert_eq!(err, MouseMotionError::Short);
    }

    #[test]
    fn wire_mouse_motion_rejects_bad_magic() {
        let mut buf = [0u8; WireMouseMotion::SIZE];
        buf[0] = 0xDE;
        buf[1] = 0xAD;
        buf[2] = MOUSE_MOTION_DISCRIMINATOR;
        let err = WireMouseMotion::from_bytes(&buf).unwrap_err();
        assert_eq!(err, MouseMotionError::BadMagic);
    }

    #[test]
    fn ack_policy_input_immediate_threshold_is_one() {
        assert_eq!(AckPolicy::InputImmediate.ack_eliciting_threshold(), 1);
        assert_eq!(AckPolicy::InputImmediate.max_ack_delay_us(), 1_000);
        assert_eq!(AckPolicy::Control.ack_eliciting_threshold(), 1);
        assert_eq!(AckPolicy::Media.ack_eliciting_threshold(), 10);
    }

    #[cfg(feature = "wire-rkyv-v2")]
    #[test]
    fn remote_critical_input_debug_and_clone_work() {
        let event = RemoteCriticalInput::MouseButton {
            button: InputMouseButton::Left,
            pressed: true,
        };
        let cloned = event.clone();
        assert_eq!(format!("{event:?}"), format!("{cloned:?}"));
    }

    // ── Session bundle (Phase 2) tests ─────────────────────────────

    fn sample_viewer_to_host() -> ViewerToHost {
        ViewerToHost {
            v: 1,
            jti: "jti-abc".into(),
            sid: "jti-abc".into(),
            sub: "account-123".into(),
            aud: "device-deadbeef".into(),
            iat: 1_700_000_000_000,
            exp: 1_700_000_600_000,
            caps: SessionCaps {
                input: true,
                clipboard: false,
                mic: false,
                files: false,
                audio: true,
            },
            viewer_dtls_fp: "AA:BB:CC:DD".into(),
        }
    }

    #[test]
    fn viewer_to_host_serializes_camel_case() {
        let v = sample_viewer_to_host();
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains(r#""viewerDtlsFp":"AA:BB:CC:DD""#), "got {json}");
        assert!(json.contains(r#""jti":"jti-abc""#), "got {json}");
        assert!(!json.contains("viewer_dtls_fp"), "got {json}");
    }

    #[test]
    fn canonical_json_sorts_keys_lexicographically() {
        let v = sample_viewer_to_host();
        let bytes = canonical_json_bytes(&v).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        // 'aud' < 'caps' < 'exp' < 'iat' < 'jti' < 'sub' < 'v' < 'viewerDtlsFp'
        let mut last = 0;
        for key in &["aud", "caps", "exp", "iat", "jti", "sub", "v", "viewerDtlsFp"] {
            let pos = s.find(&format!("\"{key}\"")).expect(key);
            assert!(pos > last, "key {key} not sorted (pos={pos}, last={last})");
            last = pos;
        }
    }

    #[test]
    fn signed_bundle_round_trips() {
        let key = generate_signing_key();
        let payload = sample_viewer_to_host();
        let env = SignedBundle::new(&payload, "kid-1", &key).unwrap();
        assert_eq!(env.kid, "kid-1");
        assert_eq!(env.v, 1);
        let decoded: ViewerToHost = env.decode(&key.verifying_key()).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn signed_bundle_rejects_tampered_signature() {
        let key = generate_signing_key();
        let other_key = generate_signing_key();
        let payload = sample_viewer_to_host();
        let mut env = SignedBundle::new(&payload, "kid-1", &key).unwrap();
        env.sig = "AAAA".to_string();
        assert!(matches!(
            env.decode::<ViewerToHost>(&other_key.verifying_key()),
            Err(SignedBundleError::BadBase64) | Err(SignedBundleError::BadSignatureLength)
        ));
    }

    #[test]
    fn signed_bundle_rejects_signature_mismatch() {
        let key = generate_signing_key();
        let other_key = generate_signing_key();
        let payload = sample_viewer_to_host();
        let env = SignedBundle::new(&payload, "kid-1", &key).unwrap();
        assert!(matches!(
            env.decode::<ViewerToHost>(&other_key.verifying_key()),
            Err(SignedBundleError::SignatureMismatch)
        ));
    }

    #[test]
    fn encode_signed_bundle_matches_decode() {
        let key = generate_signing_key();
        let payload = sample_viewer_to_host();
        let envelope = encode_signed_bundle(&payload, &key).unwrap();
        let decoded = decode_signed_bundle::<ViewerToHost>(&envelope, &key.verifying_key())
            .expect("decode");
        assert_eq!(decoded, payload);
    }

    #[test]
    fn signed_kill_round_trips() {
        let key = generate_signing_key();
        let payload = SignedKill {
            v: 1,
            jti: "kill-1".into(),
            sid: "session-xyz".into(),
            aud: "device-deadbeef".into(),
            sub: "admin-1".into(),
            iat: 1_700_000_000_000,
            exp: 1_700_000_900_000,
            reason: "fired_employee".into(),
        };
        let env = SignedBundle::new(&payload, "kid-1", &key).unwrap();
        let decoded: SignedKill = env.decode(&key.verifying_key()).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn ice_allowlist_validation() {
        assert!(ice_url_is_valid("stun:stun.l.google.com:19302"));
        assert!(ice_url_is_valid("turn:turn.example.com:3478?transport=udp"));
        assert!(ice_url_is_valid("turns:turn.example.com:5349"));
        assert!(!ice_url_is_valid("http://attacker.example/relay"));
        assert!(!ice_url_is_valid(""));
        assert!(!ice_url_is_valid("javascript:alert(1)"));
    }

    #[test]
    fn signed_kill_envelope_round_trip() {
        let key = generate_signing_key();
        let payload = SignedKill {
            v: 1,
            jti: "kill-1".into(),
            sid: "11111111-2222-3333-4444-555555555555".into(),
            aud: "66666666-7777-8888-9999-aaaaaaaaaaaa".into(),
            sub: "admin-1".into(),
            iat: 1_700_000_000_000,
            exp: 1_700_000_900_000,
            reason: "fired_employee".into(),
        };
        let envelope = SignedBundle::new(&payload, "kid-1", &key).unwrap();
        let wrapped = SignedKillEnvelope {
            payload: payload.clone(),
            envelope,
        };
        let encoded = serde_json::to_string(&wrapped).unwrap();
        let decoded: SignedKillEnvelope = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.payload, payload);
        assert_eq!(decoded.envelope.kid, "kid-1");
        let back: SignedKill = decoded.envelope.decode(&key.verifying_key()).unwrap();
        assert_eq!(back, payload);
    }
}
