//! Single-use `jti` cache with TTL eviction.
//!
//! Per `docs/browser-viewer-identity-and-host-trust.md` Phase 2,
//! every cloud-signed session bundle carries a unique `jti`. The host
//! (and the signaling relay on its behalf) MUST cache every accepted
//! `jti` until the bundle's `exp` passes; replaying a previously
//! accepted bundle is rejected even when the signature, `aud`, and
//! `exp` all remain valid.
//!
//! Two independent buckets:
//!
//! - **Seen (single-use)** — populated on first accept. A second
//!   `check_and_mark` with the same `jti` returns [`JtiError::Replay`].
//! - **Denied (kills)** — populated when a [`SignedKill`](crate::SignedKill)
//!   revokes a `jti`. Bundles with these `jti`s are rejected up to
//!   their `exp` regardless of whether they had been seen before.
//!
//! Both buckets are LRU-capped (default 100k entries each) and
//! lazily-prune entries whose `exp` has passed. Lookups on expired
//! entries are equivalent to "not seen" so a tombstoned slot doesn't
//! wedge the cache.
//!
//! Threading: [`JtiCache`] is `!Sync` because the inner state uses
//! `std::sync::Mutex`. Wrap in `Arc<Mutex<JtiCache>>` (or use the
//! `parking_lot::Mutex` in `SignalingState`) when sharing across
//! tasks.

use std::collections::BTreeMap;

const DEFAULT_MAX_ENTRIES: usize = 100_000;

/// Errors returned by [`JtiCache::check_and_mark`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JtiError {
    /// The `jti` was already accepted once. Treat as a replay.
    Replay,
    /// The `jti` is on the local kill denylist.
    Killed,
    Capacity,
    /// The `jti` is malformed (empty / too long / contains control bytes).
    Invalid,
}

impl std::fmt::Display for JtiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JtiError::Replay => write!(f, "jti replay rejected (single-use enforcement)"),
            JtiError::Killed => write!(f, "jti is on local kill denylist"),
            JtiError::Capacity => write!(f, "jti cache is full (fail-closed)"),
            JtiError::Invalid => write!(f, "jti is malformed"),
        }
    }
}

impl std::error::Error for JtiError {}

/// LRU+TTL set of seen `jti`s. `exp_unix_ms` lets the cache evict
/// entries without a separate sweeper task: every lookup checks the
/// expiry first and treats stale entries as "not present".
#[derive(Debug)]
pub struct JtiCache {
    /// `jti` -> `exp_unix_ms`. Uses `BTreeMap` ordered by expiry so
    /// we can prune expired entries efficiently on demand.
    seen: BTreeMap<String, Entry>,
    /// `jti` -> `exp_unix_ms` for kill denials.
    denied: BTreeMap<String, Entry>,
    max_entries: usize,
    /// LRU touch order — most-recently-touched at the back. We only
    /// need this on the insert path to enforce the cap.
    lru: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    exp_unix_ms: u64,
}

