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
# Usage: scripts/build-deb.sh [--out-dir DIR]
#   --out-dir DIR  write the package into DIR (which must already exist)
#                  and skip the staging step. install-deb.sh uses this to
#                  build straight into its own staging directory, so the
#                  package it installs is guaranteed to be this build's.
#
# Prerequisites:
#   cargo install cargo-deb
#   (plus the build deps cargo-deb needs: libgtk-4-dev, libadwaita-1-dev)

set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
crate_dir="$(dirname "$script_dir")"

out_dir=""
while [ $# -gt 0 ]; do
  case "$1" in
    --out-dir)
      [ $# -ge 2 ] || { echo "error: --out-dir needs a directory" >&2; exit 2; }
      out_dir="$(cd "$2" && pwd)"
      shift 2
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      echo "usage: $0 [--out-dir DIR]" >&2
      exit 2
      ;;
  esac
done

cd "$crate_dir"

missing=()
command -v cargo >/dev/null || missing+=("cargo (install Rust via https://rustup.rs)")
cargo deb --version >/dev/null 2>&1 || missing+=("cargo-deb (cargo install cargo-deb)")
command -v pkg-config >/dev/null || missing+=("pkg-config")
for lib in gtk4 libadwaita-1 libudev; do
  pkg-config --exists "$lib" 2>/dev/null || missing+=("$lib development files")
done
if [ ${#missing[@]} -gt 0 ]; then
  echo "error: missing build prerequisites:" >&2
  printf '  - %s\n' "${missing[@]}" >&2
  echo "Install the system packages with:" >&2
  echo "  sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev libudev-dev" >&2
  exit 1
fi

echo "[1/5] Building rmSync release binary..."
cargo build --release

echo "[2/5] Running tests..."
cargo test --release

echo "[3/5] Building .deb package..."
if [ -n "$out_dir" ]; then
  # cargo-deb prints the path of the package it wrote as its last line.
  deb="$(cargo deb --output "$out_dir/" | tail -n 1)"
  [ -f "$deb" ] || { echo "error: cargo deb did not report a package path" >&2; exit 1; }
  echo "[4/5] Package written to the requested directory."
  echo "[5/5] Done."
  echo
  echo "  built:  $deb"
  exit 0
fi
deb="$(cargo deb | tail -n 1)"
[ -f "$deb" ] || { echo "error: cargo deb did not report a package path" >&2; exit 1; }
deb="$(realpath --relative-to="$crate_dir" "$deb")"

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
echo "  sudo apt install --reinstall $staged"
echo
echo "or build and install in one step:"
echo "  scripts/install-deb.sh"
