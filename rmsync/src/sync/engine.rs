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
use crate::sync::user_delete::apply_pending_deletes;

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

/// Which side a deletion is applied to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteSide {
    /// Removed from the computer's sync folder.
    Local,
    /// Removed from the reMarkable.
    Remote,
}

impl DeleteSide {
    pub fn label(self) -> &'static str {
        match self {
            DeleteSide::Local => "computer",
            DeleteSide::Remote => "reMarkable",
        }
    }
}

/// A deletion the plan wants to perform, named for the confirmation list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedDelete {
    pub uuid: String,
    pub name: String,
    pub side: DeleteSide,
}

impl PlannedDelete {
    pub fn describe(&self) -> String {
        format!("“{}” — from the {}", self.name, self.side.label())
    }
}

/// When a plan's deletions must be confirmed by the user first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeletePolicy {
    pub max_count: usize,
    pub max_percent: usize,
}

impl Default for DeletePolicy {
    fn default() -> Self {
        Self { max_count: 5, max_percent: 10 }
    }
}

impl DeletePolicy {
    /// True when `deletes` removals out of a `library` of documents is more
    /// than may proceed unattended.
    pub fn requires_confirmation(&self, deletes: usize, library: usize) -> bool {
        deletes > self.max_count || deletes * 100 > self.max_percent * library
    }
}

impl SyncPlan {
    /// Deletions that remove a document from one side. `DeleteBoth` is left out:
    /// neither side still has the document, so only the state row goes.
    pub fn planned_deletes(&self) -> Vec<PlannedDelete> {
        self.actions
            .iter()
            .filter_map(|a| {
                let side = match a.action_type {
                    SyncActionType::DeleteLocal => DeleteSide::Local,
                    SyncActionType::DeleteRemote => DeleteSide::Remote,
                    _ => return None,
                };
                Some(PlannedDelete { uuid: a.uuid.clone(), name: a.visible_name.clone(), side })
            })
            .collect()
    }
}

/// Smallest synced library for which a scan that lost most of it is suspicious.
const SANITY_MIN_BASELINE: usize = 10;

/// Refuse to plan from a scan that looks broken rather than treat every
/// missing document as a deletion.
///
/// An empty scan against a non-empty baseline is always suspect. The device
/// listing is additionally held to "did not lose more than half": nothing the
/// app does removes most of the device's library in one go, whereas the local
/// folder can legitimately shrink a lot (the duplicate cleanup does exactly
/// that) and is covered by the delete confirmation instead.
pub fn check_scan_sanity(local_docs: usize, remote_docs: usize, baseline_docs: usize) -> Option<String> {
    if baseline_docs >= 3 && remote_docs == 0 {
        return Some(format!(
            "The reMarkable returned no documents, but {baseline_docs} were synced before. \
             Nothing was changed; check the device and try again."
        ));
    }
    if baseline_docs >= SANITY_MIN_BASELINE && remote_docs * 2 < baseline_docs {
        return Some(format!(
            "The reMarkable returned only {remote_docs} documents, but {baseline_docs} were synced before. \
             Nothing was changed; the device listing may be incomplete."
        ));
    }
    if baseline_docs >= 3 && local_docs == 0 {
        return Some(format!(
            "The sync folder contains no documents, but {baseline_docs} were synced before. \
             Nothing was changed; check that the sync folder is the right one and is mounted."
        ));
    }
    None
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

    let mut plan = SyncPlan { actions, ..Default::default() };
    plan.finalize();
    plan
}

impl SyncPlan {
    /// Order the actions and recompute the totals. Call after changing `actions`.
    pub fn finalize(&mut self) {
        self.actions.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| action_rank(&a.action_type).cmp(&action_rank(&b.action_type)))
                .then_with(|| a.uuid.cmp(&b.uuid))
        });
        self.total_pull = 0;
        self.total_push = 0;
        self.total_delete = 0;
        self.total_conflict = 0;
        self.total_skip = 0;
        for a in &self.actions {
            match &a.action_type {
                SyncActionType::Pull => self.total_pull += 1,
                SyncActionType::Push => self.total_push += 1,
                SyncActionType::DeleteLocal
                | SyncActionType::DeleteRemote
                | SyncActionType::DeleteBoth => self.total_delete += 1,
                SyncActionType::Conflict { .. } => self.total_conflict += 1,
                SyncActionType::Skip => self.total_skip += 1,
            }
        }
    }
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

