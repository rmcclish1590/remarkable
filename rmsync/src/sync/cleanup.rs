//! One-off cleanup of the "<name> (conflict YYYY-MM-DD)" documents that
//! earlier versions of the sync created whenever a file changed on both sides.
//!
//! The flow is plan, confirm, apply: `plan_cleanup` only reads, the UI shows
//! its findings, and `apply_cleanup` acts on the ones the user confirmed.
//! Nothing is deleted. A duplicate is *moved* to
//! `.rmsync/backups/cleanup-<date>-<time>/<uuid>/`, and when the duplicate is
//! the newer version the original's old content is copied there first. The
//! next sync then removes the duplicate from the device too, because its state
//! baseline is updated to match what was moved (see `apply_cleanup`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use filetime::{set_file_mtime, FileTime};
use sha2::{Digest, Sha256};

use crate::remarkable::metadata::RemarkableMetadata;
use crate::sync::engine::civil_from_days;
use crate::sync::scanner::RAW_SUBDIR;
use crate::sync::state_db::{StateDb, SyncStatus};
use crate::sync::transfer::{backup_root, is_safe_uuid, local_files_for_uuid, scan_local_hash_for_uuid};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// Same content as the original: just remove the duplicate.
    RemoveIdentical,
    /// Differs, and the original is at least as recent: keep it, back up the duplicate.
    KeepOriginal,
    /// Differs, and the duplicate is more recent: its content replaces the
    /// original's (which is backed up first), then the duplicate is removed.
    KeepDuplicate,
}

#[derive(Debug, Clone)]
pub struct DuplicateFinding {
    pub duplicate_uuid: String,
    pub duplicate_name: String,
    pub original_uuid: String,
    pub original_name: String,
    pub outcome: CleanupOutcome,
}

impl DuplicateFinding {
    /// One line for the confirmation list.
    pub fn describe(&self) -> String {
        match self.outcome {
            CleanupOutcome::RemoveIdentical => {
                format!("“{}” — identical to “{}”; remove", self.duplicate_name, self.original_name)
            }
            CleanupOutcome::KeepOriginal => format!(
                "“{}” — differs; “{}” is newer and is kept, the duplicate is backed up",
                self.duplicate_name, self.original_name
            ),
            CleanupOutcome::KeepDuplicate => format!(
                "“{}” — differs and is newer; its content replaces “{}”, the old version is backed up",
                self.duplicate_name, self.original_name
            ),
        }
    }
}

#[derive(Debug, Default)]
pub struct CleanupReport {
    pub removed: usize,
    pub originals_replaced: usize,
    pub skipped: usize,
    pub backup_dir: Option<PathBuf>,
    pub errors: Vec<String>,
}

impl CleanupReport {
    pub fn summary(&self) -> String {
        let mut s = format!("Removed {} duplicate(s)", self.removed);
        if self.originals_replaced > 0 {
            s.push_str(&format!(", updated {} original(s) with newer content", self.originals_replaced));
        }
        if self.skipped > 0 {
            s.push_str(&format!(", skipped {}", self.skipped));
        }
        if let Some(dir) = &self.backup_dir {
            s.push_str(&format!(". Backups: {}", dir.display()));
        }
        s
    }
}

/// If `name` is "<base> (conflict YYYY-MM-DD)", return `<base>`.
pub fn strip_conflict_suffix(name: &str) -> Option<&str> {
    let inner = name.strip_suffix(')')?;
    let (base, date) = inner.rsplit_once(" (conflict ")?;
    let b = date.as_bytes();
    let well_formed = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    (well_formed && !base.is_empty()).then_some(base)
}

struct Item {
    uuid: String,
    meta: RemarkableMetadata,
}

