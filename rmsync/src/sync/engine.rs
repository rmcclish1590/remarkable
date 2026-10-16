//! Three-state diff and sync orchestration across local, remote, and DB state.
//!
//! `compute_sync_plan` is a pure function: given the current local manifest,
//! the current remote manifest, and the synced baseline in SQLite, it produces
//! an ordered list of `SyncAction`s. No files are touched; executors live in
//! specs 12/13.

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::sync::scanner::{LocalDocumentSnapshot, LocalManifest, RemoteDocumentSnapshot, RemoteManifest};
use crate::sync::state_db::{StateDb, SyncFileState, SyncStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncActionType {
    Pull,
    Push,
    DeleteLocal,
    DeleteRemote,
    Conflict { local_mtime: u64, remote_mtime: u64 },
    DeleteBoth,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncAction {
    pub uuid: String,
    pub visible_name: String,
    pub action_type: SyncActionType,
    pub priority: u8,
}

#[derive(Debug, Default)]
pub struct SyncPlan {
    pub actions: Vec<SyncAction>,
    pub total_pull: usize,
    pub total_push: usize,
    pub total_delete: usize,
    pub total_conflict: usize,
    pub total_skip: usize,
}

impl SyncPlan {
    pub fn has_conflicts(&self) -> bool {
        self.total_conflict > 0
    }

    pub fn conflicts(&self) -> Vec<&SyncAction> {
        self.actions
            .iter()
            .filter(|a| matches!(a.action_type, SyncActionType::Conflict { .. }))
            .collect()
    }

    pub fn actionable(&self) -> Vec<&SyncAction> {
        self.actions
            .iter()
            .filter(|a| !matches!(a.action_type, SyncActionType::Skip))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.actionable().is_empty()
    }

    pub fn summary(&self) -> String {
        format!(
            "Pull {}, Push {}, Delete {}, Conflicts {}, Skip {}",
            self.total_pull, self.total_push, self.total_delete, self.total_conflict, self.total_skip
        )
    }
}

/// Compute the sync plan by diffing local, remote, and synced states.
pub fn compute_sync_plan(
    local: &LocalManifest,
    remote: &RemoteManifest,
    db: &StateDb,
) -> Result<SyncPlan> {
    let synced_vec = db.get_all_states()?;
    Ok(compute_sync_plan_from_parts(local, remote, &synced_vec))
}

/// Variant of `compute_sync_plan` that takes the synced-state vector directly.
/// Makes the diff trivially unit-testable without touching SQLite.
pub fn compute_sync_plan_from_parts(
    local: &LocalManifest,
    remote: &RemoteManifest,
    synced: &[SyncFileState],
) -> SyncPlan {
    let local_map: HashMap<&str, &LocalDocumentSnapshot> = local
        .documents
        .iter()
        .map(|d| (d.uuid.as_str(), d))
        .collect();
    let remote_map: HashMap<&str, &RemoteDocumentSnapshot> = remote
        .documents
        .iter()
        .map(|d| (d.uuid.as_str(), d))
        .collect();
    let synced_map: HashMap<&str, &SyncFileState> =
        synced.iter().map(|s| (s.uuid.as_str(), s)).collect();

    let mut all_uuids: HashSet<&str> = HashSet::new();
    all_uuids.extend(local_map.keys().copied());
    all_uuids.extend(remote_map.keys().copied());
    all_uuids.extend(synced_map.keys().copied());

    let mut actions = Vec::with_capacity(all_uuids.len());
    for uuid in all_uuids {
        let action = classify(
            uuid,
            local_map.get(uuid).copied(),
            remote_map.get(uuid).copied(),
            synced_map.get(uuid).copied(),
        );
        actions.push(action);
    }

    actions.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| action_rank(&a.action_type).cmp(&action_rank(&b.action_type)))
            .then_with(|| a.uuid.cmp(&b.uuid))
    });

    let mut plan = SyncPlan::default();
    for a in &actions {
        match &a.action_type {
            SyncActionType::Pull => plan.total_pull += 1,
            SyncActionType::Push => plan.total_push += 1,
            SyncActionType::DeleteLocal
            | SyncActionType::DeleteRemote
            | SyncActionType::DeleteBoth => plan.total_delete += 1,
            SyncActionType::Conflict { .. } => plan.total_conflict += 1,
            SyncActionType::Skip => plan.total_skip += 1,
        }
    }
    plan.actions = actions;
    plan
}

fn classify(
    uuid: &str,
    local: Option<&LocalDocumentSnapshot>,
    remote: Option<&RemoteDocumentSnapshot>,
    synced: Option<&SyncFileState>,
) -> SyncAction {
    let (visible_name, priority) = derive_name_and_priority(local, remote, synced);
    let action_type = match (local, remote, synced) {
        (Some(l), Some(r), Some(s)) => {
            let synced_hash = s.synced_hash.as_deref().unwrap_or("");
            let local_changed = l.content_hash != synced_hash;
            let remote_changed = r.content_hash != synced_hash;
            match (local_changed, remote_changed) {
                (false, false) => SyncActionType::Skip,
                (true, false) => SyncActionType::Push,
                (false, true) => SyncActionType::Pull,
                (true, true) => {
                    if l.content_hash == r.content_hash {
                        SyncActionType::Skip
                    } else {
                        SyncActionType::Conflict {
                            local_mtime: l.mtime,
                            remote_mtime: r.mtime,
                        }
                    }
                }
            }
        }
        (None, Some(_), None) => SyncActionType::Pull,
        // A "(conflict …)" copy made by an earlier version exists only on this
        // computer; pushing it would put the clutter on the device. The
        // cleanup in settings removes these.
        (Some(_), None, None) if crate::sync::cleanup::strip_conflict_suffix(&visible_name).is_some() => {
            SyncActionType::Skip
        }
        (Some(_), None, None) => SyncActionType::Push,
        (Some(l), Some(r), None) => {
            if l.content_hash == r.content_hash {
                SyncActionType::Skip
            } else {
                SyncActionType::Conflict {
                    local_mtime: l.mtime,
                    remote_mtime: r.mtime,
                }
            }
        }
        (Some(l), None, Some(s)) => {
            if l.content_hash == s.synced_hash.as_deref().unwrap_or("") {
                SyncActionType::DeleteLocal
            } else {
                SyncActionType::Push
            }
        }
        (None, Some(r), Some(s)) => {
            if r.content_hash == s.synced_hash.as_deref().unwrap_or("") {
                SyncActionType::DeleteRemote
            } else {
                SyncActionType::Pull
            }
        }
        (None, None, Some(_)) => SyncActionType::DeleteBoth,
        (None, None, None) => SyncActionType::Skip,
    };

    SyncAction {
        uuid: uuid.to_string(),
        visible_name,
        action_type,
        priority,
    }
}

