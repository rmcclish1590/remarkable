# Spec 22 — Wire Sync Engine to UI

**Layer:** 6 — Integration  
**Dependencies:** 11-14 (sync core), 15-19 (UI shell), 16 (folder browser)  
**Estimated effort:** 2–3 hours  

## Objective

Connect the sync engine to the GTK4 UI so that clicking "Sync Now" triggers the full sync pipeline: scan remote, scan local, compute diff, execute plan, update UI — all with real-time progress and non-blocking operation.

## Context

All the pieces exist independently: the sync engine can compute plans and execute transfers, the UI can display progress and status. This spec wires them together with proper threading (sync runs on Tokio, UI updates on GTK main thread).

## Technical Requirements

### 1. Sync orchestrator (`src/sync/engine.rs` — extend)

```rust
/// The top-level sync coordinator.
pub struct SyncOrchestrator {
    config: AppConfig,
    db: StateDb,
    connection: DeviceConnection,
}

impl SyncOrchestrator {
    pub fn new(config: AppConfig) -> Result<Self>

    /// Run the complete sync pipeline.
    /// Returns a channel receiver for progress events.
    pub async fn run_sync(&mut self) -> Result<SyncReport>
    
    /// Run sync with a progress callback (called from Tokio context).
    pub async fn run_sync_with_progress<F>(&mut self, callback: F) -> Result<SyncReport>
    where
        F: Fn(SyncProgressEvent) + Send + 'static,
}

#[derive(Debug, Clone)]
pub enum SyncProgressEvent {
    Phase(SyncPhase),
    ScanProgress(ScanProgress),
    TransferProgress(TransferProgress),
    ConflictResolved(ConflictNotification),
    Error(String),
    Complete(SyncReport),
}

#[derive(Debug, Clone)]
pub enum SyncPhase {
    Connecting,
    ScanningRemote,
    ScanningLocal,
    ComputingDiff,
    Pulling,
    Pushing,
    ResolvingConflicts,
    Finalizing,
}

#[derive(Debug, Clone)]
pub struct SyncReport {
    pub pulled: usize,
    pub pushed: usize,
    pub deleted: usize,
    pub conflicts_resolved: usize,
    pub errors: Vec<String>,
    pub duration_ms: u64,
}
```

### 2. Full sync pipeline

`run_sync` executes this sequence:

```rust
async fn run_sync_with_progress(&mut self, cb: F) -> Result<SyncReport> {
    let start = Instant::now();
    
    // 1. Connect
    cb(SyncProgressEvent::Phase(SyncPhase::Connecting));
    self.connection.connect().await?;
    
    // 2. Scan remote
    cb(SyncProgressEvent::Phase(SyncPhase::ScanningRemote));
    let remote = scan_remote_with_progress(&self.connection, |p| {
        cb(SyncProgressEvent::ScanProgress(p));
    }).await?;
    
    // 3. Scan local
    cb(SyncProgressEvent::Phase(SyncPhase::ScanningLocal));
    let local = scan_local(&self.config.sync.sync_dir)?;
    
    // 4. Compute diff
    cb(SyncProgressEvent::Phase(SyncPhase::ComputingDiff));
    let plan = compute_sync_plan(&local, &remote, &self.db)?;
    
    // 5. Handle conflicts
    if plan.has_conflicts() {
        cb(SyncProgressEvent::Phase(SyncPhase::ResolvingConflicts));
        let resolutions = resolve_all_conflicts(
            &self.connection, &plan, &local, &remote,
            &self.config.sync.sync_dir, &self.db
        ).await?;
        for r in &resolutions {
            cb(SyncProgressEvent::ConflictResolved(r.to_notification()));
        }
    }
    
    // 6. Execute pulls
    if plan.total_pull > 0 {
        cb(SyncProgressEvent::Phase(SyncPhase::Pulling));
        pull_batch(&self.connection, &plan, &self.config.sync.sync_dir, &self.db, |p| {
            cb(SyncProgressEvent::TransferProgress(p));
        }).await?;
    }
    
    // 7. Execute pushes
    if plan.total_push > 0 {
        cb(SyncProgressEvent::Phase(SyncPhase::Pushing));
        push_batch(&self.connection, &plan, &self.config.sync.sync_dir, &self.db, |p| {
            cb(SyncProgressEvent::TransferProgress(p));
        }).await?;
    }
    
    // 8. Execute deletes (with confirmation if config says so)
    // ... handle DeleteLocal and DeleteRemote actions ...
    
    // 9. Reload xochitl if any pushes/deletes were sent
    if plan.total_push > 0 || plan.total_delete > 0 {
        reload_xochitl(&self.connection).await.ok(); // Non-fatal
    }
    
    // 10. Disconnect
    cb(SyncProgressEvent::Phase(SyncPhase::Finalizing));
    self.connection.disconnect().await;
    
    let report = SyncReport { /* ... */ };
    cb(SyncProgressEvent::Complete(report.clone()));
    Ok(report)
}
```

