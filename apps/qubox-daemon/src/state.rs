//! # redb-backed persistent state store
//!
//! ## Tables (15)
//!
//! | Table | Key | Value | Description |
//! |-------|-----|-------|-------------|
//! | `meta` | `&str` | `&[u8]` | Schema version and other metadata |
//! | `pairings` | `&str` | `&[u8]` (bincode `Pairing`) | Paired peers |
//! | `host_state` | `&str` | `&[u8]` (bincode `HostState`) | Host state |
//! | `client_state` | `&str` | `&[u8]` (bincode `ClientState`) | Client state |
//! | `settings` | `&str` | `&str` | Generic key–value settings |
//! | `tuf_root` | `&str` | `&[u8]` | TUF root.json metadata bytes |
//! | `tuf_targets` | `&str` | `&[u8]` | TUF targets.json metadata bytes |
//! | `tuf_snapshot` | `&str` | `&[u8]` | TUF snapshot.json metadata bytes |
//! | `tuf_timestamp` | `&str` | `&[u8]` | TUF timestamp.json metadata bytes |
//! | `update_history` | `u64` | `&[u8]` (bincode `UpdateRecord`) | Capped at 100 entries |
//! | `session_history` | `u64` | `&[u8]` (bincode `SessionRecord`) | Capped at 1000 entries |
//! | `sync_rules` | `&str` | `&[u8]` (bincode `SyncRule`) | ADR-022 watch rules |
//! | `tracked_files` | `&str` | `&[u8]` (bincode `TrackedFile`) | ADR-022 tracked files |
//! | `sync_outbox` | `&str` | `&[u8]` (bincode `OutboxJob`) | ADR-022 outbox jobs |
//! | `sync_conflicts` | `&str` | `&[u8]` (bincode `SyncConflict`) | ADR-022 conflicts |
//!
//! ## Locking discipline
//!
//! redb uses MVCC with a single writer and multiple concurrent readers.
//! All writes are serialized through the daemon's single-threaded event loop.
//! Reads never block on concurrent writes (MVCC). Consistent multi-table reads
//! use the same ReadTransaction. Keep write transactions short (<10 ms).

use std::path::Path;

use anyhow::Result;
use qubox_sync::{
    ConflictResolution, OutboxJob, OutboxStatus, SyncConflict, SyncRule, SyncState, TrackedFile,
};
use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const PAIRINGS: TableDefinition<&str, &[u8]> = TableDefinition::new("pairings");
const HOST_STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("host_state");
const CLIENT_STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("client_state");
const SETTINGS: TableDefinition<&str, &str> = TableDefinition::new("settings");
const TUF_ROOT: TableDefinition<&str, &[u8]> = TableDefinition::new("tuf_root");
const TUF_TARGETS: TableDefinition<&str, &[u8]> = TableDefinition::new("tuf_targets");
const TUF_SNAPSHOT: TableDefinition<&str, &[u8]> = TableDefinition::new("tuf_snapshot");
const TUF_TIMESTAMP: TableDefinition<&str, &[u8]> = TableDefinition::new("tuf_timestamp");
const UPDATE_HISTORY: TableDefinition<u64, &[u8]> = TableDefinition::new("update_history");
const SESSION_HISTORY: TableDefinition<u64, &[u8]> = TableDefinition::new("session_history");
const SYNC_RULES: TableDefinition<&str, &[u8]> = TableDefinition::new("sync_rules");
const TRACKED_FILES: TableDefinition<&str, &[u8]> = TableDefinition::new("tracked_files");
const SYNC_OUTBOX: TableDefinition<&str, &[u8]> = TableDefinition::new("sync_outbox");
const SYNC_CONFLICTS: TableDefinition<&str, &[u8]> = TableDefinition::new("sync_conflicts");

const SCHEMA_VERSION_KEY: &str = "schema_version";
const LATEST_KEY: &str = "latest";
const SCHEMA_VERSION: u32 = 2;
const SYNC_GLOBAL_IGNORES_KEY: &str = "sync_global_ignores";