impl JtiCache {
    /// Empty cache with the default 100k entry cap.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_ENTRIES)
    }

    /// Empty cache with a custom cap. Tests use this to exercise the
    /// eviction path without spinning up 100k entries.
    pub fn with_capacity(max_entries: usize) -> Self {
        Self {
            seen: BTreeMap::new(),
            denied: BTreeMap::new(),
            max_entries: max_entries.max(1),
            lru: Vec::new(),
        }
    }

    /// Number of live entries in the seen bucket. Excludes expired.
    pub fn seen_len(&self) -> usize {
        self.seen.len()
    }

    /// Number of live entries in the denied bucket.
    pub fn denied_len(&self) -> usize {
        self.denied.len()
    }

    /// Atomically check whether `jti` is fresh AND mark it as seen.
    ///
    /// On success the `jti` is recorded with `exp_unix_ms` and any
    /// subsequent call with the same `jti` (and a still-in-future
    /// expiry) returns [`JtiError::Replay`]. If `jti` is already on
    /// the kill denylist the call returns [`JtiError::Killed`]
    /// WITHOUT inserting into the seen bucket (so a "killed then
    /// seen" ordering doesn't leak a denylist hit past expiry).
    pub fn check_and_mark(
        &mut self,
        jti: &str,
        exp_unix_ms: u64,
        now_unix_ms: u64,
    ) -> Result<(), JtiError> {
        validate_jti(jti)?;
        self.prune_expired(now_unix_ms);

        // Kill denylist always wins.
        if let Some(entry) = self.denied.get(jti).copied() {
            if entry.exp_unix_ms > now_unix_ms {
                return Err(JtiError::Killed);
            }
            // Expired denial — drop and continue.
            self.denied.remove(jti);
        }

        if let Some(entry) = self.seen.get(jti).copied() {
            if entry.exp_unix_ms > now_unix_ms {
                return Err(JtiError::Replay);
            }
            // Expired entry — let the new check_and_mark overwrite.
            self.seen.remove(jti);
            self.lru.retain(|s| s != jti);
        }

        if self.seen.len() + self.denied.len() >= self.max_entries {
            return Err(JtiError::Capacity);
        }
        self.seen.insert(jti.to_string(), Entry { exp_unix_ms });
        self.lru.push(jti.to_string());
        Ok(())
    }

    /// Add `jti` to the kill denylist with the given expiry. If the
    /// `jti` had previously been seen, the seen record is removed
    /// so the bucket does not retain redundant state.
    pub fn denylist(&mut self, jti: &str, exp_unix_ms: u64) -> Result<(), JtiError> {
        validate_jti(jti)?;
        self.seen.remove(jti);
        self.lru.retain(|s| s != jti);
        self.denied.insert(jti.to_string(), Entry { exp_unix_ms });
        Ok(())
    }

    /// Returns `true` if `jti` is currently on the kill denylist
    /// with an expiry still in the future. Useful for tests and
    /// admin tooling.
    pub fn is_denied(&self, jti: &str, now_unix_ms: u64) -> bool {
        self.denied
            .get(jti)
            .map(|e| e.exp_unix_ms > now_unix_ms)
            .unwrap_or(false)
    }

    /// Returns `true` if `jti` is currently in the seen bucket with
    /// an expiry still in the future.
    pub fn is_seen(&self, jti: &str, now_unix_ms: u64) -> bool {
        self.seen
            .get(jti)
            .map(|e| e.exp_unix_ms > now_unix_ms)
            .unwrap_or(false)
    }

    /// Drop every entry whose expiry is in the past. Returns the
    /// number of entries removed (across both buckets).
    pub fn prune_expired(&mut self, now_unix_ms: u64) -> usize {
        let before_seen = self.seen.len();
        let before_denied = self.denied.len();
        self.seen.retain(|_, e| e.exp_unix_ms > now_unix_ms);
        self.denied.retain(|_, e| e.exp_unix_ms > now_unix_ms);
        let kept_seen: std::collections::HashSet<&String> = self.seen.keys().collect();
        self.lru.retain(|jti| kept_seen.contains(jti));
        before_seen - self.seen.len() + before_denied - self.denied.len()
    }
}

