//! Schedule algebra for ADR-013. No I/O, no time-of-day —
//! arithmetic on `target_interval_us` only.

use std::time::{SystemTime, UNIX_EPOCH};

use qubox_proto::FramePacingSchedule;

/// Monotonic microsecond timestamp. Uses `SystemTime` but the
/// caller must NOT rebase from `SystemTime` — see ADR-013 pitfalls §2.
pub fn unix_micros_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO)
        .as_micros()
        .min(u128::from(u64::MAX)) as u64
}

/// Sender-side monotonic send-base reference. Not serialised;
/// computed locally on both sides from `effective_frame_index`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendBase {
    pub frame_index: u32,
    pub instant_us: u64,
}

impl SendBase {
    pub fn from_anchor(anchor_frame: u32, anchor_instant_us: u64) -> Self {
        Self { frame_index: anchor_frame, instant_us: anchor_instant_us }
    }
}

/// Compute the deadline for frame N relative to a send-base.
pub fn deadline_for(n: u32, base: SendBase, s: &FramePacingSchedule) -> Option<u64> {
    let delta_frames = n.checked_sub(base.frame_index)? as u64;
    let offset_us = delta_frames.checked_mul(u64::from(s.target_interval_us))?;
    base.instant_us.checked_add(offset_us)
}

/// How many pacing slots does frame N consume given its byte budget
/// and the anti-burst guard? Returns 0 if `bytes_per_frame == 0`.
pub fn slots_per_frame(s: &FramePacingSchedule) -> u32 {
    if s.bytes_per_frame == 0 { return 0; }
    let bpf = u64::from(s.bytes_per_frame);
    let max = u64::from(s.max_burst_bytes.max(1));
    u32::try_from(bpf.div_ceil(max)).unwrap_or(u32::MAX)
}

/// Bytes that should leave the host in the *k-th* slot of frame N.
/// `bytes_in_slot(N, k) = min(max_burst_bytes, bytes_per_frame - k*max_burst_bytes)`
pub fn bytes_in_slot(s: &FramePacingSchedule, k: u32) -> u32 {
    let max = u64::from(s.max_burst_bytes);
    let sent_before = u64::from(k) * max;
    let bpf = u64::from(s.bytes_per_frame);
    if sent_before >= bpf { 0 } else { (bpf - sent_before).min(max) as u32 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qubox_proto::ResolutionTag;

    fn sched_4k144() -> FramePacingSchedule {
        FramePacingSchedule {
            version: 1, schedule_id: 1, effective_frame_index: 0,
            target_interval_us: 6_944,
            bytes_per_frame: 69_444,
            max_burst_bytes: 1_800,
            jitter_offset_us: 1_500,
            resolution: ResolutionTag::R2160P144,
        }
    }

    #[test]
    fn slots_for_4k144_is_39() {
        assert_eq!(slots_per_frame(&sched_4k144()), 39);
    }

    #[test]
    fn first_slot_is_full_mtu_burst() {
        assert_eq!(bytes_in_slot(&sched_4k144(), 0), 1_800);
    }

    #[test]
    fn last_slot_carries_the_tail() {
        assert_eq!(bytes_in_slot(&sched_4k144(), 38), 1_044);
    }

    #[test]
    fn slot_past_last_is_zero() {
        assert_eq!(bytes_in_slot(&sched_4k144(), 39), 0);
    }

    #[test]
    fn deadline_grows_linearly_with_frame_index() {
        let s = sched_4k144();
        let base = SendBase::from_anchor(0, 0);
        assert_eq!(deadline_for(0, base, &s), Some(0));
        assert_eq!(deadline_for(1, base, &s), Some(6_944));
        assert_eq!(deadline_for(144, base, &s), Some(144 * 6_944));
    }
}
