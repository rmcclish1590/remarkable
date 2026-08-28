#!/bin/bash
# Build the rmSync .deb package.
#
# The finished package is also copied to a temp directory that apt's
# sandboxed `_apt` user can read. Packages left under the repo can't be
# read by `_apt` when the user's home directory is mode 750, which makes
# `apt install` fall back to unsandboxed root and warn:
#   N: Download unsandboxed as root as file '...' couldn't be accessed
#      by user '_apt'. - pkgAcquire::Run (13: Permission denied)
#
# The staging directory is created with mktemp, so its name is not
# predictable. A fixed path under the world-writable /tmp could be
# pre-created by another local user, who could then swap the package
# out from under the `sudo apt install` below.
#
# Prerequisites:
#   cargo install cargo-deb
#   (plus the build deps cargo-deb needs: libgtk-4-dev, libadwaita-1-dev)

set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
crate_dir="$(dirname "$script_dir")"
cd "$crate_dir"

echo "[1/5] Building rmSync release binary..."
cargo build --release

echo "[2/5] Running tests..."
cargo test --release

echo "[3/5] Building .deb package..."
cargo deb

deb="$(ls -t target/debian/rmsync_*.deb | head -n 1)"

echo "[4/5] Copying package to a temp directory apt can read..."
staging="$(mktemp -d "${TMPDIR:-/tmp}/rmsync-pkg.XXXXXX")"
chmod 755 "$staging"
install -m 644 "$deb" "$staging/"
staged="$staging/$(basename "$deb")"

echo "[5/5] Done."
echo
echo "  built:  $crate_dir/$deb"
echo "  staged: $staged"
echo
echo "Install with:"
echo "  sudo apt install $staged"
echo
echo "or let the install script stage it for you:"
echo "  scripts/install-deb.sh"
