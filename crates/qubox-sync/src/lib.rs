//! Context-aware file sync pure logic (ADR-022).
//!
//! Vector clocks, content hashes, ignore rules, atomic apply, transfer
//! planning, and shared domain types used by daemon redb + FileSync wire.

pub mod watcher;

pub use watcher::{action_to_sync_state, evaluate_watch_event, WatchAction};

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub type NodeId = String;
pub type FileId = String;
pub type PeerId = String;
pub type RuleId = String;
pub type JobId = String;
pub type ConflictId = String;

/// Simple vector clock: node → counter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct VectorClock(pub HashMap<NodeId, u64>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockRelation {
    Ahead,
    Behind,
    Equal,
    Conflict,
}

impl VectorClock {
    pub fn empty() -> Self {
        Self(HashMap::new())
    }

    pub fn compare(&self, other: &VectorClock) -> ClockRelation {
        let mut self_gt = false;
        let mut other_gt = false;
        let keys: HashSet<_> = self.0.keys().chain(other.0.keys()).cloned().collect();
        for k in keys {
            let a = *self.0.get(&k).unwrap_or(&0);
            let b = *other.0.get(&k).unwrap_or(&0);
            if a > b {
                self_gt = true;
            }
            if b > a {
                other_gt = true;
            }
        }
        match (self_gt, other_gt) {
            (true, false) => ClockRelation::Ahead,
            (false, true) => ClockRelation::Behind,
            (false, false) => ClockRelation::Equal,
            (true, true) => ClockRelation::Conflict,
        }
    }

    pub fn merge(&mut self, other: &VectorClock) {
        for (k, v) in &other.0 {
            let e = self.0.entry(k.clone()).or_insert(0);
            if *v > *e {
                *e = *v;
            }
        }
    }