/// Baselines to record for documents both sides already agree on.
///
/// A `Skip` leaves no state row behind, so the next one-sided change to a
/// matched document would be diffed against nothing and look like a conflict.
/// Recording the agreed hash is what lets an existing local file be recognised
/// by UUID and updated in place. Also repairs baselines left stale by an
/// earlier hashing scheme.
pub fn matched_baselines(
    local: &LocalManifest,
    remote: &RemoteManifest,
    synced: &[SyncFileState],
) -> Vec<SyncFileState> {
    let remote_map: HashMap<&str, &RemoteDocumentSnapshot> =
        remote.documents.iter().map(|d| (d.uuid.as_str(), d)).collect();
    let synced_map: HashMap<&str, &SyncFileState> =
        synced.iter().map(|s| (s.uuid.as_str(), s)).collect();
    let now = now_secs();

    let mut updates = Vec::new();
    for l in &local.documents {
        let Some(r) = remote_map.get(l.uuid.as_str()) else { continue };
        if l.content_hash != r.content_hash {
            continue;
        }
        let existing = synced_map.get(l.uuid.as_str()).copied();
        if existing.is_some_and(|s| s.synced_hash.as_deref() == Some(l.content_hash.as_str())) {
            continue;
        }
        let mut state = existing.cloned().unwrap_or_else(|| SyncFileState {
            uuid: l.uuid.clone(),
            visible_name: String::new(),
            parent_uuid: String::new(),
            doc_type: String::new(),
            local_hash: None,
            remote_hash: None,
            synced_hash: None,
            local_mtime: None,
            remote_mtime: None,
            synced_mtime: None,
            last_sync_at: None,
            sync_status: SyncStatus::Synced,
            conflict_info: None,
        });
        state.visible_name = r.metadata.visible_name.clone();
        state.parent_uuid = r.metadata.parent.clone();
        state.doc_type = r.metadata.doc_type.clone();
        state.local_hash = Some(l.content_hash.clone());
        state.remote_hash = Some(r.content_hash.clone());
        state.synced_hash = Some(l.content_hash.clone());
        state.local_mtime = Some(l.mtime);
        state.remote_mtime = Some(r.mtime);
        state.synced_mtime = Some(l.mtime.max(r.mtime));
        state.last_sync_at = Some(now);
        state.sync_status = SyncStatus::Synced;
        state.conflict_info = None;
        updates.push(state);
    }
    updates
}

fn derive_name_and_priority(
    local: Option<&LocalDocumentSnapshot>,
    remote: Option<&RemoteDocumentSnapshot>,
    synced: Option<&SyncFileState>,
) -> (String, u8) {
    if let Some(r) = remote {
        let prio = if r.metadata.doc_type == "CollectionType" {
            0
        } else {
            1
        };
        return (r.metadata.visible_name.clone(), prio);
    }
    if let Some(l) = local {
        let prio = if l.metadata.doc_type == "CollectionType" {
            0
        } else {
            1
        };
        return (l.metadata.visible_name.clone(), prio);
    }
    if let Some(s) = synced {
        let prio = if s.doc_type == "CollectionType" { 0 } else { 1 };
        return (s.visible_name.clone(), prio);
    }
    (String::new(), 1)
}

// =========================================================================
// Conflict resolution (spec 14)
// =========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictWinner {
    Local,
    Remote,
}

/// Modified times closer together than this are treated as simultaneous.
/// Covers filesystem timestamp granularity and sub-second clock drift; a tie
/// goes to the device, the same side that wins identical timestamps.
pub const TIMESTAMP_TOLERANCE_SECS: i64 = 2;

#[derive(Debug, Clone)]
pub struct ConflictResolution {
    pub uuid: String,
    pub visible_name: String,
    pub winner: ConflictWinner,
    /// Where the overwritten version is kept, relative to
    /// `<sync_dir>/.rmsync/backups/`. Deliberately outside `raw/`: a backup is
    /// not a document, so it never shows up in the library or gets pushed.
    pub backup_subdir: String,
    pub local_mtime: u64,
    pub remote_mtime: u64,
    /// Device clock minus desktop clock, in seconds, used to compare the two.
    pub clock_offset_secs: i64,
}

#[derive(Debug, Clone)]
pub struct ConflictNotification {
    pub document_name: String,
    pub winner_source: String,
    pub winner_mtime: u64,
    pub loser_mtime: u64,
    pub backup_location: String,
    pub time_difference_human: String,
}

impl ConflictResolution {
    pub fn to_notification(&self) -> ConflictNotification {
        let (winner_source, winner_mtime, loser_mtime) = match self.winner {
            ConflictWinner::Local => ("local", self.local_mtime, self.remote_mtime),
            ConflictWinner::Remote => ("reMarkable", self.remote_mtime, self.local_mtime),
        };
        let delta = (self.local_mtime as i64 - (self.remote_mtime as i64 - self.clock_offset_secs))
            .unsigned_abs();
        ConflictNotification {
            document_name: self.visible_name.clone(),
            winner_source: winner_source.to_string(),
            winner_mtime,
            loser_mtime,
            backup_location: self.backup_subdir.clone(),
            time_difference_human: humanise_delta(delta, winner_source),
        }
    }
}