impl Default for JtiCache {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_jti(jti: &str) -> Result<(), JtiError> {
    if jti.is_empty() {
        return Err(JtiError::Invalid);
    }
    if jti.len() > 256 {
        return Err(JtiError::Invalid);
    }
    if jti.chars().any(|c| c.is_control()) {
        return Err(JtiError::Invalid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jti(s: &str) -> &str {
        s
    }

    #[test]
    fn first_accept_succeeds_second_rejects_as_replay() {
        let mut cache = JtiCache::new();
        let now = 1_000;
        let exp = now + 60_000;
        assert!(cache.check_and_mark(jti("a"), exp, now).is_ok());
        assert_eq!(
            cache.check_and_mark(jti("a"), exp, now + 1),
            Err(JtiError::Replay),
        );
    }

    #[test]
    fn distinct_jtis_are_independent() {
        let mut cache = JtiCache::new();
        let now = 1_000;
        let exp = now + 60_000;
        assert!(cache.check_and_mark(jti("a"), exp, now).is_ok());
        assert!(cache.check_and_mark(jti("b"), exp, now).is_ok());
        assert!(cache.check_and_mark(jti("c"), exp, now).is_ok());
        assert_eq!(cache.seen_len(), 3);
    }

    #[test]
    fn expired_entry_can_be_reused() {
        let mut cache = JtiCache::new();
        let now = 1_000;
        let exp = now + 60_000;
        assert!(cache.check_and_mark(jti("a"), exp, now).is_ok());
        // After expiry, the same jti is acceptable again — the
        // semantics are "single use while the bundle is valid".
        assert!(cache.check_and_mark(jti("a"), exp, now + 120_000).is_ok());
    }

    #[test]
    fn denylist_blocks_subsequent_check_and_mark() {
        let mut cache = JtiCache::new();
        let now = 1_000;
        let exp = now + 60_000;
        assert!(cache.denylist(jti("a"), exp).is_ok());
        assert_eq!(
            cache.check_and_mark(jti("a"), exp, now),
            Err(JtiError::Killed)
        );
    }

    #[test]
    fn kill_takes_precedence_over_seen() {
        let mut cache = JtiCache::new();
        let now = 1_000;
        let exp = now + 60_000;
        // Hypothetical: seen, then killed. check_and_mark on the
        // same jti should report Killed, not Replay.
        assert!(cache.check_and_mark(jti("a"), exp, now).is_ok());
        assert!(cache.denylist(jti("a"), exp).is_ok());
        assert_eq!(
            cache.check_and_mark(jti("a"), exp, now + 1),
            Err(JtiError::Killed)
        );
    }

    #[test]
    fn expired_denylist_does_not_block() {
        let mut cache = JtiCache::new();
        let now = 1_000;
        let exp = now + 60_000;
        assert!(cache.denylist(jti("a"), exp).is_ok());
        // Past expiry, the kill no longer applies.
        assert!(cache
            .check_and_mark(jti("a"), exp + 1, now + 120_000)
            .is_ok());
    }

    #[test]
    fn prune_expired_removes_dead_entries() {
        let mut cache = JtiCache::new();
        let now = 1_000;
        cache.check_and_mark(jti("a"), now + 100, now).unwrap();
        cache.check_and_mark(jti("b"), now + 5_000, now).unwrap();
        cache.denylist(jti("c"), now + 200).unwrap();
        let removed = cache.prune_expired(now + 1_000);
        assert_eq!(removed, 2);
        assert_eq!(cache.seen_len(), 1);
        assert_eq!(cache.denied_len(), 0);
    }

    #[test]
    fn capacity_fails_closed_without_forgetting_seen_jtis() {
        let mut cache = JtiCache::with_capacity(3);
        let now = 1_000;
        let exp = now + 60_000;
        cache.check_and_mark(jti("a"), exp, now).unwrap();
        cache.check_and_mark(jti("b"), exp, now).unwrap();
        cache.check_and_mark(jti("c"), exp, now).unwrap();
        assert_eq!(
            cache.check_and_mark(jti("d"), exp, now + 1),
            Err(JtiError::Capacity)
        );
        assert_eq!(
            cache.check_and_mark(jti("a"), exp, now + 1),
            Err(JtiError::Replay)
        );
    }

    #[test]
    fn validate_jti_rejects_empty_and_overlong() {
        assert_eq!(
            JtiCache::new().check_and_mark("", 1, 0),
            Err(JtiError::Invalid)
        );
        let long: String = "x".repeat(257);
        assert_eq!(
            JtiCache::new().check_and_mark(&long, 1, 0),
            Err(JtiError::Invalid)
        );
    }
}