    pub fn bump(&mut self, node: &str) {
        let e = self.0.entry(node.to_string()).or_insert(0);
        *e += 1;
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    Synced,
    LockedByProcess,
    Pending,
    Conflict,
    Disabled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutboxStatus {
    Queued,
    InFlight,
    Failed,
    Done,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictResolution {
    KeepLocal,
    KeepRemote,
    KeepBoth,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SyncRule {
    pub rule_id: RuleId,
    pub paths: Vec<String>,
    pub process_names: Vec<String>,
    pub peer_ids: Vec<PeerId>,
    pub enabled: bool,
    pub max_file_bytes: u64,
    pub ignore_globs: Vec<String>,
}

impl Default for SyncRule {
    fn default() -> Self {
        Self {
            rule_id: String::new(),
            paths: Vec::new(),
            process_names: Vec::new(),
            peer_ids: Vec::new(),
            enabled: true,
            max_file_bytes: 256 * 1024 * 1024,
            ignore_globs: default_ignore_globs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackedFile {
    pub file_id: FileId,
    pub local_path: String,
    pub vector_clock: VectorClock,
    pub content_hash: String,
    pub size_bytes: u64,
    pub sync_state: SyncState,
    pub rule_id: Option<RuleId>,
    pub updated_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutboxJob {
    pub job_id: JobId,
    pub file_id: FileId,
    pub target_peer: PeerId,
    pub status: OutboxStatus,
    pub retry_count: u32,
    pub queued_at_unix: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SyncConflict {
    pub conflict_id: ConflictId,
    pub file_id: FileId,
    pub local_path: String,
    pub remote_path: String,
    pub peer_id: PeerId,
    pub local_clock: VectorClock,
    pub remote_clock: VectorClock,
    pub created_at_unix: u64,
}

/// Wire handshake messages on FileSync bi (JSON length-prefixed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileSyncMessage {
    ManifestExchange {
        peer_id: PeerId,
        files: Vec<ManifestEntry>,
    },
    PullRequest {
        file_ids: Vec<FileId>,
    },
    PushOffer {
        file_ids: Vec<FileId>,
    },
    TransferComplete {
        file_id: FileId,
        hash: String,
    },
    ConflictNotice {
        file_id: FileId,
        clock_a: VectorClock,
        clock_b: VectorClock,
    },
    LockLease {
        file_id: FileId,
        holder_peer: PeerId,
        expires_unix: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestEntry {
    pub file_id: FileId,
    pub clock: VectorClock,
    pub hash: String,
    pub size: u64,
}

/// Binary bulk header after stream envelope:
/// `[u16 path_len][path utf8][u64 size][32 blake3][u16 file_id_len][file_id utf8]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTransferHeader {
    pub file_id: FileId,
    pub relative_path: String,
    pub size: u64,
    pub blake3: [u8; 32],
}

impl FileTransferHeader {
    pub fn encode(&self) -> Vec<u8> {
        let path_b = self.relative_path.as_bytes();
        let id_b = self.file_id.as_bytes();
        let mut out = Vec::with_capacity(2 + path_b.len() + 8 + 32 + 2 + id_b.len());
        out.extend_from_slice(&(path_b.len() as u16).to_be_bytes());
        out.extend_from_slice(path_b);
        out.extend_from_slice(&self.size.to_be_bytes());
        out.extend_from_slice(&self.blake3);
        out.extend_from_slice(&(id_b.len() as u16).to_be_bytes());
        out.extend_from_slice(id_b);
        out
    }

    pub fn decode(buf: &[u8]) -> anyhow::Result<(Self, usize)> {
        if buf.len() < 2 {
            anyhow::bail!("FileTransferHeader short (path len)");
        }
        let path_len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
        let mut off = 2;
        if buf.len() < off + path_len + 8 + 32 + 2 {
            anyhow::bail!("FileTransferHeader short (path/body)");
        }
        let relative_path = String::from_utf8(buf[off..off + path_len].to_vec())?;
        off += path_len;
        let size = u64::from_be_bytes(buf[off..off + 8].try_into()?);
        off += 8;
        let mut blake3 = [0u8; 32];
        blake3.copy_from_slice(&buf[off..off + 32]);
        off += 32;
        let id_len = u16::from_be_bytes([buf[off], buf[off + 1]]) as usize;
        off += 2;
        if buf.len() < off + id_len {
            anyhow::bail!("FileTransferHeader short (file_id)");
        }
        let file_id = String::from_utf8(buf[off..off + id_len].to_vec())?;
        off += id_len;
        Ok((
            Self {
                file_id,
                relative_path,
                size,
                blake3,
            },
            off,
        ))
    }
}

pub fn default_ignore_globs() -> Vec<String> {
    vec![
        // Always exclude VCS / temp by default (user can still add more).
        ".git".into(),
        ".git/**".into(),
        ".svn".into(),
        ".hg".into(),
        "node_modules".into(),
        "node_modules/**".into(),
        "target".into(),
        "target/**".into(),
        "*.tmp".into(),
        "*.part".into(),
        "*~".into(),
        "*.sa~".into(),
        "*.qubox-partial".into(),
        ".DS_Store".into(),
        "Thumbs.db".into(),
    ]
}

/// Named presets users can enable from CLI/GUI.
pub fn ignore_preset(name: &str) -> Option<Vec<String>> {
    match name.to_ascii_lowercase().as_str() {
        "default" | "safe" => Some(default_ignore_globs()),
        "git" => Some(vec![".git".into(), ".git/**".into()]),
        "emulator-saves" => Some(vec![
            ".git".into(),
            ".git/**".into(),
            "*.gba".into(),
            "*.nes".into(),
            "*.sfc".into(),
            "*.smc".into(),
            "*.iso".into(),
            "*.rom".into(),
            "*.tmp".into(),
            "*.part".into(),
        ]),
        "dev" => Some(vec![
            ".git".into(),
            ".git/**".into(),
            "node_modules".into(),
            "node_modules/**".into(),
            "target".into(),
            "target/**".into(),
            ".venv".into(),
            "__pycache__".into(),
            "*.o".into(),
            "*.pyc".into(),
        ]),
        _ => None,
    }
}

/// Merge global + rule-level ignore globs (dedup, preserve order).
pub fn merge_ignore_globs(global: &[String], rule: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for g in global.iter().chain(rule.iter()) {
        if seen.insert(g.clone()) {
            out.push(g.clone());
        }
    }
    out
}

/// Heuristic ignore: suffix/name match for common temp patterns + `.git`.
pub fn should_ignore_path(path: &Path, extra_globs: &[String]) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.ends_with(".tmp")
        || name.ends_with(".part")
        || name.ends_with('~')
        || name.ends_with(".sa~")
        || name.ends_with(".qubox-partial")
        || name == ".git"
    {
        return true;
    }
    for comp in path.components() {
        if comp.as_os_str() == ".git" {
            return true;
        }
    }
    for g in extra_globs {
        let g = g.to_ascii_lowercase();
        if let Some(suf) = g.strip_prefix('*') {
            if name.ends_with(suf) {
                return true;
            }
        } else if name == g || path.to_string_lossy().contains(&g) {
            return true;
        }
    }
    false
}

pub fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub fn content_hash_file(path: &Path) -> anyhow::Result<(String, [u8; 32], u64)> {
    let mut f = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    let hash = hasher.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(hash.as_bytes());
    Ok((hash.to_hex().to_string(), arr, size))
}

/// Write to `target.qubox-partial`, fsync, verify blake3, atomic rename.
pub fn atomic_apply_file(
    target: &Path,
    data: &[u8],
    expected_blake3: &[u8; 32],
) -> anyhow::Result<()> {
    let actual = blake3::hash(data);
    if actual.as_bytes() != expected_blake3 {
        anyhow::bail!("blake3 mismatch on atomic apply for {}", target.display());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let partial = partial_path(target);
    {
        let mut f = File::create(&partial)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&partial, target)?;
    if let Some(parent) = target.parent() {
        let _ = File::open(parent).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// Stream-oriented apply: partial already written; verify hash of partial then rename.
pub fn atomic_finalize_partial(target: &Path, expected_blake3: &[u8; 32]) -> anyhow::Result<()> {
    let partial = partial_path(target);
    let (_hex, arr, _size) = content_hash_file(&partial)?;
    if &arr != expected_blake3 {
        let _ = fs::remove_file(&partial);
        anyhow::bail!("blake3 mismatch on finalize for {}", target.display());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&partial, target)?;
    Ok(())
}

pub fn partial_path(target: &Path) -> PathBuf {
    let mut p = target.as_os_str().to_owned();
    p.push(".qubox-partial");
    PathBuf::from(p)
}

/// Conflict quarantine path: `{stem}.conflict.{peer}.{utc}.{ext}`
pub fn conflict_path(local: &Path, peer_id: &str, utc_unix: u64) -> PathBuf {
    let stem = local.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = local
        .extension()
        .and_then(|s| s.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let safe_peer: String = peer_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let name = format!("{stem}.conflict.{safe_peer}.{utc_unix}{ext}");
    local.with_file_name(name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferAction {
    Pull(FileId),
    Push(FileId),
    Conflict(FileId),
    Skip(FileId),
}

/// Compare local vs remote manifest entries → pull/push/conflict plan.
pub fn plan_transfers(
    local: &HashMap<FileId, ManifestEntry>,
    remote: &HashMap<FileId, ManifestEntry>,
) -> Vec<TransferAction> {
    let mut actions = Vec::new();
    let mut seen = HashSet::new();
    for (id, rem) in remote {
        seen.insert(id.clone());
        match local.get(id) {
            None => actions.push(TransferAction::Pull(id.clone())),
            Some(loc) => match loc.clock.compare(&rem.clock) {
                ClockRelation::Equal => {
                    if loc.hash != rem.hash {
                        actions.push(TransferAction::Conflict(id.clone()));
                    } else {
                        actions.push(TransferAction::Skip(id.clone()));
                    }
                }
                ClockRelation::Behind => actions.push(TransferAction::Pull(id.clone())),
                ClockRelation::Ahead => actions.push(TransferAction::Push(id.clone())),
                ClockRelation::Conflict => actions.push(TransferAction::Conflict(id.clone())),
            },
        }
    }
    for (id, _) in local {
        if !seen.contains(id) {
            actions.push(TransferAction::Push(id.clone()));
        }
    }
    actions
}

/// Case-insensitive process name match (e.g. `mgba` matches `mgba.exe`).
pub fn process_matches(running_names: &[String], matchers: &[String]) -> bool {
    let running: Vec<String> = running_names
        .iter()
        .map(|n| n.to_ascii_lowercase())
        .collect();
    for m in matchers {
        let m = m.to_ascii_lowercase();
        let m_stem = m.trim_end_matches(".exe");
        for r in &running {
            let r_stem = r.trim_end_matches(".exe");
            if r == &m || r_stem == m_stem || r.contains(m_stem) {
                return true;
            }
        }
    }
    false
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether an outbox job should be considered for push toward `peer_hint`.
/// Empty `target_peer` or empty `peer_hint` means "any peer".
pub fn job_eligible_for_peer(job: &OutboxJob, peer_hint: &str) -> bool {
    if matches!(job.status, OutboxStatus::InFlight | OutboxStatus::Done) {
        return false;
    }
    if !peer_hint.is_empty() && !job.target_peer.is_empty() && job.target_peer != peer_hint {
        return false;
    }
    true
}

/// Skip process-locked or conflicted tracked files during drain.
pub fn tracked_file_pushable(tf: &TrackedFile) -> bool {
    !matches!(
        tf.sync_state,
        SyncState::LockedByProcess | SyncState::Conflict
    )
}

/// Refuse paths that escape allowlisted roots (symlink / `..` safe check).
pub fn path_within_roots(path: &Path, roots: &[PathBuf]) -> bool {
    let Ok(canon) = path.canonicalize() else {
        // Not yet existing: check parent chain against roots via normalize.
        let mut cur = path.to_path_buf();
        if !cur.is_absolute() {
            return false;
        }
        while let Some(parent) = cur.parent() {
            if parent.as_os_str().is_empty() {
                break;
            }
            if let Ok(c) = parent.canonicalize() {
                return roots.iter().any(|r| {
                    r.canonicalize()
                        .map(|rc| c.starts_with(&rc) || c == rc)
                        .unwrap_or(false)
                });
            }
            cur = parent.to_path_buf();
        }
        return false;
    };
    roots.iter().any(|r| {
        r.canonicalize()
            .map(|rc| canon.starts_with(&rc))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_compare_and_merge() {
        let mut a = VectorClock::empty();
        a.bump("n1");
        let mut b = VectorClock::empty();
        b.bump("n2");
        assert_eq!(a.compare(&b), ClockRelation::Conflict);
        a.merge(&b);
        assert_eq!(a.0.get("n1"), Some(&1));
        assert_eq!(a.0.get("n2"), Some(&1));
        assert_eq!(a.compare(&b), ClockRelation::Ahead);
    }

    #[test]
    fn ignore_git_and_tmp() {
        assert!(should_ignore_path(Path::new("/x/.git/config"), &[]));
        assert!(should_ignore_path(Path::new("/x/foo.tmp"), &[]));
        assert!(!should_ignore_path(Path::new("/x/game.sav"), &[]));
    }

    #[test]
    fn transfer_header_roundtrip() {
        let h = FileTransferHeader {
            file_id: "f1".into(),
            relative_path: "saves/game.sav".into(),
            size: 128,
            blake3: [7u8; 32],
        };
        let enc = h.encode();
        let (dec, n) = FileTransferHeader::decode(&enc).unwrap();
        assert_eq!(n, enc.len());
        assert_eq!(dec, h);
    }

    #[test]
    fn atomic_apply_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("game.sav");
        let data = b"save-data-bytes";
        let hash = blake3::hash(data);
        let mut arr = [0u8; 32];
        arr.copy_from_slice(hash.as_bytes());
        atomic_apply_file(&target, data, &arr).unwrap();
        assert_eq!(fs::read(&target).unwrap(), data);
        let (hex, _, size) = content_hash_file(&target).unwrap();
        assert_eq!(size, data.len() as u64);
        assert_eq!(hex, content_hash(data));
    }

    #[test]
    fn plan_pull_push_conflict() {
        let mut local = HashMap::new();
        let mut remote = HashMap::new();
        let mut c1 = VectorClock::empty();
        c1.bump("a");
        local.insert(
            "f1".into(),
            ManifestEntry {
                file_id: "f1".into(),
                clock: c1.clone(),
                hash: "h1".into(),
                size: 1,
            },
        );
        remote.insert(
            "f1".into(),
            ManifestEntry {
                file_id: "f1".into(),
                clock: c1,
                hash: "h1".into(),
                size: 1,
            },
        );
        let mut c2 = VectorClock::empty();
        c2.bump("b");
        remote.insert(
            "f2".into(),
            ManifestEntry {
                file_id: "f2".into(),
                clock: c2,
                hash: "h2".into(),
                size: 2,
            },
        );
        let mut ca = VectorClock::empty();
        ca.bump("a");
        let mut cb = VectorClock::empty();
        cb.bump("b");
        local.insert(
            "f3".into(),
            ManifestEntry {
                file_id: "f3".into(),
                clock: ca,
                hash: "ha".into(),
                size: 3,
            },
        );
        remote.insert(
            "f3".into(),
            ManifestEntry {
                file_id: "f3".into(),
                clock: cb,
                hash: "hb".into(),
                size: 3,
            },
        );
        let plan = plan_transfers(&local, &remote);
        assert!(plan.contains(&TransferAction::Skip("f1".into())));
        assert!(plan.contains(&TransferAction::Pull("f2".into())));
        assert!(plan.contains(&TransferAction::Conflict("f3".into())));
    }

    #[test]
    fn process_match_exe() {
        assert!(process_matches(
            &["mgba.exe".into(), "explorer.exe".into()],
            &["mgba".into()]
        ));
        assert!(!process_matches(&["chrome".into()], &["mgba".into()]));
    }

    #[test]
    fn job_eligible_for_peer_matrix() {
        let mut job = OutboxJob {
            job_id: "j".into(),
            file_id: "f".into(),
            target_peer: "p1".into(),
            status: OutboxStatus::Queued,
            retry_count: 0,
            queued_at_unix: 0,
            last_error: None,
        };
        assert!(job_eligible_for_peer(&job, "p1"));
        assert!(!job_eligible_for_peer(&job, "p2"));
        job.status = OutboxStatus::InFlight;
        assert!(!job_eligible_for_peer(&job, "p1"));
        job.status = OutboxStatus::Failed;
        job.target_peer.clear();
        assert!(job_eligible_for_peer(&job, "anyone"));
    }

    #[test]
    fn tracked_file_pushable_matrix() {
        let mut tf = TrackedFile {
            file_id: "f".into(),
            local_path: "/t".into(),
            vector_clock: VectorClock::empty(),
            content_hash: String::new(),
            size_bytes: 0,
            sync_state: SyncState::Pending,
            rule_id: None,
            updated_at_unix: 0,
        };
        assert!(tracked_file_pushable(&tf));
        tf.sync_state = SyncState::LockedByProcess;
        assert!(!tracked_file_pushable(&tf));
        tf.sync_state = SyncState::Conflict;
        assert!(!tracked_file_pushable(&tf));
    }

    #[test]
    fn conflict_path_format() {
        let p = conflict_path(Path::new("/saves/game.sav"), "peer/1", 100);
        assert_eq!(
            p.file_name().unwrap().to_str().unwrap(),
            "game.conflict.peer_1.100.sav"
        );
    }
}
