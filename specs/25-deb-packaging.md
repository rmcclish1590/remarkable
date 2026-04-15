# Spec 25 — Debian Package Build

**Layer:** 7 — Packaging  
**Dependencies:** All previous specs (complete, buildable application)  
**Estimated effort:** 1–2 hours  

## Objective

Create a `.deb` package for installing rmSync on Linux Mint and other Debian-based distributions, including a desktop entry, icon, and optional udev rule.

## Context

Linux Mint is Debian-based and uses `.deb` packages for software installation. The application should be installable via `sudo dpkg -i rmsync_0.1.0_amd64.deb` and appear in the application menu.

## Technical Requirements

### 1. Use `cargo-deb`

Install and configure the `cargo-deb` tool for generating .deb packages from Cargo projects.

Add to `Cargo.toml`:

```toml
[package.metadata.deb]
maintainer = "rmSync Contributors"
copyright = "2026, rmSync Contributors"
license-file = ["LICENSE", "0"]
extended-description = """
rmSync provides bi-directional synchronization between a reMarkable 2 \
tablet and your Linux desktop. It detects your tablet over USB, syncs \
documents in both directions, and lets you view your notebooks with a \
built-in document viewer."""
section = "utils"
priority = "optional"
depends = "$auto"
assets = [
    ["target/release/rmsync", "usr/bin/", "755"],
    ["assets/rmsync.desktop", "usr/share/applications/", "644"],
    ["assets/rmsync.svg", "usr/share/icons/hicolor/scalable/apps/", "644"],
    ["assets/rmsync-48.png", "usr/share/icons/hicolor/48x48/apps/rmsync.png", "644"],
    ["assets/99-remarkable.rules", "etc/udev/rules.d/", "644"],
]
```

### 2. Desktop entry (`assets/rmsync.desktop`)

```ini
[Desktop Entry]
Name=rmSync
Comment=Bi-directional sync for reMarkable 2
Exec=rmsync
Icon=rmsync
Terminal=false
Type=Application
Categories=Utility;FileTools;
Keywords=remarkable;sync;tablet;notes;
StartupNotify=true
```

### 3. Application icon (`assets/rmsync.svg`)

Create a simple SVG icon for the application. It should:
- Be a scalable SVG.
- Use a visual metaphor combining a tablet/document with sync arrows.
- Work at small sizes (16px) and large sizes (256px).
- Follow the Freedesktop icon naming spec.

Also provide a 48×48 PNG version (`assets/rmsync-48.png`) rendered from the SVG.

### 4. Udev rule (`assets/99-remarkable.rules`)

```
# reMarkable 2 USB detection rule
# This rule triggers when the reMarkable creates its USB virtual Ethernet interface
SUBSYSTEM=="net", ACTION=="add", ATTRS{idVendor}=="04b3", TAG+="remarkable"
```

This is a lightweight rule that tags the device — the application monitors udev events internally. The rule is optional and mainly helps with device identification.

### 5. Build script

Create `scripts/build-deb.sh`:

```bash
#!/bin/bash
set -e

echo "Building rmSync release binary..."
cargo build --release

echo "Running tests..."
cargo test --release

echo "Building .deb package..."
cargo deb

echo "Package built:"
ls -la target/debian/rmsync_*.deb
```

### 6. Post-install script

Create `assets/postinst` (optional):

```bash
#!/bin/bash
# Reload udev rules to pick up the remarkable rule
udevadm control --reload-rules || true
udevadm trigger || true

# Update icon cache
gtk-update-icon-cache -f /usr/share/icons/hicolor/ || true

# Update desktop database
update-desktop-database /usr/share/applications/ || true
```

Reference in Cargo.toml:
```toml
[package.metadata.deb]
maintainer-scripts = "assets/"
```

### 7. Runtime dependencies

The .deb package should declare dependencies on:
- `libgtk-4-1` (GTK4 runtime)
- `libadwaita-1-0` (libadwaita runtime)
- `libudev1` (udev library)
- `openssh-client` (SSH for device connection)

`cargo-deb` with `$auto` will detect shared library dependencies automatically. Verify the output and add explicit deps if needed:

```toml
depends = "libgtk-4-1 (>= 4.6), libadwaita-1-0 (>= 1.2), openssh-client"
```

### 8. Build verification

After building the .deb:

```bash
# Verify package contents
dpkg-deb --contents target/debian/rmsync_*.deb

# Verify package info
dpkg-deb --info target/debian/rmsync_*.deb

# Test install (on a clean system or container)
sudo dpkg -i target/debian/rmsync_*.deb

# Verify it launches
rmsync --version
rmsync  # Should open the GTK window

# Verify desktop entry
grep -r "rmsync" /usr/share/applications/

# Verify icon
ls /usr/share/icons/hicolor/scalable/apps/rmsync.svg
```

## Files to Create

- `assets/rmsync.desktop`
- `assets/rmsync.svg`
- `assets/rmsync-48.png`
- `assets/99-remarkable.rules`
- `assets/postinst`
- `scripts/build-deb.sh`
- Update `Cargo.toml` with `[package.metadata.deb]` section

## Test Strategy

1. **Package builds** — `cargo deb` completes without errors.
2. **Package contents** — `dpkg-deb --contents` shows all expected files in correct locations.
3. **Dependencies** — `dpkg-deb --info` shows correct dependency list.
4. **Install/uninstall** — package installs and uninstalls cleanly.
5. **Desktop entry** — application appears in the Mint application menu after install.
6. **Launch** — installed binary launches the GTK window.

## Acceptance Criteria

1. `cargo deb` produces a valid `.deb` package.
2. Package installs cleanly on Linux Mint.
3. Application appears in the system application menu with an icon.
4. `rmsync` command is available in PATH after install.
5. Udev rule is installed for device detection.
6. Package uninstalls cleanly with `dpkg -r rmsync`.
