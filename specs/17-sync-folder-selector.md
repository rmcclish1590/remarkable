# Spec 17 — Sync Destination Folder Selector

**Layer:** 4 — UI Shell  
**Dependencies:** 15 (main window), 10 (config persistence)  
**Estimated effort:** 30 minutes  

## Objective

Wire the "Browse" button in the toolbar to a native folder chooser dialog that lets the user select where reMarkable files are synchronized to, persisting the choice to config.

## Context

The toolbar has a read-only GtkEntry showing the current sync path and a "Browse" button. Clicking Browse opens a native folder chooser. The selected path is saved to `config.sync.sync_dir` and the necessary subdirectories (`raw/`, `.rmsync/`) are created.

## Technical Requirements

### 1. Folder chooser (`src/ui/sync_controls.rs`)

```rust
/// Wire the browse button to open a folder chooser dialog.
pub fn setup_folder_selector(
    browse_button: &gtk::Button,
    path_entry: &gtk::Entry,
    window: &adw::ApplicationWindow,
    config: Arc<Mutex<AppConfig>>,
)
```

Implementation:

1. Connect to `browse_button.connect_clicked`.
2. Create a `gtk::FileDialog::new()`.
3. Set the dialog title to "Choose Sync Destination".
4. Set the initial folder to the current `config.sync.sync_dir`.
5. Call `dialog.select_folder()` (async GTK4 API).
6. On selection:
   a. Update `path_entry` text to the selected path.
   b. Update `config.sync.sync_dir` to the selected path.
   c. Save config.
   d. Create `{path}/raw/` and `{path}/.rmsync/` directories if they don't exist.
7. On cancel: do nothing.

### 2. Path validation

After selection, verify:
- The path is writable (attempt to create the subdirectories).
- If creation fails, show an `adw::MessageDialog` with the error.

### 3. Initial state

On app launch, populate the `path_entry` with `config.sync.sync_dir` and verify the directories exist.

## Files to Create/Modify

- `src/ui/sync_controls.rs` — implement `setup_folder_selector`
- `src/ui/window.rs` — call `setup_folder_selector` during window construction

## Test Strategy

1. **Button click opens dialog** — manual test, verify native dialog appears.
2. **Selection updates entry** — select a folder, verify the entry text changes.
3. **Config persistence** — select a folder, restart app, verify the path is remembered.
4. **Directory creation** — select a new empty folder, verify `raw/` and `.rmsync/` are created.
5. **Unwritable path** — select a read-only path, verify error dialog appears.

## Acceptance Criteria

1. Browse button opens a native folder chooser.
2. Selected path is displayed in the toolbar entry.
3. Selection is persisted to config.
4. Required subdirectories are created.
5. Errors are shown in a dialog, not silently swallowed.