/// Whether the tablet's UI should be restarted after this sync.
///
/// Only when something was actually written to the device, and only if the
/// user has not turned it off. A pull-only or unchanged sync never restarts.
/// This reads what was *done*, not what was planned: a push that failed wrote
/// nothing and must not restart the UI (that is what made the tablet appear to
/// reboot on every sync).
pub fn should_restart_ui(device_writes: usize, enabled: bool) -> bool {
    enabled && device_writes > 0
}

/// How recently the tablet must have saved a page for it to count as "being
/// used" and have its UI restart deferred.
pub const DEVICE_BUSY_WINDOW_SECS: u64 = 120;

/// True if any document on the device was modified within the busy window of
/// `device_now`: the user is probably writing, and a UI restart could lose
/// what they have not saved.
pub fn device_recently_edited(remote: &RemoteManifest, device_now: u64, window_secs: u64) -> bool {
    remote
        .documents
        .iter()
        .any(|d| d.mtime <= device_now && device_now - d.mtime <= window_secs)
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
    backup_and_delete_local_document, backup_and_delete_remote_document, backup_root,
    estimate_clock_offset, pull_batch, push_batch, reload_xochitl, resolve_all_conflicts,
    TransferProgress,
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
    RestartingDeviceUi,
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
    /// The plan deletes more than may proceed unattended. Nothing was deleted;
    /// the UI asks, and re-runs the sync with deletions approved if confirmed.
    DeleteConfirmationNeeded(Vec<PlannedDelete>),
    Complete(SyncReport),
}

#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub pulled: usize,
    pub pushed: usize,
    pub deleted: usize,
    /// Of `deleted`, how many were removed from the computer / the reMarkable.
    pub deleted_local: usize,
    pub deleted_remote: usize,
    /// Names of documents the user deleted on purpose that are now gone from the device too.
    pub user_deleted: Vec<String>,
    /// Deletions held back because they need the user's confirmation.
    pub deletes_pending: Vec<PlannedDelete>,
    pub conflicts_resolved: usize,
    /// Files-level changes made on the device this run: pushes, deletions
    /// and conflicts won by the desktop. Drives whether the UI is restarted.
    pub device_writes: usize,
    pub device_ui_restarted: bool,
    pub errors: Vec<String>,
    /// Things that did not stop the sync but the user should know about.
    pub warnings: Vec<String>,
    pub duration_ms: u64,
}

impl SyncReport {
    pub fn summary(&self) -> String {
        let deleted = if self.deleted == 0 {
            "0 deleted".to_string()
        } else {
            format!(
                "{} deleted ({} from computer, {} from reMarkable)",
                self.deleted, self.deleted_local, self.deleted_remote
            )
        };
        let mut s = format!(
            "Sync complete: {} pulled, {} pushed, {deleted}, {} conflicts",
            self.pulled, self.pushed, self.conflicts_resolved
        );
        if !self.user_deleted.is_empty() {
            s.push_str(&format!("; removed from the reMarkable: {}", self.user_deleted.join(", ")));
        }
        for w in &self.warnings {
            s.push_str(&format!("\n{w}"));
        }
        if !self.deletes_pending.is_empty() {
            s.push_str(&format!("; {} deletion(s) awaiting confirmation", self.deletes_pending.len()));
        }
        s
    }
}