### 3. UI integration (`src/app.rs` — extend)

Wire the sync button to spawn a Tokio task:

```rust
fn wire_sync_button(
    sync_controls: &SyncControls,
    folder_browser: &FolderBrowser,
    config: Arc<Mutex<AppConfig>>,
) {
    sync_controls.connect_sync_clicked(move || {
        let config = config.lock().unwrap().clone();
        let (sender, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
        
        // Spawn sync on Tokio runtime
        tokio::spawn(async move {
            let mut orchestrator = SyncOrchestrator::new(config).unwrap();
            orchestrator.run_sync_with_progress(move |event| {
                sender.send(event).ok();
            }).await
        });
        
        // Handle events on GTK main thread
        receiver.attach(None, move |event| {
            match event {
                SyncProgressEvent::Phase(phase) => {
                    // Update status label with phase description
                }
                SyncProgressEvent::TransferProgress(p) => {
                    sync_controls.update_progress(&p);
                }
                SyncProgressEvent::Complete(report) => {
                    sync_controls.finish_sync(&report.summary());
                    // Reload the folder browser with updated local files
                    let tree = DocumentTree::build_from_directory(&sync_dir.join("raw")).unwrap();
                    folder_browser.load_tree(&tree);
                }
                SyncProgressEvent::Error(msg) => {
                    sync_controls.show_error(&msg);
                }
                _ => {}
            }
            glib::ControlFlow::Continue
        });
    });
}
```

### 4. Post-sync refresh

After sync completes:
1. Rebuild the `DocumentTree` from the local `raw/` directory.
2. Reload the folder browser tree.
3. If a document was open in the viewer and it was modified by sync, refresh the viewer.
4. Invalidate the SVG cache for any modified documents.
5. Update the "Last sync" timestamp in the toolbar.
6. Show conflict notifications as `adw::Toast` messages.

### 5. Cancel support

Implement a cancellation token:

```rust
use tokio_util::sync::CancellationToken;

let cancel_token = CancellationToken::new();
// Pass to orchestrator
// On cancel button click: cancel_token.cancel()
// In sync pipeline: check token.is_cancelled() between phases
```

## Files to Create/Modify

- `src/sync/engine.rs` — add `SyncOrchestrator`
- `src/app.rs` — wire sync button, folder browser refresh
- Add `tokio-util = "0.7"` to Cargo.toml for `CancellationToken`.

## Test Strategy

1. **Phase progression** — mock sync, verify all phases fire in order.
2. **Progress forwarding** — mock transfers, verify progress events reach UI channel.
3. **Post-sync refresh** — verify folder browser reloads after sync.
4. **Cancel** — start sync, cancel after scanning, verify it stops before transfers.
5. **Error handling** — simulate connection failure, verify error reaches UI.
6. **Report accuracy** — verify pulled/pushed/deleted counts match actual operations.

## Acceptance Criteria

1. Clicking "Sync Now" runs the full pipeline end-to-end.
2. Progress bar updates in real-time during transfers.
3. Phase labels update as sync progresses.
4. Document tree refreshes after sync with new/modified documents visible.
5. Cancel stops the sync cleanly between phases.
6. Errors are displayed in the UI, not silently swallowed.
7. Post-sync conflict notifications appear as toasts.
