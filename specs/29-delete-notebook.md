# 29 — Delete a notebook from the desktop (MCC-112)

Layer: UI + Sync Core. Depends on 11 (diff), 15/16 (browser), 22 (sync wiring).

## Behaviour

1. Every non-folder row in the browser has a trash button (`folder_browser.rs`).
   It raises a `DeleteRequest { uuid, name }`; it does not open the document.
2. `ui/delete_dialog.rs` asks the user to type the displayed name. The Delete
   button stays disabled until the text equals the name exactly (case-sensitive,
   no trimming). Cancel / Escape / close never reach the delete flow.
3. `sync/user_delete.rs::request_delete` (by **uuid**, never by name):
   verifies every file to remove belongs to that uuid → copies them to
   `.rmsync/backups/user-deleted/<unix-ts>_<uuid>/` (with `info.json`: uuid,
   name, deleted_at) → records a row in `pending_deletes` → removes `raw/`
   files and `.rmsync/cache/<uuid>_*`. If removal fails, the files are restored
   and the row withdrawn.
4. The app then starts a sync. `apply_pending_deletes` overrides the diff for
   queued uuids: `DeleteRemote` if the device still has the document (even if it
   changed there, so it is never re-pulled), else just clear the record. These
   are exempt from the unattended-delete confirmation (MCC-109) because the
   user already confirmed them by name.
5. The device copy is downloaded to `.rmsync/backups/deleted-<date>-<ts>/<uuid>/`
   before removal, then removed and verified gone. Only then is the pending row
   cleared. If the tablet is unreachable or removal fails, the row stays and the
   next sync retries; the document does not reappear in the list.
6. Removing files from the device counts as a device write, so the single UI
   restart rules of MCC-111 apply; the delete adds no restart of its own.

## Recovery

- **Desktop copy / tablet copy:** both are in `.rmsync/backups/` as above.
- **Restore (manual):** copy the files from the backup directory (everything
  except `info.json`) into `<sync_dir>/raw/`, keeping the layout
  (`<uuid>.metadata`, `<uuid>.content`, `<uuid>/<page>.rm`), then sync. The
  document is local-only, so the sync uploads it to the tablet again.
  `user_delete::restore_deleted` does the same in code and also withdraws a
  still-queued device delete.
- **Retention:** backups of deleted documents (`user-deleted/` and `deleted-*`)
  older than `sync.deleted_backup_retention_days` (default 30, 0 = forever; shown
  in Settings) are pruned at the start of each sync. Conflict and cleanup
  backups are never pruned automatically.

## Logging

User deletes log at INFO with name, uuid, timestamp, result and backup path.
Sync-driven removals log at INFO too (project rule: deletions are INFO), with
per-file detail at DEBUG. Page content is never logged.
