//! Frame-aware sender pacing (ADR-013).

mod pacer;
mod schedule;

pub use pacer::{
    FrameAwarePacer, FrameAwarePacerConfig, FramePacerStats, PacingDecision,
};
pub use schedule::{bytes_in_slot, deadline_for, slots_per_frame, unix_micros_now, SendBase};

/// Re-export the shared proto type for ergonomic callers.
pub use qubox_proto::FramePacingSchedule;