fn humanise_delta(delta: u64, winner_source: &str) -> String {
    let (value, unit) = if delta >= 86400 {
        (delta / 86400, "day")
    } else if delta >= 3600 {
        (delta / 3600, "hour")
    } else if delta >= 60 {
        (delta / 60, "minute")
    } else {
        (delta, "second")
    };
    let plural = if value == 1 { "" } else { "s" };
    format!("{winner_source} was {value} {unit}{plural} newer")
}

/// Decide which side's modification is the more recent.
///
/// `clock_offset_secs` is the device clock minus the desktop clock. Modified
/// times are Unix timestamps, so timezones cancel out; what does not cancel is
/// the device's clock being set wrong (a tablet that has been off for weeks
/// often is), so the device time is shifted onto the desktop clock before
/// comparing. Differences within `TIMESTAMP_TOLERANCE_SECS` are a tie.
pub fn pick_winner(local_mtime: u64, remote_mtime: u64, clock_offset_secs: i64) -> ConflictWinner {
    let remote_on_local_clock = remote_mtime as i64 - clock_offset_secs;
    if local_mtime as i64 - remote_on_local_clock > TIMESTAMP_TOLERANCE_SECS {
        ConflictWinner::Local
    } else {
        ConflictWinner::Remote
    }
}

/// Resolve a two-sided change: the most recently modified version wins and the
/// other is backed up (never silently discarded, never turned into a second
/// document).
pub fn resolve_conflict(
    action: &SyncAction,
    local: &LocalDocumentSnapshot,
    remote: &RemoteDocumentSnapshot,
    clock_offset_secs: i64,
) -> ConflictResolution {
    let winner = pick_winner(local.mtime, remote.mtime, clock_offset_secs);
    let now = now_secs();
    let loser = match winner {
        ConflictWinner::Local => "device",
        ConflictWinner::Remote => "desktop",
    };
    let backup_subdir = format!("{}/{}-{loser}-{now}", action.uuid, conflict_date_string(now));
    tracing::info!(
        uuid = %action.uuid,
        name = %local.metadata.visible_name,
        local_mtime = local.mtime,
        remote_mtime = remote.mtime,
        clock_offset_secs,
        winner = ?winner,
        backup = %backup_subdir,
        "two-sided change resolved by modified time"
    );
    ConflictResolution {
        uuid: action.uuid.clone(),
        visible_name: local.metadata.visible_name.clone(),
        winner,
        backup_subdir,
        local_mtime: local.mtime,
        remote_mtime: remote.mtime,
        clock_offset_secs,
    }
}

fn conflict_date_string(unix: u64) -> String {
    // Minimal YYYY-MM-DD formatter, no chrono dependency: convert unix seconds
    // to a civil date using the algorithm from Howard Hinnant.
    let days = (unix / 86400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert days-since-unix-epoch to a `(year, month, day)` tuple using
/// Howard Hinnant's algorithm — cheaper than pulling in `chrono` for two
/// date-formatting sites.
pub fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// =========================================================================
// Sync orchestrator (spec 22)
// =========================================================================

use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::device::connection::{ConnectionError, DeviceConnection, HostKeyChange};
use crate::sync::scanner::{scan_local, scan_remote_with_progress, ScanProgress};
use crate::sync::transfer::{
    delete_local_document, delete_remote_document, estimate_clock_offset, pull_batch, push_batch,
    reload_xochitl,
    resolve_all_conflicts, TransferProgress,
};

#[derive(Debug, Clone)]
pub enum SyncPhase {
    Connecting,
    ScanningRemote,
    ScanningLocal,
    ComputingDiff,
    Pulling,
    Pushing,
    Deleting,
    ResolvingConflicts,
    Finalizing,
}

#[derive(Debug, Clone)]
pub enum SyncProgressEvent {
    Phase(SyncPhase),
    ScanProgress(ScanProgress),
    TransferProgress(TransferProgress),
    ConflictResolved(ConflictNotification),
    Error(String),
    /// The device presented a different SSH host key than the pinned one.
    /// The connection was refused; the UI decides whether to re-pin.
    HostKeyMismatch(HostKeyChange),
    Complete(SyncReport),
}

#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub pulled: usize,
    pub pushed: usize,
    pub deleted: usize,
    pub conflicts_resolved: usize,
    pub errors: Vec<String>,
    /// Things that did not stop the sync but the user should know about.
    pub warnings: Vec<String>,
    pub duration_ms: u64,
}

impl SyncReport {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "Sync complete: {} pulled, {} pushed, {} deleted, {} conflicts",
            self.pulled, self.pushed, self.deleted, self.conflicts_resolved
        );
        for w in &self.warnings {
            s.push_str(&format!("\n{w}"));
        }
        s
    }
}

pub struct SyncOrchestrator {
    pub config: AppConfig,
    pub db: StateDb,
    pub connection: DeviceConnection,
    pub cancel: CancellationToken,
}