fn read_items(raw: &Path) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    for entry in std::fs::read_dir(raw).with_context(|| format!("reading {}", raw.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(uuid) = name.strip_suffix(".metadata") else { continue };
        if !is_safe_uuid(uuid) {
            continue;
        }
        match RemarkableMetadata::from_file(&entry.path()) {
            Ok(meta) => items.push(Item { uuid: uuid.to_string(), meta }),
            Err(e) => tracing::warn!(uuid, error = format!("{e:#}"), "skipping unreadable metadata"),
        }
    }
    Ok(items)
}

/// Content files of a document, keyed by name relative to the uuid (".content",
/// "/page.rm"). The `.metadata` file is left out: it holds the name, which
/// differs between a duplicate and its original by design.
fn content_files(raw: &Path, uuid: &str) -> Result<Vec<(PathBuf, String)>> {
    let mut files: Vec<(PathBuf, String)> = local_files_for_uuid(raw, uuid)?
        .into_iter()
        .filter_map(|(path, rel)| {
            let key = rel.strip_prefix(uuid)?.to_string();
            (key != ".metadata").then_some((path, key))
        })
        .collect();
    files.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(files)
}

fn content_fingerprint(files: &[(PathBuf, String)]) -> Result<String> {
    let mut h = Sha256::new();
    for (path, key) in files {
        h.update(key.as_bytes());
        h.update([0u8]);
        h.update(std::fs::read(path).with_context(|| format!("reading {}", path.display()))?);
        h.update([0u8]);
    }
    Ok(format!("{:x}", h.finalize()))
}

