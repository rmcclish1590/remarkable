//! SQLite-backed sync state database.
//!
//! Tracks the "synced baseline" hash/mtime for every file so the three-state
//! diff (spec 11) can decide whether a change is local, remote, or both.

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS sync_state (
    uuid            TEXT PRIMARY KEY,
    visible_name    TEXT NOT NULL,
    parent_uuid     TEXT NOT NULL DEFAULT '',
    doc_type        TEXT NOT NULL,
    local_hash      TEXT,
    remote_hash     TEXT,
    synced_hash     TEXT,
    local_mtime     INTEGER,
    remote_mtime    INTEGER,
    synced_mtime    INTEGER,
    last_sync_at    INTEGER,
    sync_status     TEXT NOT NULL DEFAULT 'pending',
    conflict_info   TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sync_status ON sync_state(sync_status);
CREATE INDEX IF NOT EXISTS idx_parent ON sync_state(parent_uuid);

CREATE TABLE IF NOT EXISTS sync_log (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid            TEXT NOT NULL,
    action          TEXT NOT NULL,
    direction       TEXT,
    timestamp       INTEGER NOT NULL,
    hash_before     TEXT,
    hash_after      TEXT,
    details         TEXT
);

CREATE INDEX IF NOT EXISTS idx_log_uuid ON sync_log(uuid);
CREATE INDEX IF NOT EXISTS idx_log_timestamp ON sync_log(timestamp);

CREATE TABLE IF NOT EXISTS schema_version (
    version         INTEGER PRIMARY KEY
);
"#;

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    Pending,
    Synced,
    Conflict,
    Error,
}