impl SyncOrchestrator {
    pub fn new(config: AppConfig) -> anyhow::Result<Self> {
        let db_path = config.sync.sync_dir.join(".rmsync").join("state.db");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = StateDb::open(&db_path)?;
        let connection = DeviceConnection::new(config.to_connection_config());
        Ok(Self {
            config,
            db,
            connection,
            cancel: CancellationToken::new(),
        })
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub async fn run_sync(&mut self) -> anyhow::Result<SyncReport> {
        self.run_sync_with_progress(|_| {}).await
    }

    pub async fn run_sync_with_progress<F>(&mut self, callback: F) -> anyhow::Result<SyncReport>
    where
        F: Fn(SyncProgressEvent) + Send + Sync + Clone + 'static,
    {
        let start = Instant::now();
        let sync_dir = self.config.sync.sync_dir.clone();
        let mut report = SyncReport::default();
        tracing::info!(
            sync_dir = %sync_dir.display(),
            host = %self.config.device.host,
            "sync started"
        );

        // 1. Connect
        callback(SyncProgressEvent::Phase(SyncPhase::Connecting));
        tracing::debug!(
            host = %self.config.device.host,
            port = self.config.device.port,
            username = %self.config.device.username,
            "connecting to device"
        );
        if let Err(e) = self.connection.connect().await {
            let msg = format!("connect failed: {e}");
            tracing::error!(error = format!("{e:#}"), "could not connect to device");
            if let ConnectionError::HostKeyMismatch(change) = &e {
                callback(SyncProgressEvent::HostKeyMismatch(change.clone()));
            }
            callback(SyncProgressEvent::Error(msg.clone()));
            report.errors.push(msg);
            report.duration_ms = start.elapsed().as_millis() as u64;
            callback(SyncProgressEvent::Complete(report.clone()));
            return Ok(report);
        }
        if self.cancel.is_cancelled() {
            return Ok(self.finalise(start, report, &callback).await);
        }

        // The device clock decides which side of a two-sided change is newer,
        // so measure it up front and say plainly when that is not possible.
        let clock_offset = match estimate_clock_offset(&self.connection).await {
            Ok(offset) => offset,
            Err(e) => {
                let note = format!(
                    "Could not measure the reMarkable's clock ({e:#}); file times are compared as-is, \
                     so which side is newer may be misjudged if the clocks differ."
                );
                tracing::warn!(error = format!("{e:#}"), fallback = "offset 0", "could not measure device clock");
                report.warnings.push(note);
                0
            }
        };

        // 2. Scan remote
        callback(SyncProgressEvent::Phase(SyncPhase::ScanningRemote));
        let scan_cb = callback.clone();
        let remote = match scan_remote_with_progress(&self.connection, move |p| {
            scan_cb(SyncProgressEvent::ScanProgress(p));
        })
        .await
        {
            Ok(m) => {
                tracing::info!(
                    documents = m.total_documents,
                    bytes = m.total_size_bytes,
                    "remote scan complete"
                );
                m
            }
            Err(e) => {
                let msg = format!("remote scan failed: {e}");
                tracing::error!(error = format!("{e:#}"), "remote scan failed");
                callback(SyncProgressEvent::Error(msg.clone()));
                report.errors.push(msg);
                return Ok(self.finalise(start, report, &callback).await);
            }
        };
        if self.cancel.is_cancelled() {
            return Ok(self.finalise(start, report, &callback).await);
        }

        // 3. Scan local
        callback(SyncProgressEvent::Phase(SyncPhase::ScanningLocal));
        let local = match scan_local(&sync_dir) {
            Ok(m) => {
                tracing::info!(
                    documents = m.total_documents,
                    bytes = m.total_size_bytes,
                    path = %sync_dir.join("raw").display(),
                    "local scan complete"
                );
                m
            }
            Err(e) => {
                let msg = format!("local scan failed: {e}");
                tracing::error!(
                    path = %sync_dir.display(),
                    error = format!("{e:#}"),
                    "local scan failed"
                );
                callback(SyncProgressEvent::Error(msg.clone()));
                report.errors.push(msg);
                return Ok(self.finalise(start, report, &callback).await);
            }
        };

        // 4. Compute diff
        callback(SyncProgressEvent::Phase(SyncPhase::ComputingDiff));
        let plan = match compute_sync_plan(&local, &remote, &self.db) {
            Ok(p) => {
                tracing::info!(
                    pull = p.total_pull,
                    push = p.total_push,
                    delete = p.total_delete,
                    conflicts = p.has_conflicts(),
                    "sync plan computed"
                );
                p
            }
            Err(e) => {
                let msg = format!("diff failed: {e}");
                tracing::error!(error = format!("{e:#}"), "diff failed");
                callback(SyncProgressEvent::Error(msg.clone()));
                report.errors.push(msg);
                return Ok(self.finalise(start, report, &callback).await);
            }
        };

        // Adopt documents both sides already agree on, so they are matched by
        // UUID from now on instead of being re-diffed against no baseline.
        match self.db.get_all_states() {
            Ok(states) => {
                for state in matched_baselines(&local, &remote, &states) {
                    tracing::info!(
                        uuid = %state.uuid,
                        name = %state.visible_name,
                        "matched existing local document by uuid; recording baseline"
                    );
                    if let Err(e) = self.db.upsert_state(&state) {
                        tracing::warn!(uuid = %state.uuid, error = format!("{e:#}"), "recording baseline failed");
                    }
                }
            }
            Err(e) => tracing::warn!(error = format!("{e:#}"), "reading sync state failed"),
        }
        for a in &plan.actions {
            tracing::debug!(uuid = %a.uuid, name = %a.visible_name, action = ?a.action_type, "sync decision");
        }

        // 5. Resolve conflicts
        if plan.has_conflicts() && !self.cancel.is_cancelled() {
            callback(SyncProgressEvent::Phase(SyncPhase::ResolvingConflicts));
            let (resolutions, errors) =
                resolve_all_conflicts(&self.connection, &plan, &local, &remote, &sync_dir, &self.db, clock_offset).await;
            report.conflicts_resolved = resolutions.len();
            tracing::info!(resolved = resolutions.len(), failed = errors.len(), "two-sided changes processed");
            for r in resolutions {
                callback(SyncProgressEvent::ConflictResolved(r.to_notification()));
            }
            report.errors.extend(errors);
        }

        // 6. Pulls
        if plan.total_pull > 0 && !self.cancel.is_cancelled() {
            callback(SyncProgressEvent::Phase(SyncPhase::Pulling));
            let tcb = callback.clone();
            match pull_batch(&self.connection, &plan, &sync_dir, &self.db, move |p| {
                tcb(SyncProgressEvent::TransferProgress(p));
            })
            .await
            {
                Ok(results) => {
                    report.pulled = results.iter().filter(|r| r.success).count();
                    tracing::info!(
                        pulled = report.pulled,
                        failed = results.len() - report.pulled,
                        "pull batch complete"
                    );
                    for r in results.iter().filter(|r| !r.success) {
                        if let Some(e) = &r.error {
                            tracing::error!(uuid = %r.uuid, error = %e, "pull failed");
                            report.errors.push(format!("pull “{}”: {e}", name_of(&plan, &r.uuid)));
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("pull batch failed: {e}");
                    tracing::error!(error = format!("{e:#}"), "pull batch failed");
                    callback(SyncProgressEvent::Error(msg.clone()));
                    report.errors.push(msg);
                }
            }
        }

        // 7. Pushes
        if plan.total_push > 0 && !self.cancel.is_cancelled() {
            callback(SyncProgressEvent::Phase(SyncPhase::Pushing));
            let tcb = callback.clone();
            match push_batch(&self.connection, &plan, &sync_dir, &self.db, move |p| {
                tcb(SyncProgressEvent::TransferProgress(p));
            })
            .await
            {
                Ok(results) => {
                    report.pushed = results.iter().filter(|r| r.success).count();
                    tracing::info!(
                        pushed = report.pushed,
                        failed = results.len() - report.pushed,
                        "push batch complete"
                    );
                    for r in results.iter().filter(|r| !r.success) {
                        if let Some(e) = &r.error {
                            tracing::error!(uuid = %r.uuid, error = %e, "push failed");
                            report.errors.push(format!("push “{}”: {e}", name_of(&plan, &r.uuid)));
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("push batch failed: {e}");
                    tracing::error!(error = format!("{e:#}"), "push batch failed");
                    callback(SyncProgressEvent::Error(msg.clone()));
                    report.errors.push(msg);
                }
            }
        }

        // 8. Deletes
        if plan.total_delete > 0 && !self.cancel.is_cancelled() {
            callback(SyncProgressEvent::Phase(SyncPhase::Deleting));
            // Deletions are the only destructive step and the hardest to
            // reconstruct after the fact, so each one is logged at INFO
            // with its uuid — a user who loses a document needs to be able
            // to see whether rmSync removed it, and from which side.
            tracing::info!(count = plan.total_delete, "applying deletions");
            for action in plan.actions.iter() {
                match &action.action_type {
                    SyncActionType::DeleteLocal => {
                        tracing::info!(uuid = %action.uuid, side = "local", "deleting document");
                        if let Err(e) = delete_local_document(&action.uuid, &sync_dir) {
                            tracing::error!(
                                uuid = %action.uuid,
                                error = format!("{e:#}"),
                                "local delete failed"
                            );
                            report.errors.push(format!("delete local {}: {e}", action.uuid));
                        } else {
                            let _ = self.db.delete_state(&action.uuid);
                            report.deleted += 1;
                        }
                    }
                    SyncActionType::DeleteRemote => {
                        tracing::info!(uuid = %action.uuid, side = "remote", "deleting document");
                        if let Err(e) = delete_remote_document(&self.connection, &action.uuid).await {
                            tracing::error!(
                                uuid = %action.uuid,
                                error = format!("{e:#}"),
                                "remote delete failed"
                            );
                            report.errors.push(format!("delete remote {}: {e}", action.uuid));
                        } else {
                            let _ = self.db.delete_state(&action.uuid);
                            report.deleted += 1;
                        }
                    }
                    SyncActionType::DeleteBoth => {
                        let _ = delete_local_document(&action.uuid, &sync_dir);
                        let _ = delete_remote_document(&self.connection, &action.uuid).await;
                        let _ = self.db.delete_state(&action.uuid);
                        report.deleted += 1;
                    }
                    _ => {}
                }
            }
        }

        // 9. Reload xochitl
        if (plan.total_push > 0 || plan.total_delete > 0 || plan.total_conflict > 0)
            && !self.cancel.is_cancelled() {
            let _ = reload_xochitl(&mut self.connection).await;
        }

        Ok(self.finalise(start, report, &callback).await)
    }

    async fn finalise<F>(
        &mut self,
        start: Instant,
        mut report: SyncReport,
        callback: &F,
    ) -> SyncReport
    where
        F: Fn(SyncProgressEvent) + Send + Sync + Clone + 'static,
    {
        callback(SyncProgressEvent::Phase(SyncPhase::Finalizing));
        self.connection.disconnect().await;
        report.duration_ms = start.elapsed().as_millis() as u64;
        if report.errors.is_empty() {
            tracing::info!(
                pulled = report.pulled,
                pushed = report.pushed,
                deleted = report.deleted,
                conflicts_resolved = report.conflicts_resolved,
                duration_ms = report.duration_ms,
                "sync finished"
            );
        } else {
            tracing::warn!(
                pulled = report.pulled,
                pushed = report.pushed,
                deleted = report.deleted,
                errors = report.errors.len(),
                duration_ms = report.duration_ms,
                first_error = %report.errors[0],
                "sync finished with errors"
            );
        }
        callback(SyncProgressEvent::Complete(report.clone()));
        report
    }
}

/// A document's name for messages, falling back to its uuid.
fn name_of<'a>(plan: &'a SyncPlan, uuid: &'a str) -> &'a str {
    plan.actions
        .iter()
        .find(|a| a.uuid == uuid)
        .map(|a| a.visible_name.as_str())
        .unwrap_or(uuid)
}

fn action_rank(a: &SyncActionType) -> u8 {
    match a {
        SyncActionType::Pull => 0,
        SyncActionType::Push => 1,
        SyncActionType::Conflict { .. } => 2,
        SyncActionType::DeleteLocal => 3,
        SyncActionType::DeleteRemote => 4,
        SyncActionType::DeleteBoth => 5,
        SyncActionType::Skip => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remarkable::metadata::RemarkableMetadata;
    use crate::sync::state_db::SyncStatus;
    use std::path::PathBuf;

    fn meta(name: &str, doc_type: &str) -> RemarkableMetadata {
        serde_json::from_str(&format!(
            r#"{{"deleted":false,"lastModified":"1","parent":"","pinned":false,"type":"{doc_type}","visibleName":"{name}"}}"#
        ))
        .unwrap()
    }

    fn local(uuid: &str, hash: &str, doc_type: &str) -> LocalDocumentSnapshot {
        LocalDocumentSnapshot {
            uuid: uuid.into(),
            metadata: meta(uuid, doc_type),
            content: None,
            content_hash: hash.into(),
            total_size_bytes: 0,
            mtime: 10,
            page_count: 0,
            has_pdf: false,
            file_list: vec![],
        }
    }
    fn remote(uuid: &str, hash: &str, doc_type: &str) -> RemoteDocumentSnapshot {
        RemoteDocumentSnapshot {
            uuid: uuid.into(),
            metadata: meta(uuid, doc_type),
            content: None,
            content_hash: hash.into(),
            total_size_bytes: 0,
            mtime: 20,
            page_count: 0,
            has_pdf: false,
            file_list: vec![],
        }
    }
    fn synced(uuid: &str, hash: &str, doc_type: &str) -> SyncFileState {
        SyncFileState {
            uuid: uuid.into(),
            visible_name: uuid.into(),
            parent_uuid: String::new(),
            doc_type: doc_type.into(),
            local_hash: Some(hash.into()),
            remote_hash: Some(hash.into()),
            synced_hash: Some(hash.into()),
            local_mtime: None,
            remote_mtime: None,
            synced_mtime: None,
            last_sync_at: None,
            sync_status: SyncStatus::Synced,
            conflict_info: None,
        }
    }

    fn plan_with(
        local_docs: Vec<LocalDocumentSnapshot>,
        remote_docs: Vec<RemoteDocumentSnapshot>,
        synced_states: Vec<SyncFileState>,
    ) -> SyncPlan {
        let local = LocalManifest {
            documents: local_docs,
            scanned_at: 0,
            total_documents: 0,
            total_size_bytes: 0,
            sync_dir: PathBuf::from("/tmp"),
        };
        let remote = RemoteManifest {
            documents: remote_docs,
            scanned_at: 0,
            total_documents: 0,
            total_size_bytes: 0,
        };
        compute_sync_plan_from_parts(&local, &remote, &synced_states)
    }

    fn action_for<'a>(plan: &'a SyncPlan, uuid: &str) -> &'a SyncAction {
        plan.actions.iter().find(|a| a.uuid == uuid).unwrap()
    }

    #[test]
    fn all_in_sync_is_skip() {
        let p = plan_with(
            vec![local("a", "h", "DocumentType")],
            vec![remote("a", "h", "DocumentType")],
            vec![synced("a", "h", "DocumentType")],
        );
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Skip));
        assert!(p.is_empty());
    }

    #[test]
    fn new_remote_document_is_pull() {
        let p = plan_with(vec![], vec![remote("a", "h", "DocumentType")], vec![]);
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Pull));
        assert_eq!(p.total_pull, 1);
    }