fn newest_mtime(files: &[(PathBuf, String)]) -> u64 {
    files
        .iter()
        .filter_map(|(p, _)| std::fs::metadata(p).ok()?.modified().ok())
        .filter_map(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .max()
        .unwrap_or(0)
}

/// When this version of the document was last modified, in seconds.
///
/// Prefers the `lastModified` in the metadata over file mtimes: the old sync
/// made its backups with a plain copy, which stamps the files with the time of
/// the conflict, so a stale desktop copy would look newer than the device
/// original. `lastModified` is carried across unchanged.
fn modified_secs(raw: &Path, uuid: &str, files: &[(PathBuf, String)]) -> Option<u64> {
    RemarkableMetadata::from_file(&raw.join(format!("{uuid}.metadata")))
        .ok()
        .and_then(|m| m.last_modified_ms().ok())
        .filter(|ms| *ms > 0)
        .map(|ms| ms / 1000)
        .or_else(|| Some(newest_mtime(files)).filter(|t| *t > 0))
}

fn assess(raw: &Path, duplicate_uuid: &str, original_uuid: &str) -> Result<CleanupOutcome> {
    let dup = content_files(raw, duplicate_uuid)?;
    let orig = content_files(raw, original_uuid)?;
    if content_fingerprint(&dup)? == content_fingerprint(&orig)? {
        return Ok(CleanupOutcome::RemoveIdentical);
    }
    // A tie (or no usable time) keeps the original: it is the one the device
    // and the UI know by name.
    let dup_time = modified_secs(raw, duplicate_uuid, &dup);
    let orig_time = modified_secs(raw, original_uuid, &orig);
    Ok(match (dup_time, orig_time) {
        (Some(d), Some(o)) if d > o => CleanupOutcome::KeepDuplicate,
        _ => CleanupOutcome::KeepOriginal,
    })
}

/// Find conflict duplicates under `<sync_dir>/raw` and decide what to do with
/// each. Read-only. A duplicate is only reported when exactly one live document
/// with the base name exists in the same folder — if the original is missing or
/// ambiguous we leave it alone rather than guess.
pub fn plan_cleanup(sync_dir: &Path) -> Result<Vec<DuplicateFinding>> {
    let raw = sync_dir.join(RAW_SUBDIR);
    if !raw.is_dir() {
        return Ok(Vec::new());
    }
    let items: Vec<Item> = read_items(&raw)?
        .into_iter()
        .filter(|i| i.meta.is_document() && !i.meta.is_deleted())
        .collect();

    let mut by_name: HashMap<(&str, &str), Vec<&Item>> = HashMap::new();
    for i in &items {
        if strip_conflict_suffix(&i.meta.visible_name).is_none() {
            by_name.entry((i.meta.visible_name.as_str(), i.meta.parent.as_str())).or_default().push(i);
        }
    }

    let mut findings = Vec::new();
    for dup in &items {
        let Some(base) = strip_conflict_suffix(&dup.meta.visible_name) else { continue };
        let originals = by_name.get(&(base, dup.meta.parent.as_str())).map(Vec::as_slice).unwrap_or(&[]);
        let [orig] = originals else {
            tracing::info!(
                uuid = %dup.uuid,
                name = %dup.meta.visible_name,
                candidates = originals.len(),
                "conflict duplicate has no single original; leaving it alone"
            );
            continue;
        };
        let outcome = assess(&raw, &dup.uuid, &orig.uuid)?;
        findings.push(DuplicateFinding {
            duplicate_uuid: dup.uuid.clone(),
            duplicate_name: dup.meta.visible_name.clone(),
            original_uuid: orig.uuid.clone(),
            original_name: orig.meta.visible_name.clone(),
            outcome,
        });
    }
    findings.sort_by(|a, b| (&a.original_name, &a.duplicate_name).cmp(&(&b.original_name, &b.duplicate_name)));
    tracing::info!(duplicates = findings.len(), "conflict duplicate scan complete");
    Ok(findings)
}

/// Apply the findings the user confirmed.
///
/// Each finding is re-assessed from disk first, so the run is safe if the
/// library changed since the plan and safe to repeat (a duplicate that is
/// already gone is skipped). The one thing a stale confirmation must never do
/// is overwrite an original it was not confirmed to overwrite.
pub fn apply_cleanup(
    sync_dir: &Path,
    db: &StateDb,
    confirmed: &[DuplicateFinding],
    now: u64,
) -> CleanupReport {
    let mut report = CleanupReport::default();
    let (y, m, d) = civil_from_days((now / 86400) as i64);
    let run_dir = backup_root(sync_dir).join(format!("cleanup-{y:04}-{m:02}-{d:02}-{now}"));
    for f in confirmed {
        match apply_one(sync_dir, db, f, &run_dir) {
            Ok(Some(replaced)) => {
                report.removed += 1;
                report.originals_replaced += usize::from(replaced);
                report.backup_dir = Some(run_dir.clone());
            }
            Ok(None) => report.skipped += 1,
            Err(e) => {
                tracing::error!(
                    duplicate = %f.duplicate_uuid,
                    original = %f.original_uuid,
                    error = format!("{e:#}"),
                    "cleanup of conflict duplicate failed; nothing further was changed for it"
                );
                report.errors.push(format!("{}: {e}", f.duplicate_name));
            }
        }
    }
    tracing::info!(
        removed = report.removed,
        replaced = report.originals_replaced,
        skipped = report.skipped,
        failed = report.errors.len(),
        "conflict duplicate cleanup finished"
    );
    report
}

/// `Ok(Some(original_replaced))` when the duplicate was removed, `Ok(None)`
/// when it was skipped.
fn apply_one(
    sync_dir: &Path,
    db: &StateDb,
    f: &DuplicateFinding,
    run_dir: &Path,
) -> Result<Option<bool>> {
    if !is_safe_uuid(&f.duplicate_uuid) || !is_safe_uuid(&f.original_uuid) {
        anyhow::bail!("unsafe uuid in cleanup finding");
    }
    let raw = sync_dir.join(RAW_SUBDIR);
    if !raw.join(format!("{}.metadata", f.duplicate_uuid)).exists() {
        tracing::debug!(uuid = %f.duplicate_uuid, "duplicate already gone; skipping");
        return Ok(None);
    }
    if !raw.join(format!("{}.metadata", f.original_uuid)).exists() {
        tracing::warn!(uuid = %f.duplicate_uuid, "original no longer exists; leaving duplicate alone");
        return Ok(None);
    }

    let outcome = assess(&raw, &f.duplicate_uuid, &f.original_uuid)?;
    if outcome == CleanupOutcome::KeepDuplicate && f.outcome != CleanupOutcome::KeepDuplicate {
        tracing::warn!(uuid = %f.duplicate_uuid, "duplicate became newer since confirmation; skipping");
        return Ok(None);
    }

    // What the baseline should say about the duplicate once it has left raw/.
    let dup_hash = scan_local_hash_for_uuid(&f.duplicate_uuid, &raw);

    let mut replaced = false;
    if outcome == CleanupOutcome::KeepDuplicate {
        let backup = run_dir.join(&f.original_uuid);
        std::fs::create_dir_all(&backup)?;
        for (path, rel) in local_files_for_uuid(&raw, &f.original_uuid)? {
            let dest = backup.join(&rel);
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(&path, &dest).with_context(|| format!("backing up {}", path.display()))?;
        }
        replace_content(&raw, &f.duplicate_uuid, &f.original_uuid)?;
        replaced = true;
        tracing::info!(
            original = %f.original_uuid,
            name = %f.original_name,
            backup = %backup.display(),
            "replaced original with newer duplicate content; old version backed up"
        );
    }

    move_to_backup(&raw, &f.duplicate_uuid, &run_dir.join(&f.duplicate_uuid))?;
    tracing::info!(
        uuid = %f.duplicate_uuid,
        name = %f.duplicate_name,
        original = %f.original_uuid,
        outcome = ?outcome,
        backup = %run_dir.join(&f.duplicate_uuid).display(),
        "conflict duplicate moved to backup"
    );

    // Tell the diff the duplicate's content was in sync, so the next sync
    // deletes the device copy if it still matches (or, if the device copy was
    // since edited, pulls it back instead of destroying that edit).
    if let Some(mut state) = db.get_state(&f.duplicate_uuid)? {
        state.local_hash = dup_hash.clone();
        state.synced_hash = dup_hash;
        state.sync_status = SyncStatus::Synced;
        db.upsert_state(&state)?;
    }
    Ok(Some(replaced))
}

/// Make `original`'s content files identical to `duplicate`'s, keeping the
/// original's `.metadata` (its name and folder) and the duplicate's mtimes.
fn replace_content(raw: &Path, duplicate: &str, original: &str) -> Result<()> {
    for (path, _) in content_files(raw, original)? {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    for (src, key) in content_files(raw, duplicate)? {
        // key is ".content" or "/page.rm"; the same suffix works for either uuid.
        let dest = raw.join(format!("{original}{key}"));
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(&src, &dest).with_context(|| format!("copying {}", src.display()))?;
        if let Ok(t) = std::fs::metadata(&src).and_then(|m| m.modified()) {
            let _ = set_file_mtime(&dest, FileTime::from_system_time(t));
        }
    }
    Ok(())
}

/// Move every `raw/<uuid>.*` file and the `raw/<uuid>/` directory into `dest`.
fn move_to_backup(raw: &Path, uuid: &str, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(raw)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&format!("{uuid}.")) || name == uuid {
            std::fs::rename(entry.path(), dest.join(&name))
                .with_context(|| format!("moving {} to backup", entry.path().display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_is_recognised_only_in_the_exact_format() {
        assert_eq!(strip_conflict_suffix("Notes (conflict 2026-10-02)"), Some("Notes"));
        assert_eq!(strip_conflict_suffix("A (b) (conflict 2026-10-02)"), Some("A (b)"));
        assert_eq!(strip_conflict_suffix("Notes"), None);
        assert_eq!(strip_conflict_suffix("Notes (conflict soon)"), None);
        assert_eq!(strip_conflict_suffix("Notes (conflict 2026-1-02)"), None);
        assert_eq!(strip_conflict_suffix(" (conflict 2026-10-02)"), None);
        assert_eq!(strip_conflict_suffix("Notes (conflict 2026-10-02) final"), None);
    }
}
