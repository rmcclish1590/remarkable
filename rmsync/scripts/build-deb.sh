#!/bin/bash
# Build the rmSync .deb package.
#
# Prerequisites:
#   cargo install cargo-deb
#   (plus the runtime-deps cargo-deb needs: libgtk-4-dev, libadwaita-1-dev)

set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
crate_dir="$(dirname "$script_dir")"
cd "$crate_dir"

echo "[1/4] Building rmSync release binary..."
cargo build --release

echo "[2/4] Running tests..."
cargo test --release

echo "[3/4] Building .deb package..."
cargo deb

echo "[4/4] Done. Package:"
ls -la target/debian/rmsync_*.deb