    #[test]
    fn new_local_document_is_push() {
        let p = plan_with(vec![local("a", "h", "DocumentType")], vec![], vec![]);
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Push));
        assert_eq!(p.total_push, 1);
    }

    #[test]
    fn modified_remotely_is_pull() {
        let p = plan_with(
            vec![local("a", "h1", "DocumentType")],
            vec![remote("a", "h2", "DocumentType")],
            vec![synced("a", "h1", "DocumentType")],
        );
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Pull));
    }

    #[test]
    fn modified_locally_is_push() {
        let p = plan_with(
            vec![local("a", "h2", "DocumentType")],
            vec![remote("a", "h1", "DocumentType")],
            vec![synced("a", "h1", "DocumentType")],
        );
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Push));
    }

    #[test]
    fn true_conflict_emits_conflict_with_mtimes() {
        let p = plan_with(
            vec![local("a", "hL", "DocumentType")],
            vec![remote("a", "hR", "DocumentType")],
            vec![synced("a", "h0", "DocumentType")],
        );
        match &action_for(&p, "a").action_type {
            SyncActionType::Conflict { local_mtime, remote_mtime } => {
                assert_eq!(*local_mtime, 10);
                assert_eq!(*remote_mtime, 20);
            }
            _ => panic!("expected Conflict"),
        }
        assert_eq!(p.total_conflict, 1);
    }

    #[test]
    fn false_conflict_is_skip() {
        let p = plan_with(
            vec![local("a", "hSame", "DocumentType")],
            vec![remote("a", "hSame", "DocumentType")],
            vec![synced("a", "h0", "DocumentType")],
        );
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Skip));
    }

    #[test]
    fn deleted_remotely_unmodified_locally_is_delete_local() {
        let p = plan_with(
            vec![local("a", "h1", "DocumentType")],
            vec![],
            vec![synced("a", "h1", "DocumentType")],
        );
        assert!(matches!(
            action_for(&p, "a").action_type,
            SyncActionType::DeleteLocal
        ));
    }

    #[test]
    fn deleted_remotely_modified_locally_is_push() {
        let p = plan_with(
            vec![local("a", "h2", "DocumentType")],
            vec![],
            vec![synced("a", "h1", "DocumentType")],
        );
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Push));
    }

    #[test]
    fn deleted_locally_unmodified_remotely_is_delete_remote() {
        let p = plan_with(
            vec![],
            vec![remote("a", "h1", "DocumentType")],
            vec![synced("a", "h1", "DocumentType")],
        );
        assert!(matches!(
            action_for(&p, "a").action_type,
            SyncActionType::DeleteRemote
        ));
    }

    #[test]
    fn deleted_locally_modified_remotely_is_pull() {
        let p = plan_with(
            vec![],
            vec![remote("a", "h2", "DocumentType")],
            vec![synced("a", "h1", "DocumentType")],
        );
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Pull));
    }

    #[test]
    fn deleted_both_sides_is_delete_both() {
        let p = plan_with(vec![], vec![], vec![synced("a", "h1", "DocumentType")]);
        assert!(matches!(
            action_for(&p, "a").action_type,
            SyncActionType::DeleteBoth
        ));
    }

    #[test]
    fn both_new_identical_is_skip_both_new_different_is_conflict() {
        let p_same = plan_with(
            vec![local("a", "h", "DocumentType")],
            vec![remote("a", "h", "DocumentType")],
            vec![],
        );
        assert!(matches!(action_for(&p_same, "a").action_type, SyncActionType::Skip));

        let p_diff = plan_with(
            vec![local("a", "hL", "DocumentType")],
            vec![remote("a", "hR", "DocumentType")],
            vec![],
        );
        assert!(matches!(
            action_for(&p_diff, "a").action_type,
            SyncActionType::Conflict { .. }
        ));
    }

    #[test]
    fn folders_sort_before_documents() {
        let p = plan_with(
            vec![],
            vec![
                remote("doc", "h", "DocumentType"),
                remote("fld", "h", "CollectionType"),
            ],
            vec![],
        );
        let first = &p.actions[0];
        assert_eq!(first.uuid, "fld");
        assert_eq!(first.priority, 0);
    }

    #[test]
    fn summary_counts_and_helpers() {
        let p = plan_with(
            vec![local("a", "hL", "DocumentType")],
            vec![remote("a", "hR", "DocumentType"), remote("b", "h", "DocumentType")],
            vec![synced("a", "h0", "DocumentType")],
        );
        assert_eq!(p.total_conflict, 1);
        assert_eq!(p.total_pull, 1);
        assert!(p.has_conflicts());
        assert_eq!(p.conflicts().len(), 1);
        assert!(!p.is_empty());
        let s = p.summary();
        assert!(s.contains("Pull 1"));
        assert!(s.contains("Conflicts 1"));
    }

    #[test]
    fn pure_none_combination_is_skip() {
        let p = plan_with(vec![], vec![], vec![]);
        assert_eq!(p.actions.len(), 0);
        assert!(p.is_empty());
    }

    // --- conflict resolution (spec 14) ---

    fn action_conflict(uuid: &str, lm: u64, rm: u64) -> SyncAction {
        SyncAction {
            uuid: uuid.into(),
            visible_name: uuid.into(),
            action_type: SyncActionType::Conflict {
                local_mtime: lm,
                remote_mtime: rm,
            },
            priority: 1,
        }
    }

    fn local_with_mtime(uuid: &str, hash: &str, mtime: u64) -> LocalDocumentSnapshot {
        let mut d = local(uuid, hash, "DocumentType");
        d.mtime = mtime;
        d
    }
    fn remote_with_mtime(uuid: &str, hash: &str, mtime: u64) -> RemoteDocumentSnapshot {
        let mut d = remote(uuid, hash, "DocumentType");
        d.mtime = mtime;
        d
    }

    #[test]
    fn conflict_remote_wins_when_newer() {
        let a = action_conflict("a", 100, 200);
        let l = local_with_mtime("a", "hL", 100);
        let r = remote_with_mtime("a", "hR", 200);
        assert_eq!(resolve_conflict(&a, &l, &r, 0).winner, ConflictWinner::Remote);
    }

    #[test]
    fn conflict_local_wins_when_newer() {
        let a = action_conflict("a", 300, 200);
        let l = local_with_mtime("a", "hL", 300);
        let r = remote_with_mtime("a", "hR", 200);
        assert_eq!(resolve_conflict(&a, &l, &r, 0).winner, ConflictWinner::Local);
    }

    #[test]
    fn conflict_identical_timestamps_favour_device() {
        assert_eq!(pick_winner(100, 100, 0), ConflictWinner::Remote);
    }

    #[test]
    fn conflict_within_tolerance_is_a_tie() {
        assert_eq!(pick_winner(102, 100, 0), ConflictWinner::Remote);
        assert_eq!(pick_winner(103, 100, 0), ConflictWinner::Local);
    }

    #[test]
    fn fast_device_clock_does_not_beat_a_genuinely_newer_desktop_edit() {
        // Device clock runs 1h fast. Its edit stamped 1000+3600 really happened
        // at desktop-time 1000; the desktop edit at 1500 is newer.
        assert_eq!(pick_winner(1500, 4600, 3600), ConflictWinner::Local);
        // Without compensation the device would wrongly win.
        assert_eq!(pick_winner(1500, 4600, 0), ConflictWinner::Remote);
    }

    #[test]
    fn slow_device_clock_does_not_lose_a_genuinely_newer_device_edit() {
        // Device clock runs 1h slow. Its edit at stamp 5000-3600=1400 really
        // happened at desktop-time 5000, after the desktop edit at 2000.
        assert_eq!(pick_winner(2000, 1400, -3600), ConflictWinner::Remote);
        assert_eq!(pick_winner(2000, 1400, 0), ConflictWinner::Local);
    }

    #[test]
    fn conflict_backup_lives_under_the_uuid_and_names_the_loser() {
        let a = action_conflict("a", 1, 2);
        let l = local_with_mtime("a", "hL", 1);
        let r = remote_with_mtime("a", "hR", 2);
        let res = resolve_conflict(&a, &l, &r, 0);
        assert!(res.backup_subdir.starts_with("a/"));
        assert!(res.backup_subdir.contains("-desktop-"));
        assert!(!res.backup_subdir.contains("conflict"));
    }

    #[test]
    fn conflict_notification_has_human_delta() {
        let a = action_conflict("a", 100, 100 + 7200);
        let l = local_with_mtime("a", "hL", 100);
        let r = remote_with_mtime("a", "hR", 100 + 7200);
        let n = resolve_conflict(&a, &l, &r, 0).to_notification();
        assert_eq!(n.winner_source, "reMarkable");
        assert!(n.time_difference_human.contains("hour"));
    }

    #[test]
    fn conflict_notification_delta_is_skew_adjusted() {
        // Raw stamps differ by 2h, but 1h of that is clock offset.
        let a = action_conflict("a", 100, 100 + 7200);
        let l = local_with_mtime("a", "hL", 100);
        let r = remote_with_mtime("a", "hR", 100 + 7200);
        let n = resolve_conflict(&a, &l, &r, 3600).to_notification();
        assert_eq!(n.time_difference_human, "reMarkable was 1 hour newer");
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        // 2025-01-01 is day 20089 since 1970-01-01 (Gregorian).
        assert_eq!(civil_from_days(20089), (2025, 1, 1));
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn compute_sync_plan_integrates_with_state_db() {
        let db = StateDb::open_in_memory().unwrap();
        db.upsert_state(&synced("a", "h1", "DocumentType")).unwrap();
        let local = LocalManifest {
            documents: vec![local("a", "h1", "DocumentType")],
            scanned_at: 0,
            total_documents: 0,
            total_size_bytes: 0,
            sync_dir: PathBuf::from("/tmp"),
        };
        let rem = RemoteManifest {
            documents: vec![remote("a", "h2", "DocumentType")],
            scanned_at: 0,
            total_documents: 0,
            total_size_bytes: 0,
        };
        let plan = compute_sync_plan(&local, &rem, &db).unwrap();
        assert_eq!(plan.total_pull, 1);
    }

    // --- matching existing local documents by UUID (MCC-101) ---

    fn manifests(
        l: Vec<LocalDocumentSnapshot>,
        r: Vec<RemoteDocumentSnapshot>,
    ) -> (LocalManifest, RemoteManifest) {
        (
            LocalManifest {
                documents: l,
                scanned_at: 0,
                total_documents: 0,
                total_size_bytes: 0,
                sync_dir: PathBuf::from("/tmp"),
            },
            RemoteManifest {
                documents: r,
                scanned_at: 0,
                total_documents: 0,
                total_size_bytes: 0,
            },
        )
    }

    #[test]
    fn first_sync_of_existing_document_skips_and_records_baseline() {
        let (l, r) = manifests(
            vec![local("a", "h1", "DocumentType")],
            vec![remote("a", "h1", "DocumentType")],
        );
        let plan = compute_sync_plan_from_parts(&l, &r, &[]);
        assert_eq!(plan.total_skip, 1);
        assert_eq!(plan.total_conflict + plan.total_pull + plan.total_push, 0);

        let baselines = matched_baselines(&l, &r, &[]);
        assert_eq!(baselines.len(), 1);
        assert_eq!(baselines[0].uuid, "a");
        assert_eq!(baselines[0].synced_hash.as_deref(), Some("h1"));
    }

    #[test]
    fn unchanged_resync_creates_nothing() {
        let (l, r) = manifests(
            vec![local("a", "h1", "DocumentType")],
            vec![remote("a", "h1", "DocumentType")],
        );
        let states = [synced("a", "h1", "DocumentType")];
        let plan = compute_sync_plan_from_parts(&l, &r, &states);
        assert_eq!(plan.total_skip, 1);
        assert_eq!(plan.total_conflict + plan.total_pull + plan.total_push, 0);
        assert!(matched_baselines(&l, &r, &states).is_empty());
    }

    #[test]
    fn device_edit_after_baseline_pulls_in_place() {
        // First sync adopted the document; the device then changed it.
        let (l, r) = manifests(
            vec![local("a", "h1", "DocumentType")],
            vec![remote("a", "h2", "DocumentType")],
        );
        let states = [synced("a", "h1", "DocumentType")];
        let plan = compute_sync_plan_from_parts(&l, &r, &states);
        assert_eq!(plan.total_pull, 1);
        assert_eq!(plan.total_conflict, 0);
        assert!(matched_baselines(&l, &r, &states).is_empty());
    }

    #[test]
    fn device_edit_right_after_adoption_is_not_a_conflict() {
        let (l, r) = manifests(
            vec![local("a", "h1", "DocumentType")],
            vec![remote("a", "h1", "DocumentType")],
        );
        let adopted = matched_baselines(&l, &r, &[]);
        let (l2, r2) = manifests(
            vec![local("a", "h1", "DocumentType")],
            vec![remote("a", "h2", "DocumentType")],
        );
        let plan = compute_sync_plan_from_parts(&l2, &r2, &adopted);
        assert_eq!(plan.total_pull, 1);
        assert_eq!(plan.total_conflict, 0);
    }

    #[test]
    fn stale_baseline_is_repaired_when_sides_agree() {
        let (l, r) = manifests(
            vec![local("a", "new", "DocumentType")],
            vec![remote("a", "new", "DocumentType")],
        );
        let fixed = matched_baselines(&l, &r, &[synced("a", "old", "DocumentType")]);
        assert_eq!(fixed.len(), 1);
        assert_eq!(fixed[0].synced_hash.as_deref(), Some("new"));
    }

    #[test]
    fn renamed_on_device_keeps_the_same_uuid_entry() {
        let (l, mut r) = manifests(
            vec![local("a", "h1", "DocumentType")],
            vec![remote("a", "h1", "DocumentType")],
        );
        r.documents[0].metadata = meta("Renamed", "DocumentType");
        let states = [synced("a", "h1", "DocumentType")];
        // Same UUID, same content: one skip, no second document.
        let plan = compute_sync_plan_from_parts(&l, &r, &states);
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.total_skip, 1);
    }

    // ---- push fixes (MCC-110) ----

    #[test]
    fn local_only_conflict_duplicates_are_never_pushed() {
        let mut l = local("dup", "h", "DocumentType");
        l.metadata.visible_name = "Home Designs (conflict 2026-10-02)".into();
        let plan = plan_with(vec![l], vec![], vec![]);
        assert_eq!(action_for(&plan, "dup").action_type, SyncActionType::Skip);
        assert_eq!(plan.total_push, 0);
    }

    #[test]
    fn ordinary_local_only_documents_still_push_even_with_parentheses() {
        let mut l = local("new", "h", "DocumentType");
        l.metadata.visible_name = "Notes (draft)".into();
        let plan = plan_with(vec![l], vec![], vec![]);
        assert_eq!(action_for(&plan, "new").action_type, SyncActionType::Push);
    }

    #[test]
    fn warnings_appear_in_the_summary() {
        let r = SyncReport { warnings: vec!["Could not measure the clock".into()], ..Default::default() };
        assert!(r.summary().contains("Could not measure the clock"));
    }
}
