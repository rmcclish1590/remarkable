# Spec 01 — Project Scaffolding

**Layer:** 0 — Foundation  
**Dependencies:** None  
**Estimated effort:** 1 hour  

## Objective

Initialize the Rust project with Cargo, configure all dependencies, establish the module structure, and produce a GTK4 window that opens and closes cleanly. This is the skeleton that every other spec builds on.

## Context

We are building `rmsync`, a Linux Mint desktop application for bi-directional sync with a reMarkable 2 tablet. The stack is Rust + GTK4 (gtk-rs) + libadwaita. The application will use Tokio for async operations and SQLite for state tracking.

## Technical Requirements

### 1. Create the Cargo project

```bash
cargo init rmsync
```

### 2. Cargo.toml dependencies

```toml
[package]
name = "rmsync"
version = "0.1.0"
edition = "2021"
description = "Bi-directional sync for reMarkable 2 tablets on Linux"
license = "MIT"

[dependencies]
# UI
gtk = { version = "0.9", package = "gtk4", features = ["v4_6"] }
libadwaita = { version = "0.7", features = ["v1_2"] }

# Async runtime
tokio = { version = "1", features = ["full"] }

# SSH/SFTP
russh = "0.46"
russh-sftp = "2"

# USB detection
udev = "0.9"

# File watching
notify = "7"

# Database
rusqlite = { version = "0.32", features = ["bundled"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"

# Binary parsing
nom = "7"

# SVG
resvg = "0.44"

# Hashing
sha2 = "0.10"

# Logging
tracing = "0.1"
tracing-subscriber = "0.3"

# Error handling
anyhow = "1"
thiserror = "2"

# Unique IDs
uuid = { version = "1", features = ["v4"] }
```

> **Note to implementer:** Pin exact versions at build time. The versions above are targets — use the latest compatible release for each.

### 3. Module structure

Create the following directory layout under `src/`:

```
src/
├── main.rs              # Entry point: init GTK app, load config, launch UI
├── app.rs               # GtkApplication subclass, window setup
├── config.rs            # Config loading/saving (TOML)
├── ui/
│   ├── mod.rs
│   ├── window.rs        # Main application window
│   ├── folder_browser.rs # Document tree sidebar
│   ├── viewer.rs        # Document viewer panel
│   ├── sync_controls.rs # Sync button, progress bar, folder selector
│   └── device_status.rs # Connection status indicator
├── sync/
│   ├── mod.rs
│   ├── engine.rs        # Three-state diff and sync orchestration
│   ├── state_db.rs      # SQLite state database operations
│   ├── scanner.rs       # Local and remote file scanning
│   └── transfer.rs      # SFTP push/pull operations
├── device/
│   ├── mod.rs
│   ├── monitor.rs       # udev USB detection
│   └── connection.rs    # SSH/SFTP session management
├── remarkable/
│   ├── mod.rs
│   ├── metadata.rs      # .metadata JSON parser
│   ├── rm_parser.rs     # .rm v6 binary parser
│   ├── svg_renderer.rs  # Stroke data → SVG conversion
│   └── document.rs      # Document model (tree of notebooks/folders)
└── error.rs             # Application error types
```

Every `mod.rs` should declare its submodules as `pub mod`. Each leaf file should contain a placeholder struct or function with a `todo!()` body and a doc comment explaining its purpose.

### 4. main.rs implementation

```rust
// Initialize tracing subscriber for logging
// Create a GtkApplication with app ID "com.rmsync.app"
// Connect the "activate" signal to build and present the main window
// The window should:
//   - Use libadwaita::ApplicationWindow
//   - Set default size to 1200x800
//   - Set title to "rmSync"
//   - Show an empty placeholder label "rmSync — Ready" centered in the window
// Run the GTK application
```

### 5. Build and run verification

The project must compile with `cargo build` and launch a GTK4 window with `cargo run` that displays the placeholder text and can be closed cleanly.

## Files to Create

- `rmsync/Cargo.toml`
- `rmsync/src/main.rs`
- `rmsync/src/app.rs`
- `rmsync/src/config.rs`
- `rmsync/src/error.rs`
- All `mod.rs` and placeholder files under `src/ui/`, `src/sync/`, `src/device/`, `src/remarkable/`

## Acceptance Criteria

1. `cargo build` completes with zero errors (warnings acceptable at this stage).
2. `cargo run` opens a GTK4/libadwaita window titled "rmSync" at 1200×800.
3. The window displays "rmSync — Ready" centered.
4. Closing the window exits the process cleanly (exit code 0).
5. Every module file exists with a placeholder and doc comment.
6. `cargo clippy` produces no errors.
