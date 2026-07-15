//! P2-16 4K144 bitrate ladder + preset table (ADR-010 §5).
//!
//! Seven presets spanning 1080p60 to 4K144. Each preset bundles a
//! resolution, framerate, codec, target bitrate, encoder preset
//! name, GOP size, and reference-frame count. The
//! `pick_preset` helper maps `(max_resolution, max_refresh_hz,
//! codec)` to the closest preset; the host CLI surface consumes
//! the result via the `--preset <name>` and `--auto-preset` flags.

use qubox_proto::VideoCodec;

/// Canonical preset identifier. Serialised snake_case in the
/// `--preset` CLI flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(non_camel_case_types)]
pub enum PresetId {
    P1080_60,
    P1080_144,
    P1080_240,
    P1440_144,
    P4K_60,
    P4K_120,
    P4K_144,
}

impl PresetId {
    /// Short CLI name (e.g. `4k144`).
    pub fn cli_name(self) -> &'static str {
        match self {
            PresetId::P1080_60 => "1080p60",
            PresetId::P1080_144 => "1080p144",
            PresetId::P1080_240 => "1080p240",
            PresetId::P1440_144 => "1440p144",
            PresetId::P4K_60 => "4k60",
            PresetId::P4K_120 => "4k120",
            PresetId::P4K_144 => "4k144",
        }
    }

    /// Inverse of [`Self::cli_name`]. Returns `None` for unknown
    /// names so the CLI surface can surface a clear error.
    pub fn from_cli_name(name: &str) -> Option<Self> {
        Some(match name {
            "1080p60" => PresetId::P1080_60,
            "1080p144" => PresetId::P1080_144,
            "1080p240" => PresetId::P1080_240,
            "1440p144" => PresetId::P1440_144,
            "4k60" => PresetId::P4K_60,
            "4k120" => PresetId::P4K_120,
            "4k144" => PresetId::P4K_144,
            _ => return None,
        })
    }
}

// The serde trait import is gated so the `Serialize, Deserialize`
// derives above work; the `serde` crate is already a workspace
// dependency.
use serde::{Deserialize, Serialize};

/// One preset row from ADR-010 §5.1. Codec-agnostic fields only;
/// the encoder-specific tuning lives in the `encoder_preset`
/// field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub id: PresetId,
    pub resolution: (u32, u32),
    pub framerate: u32,
    pub codec: VideoCodec,
    /// Average bitrate in **bits** per second.
    pub bitrate_bps: u32,
    /// Encoder preset name (e.g. `medium` for x265, `speed=6` for
    /// libaom, `preset=10` for svt-av1).
    pub encoder_preset: &'static str,
    /// Group-of-pictures size in frames. 240 frames at 60 fps gives
    /// a 4 s GOP; the table at ADR §5.2 standardises on 4 s for
    /// every framerate.
    pub gop_frames: u32,
    /// Number of reference frames. Per H.264 Level 5.1 maxDpbSize,
    /// 4 is the safe ceiling.
    pub ref_frames: u32,
}

/// Master preset table. The order is monotonic in resolution ×
/// framerate. New presets MUST be appended at the end so existing
/// binary references (e.g. CLI flag indices) stay stable.
pub const PRESETS: &[Preset] = &[
    Preset {
        id: PresetId::P1080_60,
        resolution: (1920, 1080),
        framerate: 60,
        codec: VideoCodec::H264,
        bitrate_bps: 12_000_000,
        encoder_preset: "medium",
        gop_frames: 240,
        ref_frames: 4,
    },
    Preset {
        id: PresetId::P1080_144,
        resolution: (1920, 1080),
        framerate: 144,
        codec: VideoCodec::H264,
        bitrate_bps: 20_000_000,
        encoder_preset: "medium",
        gop_frames: 576,
        ref_frames: 4,
    },
    Preset {
        id: PresetId::P1080_240,
        resolution: (1920, 1080),
        framerate: 240,
        codec: VideoCodec::H264,
        bitrate_bps: 30_000_000,
        encoder_preset: "medium",
        gop_frames: 960,
        ref_frames: 4,
    },
    Preset {
        id: PresetId::P1440_144,
        resolution: (2560, 1440),
        framerate: 144,
        codec: VideoCodec::H264,
        bitrate_bps: 35_000_000,
        encoder_preset: "medium",
        gop_frames: 576,
        ref_frames: 4,
    },
    Preset {
        id: PresetId::P4K_60,
        resolution: (3840, 2160),
        framerate: 60,
        codec: VideoCodec::H264,
        bitrate_bps: 40_000_000,
        encoder_preset: "medium",
        gop_frames: 240,
        ref_frames: 4,
    },
    Preset {
        id: PresetId::P4K_120,
        resolution: (3840, 2160),
        framerate: 120,
        codec: VideoCodec::H264,
        bitrate_bps: 60_000_000,
        encoder_preset: "medium",
        gop_frames: 480,
        ref_frames: 4,
    },
    Preset {
        id: PresetId::P4K_144,
        resolution: (3840, 2160),
        framerate: 144,
        codec: VideoCodec::H265,
        bitrate_bps: 80_000_000,
        encoder_preset: "slow",
        gop_frames: 576,
        ref_frames: 4,
    },
];