pub struct SyncOrchestrator {
    pub config: AppConfig,
    pub db: StateDb,
    pub connection: DeviceConnection,
    pub cancel: CancellationToken,
    deletes_approved: bool,
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
            deletes_approved: false,
        })
    }

    /// Let this run apply deletions without asking again, because the user
    /// has already seen the list and confirmed it.
    pub fn approve_deletes(&mut self) {
        self.deletes_approved = true;
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

        // A scan that came back empty or far smaller than last time more likely
        // failed than reflects a real change; planning from it would turn
        // every "missing" document into a deletion.
        let baseline_docs = match self.db.get_all_states() {
            Ok(states) => states.iter().filter(|s| s.synced_hash.is_some()).count(),
            Err(_) => 0,
        };
        if let Some(msg) = check_scan_sanity(local.total_documents, remote.total_documents, baseline_docs) {
            tracing::error!(
                local = local.total_documents,
                remote = remote.total_documents,
                baseline = baseline_docs,
                "scan result looks incomplete; aborting before any change"
            );
            callback(SyncProgressEvent::Error(msg.clone()));
            report.errors.push(msg);
            return Ok(self.finalise(start, report, &callback).await);
        }

        // 4. Compute diff
        callback(SyncProgressEvent::Phase(SyncPhase::ComputingDiff));
        let mut plan = match compute_sync_plan(&local, &remote, &self.db) {
            Ok(p) => {
                tracing::info!(
                    pull = p.total_pull,
                    push = p.total_push,
                    delete = p.total_delete,
                    conflicts = p.total_conflict,
                    skip = p.total_skip,
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

        // Documents the user deleted on purpose are removed from the device
        // whatever the diff says, and never pulled back.
        let pending = self.db.pending_deletes().unwrap_or_else(|e| {
            tracing::warn!(error = format!("{e:#}"), "could not read queued deletes");
            Vec::new()
        });
        if !pending.is_empty() {
            for p in &pending {
                tracing::info!(uuid = %p.uuid, name = %p.name, "applying queued user delete");
            }
            apply_pending_deletes(&mut plan, &pending, &remote);
        }
        let user_deleted: HashSet<&str> = pending.iter().map(|p| p.uuid.as_str()).collect();

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
        for a in plan.actionable() {
            tracing::debug!(uuid = %a.uuid, name = %a.visible_name, action = ?a.action_type, "planned action");
        }

        // 5. Resolve conflicts
        if plan.has_conflicts() && !self.cancel.is_cancelled() {
            callback(SyncProgressEvent::Phase(SyncPhase::ResolvingConflicts));
            let (resolutions, errors) =
                resolve_all_conflicts(&self.connection, &plan, &local, &remote, &sync_dir, &self.db, clock_offset).await;
            report.conflicts_resolved = resolutions.len();
            report.device_writes += resolutions
                .iter()
                .filter(|r| matches!(r.winner, ConflictWinner::Local))
                .count();
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
                    report.device_writes += report.pushed;
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
        // Deletes the user asked for by name were already confirmed; only the
        // rest count against the unattended limit.
        let planned: Vec<PlannedDelete> = plan
            .planned_deletes()
            .into_iter()
            .filter(|d| !user_deleted.contains(d.uuid.as_str()))
            .collect();
        let policy = DeletePolicy::default();
        let held_back = self.config.sync.confirm_deletes
            && !self.deletes_approved
            && policy.requires_confirmation(planned.len(), plan.actions.len());
        if held_back && !self.cancel.is_cancelled() {
            tracing::warn!(
                count = planned.len(),
                library = plan.actions.len(),
                "deletions exceed the unattended limit; waiting for confirmation"
            );
            for d in &planned {
                tracing::info!(uuid = %d.uuid, name = %d.name, side = d.side.label(), "deletion awaiting confirmation");
            }
            report.deletes_pending = planned.clone();
            callback(SyncProgressEvent::DeleteConfirmationNeeded(planned.clone()));
        }
        if plan.total_delete > 0 && !self.cancel.is_cancelled() {
            callback(SyncProgressEvent::Phase(SyncPhase::Deleting));
            // Deletions are the only destructive step and the hardest to
            // reconstruct after the fact, so each one is logged at INFO
            // with its uuid and side, and the document is copied to
            // `.rmsync/backups/deleted-*` before it is removed.
            let backup_dir = backup_root(&sync_dir)
                .join(format!("deleted-{}-{}", conflict_date_string(now_secs()), now_secs()));
            tracing::info!(
                count = planned.len(),
                backup = %backup_dir.display(),
                "applying deletions"
            );
            for action in plan.actions.iter() {
                let doc_backup = backup_dir.join(&action.uuid);
                let by_user = user_deleted.contains(action.uuid.as_str());
                if held_back && !by_user {
                    continue;
                }
                match &action.action_type {
                    SyncActionType::DeleteLocal => {
                        tracing::info!(uuid = %action.uuid, name = %action.visible_name, side = "local", "deleting document");
                        match backup_and_delete_local_document(&action.uuid, &sync_dir, &doc_backup) {
                            Ok(()) => {
                                let _ = self.db.delete_state(&action.uuid);
                                report.deleted += 1;
                                report.deleted_local += 1;
                            }
                            Err(e) => {
                                tracing::error!(uuid = %action.uuid, error = format!("{e:#}"), "local delete failed");
                                report.errors.push(format!("delete local {}: {e}", action.uuid));
                            }
                        }
                    }
                    SyncActionType::DeleteRemote => {
                        tracing::info!(uuid = %action.uuid, name = %action.visible_name, side = "remote", "deleting document");
                        match backup_and_delete_remote_document(&self.connection, &action.uuid, &sync_dir, &doc_backup).await {
                            Ok(()) => {
                                let _ = self.db.delete_state(&action.uuid);
                                let _ = self.db.clear_pending_delete(&action.uuid);
                                report.deleted += 1;
                                report.deleted_remote += 1;
                                report.device_writes += 1;
                                if by_user {
                                    report.user_deleted.push(action.visible_name.clone());
                                }
                            }
                            Err(e) => {
                                tracing::error!(uuid = %action.uuid, error = format!("{e:#}"), "remote delete failed");
                                // A queued user delete stays queued and is retried next sync.
                                report.errors.push(format!(
                                    "delete “{}” from the reMarkable: {e}{}",
                                    action.visible_name,
                                    if by_user { " (will retry on the next sync)" } else { "" }
                                ));
                            }
                        }
                    }
                    SyncActionType::DeleteBoth => {
                        // Neither side has it any more; only the stale row goes.
                        tracing::info!(uuid = %action.uuid, name = %action.visible_name, "dropping state of document gone from both sides");
                        let _ = self.db.delete_state(&action.uuid);
                        let _ = self.db.clear_pending_delete(&action.uuid);
                        if by_user {
                            report.user_deleted.push(action.visible_name.clone());
                        }
                    }
                    _ => {}
                }
            }
        }

        // 9. Reload xochitl, only if this sync actually changed the device.
        if should_restart_ui(report.device_writes, self.config.sync.restart_device_ui)
            && !self.cancel.is_cancelled()
        {
            let device_now = (now_secs() as i64 + clock_offset).max(0) as u64;
            if device_recently_edited(&remote, device_now, DEVICE_BUSY_WINDOW_SECS) {
                tracing::warn!(
                    writes = report.device_writes,
                    "device was edited moments ago; not restarting its UI"
                );
                report.warnings.push(
                    "The reMarkable was in use, so its screen was not restarted to avoid losing unsaved work. \
                     Synced changes will appear the next time its interface restarts."
                        .to_string(),
                );
            } else {
                callback(SyncProgressEvent::Phase(SyncPhase::RestartingDeviceUi));
                tracing::info!(
                    writes = report.device_writes,
                    "restarting the device UI (xochitl) to show changes"
                );
                match reload_xochitl(&mut self.connection).await {
                    Ok(()) => report.device_ui_restarted = true,
                    Err(e) => tracing::warn!(error = format!("{e:#}"), "restarting the device UI failed"),
                }
            }
        } else {
            tracing::debug!(
                writes = report.device_writes,
                enabled = self.config.sync.restart_device_ui,
                "no device UI restart needed"
            );
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
                deleted_local = report.deleted_local,
                deleted_remote = report.deleted_remote,
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

    // ---- device UI restart (MCC-111) ----

    #[test]
    fn restart_only_when_something_was_written_and_enabled() {
        assert!(!should_restart_ui(0, true), "pull-only or unchanged sync");
        assert!(should_restart_ui(1, true));
        assert!(!should_restart_ui(5, false), "user turned it off");
    }

    fn remote_edited_at(mtime: u64) -> RemoteManifest {
        RemoteManifest {
            documents: vec![remote("a", "h", "DocumentType")]
                .into_iter()
                .map(|mut d| {
                    d.mtime = mtime;
                    d
                })
                .collect(),
            scanned_at: 0,
            total_documents: 1,
            total_size_bytes: 0,
        }
    }

    #[test]
    fn a_device_edited_moments_ago_is_considered_busy() {
        assert!(device_recently_edited(&remote_edited_at(990), 1000, DEVICE_BUSY_WINDOW_SECS));
        assert!(!device_recently_edited(&remote_edited_at(100), 1000, DEVICE_BUSY_WINDOW_SECS));
        assert!(!device_recently_edited(&RemoteManifest {
            documents: vec![], scanned_at: 0, total_documents: 0, total_size_bytes: 0
        }, 1000, DEVICE_BUSY_WINDOW_SECS));
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

    // ---- deletion safeguards (MCC-109) ----

    fn plan_of(actions: Vec<(&str, SyncActionType)>) -> SyncPlan {
        let mut plan = SyncPlan::default();
        for (uuid, t) in actions {
            if matches!(t, SyncActionType::DeleteLocal | SyncActionType::DeleteRemote | SyncActionType::DeleteBoth) {
                plan.total_delete += 1;
            }
            plan.actions.push(SyncAction {
                uuid: uuid.into(),
                visible_name: format!("doc {uuid}"),
                action_type: t,
                priority: 1,
            });
        }
        plan
    }

    #[test]
    fn empty_device_listing_aborts_instead_of_planning_deletions() {
        let msg = check_scan_sanity(79, 0, 79).expect("must abort");
        assert!(msg.contains("no documents"));
    }

    #[test]
    fn device_listing_that_lost_most_of_the_library_aborts() {
        assert!(check_scan_sanity(79, 30, 79).is_some());
        assert!(check_scan_sanity(79, 40, 79).is_none(), "just over half is believable");
    }

    #[test]
    fn empty_sync_folder_aborts_but_a_shrunken_one_is_left_to_confirmation() {
        assert!(check_scan_sanity(0, 79, 79).is_some());
        assert!(check_scan_sanity(17, 89, 89).is_none(), "cleanup legitimately shrinks the folder");
    }

    #[test]
    fn first_sync_and_tiny_libraries_never_trip_the_sanity_check() {
        assert!(check_scan_sanity(0, 79, 0).is_none());
        assert!(check_scan_sanity(0, 0, 0).is_none());
        assert!(check_scan_sanity(1, 1, 2).is_none());
    }

    #[test]
    fn plan_over_the_threshold_requires_confirmation() {
        let policy = DeletePolicy::default();
        assert!(policy.requires_confirmation(72, 89));
        assert!(policy.requires_confirmation(6, 1000), "more than the absolute limit");
        assert!(policy.requires_confirmation(5, 20), "25% of a small library");
    }

    #[test]
    fn normal_small_deletion_proceeds_unattended() {
        let policy = DeletePolicy::default();
        assert!(!policy.requires_confirmation(0, 89));
        assert!(!policy.requires_confirmation(1, 89));
        assert!(!policy.requires_confirmation(5, 100));
    }

    #[test]
    fn planned_deletes_name_the_side_and_skip_delete_both() {
        let plan = plan_of(vec![
            ("a", SyncActionType::DeleteLocal),
            ("b", SyncActionType::DeleteRemote),
            ("c", SyncActionType::DeleteBoth),
            ("d", SyncActionType::Pull),
        ]);
        let d = plan.planned_deletes();
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].uuid.as_str(), d[0].side), ("a", DeleteSide::Local));
        assert_eq!((d[1].uuid.as_str(), d[1].side), ("b", DeleteSide::Remote));
        assert!(d[1].describe().contains("reMarkable"));
    }

    #[test]
    fn report_summary_shows_which_side_deletions_hit() {
        let r = SyncReport { deleted: 3, deleted_local: 1, deleted_remote: 2, ..Default::default() };
        assert!(r.summary().contains("3 deleted (1 from computer, 2 from reMarkable)"));
        let held = SyncReport {
            deletes_pending: vec![PlannedDelete { uuid: "a".into(), name: "A".into(), side: DeleteSide::Remote }],
            ..Default::default()
        };
        assert!(held.summary().contains("1 deletion(s) awaiting confirmation"));
    }
}