impl SyncStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SyncStatus::Pending => "pending",
            SyncStatus::Synced => "synced",
            SyncStatus::Conflict => "conflict",
            SyncStatus::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(SyncStatus::Pending),
            "synced" => Ok(SyncStatus::Synced),
            "conflict" => Ok(SyncStatus::Conflict),
            "error" => Ok(SyncStatus::Error),
            other => Err(anyhow!("unknown sync_status value: {other}")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SyncFileState {
    pub uuid: String,
    pub visible_name: String,
    pub parent_uuid: String,
    pub doc_type: String,
    pub local_hash: Option<String>,
    pub remote_hash: Option<String>,
    pub synced_hash: Option<String>,
    pub local_mtime: Option<u64>,
    pub remote_mtime: Option<u64>,
    pub synced_mtime: Option<u64>,
    pub last_sync_at: Option<u64>,
    pub sync_status: SyncStatus,
    pub conflict_info: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SyncLogEntry {
    pub uuid: String,
    pub action: String,
    pub direction: Option<String>,
    pub timestamp: u64,
    pub hash_before: Option<String>,
    pub hash_after: Option<String>,
    pub details: Option<String>,
}

pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path)
            .with_context(|| format!("opening state db at {}", db_path.display()))?;
        let db = StateDb { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory state db")?;
        let db = StateDb { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(SCHEMA_SQL)
            .context("applying state db schema")?;
        self.conn
            .execute(
                "INSERT OR IGNORE INTO schema_version (version) VALUES (?1)",
                params![SCHEMA_VERSION],
            )
            .context("recording schema version")?;
        Ok(())
    }

    pub fn upsert_state(&self, state: &SyncFileState) -> Result<()> {
        let now = now_ms();
        self.conn
            .execute(
                r#"
                INSERT INTO sync_state (
                    uuid, visible_name, parent_uuid, doc_type,
                    local_hash, remote_hash, synced_hash,
                    local_mtime, remote_mtime, synced_mtime,
                    last_sync_at, sync_status, conflict_info,
                    created_at, updated_at
                ) VALUES (
                    ?1, ?2, ?3, ?4,
                    ?5, ?6, ?7,
                    ?8, ?9, ?10,
                    ?11, ?12, ?13,
                    ?14, ?14
                )
                ON CONFLICT(uuid) DO UPDATE SET
                    visible_name  = excluded.visible_name,
                    parent_uuid   = excluded.parent_uuid,
                    doc_type      = excluded.doc_type,
                    local_hash    = excluded.local_hash,
                    remote_hash   = excluded.remote_hash,
                    synced_hash   = excluded.synced_hash,
                    local_mtime   = excluded.local_mtime,
                    remote_mtime  = excluded.remote_mtime,
                    synced_mtime  = excluded.synced_mtime,
                    last_sync_at  = excluded.last_sync_at,
                    sync_status   = excluded.sync_status,
                    conflict_info = excluded.conflict_info,
                    updated_at    = excluded.updated_at
                "#,
                params![
                    state.uuid,
                    state.visible_name,
                    state.parent_uuid,
                    state.doc_type,
                    state.local_hash,
                    state.remote_hash,
                    state.synced_hash,
                    state.local_mtime.map(|v| v as i64),
                    state.remote_mtime.map(|v| v as i64),
                    state.synced_mtime.map(|v| v as i64),
                    state.last_sync_at.map(|v| v as i64),
                    state.sync_status.as_str(),
                    state.conflict_info,
                    now,
                ],
            )
            .with_context(|| format!("upserting sync_state for uuid {}", state.uuid))?;
        Ok(())
    }

    pub fn get_state(&self, uuid: &str) -> Result<Option<SyncFileState>> {
        let row = self
            .conn
            .query_row(
                "SELECT uuid, visible_name, parent_uuid, doc_type,
                        local_hash, remote_hash, synced_hash,
                        local_mtime, remote_mtime, synced_mtime,
                        last_sync_at, sync_status, conflict_info
                 FROM sync_state WHERE uuid = ?1",
                params![uuid],
                row_to_state,
            )
            .optional()
            .with_context(|| format!("loading sync_state for uuid {uuid}"))?;
        row.transpose()
    }

    pub fn get_all_states(&self) -> Result<Vec<SyncFileState>> {
        let mut stmt = self.conn.prepare(
            "SELECT uuid, visible_name, parent_uuid, doc_type,
                    local_hash, remote_hash, synced_hash,
                    local_mtime, remote_mtime, synced_mtime,
                    last_sync_at, sync_status, conflict_info
             FROM sync_state ORDER BY uuid",
        )?;
        let rows = stmt.query_map([], row_to_state)?;
        collect_states(rows)
    }

    pub fn get_by_status(&self, status: SyncStatus) -> Result<Vec<SyncFileState>> {
        let mut stmt = self.conn.prepare(
            "SELECT uuid, visible_name, parent_uuid, doc_type,
                    local_hash, remote_hash, synced_hash,
                    local_mtime, remote_mtime, synced_mtime,
                    last_sync_at, sync_status, conflict_info
             FROM sync_state WHERE sync_status = ?1 ORDER BY uuid",
        )?;
        let rows = stmt.query_map(params![status.as_str()], row_to_state)?;
        collect_states(rows)
    }

    pub fn delete_state(&self, uuid: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM sync_state WHERE uuid = ?1", params![uuid])
            .with_context(|| format!("deleting sync_state for uuid {uuid}"))?;
        Ok(())
    }

    pub fn mark_synced(&self, uuid: &str) -> Result<()> {
        let now = now_ms();
        let affected = self
            .conn
            .execute(
                "UPDATE sync_state
                 SET synced_hash  = local_hash,
                     synced_mtime = local_mtime,
                     sync_status  = 'synced',
                     last_sync_at = ?1,
                     updated_at   = ?1
                 WHERE uuid = ?2",
                params![now, uuid],
            )
            .with_context(|| format!("marking sync_state synced for uuid {uuid}"))?;
        if affected == 0 {
            return Err(anyhow!("no sync_state row for uuid {uuid}"));
        }
        Ok(())
    }

    pub fn mark_conflict(&self, uuid: &str, info: &str) -> Result<()> {
        let now = now_ms();
        let affected = self
            .conn
            .execute(
                "UPDATE sync_state
                 SET sync_status   = 'conflict',
                     conflict_info = ?1,
                     updated_at    = ?2
                 WHERE uuid = ?3",
                params![info, now, uuid],
            )
            .with_context(|| format!("marking sync_state conflict for uuid {uuid}"))?;
        if affected == 0 {
            return Err(anyhow!("no sync_state row for uuid {uuid}"));
        }
        Ok(())
    }

    pub fn log_action(&self, entry: &SyncLogEntry) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO sync_log
                    (uuid, action, direction, timestamp, hash_before, hash_after, details)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    entry.uuid,
                    entry.action,
                    entry.direction,
                    entry.timestamp as i64,
                    entry.hash_before,
                    entry.hash_after,
                    entry.details,
                ],
            )
            .context("writing sync_log entry")?;
        Ok(())
    }

    pub fn get_recent_log(&self, limit: u32) -> Result<Vec<SyncLogEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT uuid, action, direction, timestamp, hash_before, hash_after, details
             FROM sync_log ORDER BY timestamp DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_log_entry)?;
        collect_log(rows)
    }

    pub fn get_log_for_uuid(&self, uuid: &str) -> Result<Vec<SyncLogEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT uuid, action, direction, timestamp, hash_before, hash_after, details
             FROM sync_log WHERE uuid = ?1 ORDER BY timestamp DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![uuid], row_to_log_entry)?;
        collect_log(rows)
    }

    pub fn transaction<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        let tx = self.conn.unchecked_transaction()?;
        match f(&self.conn) {
            Ok(v) => {
                tx.commit()?;
                Ok(v)
            }
            Err(e) => {
                let _ = tx.rollback();
                Err(e)
            }
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn row_to_state(row: &Row<'_>) -> rusqlite::Result<Result<SyncFileState>> {
    let status_str: String = row.get(11)?;
    let status = SyncStatus::parse(&status_str);
    let state = status.map(|sync_status| SyncFileState {
        uuid: row.get(0).unwrap(),
        visible_name: row.get(1).unwrap(),
        parent_uuid: row.get(2).unwrap(),
        doc_type: row.get(3).unwrap(),
        local_hash: row.get(4).unwrap(),
        remote_hash: row.get(5).unwrap(),
        synced_hash: row.get(6).unwrap(),
        local_mtime: row.get::<_, Option<i64>>(7).unwrap().map(|v| v as u64),
        remote_mtime: row.get::<_, Option<i64>>(8).unwrap().map(|v| v as u64),
        synced_mtime: row.get::<_, Option<i64>>(9).unwrap().map(|v| v as u64),
        last_sync_at: row.get::<_, Option<i64>>(10).unwrap().map(|v| v as u64),
        sync_status,
        conflict_info: row.get(12).unwrap(),
    });
    Ok(state)
}

fn row_to_log_entry(row: &Row<'_>) -> rusqlite::Result<SyncLogEntry> {
    Ok(SyncLogEntry {
        uuid: row.get(0)?,
        action: row.get(1)?,
        direction: row.get(2)?,
        timestamp: row.get::<_, i64>(3)? as u64,
        hash_before: row.get(4)?,
        hash_after: row.get(5)?,
        details: row.get(6)?,
    })
}

fn collect_states<I>(rows: I) -> Result<Vec<SyncFileState>>
where
    I: Iterator<Item = rusqlite::Result<Result<SyncFileState>>>,
{
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

fn collect_log<I>(rows: I) -> Result<Vec<SyncLogEntry>>
where
    I: Iterator<Item = rusqlite::Result<SyncLogEntry>>,
{
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state(uuid: &str) -> SyncFileState {
        SyncFileState {
            uuid: uuid.to_string(),
            visible_name: "Notebook A".to_string(),
            parent_uuid: "".to_string(),
            doc_type: "DocumentType".to_string(),
            local_hash: Some("aaa".to_string()),
            remote_hash: Some("bbb".to_string()),
            synced_hash: None,
            local_mtime: Some(1_700_000_000_000),
            remote_mtime: Some(1_700_000_001_000),
            synced_mtime: None,
            last_sync_at: None,
            sync_status: SyncStatus::Pending,
            conflict_info: None,
        }
    }

    #[test]
    fn open_and_migrate_creates_tables() {
        let db = StateDb::open_in_memory().unwrap();
        let names: Vec<String> = db
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(names.contains(&"sync_state".to_string()));
        assert!(names.contains(&"sync_log".to_string()));
        assert!(names.contains(&"schema_version".to_string()));
    }

    #[test]
    fn upsert_and_get_roundtrip() {
        let db = StateDb::open_in_memory().unwrap();
        let s = sample_state("u1");
        db.upsert_state(&s).unwrap();

        let got = db.get_state("u1").unwrap().expect("row present");
        assert_eq!(got.uuid, "u1");
        assert_eq!(got.visible_name, "Notebook A");
        assert_eq!(got.parent_uuid, "");
        assert_eq!(got.doc_type, "DocumentType");
        assert_eq!(got.local_hash.as_deref(), Some("aaa"));
        assert_eq!(got.remote_hash.as_deref(), Some("bbb"));
        assert_eq!(got.synced_hash, None);
        assert_eq!(got.local_mtime, Some(1_700_000_000_000));
        assert_eq!(got.remote_mtime, Some(1_700_000_001_000));
        assert_eq!(got.synced_mtime, None);
        assert_eq!(got.last_sync_at, None);
        assert_eq!(got.sync_status, SyncStatus::Pending);
        assert_eq!(got.conflict_info, None);
    }

    #[test]
    fn mark_synced_copies_hashes() {
        let db = StateDb::open_in_memory().unwrap();
        db.upsert_state(&sample_state("u1")).unwrap();
        db.mark_synced("u1").unwrap();

        let got = db.get_state("u1").unwrap().unwrap();
        assert_eq!(got.synced_hash.as_deref(), Some("aaa"));
        assert_eq!(got.synced_mtime, Some(1_700_000_000_000));
        assert_eq!(got.sync_status, SyncStatus::Synced);
        assert!(got.last_sync_at.unwrap() > 0);
    }

    #[test]
    fn mark_conflict_updates_status_and_info() {
        let db = StateDb::open_in_memory().unwrap();
        db.upsert_state(&sample_state("u1")).unwrap();
        db.mark_conflict("u1", r#"{"reason":"both changed"}"#).unwrap();

        let got = db.get_state("u1").unwrap().unwrap();
        assert_eq!(got.sync_status, SyncStatus::Conflict);
        assert_eq!(
            got.conflict_info.as_deref(),
            Some(r#"{"reason":"both changed"}"#)
        );
    }

    #[test]
    fn delete_state_removes_row() {
        let db = StateDb::open_in_memory().unwrap();
        db.upsert_state(&sample_state("u1")).unwrap();
        db.delete_state("u1").unwrap();
        assert!(db.get_state("u1").unwrap().is_none());
    }

    #[test]
    fn log_action_retrieval_order() {
        let db = StateDb::open_in_memory().unwrap();
        for (i, ts) in [100_u64, 200, 300].iter().enumerate() {
            db.log_action(&SyncLogEntry {
                uuid: format!("u{i}"),
                action: "push".to_string(),
                direction: Some("to_remote".to_string()),
                timestamp: *ts,
                hash_before: None,
                hash_after: Some(format!("h{i}")),
                details: None,
            })
            .unwrap();
        }

        let recent = db.get_recent_log(2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].timestamp, 300);
        assert_eq!(recent[1].timestamp, 200);
    }

    #[test]
    fn get_by_status_filters() {
        let db = StateDb::open_in_memory().unwrap();
        let mut pending = sample_state("u_pending");
        pending.sync_status = SyncStatus::Pending;
        let mut synced = sample_state("u_synced");
        synced.sync_status = SyncStatus::Synced;
        let mut conflict = sample_state("u_conflict");
        conflict.sync_status = SyncStatus::Conflict;

        db.upsert_state(&pending).unwrap();
        db.upsert_state(&synced).unwrap();
        db.upsert_state(&conflict).unwrap();

        let conflicts = db.get_by_status(SyncStatus::Conflict).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].uuid, "u_conflict");
    }

    #[test]
    fn migrate_is_idempotent() {
        let db = StateDb::open_in_memory().unwrap();
        db.migrate().unwrap();
        db.migrate().unwrap();
        let versions: Vec<i64> = db
            .conn
            .prepare("SELECT version FROM schema_version")
            .unwrap()
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(versions, vec![SCHEMA_VERSION]);
    }

    #[test]
    fn transaction_rolls_back_on_error() {
        let db = StateDb::open_in_memory().unwrap();
        let result: Result<()> = db.transaction(|conn| {
            conn.execute(
                "INSERT INTO sync_state (
                    uuid, visible_name, parent_uuid, doc_type,
                    sync_status, created_at, updated_at
                 ) VALUES ('u_tx', 'X', '', 'DocumentType', 'pending', 0, 0)",
                [],
            )?;
            Err(anyhow!("forced rollback"))
        });
        assert!(result.is_err());
        assert!(db.get_state("u_tx").unwrap().is_none());
    }
}
