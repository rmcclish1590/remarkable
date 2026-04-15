# Spec 15 — GTK4 Main Window Layout

**Layer:** 4 — UI Shell  
**Dependencies:** 01 (project scaffolding), 10 (config persistence)  
**Estimated effort:** 1–2 hours  

## Objective

Build the main application window with the three-panel layout (sidebar, viewer, toolbar) using GTK4 and libadwaita, wired to the config system for window state persistence.

## Context

The UI has three main regions: a top toolbar with sync controls and device status, a left sidebar with the document/folder browser, and a main content area for the document viewer. This spec builds the structural shell — subsequent specs fill each panel with functional widgets.

## Technical Requirements

### 1. Application setup (`src/app.rs`)

```rust
pub struct RmSyncApp {
    app: adw::Application,
    config: Arc<Mutex<AppConfig>>,
}

impl RmSyncApp {
    pub fn new() -> Self {
        let app = adw::Application::builder()
            .application_id("com.rmsync.app")
            .build();
        // Load config
        // Connect activate signal
        Self { app, config }
    }

    pub fn run(&self) -> i32
}
```

### 2. Main window (`src/ui/window.rs`)

Create a GTK4 `adw::ApplicationWindow` with this structure:

```
ApplicationWindow
└── GtkBox (vertical)
    ├── HeaderBar (adw::HeaderBar)
    │   ├── [Left]  Device status indicator (placeholder GtkLabel)
    │   ├── [Title] "rmSync"
    │   └── [Right] Sync button (placeholder GtkButton)
    │
    ├── Toolbar (GtkBox horizontal)
    │   ├── GtkLabel "Sync to:"
    │   ├── GtkEntry (showing current sync path, read-only)
    │   ├── GtkButton "Browse"
    │   ├── GtkSeparator (vertical)
    │   ├── GtkLabel "Last sync: Never"
    │   └── Spacer
    │
    └── GtkPaned (horizontal, resizable)
        ├── [Left panel - sidebar]
        │   └── GtkScrolledWindow
        │       └── GtkBox (placeholder — "Documents will appear here")
        │       Width: sidebar_width from config (default 280px)
        │
        └── [Right panel - viewer]
            └── GtkScrolledWindow
                └── GtkBox (placeholder — "Select a document to view")
```

### 3. Implementation details

```rust
pub struct MainWindow {
    pub window: adw::ApplicationWindow,
    pub header_bar: adw::HeaderBar,
    pub sync_path_entry: gtk::Entry,
    pub browse_button: gtk::Button,
    pub last_sync_label: gtk::Label,
    pub paned: gtk::Paned,
    pub sidebar_scroll: gtk::ScrolledWindow,
    pub viewer_scroll: gtk::ScrolledWindow,
    pub device_status_label: gtk::Label,
    pub sync_button: gtk::Button,
}

impl MainWindow {
    /// Build and return the main window.
    pub fn new(app: &adw::Application, config: &AppConfig) -> Self

    /// Get references to key widgets for wiring in later specs.
    pub fn sidebar_container(&self) -> &gtk::ScrolledWindow
    pub fn viewer_container(&self) -> &gtk::ScrolledWindow
    
    /// Update the sync path display.
    pub fn set_sync_path(&self, path: &str)
    
    /// Update the last sync timestamp display.
    pub fn set_last_sync(&self, timestamp: Option<u64>)
}
```

### 4. Window state persistence

On window close:
1. Read the current window size (`window.default_size()`).
2. Read the paned position (`paned.position()`).
3. Save to config as `ui.window_width`, `ui.window_height`, `ui.sidebar_width`.
4. Call `config.save()`.

On window open:
1. Set window size from config.
2. Set paned position from config.

### 5. Styling

- Use `libadwaita` for the modern GNOME/GTK4 look.
- The header bar should use `adw::HeaderBar` (not `gtk::HeaderBar`).
- Apply CSS class `"sidebar"` to the left panel for potential custom styling.
- Use `GtkPaned` so the user can drag the sidebar divider.
- Set minimum sidebar width to 200px and minimum viewer width to 400px.

### 6. Placeholder content

For now, the sidebar shows a centered label "No documents" and the viewer shows "Select a document to view". These will be replaced by Specs 16 and 20/21.

## Files to Create/Modify

- `src/app.rs` — full implementation
- `src/ui/window.rs` — full implementation
- `src/ui/mod.rs` — export window module
- `src/main.rs` — update to use `RmSyncApp`

## Test Strategy

1. **Window launches** — `cargo run` opens a window with correct title, size, and layout.
2. **Paned resizing** — drag the sidebar divider, verify it moves.
3. **Config persistence** — resize window, close, relaunch, verify size is restored.
4. **Sync path display** — call `set_sync_path("/some/path")`, verify the entry shows it.

## Acceptance Criteria

1. Application launches with the three-panel layout visible.
2. Header bar shows title, placeholder device status, and placeholder sync button.
3. Toolbar shows sync path entry and browse button.
4. Sidebar and viewer are separated by a resizable paned divider.
5. Window size and sidebar width persist across restarts.
6. `cargo run` produces a functional window.
