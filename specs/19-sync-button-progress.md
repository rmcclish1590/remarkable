# Spec 19 — Sync Button & Progress Bar

**Layer:** 4 — UI Shell  
**Dependencies:** 15 (main window)  
**Estimated effort:** 1 hour  

## Objective

Implement the sync trigger button and a progress bar that shows real-time sync status, file counts, and transfer progress.

## Context

The user initiates sync by clicking a prominent button in the header bar. During sync, a progress bar appears at the bottom of the window showing which file is being transferred and overall percentage. The button should be disabled when no device is connected, and should change to a "Cancel" action during sync.

## Technical Requirements

### 1. Sync button (`src/ui/sync_controls.rs` — extend)

```rust
pub struct SyncControls {
    pub sync_button: gtk::Button,
    pub progress_bar: gtk::ProgressBar,
    pub progress_box: gtk::Box,        // Container that shows/hides
    pub status_label: gtk::Label,      // "Syncing: 3 of 47 files"
    pub cancel_button: gtk::Button,
    state: Arc<Mutex<SyncUiState>>,
}

#[derive(Debug, PartialEq)]
enum SyncUiState {
    Idle,
    Syncing,
    Cancelling,
}

impl SyncControls {
    pub fn new() -> Self

    /// Enable/disable the sync button based on device connection.
    pub fn set_device_connected(&self, connected: bool)

    /// Transition to syncing state (show progress, disable button).
    pub fn start_sync(&self)

    /// Update progress during sync.
    pub fn update_progress(&self, progress: &TransferProgress)

    /// Transition back to idle state (hide progress, re-enable button).
    pub fn finish_sync(&self, summary: &str)

    /// Show an error state.
    pub fn show_error(&self, message: &str)

    /// Connect the sync button click handler.
    pub fn connect_sync_clicked<F>(&self, callback: F)
    where
        F: Fn() + 'static,

    /// Connect the cancel button click handler.
    pub fn connect_cancel_clicked<F>(&self, callback: F)
    where
        F: Fn() + 'static,
}
```

### 2. Sync button behavior

| State | Button Label | Button Style | Enabled |
|-------|-------------|--------------|---------|
| Idle, disconnected | "Sync Now" | `"suggested-action"` | No |
| Idle, connected | "Sync Now" | `"suggested-action"` | Yes |
| Syncing | "Syncing..." | flat | No |
| Error | "Retry Sync" | `"destructive-action"` | Yes |

Use `adw::ButtonContent` with an icon (`"emblem-synchronizing-symbolic"`) plus label for a polished look.

### 3. Progress bar layout

The progress area sits at the bottom of the window (below the paned):

```
┌──────────────────────────────────────────────────┐
│  Syncing: "Meeting Notes" (3 of 47)              │
│  ████████████░░░░░░░░░░░░░░░░░░  38%    [Cancel] │
└──────────────────────────────────────────────────┘
```

- `GtkBox` (vertical) containing:
  - `GtkLabel` — status text with current file name
  - `GtkBox` (horizontal):
    - `GtkProgressBar` — fraction-based (0.0 to 1.0), expanding
    - `GtkButton` "Cancel" — only visible during sync

- The entire progress box is hidden when not syncing (`widget.set_visible(false)`).

### 4. Progress updates

Bridge from the Tokio sync task to GTK main thread using `glib::MainContext::channel()`:

```rust
pub fn update_progress(&self, progress: &TransferProgress) {
    let fraction = if progress.files_total > 0 {
        progress.files_done as f64 / progress.files_total as f64
    } else {
        0.0
    };
    self.progress_bar.set_fraction(fraction);
    self.status_label.set_text(&format!(
        "Syncing: \"{}\" ({} of {})",
        progress.current_file,
        progress.files_done + 1,
        progress.files_total,
    ));
}
```

### 5. Post-sync summary

After sync completes, briefly show a summary before hiding the progress area:

```rust
pub fn finish_sync(&self, summary: &str) {
    self.status_label.set_text(summary);  // e.g., "Sync complete: 5 pulled, 2 pushed"
    self.progress_bar.set_fraction(1.0);
    // Hide progress after 3 seconds
    glib::timeout_add_seconds_local_once(3, move || {
        // self.progress_box.set_visible(false);
        // self.state = SyncUiState::Idle;
    });
}
```

### 6. Last sync timestamp

After successful sync, update the toolbar's "Last sync" label with a human-readable time (e.g., "Last sync: 2 minutes ago"). Use a periodic timer to keep this updated.

## Files to Create/Modify

- `src/ui/sync_controls.rs` — extend with sync button and progress
- `src/ui/window.rs` — add progress bar area to the window layout

## Test Strategy

1. **Initial state** — button shows "Sync Now", disabled (no device).
2. **Device connected** — call `set_device_connected(true)`, button becomes enabled.
3. **Start sync** — call `start_sync()`, progress bar appears, button disabled.
4. **Progress update** — call `update_progress` with increasing values, verify bar moves.
5. **Finish sync** — call `finish_sync("Done")`, verify summary shows, then hides.
6. **Error state** — call `show_error("Failed")`, verify button shows "Retry Sync".

## Acceptance Criteria

1. Sync button is disabled when no device is connected.
2. Clicking sync triggers the connected callback.
3. Progress bar shows file-level progress with current document name.
4. Cancel button is visible only during sync.
5. Post-sync summary displays briefly before auto-hiding.
6. Last sync timestamp updates in the toolbar.