const MAX_SESSION_RECORDS: u64 = 1000;
const MAX_UPDATE_RECORDS: u64 = 100;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pairing {
    pub peer_id: String,
    pub public_key: Vec<u8>,
    pub paired_at: u64,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HostState {
    pub last_seen: u64,
    pub current_session_id: Option<String>,
    pub config_hash: String,
    pub last_child_pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientState {
    pub last_seen: u64,
    pub current_session_id: Option<String>,
    pub last_child_pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateRecord {
    pub applied_at: u64,
    pub binary_path: String,
    pub prev_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionRecord {
    pub started_at: u64,
    pub ended_at: u64,
    pub host_id: String,
    pub client_id: String,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

pub struct StateDb {
    db: redb::Database,
}

impl StateDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = redb::Database::create(path)?;
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(META)?;
            write_txn.open_table(PAIRINGS)?;
            write_txn.open_table(HOST_STATE)?;
            write_txn.open_table(CLIENT_STATE)?;
            write_txn.open_table(SETTINGS)?;
            write_txn.open_table(TUF_ROOT)?;
            write_txn.open_table(TUF_TARGETS)?;
            write_txn.open_table(TUF_SNAPSHOT)?;
            write_txn.open_table(TUF_TIMESTAMP)?;
            write_txn.open_table(UPDATE_HISTORY)?;
            write_txn.open_table(SESSION_HISTORY)?;
            write_txn.open_table(SYNC_RULES)?;
            write_txn.open_table(TRACKED_FILES)?;
            write_txn.open_table(SYNC_OUTBOX)?;
            write_txn.open_table(SYNC_CONFLICTS)?;
        }
        write_txn.commit()?;

        let store = Self { db };
        let current: u32 = store
            .get_meta(SCHEMA_VERSION_KEY)?
            .and_then(|v| {
                if v.len() == 4 {
                    Some(u32::from_le_bytes([v[0], v[1], v[2], v[3]]))
                } else {
                    None
                }
            })
            .unwrap_or(0);
        if current < SCHEMA_VERSION {
            let version_bytes: [u8; 4] = SCHEMA_VERSION.to_le_bytes();
            store.set_meta(SCHEMA_VERSION_KEY, &version_bytes)?;
        }
        Ok(store)
    }

    fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(META)?;
        Ok(table.get(key)?.map(|g| g.value().to_vec()))
    }

    fn set_meta(&self, key: &str, value: &[u8]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(META)?;
            table.insert(key, value)?;
        }
        txn.commit()?;
        Ok(())
    }

    // ── Pairings ─────────────────────────────────────────────────────

    pub fn put_pairing(&self, p: &Pairing) -> Result<()> {
        let bytes = bincode::serialize(p)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(PAIRINGS)?;
            table.insert(p.peer_id.as_str(), bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_pairing(&self, peer_id: &str) -> Result<Option<Pairing>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(PAIRINGS)?;
        match table.get(peer_id)? {
            Some(g) => Ok(Some(bincode::deserialize(g.value())?)),
            None => Ok(None),
        }
    }

    pub fn list_pairings(&self) -> Result<Vec<Pairing>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(PAIRINGS)?;
        let mut out = Vec::new();
        let iter = table.iter()?;
        for result in iter {
            let (_key, value) = result?;
            out.push(bincode::deserialize(value.value())?);
        }
        Ok(out)
    }

    pub fn delete_pairing(&self, peer_id: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(PAIRINGS)?;
            table.remove(peer_id)?;
        }
        txn.commit()?;
        Ok(())
    }

    // ── Settings ─────────────────────────────────────────────────────

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SETTINGS)?;
        Ok(table.get(key)?.map(|g| g.value().to_string()))
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(SETTINGS)?;
            table.insert(key, value)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn complete_onboarding(
        &self,
        device_name: &str,
        signaling_server: &str,
        cloud_mode: bool,
        accounts_url: Option<&str>,
    ) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(SETTINGS)?;
            table.insert("device_name", device_name)?;
            table.insert("signaling_server", signaling_server)?;
            table.insert("cloud_mode", if cloud_mode { "1" } else { "0" })?;
            if let Some(url) = accounts_url.filter(|value| !value.is_empty()) {
                table.insert("accounts_url", url)?;
            } else {
                table.remove("accounts_url")?;
            }
            table.insert("onboarding_complete", "1")?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn list_settings(&self) -> Result<Vec<(String, String)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SETTINGS)?;
        let mut out = Vec::new();
        let iter = table.iter()?;
        for result in iter {
            let (k, v) = result?;
            out.push((k.value().to_string(), v.value().to_string()));
        }
        Ok(out)
    }

    // ── Host state ───────────────────────────────────────────────────

    pub fn get_host_state(&self, host_id: &str) -> Result<Option<HostState>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(HOST_STATE)?;
        match table.get(host_id)? {
            Some(g) => Ok(Some(bincode::deserialize(g.value())?)),
            None => Ok(None),
        }
    }

    pub fn put_host_state(&self, host_id: &str, st: &HostState) -> Result<()> {
        let bytes = bincode::serialize(st)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(HOST_STATE)?;
            table.insert(host_id, bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    // ── Client state ─────────────────────────────────────────────────

    pub fn get_client_state(&self, client_id: &str) -> Result<Option<ClientState>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CLIENT_STATE)?;
        match table.get(client_id)? {
            Some(g) => Ok(Some(bincode::deserialize(g.value())?)),
            None => Ok(None),
        }
    }

    pub fn put_client_state(&self, client_id: &str, st: &ClientState) -> Result<()> {
        let bytes = bincode::serialize(st)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(CLIENT_STATE)?;
            table.insert(client_id, bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    // ── TUF metadata ─────────────────────────────────────────────────

    pub fn put_tuf_root(&self, bytes: &[u8]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(TUF_ROOT)?;
            table.insert(LATEST_KEY, bytes)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_tuf_root(&self) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(TUF_ROOT)?;
        Ok(table.get(LATEST_KEY)?.map(|g| g.value().to_vec()))
    }

    pub fn put_tuf_targets(&self, bytes: &[u8]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(TUF_TARGETS)?;
            table.insert(LATEST_KEY, bytes)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_tuf_targets(&self) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(TUF_TARGETS)?;
        Ok(table.get(LATEST_KEY)?.map(|g| g.value().to_vec()))
    }

    pub fn put_tuf_snapshot(&self, bytes: &[u8]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(TUF_SNAPSHOT)?;
            table.insert(LATEST_KEY, bytes)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_tuf_snapshot(&self) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(TUF_SNAPSHOT)?;
        Ok(table.get(LATEST_KEY)?.map(|g| g.value().to_vec()))
    }

    pub fn put_tuf_timestamp(&self, bytes: &[u8]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(TUF_TIMESTAMP)?;
            table.insert(LATEST_KEY, bytes)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_tuf_timestamp(&self) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(TUF_TIMESTAMP)?;
        Ok(table.get(LATEST_KEY)?.map(|g| g.value().to_vec()))
    }

    // ── Update history ───────────────────────────────────────────────

    pub fn record_update(&self, r: &UpdateRecord) -> Result<()> {
        let bytes = bincode::serialize(r)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(UPDATE_HISTORY)?;
            table.insert(r.applied_at, bytes.as_slice())?;
            enforce_cap::<{ MAX_UPDATE_RECORDS }>(&mut table)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn list_updates(&self) -> Result<Vec<UpdateRecord>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(UPDATE_HISTORY)?;
        let mut out = Vec::new();
        let iter = table.iter()?;
        for result in iter {
            let (_key, value) = result?;
            out.push(bincode::deserialize(value.value())?);
        }
        Ok(out)
    }

    // ── Session history ──────────────────────────────────────────────

    pub fn record_session(&self, r: &SessionRecord) -> Result<()> {
        let bytes = bincode::serialize(r)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(SESSION_HISTORY)?;
            table.insert(r.ended_at, bytes.as_slice())?;
            enforce_cap::<{ MAX_SESSION_RECORDS }>(&mut table)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SESSION_HISTORY)?;
        let mut out = Vec::new();
        let iter = table.iter()?;
        for result in iter {
            let (_key, value) = result?;
            out.push(bincode::deserialize(value.value())?);
        }
        Ok(out)
    }

    // ── ADR-022 FileSync ─────────────────────────────────────────────

    pub fn put_sync_rule(&self, rule: &SyncRule) -> Result<()> {
        let bytes = bincode::serialize(rule)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(SYNC_RULES)?;
            table.insert(rule.rule_id.as_str(), bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_sync_rule(&self, rule_id: &str) -> Result<Option<SyncRule>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SYNC_RULES)?;
        match table.get(rule_id)? {
            Some(g) => Ok(Some(bincode::deserialize(g.value())?)),
            None => Ok(None),
        }
    }

    pub fn list_sync_rules(&self) -> Result<Vec<SyncRule>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SYNC_RULES)?;
        let mut out = Vec::new();
        let iter = table.iter()?;
        for result in iter {
            let (_k, v) = result?;
            out.push(bincode::deserialize(v.value())?);
        }
        Ok(out)
    }

    pub fn delete_sync_rule(&self, rule_id: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(SYNC_RULES)?;
            table.remove(rule_id)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn set_sync_rule_enabled(&self, rule_id: &str, enabled: bool) -> Result<bool> {
        let Some(mut rule) = self.get_sync_rule(rule_id)? else {
            return Ok(false);
        };
        rule.enabled = enabled;
        self.put_sync_rule(&rule)?;
        Ok(true)
    }

    pub fn put_tracked_file(&self, f: &TrackedFile) -> Result<()> {
        let bytes = bincode::serialize(f)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(TRACKED_FILES)?;
            table.insert(f.file_id.as_str(), bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_tracked_file(&self, file_id: &str) -> Result<Option<TrackedFile>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(TRACKED_FILES)?;
        match table.get(file_id)? {
            Some(g) => Ok(Some(bincode::deserialize(g.value())?)),
            None => Ok(None),
        }
    }

    pub fn list_tracked_files(&self) -> Result<Vec<TrackedFile>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(TRACKED_FILES)?;
        let mut out = Vec::new();
        let iter = table.iter()?;
        for result in iter {
            let (_k, v) = result?;
            out.push(bincode::deserialize(v.value())?);
        }
        Ok(out)
    }

    pub fn put_outbox_job(&self, job: &OutboxJob) -> Result<()> {
        let bytes = bincode::serialize(job)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(SYNC_OUTBOX)?;
            table.insert(job.job_id.as_str(), bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_outbox_job(&self, job_id: &str) -> Result<Option<OutboxJob>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SYNC_OUTBOX)?;
        match table.get(job_id)? {
            Some(g) => Ok(Some(bincode::deserialize(g.value())?)),
            None => Ok(None),
        }
    }

    pub fn list_outbox_jobs(&self) -> Result<Vec<OutboxJob>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SYNC_OUTBOX)?;
        let mut out = Vec::new();
        let iter = table.iter()?;
        for result in iter {
            let (_k, v) = result?;
            out.push(bincode::deserialize(v.value())?);
        }
        Ok(out)
    }

    pub fn list_pending_outbox(&self) -> Result<Vec<OutboxJob>> {
        Ok(self
            .list_outbox_jobs()?
            .into_iter()
            .filter(|j| matches!(j.status, OutboxStatus::Queued | OutboxStatus::Failed))
            .collect())
    }

    pub fn delete_outbox_job(&self, job_id: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(SYNC_OUTBOX)?;
            table.remove(job_id)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Re-queue a terminal (`Failed`) outbox job. Bumps `retry_count`,
    /// resets `last_error`, and emits a `SyncJobUpdated` event so the
    /// UI clears its stale terminal badge.
    pub fn retry_outbox_job(&self, job_id: &str) -> Result<OutboxJob> {
        let mut job = self
            .get_outbox_job(job_id)?
            .ok_or_else(|| anyhow::anyhow!("outbox job not found"))?;
        job.status = OutboxStatus::Queued;
        job.retry_count = job.retry_count.saturating_add(1);
        job.last_error = None;
        self.put_outbox_job(&job)?;
        Ok(job)
    }

    pub fn put_sync_conflict(&self, c: &SyncConflict) -> Result<()> {
        let bytes = bincode::serialize(c)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(SYNC_CONFLICTS)?;
            table.insert(c.conflict_id.as_str(), bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn list_sync_conflicts(&self) -> Result<Vec<SyncConflict>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SYNC_CONFLICTS)?;
        let mut out = Vec::new();
        let iter = table.iter()?;
        for result in iter {
            let (_k, v) = result?;
            out.push(bincode::deserialize(v.value())?);
        }
        Ok(out)
    }

    pub fn delete_sync_conflict(&self, conflict_id: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(SYNC_CONFLICTS)?;
            table.remove(conflict_id)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Resolve conflict: KeepLocal drops remote quarantine; KeepRemote
    /// renames remote over local; KeepBoth leaves both and clears state.
    pub fn resolve_sync_conflict(
        &self,
        conflict_id: &str,
        resolution: ConflictResolution,
    ) -> Result<Option<SyncConflict>> {
        let conflicts = self.list_sync_conflicts()?;
        let Some(c) = conflicts.into_iter().find(|x| x.conflict_id == conflict_id) else {
            return Ok(None);
        };
        match resolution {
            ConflictResolution::KeepLocal => {
                let _ = std::fs::remove_file(&c.remote_path);
            }
            ConflictResolution::KeepRemote => {
                if Path::new(&c.remote_path).exists() {
                    let _ = std::fs::rename(&c.remote_path, &c.local_path);
                }
            }
            ConflictResolution::KeepBoth => {}
        }
        if let Some(mut tf) = self.get_tracked_file(&c.file_id)? {
            tf.sync_state = SyncState::Pending;
            self.put_tracked_file(&tf)?;
        }
        self.delete_sync_conflict(conflict_id)?;
        Ok(Some(c))
    }

    // ── Global FileSync ignore patterns (settings JSON) ─────────────

    /// Returns user/global ignore globs; seeds defaults (incl. `.git`) once.
    pub fn get_global_ignores(&self) -> Result<Vec<String>> {
        match self.get_setting(SYNC_GLOBAL_IGNORES_KEY)? {
            Some(raw) => {
                let v: Vec<String> = serde_json::from_str(&raw)
                    .unwrap_or_else(|_| qubox_sync::default_ignore_globs());
                Ok(v)
            }
            None => {
                let defaults = qubox_sync::default_ignore_globs();
                self.set_global_ignores(&defaults)?;
                Ok(defaults)
            }
        }
    }

    pub fn set_global_ignores(&self, globs: &[String]) -> Result<()> {
        let raw = serde_json::to_string(globs)?;
        self.set_setting(SYNC_GLOBAL_IGNORES_KEY, &raw)
    }

    pub fn add_global_ignore(&self, pattern: &str) -> Result<Vec<String>> {
        let mut globs = self.get_global_ignores()?;
        if !globs.iter().any(|g| g == pattern) {
            globs.push(pattern.to_string());
            self.set_global_ignores(&globs)?;
        }
        Ok(globs)
    }

    pub fn remove_global_ignore(&self, pattern: &str) -> Result<Vec<String>> {
        let mut globs = self.get_global_ignores()?;
        globs.retain(|g| g != pattern);
        // Never drop .git from defaults unless user explicitly wants empty?
        // Allow remove; user can re-add preset.
        self.set_global_ignores(&globs)?;
        Ok(globs)
    }

    pub fn apply_ignore_preset(&self, name: &str) -> Result<Vec<String>> {
        let Some(preset) = qubox_sync::ignore_preset(name) else {
            anyhow::bail!("unknown ignore preset: {name}");
        };
        let merged = qubox_sync::merge_ignore_globs(&self.get_global_ignores()?, &preset);
        self.set_global_ignores(&merged)?;
        Ok(merged)
    }

    /// Effective ignore list for a rule (global ∪ rule.ignore_globs).
    pub fn effective_ignores_for_rule(&self, rule: &SyncRule) -> Result<Vec<String>> {
        Ok(qubox_sync::merge_ignore_globs(
            &self.get_global_ignores()?,
            &rule.ignore_globs,
        ))
    }

    /// Enqueue a manual push for `local_path` to `target_peer`.
    pub fn enqueue_manual_push(
        &self,
        local_path: &str,
        target_peer: &str,
        node_id: &str,
    ) -> Result<OutboxJob> {
        use qubox_sync::{content_hash_file, new_id, now_unix, VectorClock};
        let path = Path::new(local_path);
        let (hash, _arr, size) = content_hash_file(path)?;
        let file_id = new_id();
        let mut clock = VectorClock::empty();
        clock.bump(node_id);
        let tracked = TrackedFile {
            file_id: file_id.clone(),
            local_path: local_path.to_string(),
            vector_clock: clock,
            content_hash: hash,
            size_bytes: size,
            sync_state: SyncState::Pending,
            rule_id: None,
            updated_at_unix: now_unix(),
        };
        self.put_tracked_file(&tracked)?;
        let job = OutboxJob {
            job_id: new_id(),
            file_id,
            target_peer: target_peer.to_string(),
            status: OutboxStatus::Queued,
            retry_count: 0,
            queued_at_unix: now_unix(),
            last_error: None,
        };
        self.put_outbox_job(&job)?;
        Ok(job)
    }

    #[cfg(test)]
    fn table_names(&self) -> Result<Vec<String>> {
        use redb::TableHandle;
        let txn = self.db.begin_read()?;
        let names: Vec<String> = txn.list_tables()?.map(|n| n.name().to_string()).collect();
        Ok(names)
    }
}

fn enforce_cap<const CAP: u64>(table: &mut redb::Table<'_, u64, &[u8]>) -> Result<()> {
    // Collect keys sorted by u64 (ascending = oldest first)
    let mut keys: Vec<u64> = Vec::new();
    for result in table.iter()? {
        let (k, _) = result?;
        keys.push(k.value());
    }
    if keys.len() <= CAP as usize {
        return Ok(());
    }
    let to_remove = keys.len() - CAP as usize;
    for k in &keys[..to_remove] {
        table.remove(*k)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (StateDb, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = StateDb::open(&dir.path().join("state.db")).unwrap();
        (db, dir)
    }

    #[test]
    fn state_open_creates_tables() {
        let (db, _dir) = temp_db();
        let names = db.table_names().unwrap();
        let expected: [&str; 15] = [
            "meta",
            "pairings",
            "host_state",
            "client_state",
            "settings",
            "tuf_root",
            "tuf_targets",
            "tuf_snapshot",
            "tuf_timestamp",
            "update_history",
            "session_history",
            "sync_rules",
            "tracked_files",
            "sync_outbox",
            "sync_conflicts",
        ];
        for name in &expected {
            assert!(names.contains(&name.to_string()), "missing: {name}");
        }
        assert_eq!(names.len(), expected.len());
    }

    #[test]
    fn state_schema_version_starts_at_2() {
        let (db, _dir) = temp_db();
        let raw = db.get_meta(SCHEMA_VERSION_KEY).unwrap().unwrap();
        let v = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        assert_eq!(v, 2);
    }

    #[test]
    fn state_global_ignores_seed_git() {
        let (db, _dir) = temp_db();
        let g = db.get_global_ignores().unwrap();
        assert!(g.iter().any(|x| x == ".git" || x == ".git/**"));
        db.add_global_ignore("*.rom").unwrap();
        let g2 = db.get_global_ignores().unwrap();
        assert!(g2.iter().any(|x| x == "*.rom"));
        db.remove_global_ignore("*.rom").unwrap();
        assert!(!db
            .get_global_ignores()
            .unwrap()
            .iter()
            .any(|x| x == "*.rom"));
        let after = db.apply_ignore_preset("emulator-saves").unwrap();
        assert!(after.iter().any(|x| x == "*.gba"));
    }

    #[test]
    fn complete_onboarding_writes_profile() {
        let (db, _dir) = temp_db();
        db.complete_onboarding(
            "device",
            "wss://signal.example/ws",
            true,
            Some("https://accounts.example"),
        )
        .unwrap();
        assert_eq!(db.get_setting("device_name").unwrap().as_deref(), Some("device"));
        assert_eq!(
            db.get_setting("signaling_server").unwrap().as_deref(),
            Some("wss://signal.example/ws")
        );
        assert_eq!(db.get_setting("cloud_mode").unwrap().as_deref(), Some("1"));
        assert_eq!(
            db.get_setting("accounts_url").unwrap().as_deref(),
            Some("https://accounts.example")
        );
        assert_eq!(
            db.get_setting("onboarding_complete").unwrap().as_deref(),
            Some("1")
        );
    }

    #[test]
    fn state_sync_rule_and_outbox_roundtrip() {
        let (db, _dir) = temp_db();
        let rule = SyncRule {
            rule_id: "r1".into(),
            paths: vec!["/saves".into()],
            process_names: vec!["mgba".into()],
            peer_ids: vec!["peer-a".into()],
            enabled: true,
            max_file_bytes: 1024,
            ignore_globs: vec!["*.tmp".into()],
        };
        db.put_sync_rule(&rule).unwrap();
        assert_eq!(db.get_sync_rule("r1").unwrap().unwrap(), rule);
        assert_eq!(db.list_sync_rules().unwrap().len(), 1);
        db.set_sync_rule_enabled("r1", false).unwrap();
        assert!(!db.get_sync_rule("r1").unwrap().unwrap().enabled);

        let dir = tempfile::tempdir().unwrap();
        let sav = dir.path().join("game.sav");
        std::fs::write(&sav, b"data").unwrap();
        let job = db
            .enqueue_manual_push(sav.to_str().unwrap(), "peer-a", "node-local")
            .unwrap();
        assert_eq!(job.status, OutboxStatus::Queued);
        assert_eq!(db.list_pending_outbox().unwrap().len(), 1);
        assert_eq!(db.list_tracked_files().unwrap().len(), 1);
    }

    #[test]
    fn state_pairing_roundtrip() {
        let (db, _dir) = temp_db();
        let p = Pairing {
            peer_id: "p1".into(),
            public_key: vec![1, 2, 3],
            paired_at: 1000,
            label: Some("test".into()),
        };
        db.put_pairing(&p).unwrap();
        assert_eq!(db.get_pairing("p1").unwrap().unwrap(), p);
        assert_eq!(db.list_pairings().unwrap().len(), 1);
        db.delete_pairing("p1").unwrap();
        assert!(db.get_pairing("p1").unwrap().is_none());
    }

    #[test]
    fn state_setting_roundtrip() {
        let (db, _dir) = temp_db();
        db.set_setting("k", "v").unwrap();
        assert_eq!(db.get_setting("k").unwrap(), Some("v".into()));
    }

    #[test]
    fn state_outbox_retry_roundtrip() {
        let (db, _dir) = temp_db();
        let dir = tempfile::tempdir().unwrap();
        let sav = dir.path().join("game.sav");
        std::fs::write(&sav, b"data").unwrap();
        let mut job = db
            .enqueue_manual_push(sav.to_str().unwrap(), "peer-a", "node-local")
            .unwrap();
        job.status = OutboxStatus::Failed;
        job.last_error = Some("network reset".into());
        job.retry_count = 2;
        db.put_outbox_job(&job).unwrap();
        let retried = db.retry_outbox_job(&job.job_id).unwrap();
        assert_eq!(retried.status, OutboxStatus::Queued);
        assert_eq!(retried.retry_count, 3);
        assert!(retried.last_error.is_none());
        db.delete_outbox_job(&job.job_id).unwrap();
        assert!(db.get_outbox_job(&job.job_id).unwrap().is_none());
    }

    #[test]
    fn state_session_history_caps_at_1000() {
        let (db, _dir) = temp_db();
        for i in 0..1001u64 {
            db.record_session(&SessionRecord {
                started_at: i,
                ended_at: i + 1,
                host_id: "h".into(),
                client_id: "c".into(),
                bytes_sent: 0,
                bytes_received: 0,
            })
            .unwrap();
        }
        assert_eq!(db.list_sessions().unwrap().len(), 1000);
    }

    #[test]
    fn state_concurrent_readers_succeed() {
        let db = std::sync::Arc::new(temp_db().0);
        let mut handles = Vec::new();
        let db_w = db.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..50 {
                db_w.set_setting("k", &format!("{i}")).unwrap();
                std::thread::sleep(std::time::Duration::from_micros(10));
            }
        }));
        for _ in 0..8 {
            let db_r = db.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    let _ = db_r.get_setting("k");
                    let _ = db_r.list_pairings();
                    std::thread::sleep(std::time::Duration::from_micros(5));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn state_host_state_roundtrip() {
        let (db, _dir) = temp_db();
        let st = HostState {
            last_seen: 42,
            current_session_id: Some("s".into()),
            config_hash: "a".into(),
            last_child_pid: None,
        };
        db.put_host_state("h", &st).unwrap();
        assert_eq!(db.get_host_state("h").unwrap().unwrap(), st);
    }

    #[test]
    fn state_client_state_roundtrip() {
        let (db, _dir) = temp_db();
        let st = ClientState {
            last_seen: 99,
            current_session_id: None,
            last_child_pid: None,
        };
        db.put_client_state("c", &st).unwrap();
        assert_eq!(db.get_client_state("c").unwrap().unwrap(), st);
    }

    #[test]
    fn state_tuf_metadata_roundtrip() {
        let (db, _dir) = temp_db();
        db.put_tuf_root(b"r").unwrap();
        db.put_tuf_targets(b"t").unwrap();
        db.put_tuf_snapshot(b"s").unwrap();
        db.put_tuf_timestamp(b"ts").unwrap();
        assert_eq!(db.get_tuf_root().unwrap(), Some(b"r".to_vec()));
        assert_eq!(db.get_tuf_targets().unwrap(), Some(b"t".to_vec()));
        assert_eq!(db.get_tuf_snapshot().unwrap(), Some(b"s".to_vec()));
        assert_eq!(db.get_tuf_timestamp().unwrap(), Some(b"ts".to_vec()));
    }

    #[test]
    fn state_update_history_caps_at_100() {
        let (db, _dir) = temp_db();
        for i in 0..101u64 {
            db.record_update(&UpdateRecord {
                applied_at: i,
                binary_path: format!("/bin/v{i}"),
                prev_version: if i > 0 {
                    Some(format!("v{}", i - 1))
                } else {
                    None
                },
            })
            .unwrap();
        }
        assert_eq!(db.list_updates().unwrap().len(), 100);
    }
}
