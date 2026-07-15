//! Filesystem watch → process-lock aware outbox enqueue helpers.
//!
//! Domain-level: given a path event + running process names + rule,
//! decide whether to enqueue. Full notify integration lives in the
//! daemon host sensors feature; this module is unit-tested pure logic.

use std::path::Path;

use crate::{process_matches, should_ignore_path, SyncRule, SyncState};

/// Outcome of evaluating a filesystem event against a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchAction {
    /// Enqueue push for this path.
    Enqueue,
    /// Skip: ignored by globs.
    Ignored,
    /// Skip: process lock holds the file.
    LockedByProcess,
    /// Skip: path outside rule roots.
    OutsideRule,
}

/// Evaluate whether a changed path should be enqueued for sync.
pub fn evaluate_watch_event(
    path: &Path,
    rule: &SyncRule,
    running_processes: &[String],
    global_ignores: &[String],
) -> WatchAction {
    if !rule.enabled {
        return WatchAction::OutsideRule;
    }
    let path_str = path.to_string_lossy();
    let under_rule = rule.paths.iter().any(|root| {
        path_str.starts_with(root.as_str()) || path.starts_with(Path::new(root))
    });
    if !under_rule {
        return WatchAction::OutsideRule;
    }
    let mut globs = global_ignores.to_vec();
    globs.extend(rule.ignore_globs.iter().cloned());
    if should_ignore_path(path, &globs) {
        return WatchAction::Ignored;
    }
    if process_matches(running_processes, &rule.process_names) {
        return WatchAction::LockedByProcess;
    }
    WatchAction::Enqueue
}

/// Map a watch action to a sync state for tracked-file bookkeeping.
pub fn action_to_sync_state(action: WatchAction) -> Option<SyncState> {
    match action {
        WatchAction::Enqueue => Some(SyncState::Pending),
        WatchAction::LockedByProcess => Some(SyncState::LockedByProcess),
        WatchAction::Ignored | WatchAction::OutsideRule => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyncRule;
    use std::path::PathBuf;

    #[test]
    fn enqueue_when_under_rule_and_unlocked() {
        let rule = SyncRule {
            rule_id: "r1".into(),
            paths: vec!["/tmp/saves".into()],
            process_names: vec!["mgba".into()],
            peer_ids: vec![],
            enabled: true,
            max_file_bytes: 1 << 20,
            ignore_globs: vec![],
        };
        let path = PathBuf::from("/tmp/saves/game.sav");
        assert_eq!(
            evaluate_watch_event(&path, &rule, &[], &[".git".into()]),
            WatchAction::Enqueue
        );
    }

    #[test]
    fn lock_when_process_running() {
        let rule = SyncRule {
            rule_id: "r1".into(),
            paths: vec!["/tmp/saves".into()],
            process_names: vec!["mgba".into()],
            peer_ids: vec![],
            enabled: true,
            max_file_bytes: 1 << 20,
            ignore_globs: vec![],
        };
        let path = PathBuf::from("/tmp/saves/game.sav");
        assert_eq!(
            evaluate_watch_event(&path, &rule, &["mgba".into()], &[]),
            WatchAction::LockedByProcess
        );
    }

    #[test]
    fn ignore_git_paths() {
        let rule = SyncRule {
            rule_id: "r1".into(),
            paths: vec!["/repo".into()],
            process_names: vec![],
            peer_ids: vec![],
            enabled: true,
            max_file_bytes: 1 << 20,
            ignore_globs: vec![],
        };
        let path = PathBuf::from("/repo/.git/config");
        assert_eq!(
            evaluate_watch_event(&path, &rule, &[], &[".git".into()]),
            WatchAction::Ignored
        );
    }
}
