# Remarkable

A desktop application for synchronizing data between your computer and your reMarkable tablet.

## Overview

Remarkable connects to your reMarkable tablet to provide two-way data synchronization. It creates local backups of your tablet's data and allows you to view and interact with your documents directly on your computer. The application (`rmsync`) is a native GTK4/libadwaita app targeting Linux Mint and other Debian-based distributions.

## Features

- **Data Synchronization** — Sync documents, notebooks, and files between your reMarkable tablet and your computer.
- **Backup** — Automatically back up your tablet's data to your local machine for safekeeping.
- **Document Visualization** — View and browse your reMarkable documents on your computer without needing the tablet.

## Requirements

- Linux Mint 22+ (or another Debian/Ubuntu-based distribution with GTK 4.10+ and libadwaita 1.2+)
- A reMarkable 2 tablet, connected over USB
- Runtime libraries (installed automatically by the `.deb` package): `libgtk-4-1`, `libadwaita-1-0`, `libudev1`, `openssh-client`

## Installation

### Option A — Install the .deb package (recommended)

1. Build the package (see [Building the .deb package](#building-the-deb-package) below), or use a prebuilt `rmsync_<version>_amd64.deb` if you have one.
2. Install it with the install script, which stages the package where apt's sandboxed `_apt` user can read it:

   ```bash
   cd rmsync
   ./scripts/install-deb.sh                # installs the newest package from target/debian/
   ```

   Or install a specific package file manually. If the `.deb` lives under your home directory, copy it to `/tmp` first — otherwise apt prints `N: Download unsandboxed as root ... Permission denied` because the sandboxed `_apt` user cannot read files under a private home directory:

   ```bash
   cp rmsync_0.1.0-1_amd64.deb /tmp/
   sudo apt install /tmp/rmsync_0.1.0-1_amd64.deb
   ```

   Installation adds the `rmsync` binary to `/usr/bin`, an application menu entry with icon, and a udev rule for tablet detection.

3. Verify the installation:

   ```bash
   which rmsync                                      # → /usr/bin/rmsync
   ls /usr/share/applications/rmsync.desktop         # menu entry
   ```

To uninstall:

```bash
sudo apt remove rmsync
```

### Option B — Build and run from source

1. Install the Rust toolchain (via [rustup](https://rustup.rs)) and the build dependencies:

   ```bash
   sudo apt update
   sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev libudev-dev
   ```

2. Clone the repository and build:

   ```bash
   git clone https://github.com/rmcclish1590/remarkable.git
   cd remarkable/rmsync
   cargo build --release
   ```

3. Run the app:

   ```bash
   ./target/release/rmsync
   ```

### Building the .deb package

From a source checkout, with the build dependencies from Option B installed:

```bash
cargo install cargo-deb
cd rmsync
./scripts/build-deb.sh
```

The script builds the release binary, runs the full test suite, and produces the package at `rmsync/target/debian/rmsync_<version>_amd64.deb`. It then copies the package into a temp directory that apt's sandboxed `_apt` user can read, and prints a ready-to-run install command for that copy — packages left under your home directory trigger the `Permission denied` notice described above.

## Running the application

Launch **rmSync** from the application menu (it appears under Utilities after installing the `.deb`), or run `rmsync` from a terminal.

### First-time setup

1. Connect your reMarkable 2 to your computer with its USB cable and make sure the tablet is awake. The tablet exposes a USB network interface (the app connects to it at `10.11.99.1`).
2. Launch rmSync. When you connect for the first time, the app asks for the tablet's SSH password:
   - On the tablet, open **Settings → Help → Copyrights and licenses** (on older firmware: **Settings → General → Help → Copyrights and licenses**).
   - Scroll to the bottom — the root password is listed under **GPLv3 Compliance**.
3. Enter the password once. rmSync installs an SSH key on the tablet so future connections are passwordless.
4. Choose a sync folder (defaults to `~/Documents/reMarkable`) and start a sync.

### Configuration

Settings are stored in `~/.config/rmsync/config.toml`, created with defaults on first run. Synced documents are stored in the sync folder chosen above.

### Logging

rmSync writes a log to `~/.local/share/rmsync/logs/rmsync.log`. It is the first thing to look at when something goes wrong, and the right thing to attach to a bug report.

Open the gear icon in the header bar to change how much is recorded. The setting applies immediately — no restart — and is remembered:

| Level | What it records |
|---|---|
| Errors only | Warnings and errors, nothing about normal operation. |
| Production *(default)* | What rmSync does — startup, sync phases, documents opened, deletions — plus any problems. |
| Information | The above, plus informational messages from supporting libraries (SSH, storage). |
| Debug | Everything, including per-file and per-page detail. Verbose enough to slow a large sync down; use it to reproduce a specific bug. |

To capture a problem: set **Debug**, reproduce it, then send the log file.

The log rotates at 5 MB, keeping 5 previous files (`rmsync.log.1` … `.5`). Two environment variables override the defaults:

```bash
RMSYNC_LOG_DIR=/path/to/dir rmsync   # write logs somewhere else
RUST_LOG=rmsync::sync=trace rmsync   # per-module control; overrides the UI setting
```

## Development

Run the test suite from the `rmsync` directory:

```bash
cargo test
```