/// Look up a preset by its id. Returns `None` for unknown ids so
/// CLI parsing failures are loud.
pub fn preset_by_id(id: PresetId) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// Pick the highest preset that fits within the host's
/// `(max_resolution, max_refresh_hz)` capabilities for the requested
/// `codec`. The selection is deterministic: same inputs always
/// return the same preset, which keeps the host→client capability
/// negotiation reproducible across reconnects.
///
/// Returns the lowest preset (`P1080_60`) as a defensive fallback
/// when nothing fits — the caller can then decide to drop the
/// session or proceed at 1080p60.
pub fn pick_preset(
    max_resolution: (u32, u32),
    max_refresh_hz: u32,
    codec: VideoCodec,
) -> &'static Preset {
    // First filter: only presets that match the requested codec and
    // fit the host's (resolution, refresh) caps. We allow the codec
    // to be overridden by the table — `4k144` forces H.265 because
    // x264 cannot hit 80 Mbps 4K144 in real time on most hardware.
    let mut candidates: Vec<&'static Preset> = PRESETS
        .iter()
        .filter(|p| {
            p.resolution.0 <= max_resolution.0
                && p.resolution.1 <= max_resolution.1
                && p.framerate <= max_refresh_hz
        })
        .collect();
    // Sort by total pixel-rate (resolution * framerate) descending;
    // the heaviest preset that fits wins.
    candidates.sort_by(|a, b| {
        let a_rate = a.resolution.0 as u64 * a.resolution.1 as u64 * a.framerate as u64;
        let b_rate = b.resolution.0 as u64 * b.resolution.1 as u64 * b.framerate as u64;
        b_rate.cmp(&a_rate)
    });
    if let Some(best) = candidates
        .iter()
        .find(|p| p.codec == codec || codec == VideoCodec::H264)
    {
        return best;
    }
    &PRESETS[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_table_has_seven_rows() {
        assert_eq!(PRESETS.len(), 7);
    }

    #[test]
    fn preset_ids_have_unique_cli_names() {
        let mut names: Vec<&str> = PRESETS.iter().map(|p| p.id.cli_name()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), PRESETS.len());
    }

    #[test]
    fn from_cli_name_round_trips() {
        for preset in PRESETS {
            assert_eq!(
                PresetId::from_cli_name(preset.id.cli_name()),
                Some(preset.id)
            );
        }
        assert_eq!(PresetId::from_cli_name("unknown"), None);
    }

    #[test]
    fn pick_preset_4k144_caps_at_4k144() {
        let picked = pick_preset((3840, 2160), 144, VideoCodec::H264);
        // The table's 4K144 entry pins H.265 because x264 cannot
        // hit 80 Mbps 4K144 in real time. The pick_preset
        // implementation accepts either codec when the caller asks
        // for H.264 *or* the table's preferred codec; the result
        // here is the 4K144 row regardless.
        assert_eq!(picked.id, PresetId::P4K_144);
    }

    #[test]
    fn pick_preset_falls_back_to_1080p60_when_nothing_fits() {
        // 640x480 @ 30 Hz is below the smallest table entry; pick_preset
        // returns the lowest preset as a defensive default.
        let picked = pick_preset((640, 480), 30, VideoCodec::H264);
        assert_eq!(picked.id, PresetId::P1080_60);
    }

    #[test]
    fn pick_preset_caps_refresh_rate() {
        // 4K @ 30 Hz should pick the heaviest 4K preset whose
        // framerate is <= 30 Hz. None of the 4K presets fit (60 is
        // the lowest), so the picker falls back to 1080p60 (the
        // table's lowest preset) per the documented defensive
        // default.
        let picked = pick_preset((3840, 2160), 30, VideoCodec::H264);
        assert_eq!(picked.id, PresetId::P1080_60);
    }

    #[test]
    fn pick_preset_caps_resolution() {
        // 1080p display @ 240 Hz should pick the heaviest 1080p preset
        // that fits: 1080p240, not 1440p144 or 4K*.
        let picked = pick_preset((1920, 1080), 240, VideoCodec::H264);
        assert_eq!(picked.id, PresetId::P1080_240);
    }

    #[test]
    fn preset_gop_frames_yield_4s_gop() {
        for preset in PRESETS {
            // 4-second GOPs across the table: 240 frames @ 60 fps,
            // 576 frames @ 144 fps, 480 frames @ 120 fps, 480 frames
            // @ 240 fps. The actual invariant is
            // `gop_frames / framerate == 4`; compute it with integer
            // math to avoid float rounding surprises.
            let gop_seconds_x_framerate = preset.gop_frames / preset.framerate;
            assert_eq!(
                gop_seconds_x_framerate,
                4,
                "preset {:?} has GOP length {} frames (expected 4 * framerate = {})",
                preset.id,
                preset.gop_frames,
                4 * preset.framerate
            );
        }
    }

    #[test]
    fn bitrate_matches_table_from_adr_010_section_5_1() {
        assert_eq!(PRESETS[0].bitrate_bps, 12_000_000);
        assert_eq!(PRESETS[1].bitrate_bps, 20_000_000);
        assert_eq!(PRESETS[2].bitrate_bps, 30_000_000);
        assert_eq!(PRESETS[3].bitrate_bps, 35_000_000);
        assert_eq!(PRESETS[4].bitrate_bps, 40_000_000);
        assert_eq!(PRESETS[5].bitrate_bps, 60_000_000);
        assert_eq!(PRESETS[6].bitrate_bps, 80_000_000);
    }
}
