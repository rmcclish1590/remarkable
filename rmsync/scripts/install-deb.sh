#!/bin/bash
# Install the built rmSync .deb package via apt.
#
# apt's sandboxed `_apt` user cannot read files under a private home
# directory (e.g. /home/<user> with mode 750), which makes local installs
# emit "N: Download unsandboxed as root ... Permission denied". Stage the
# package in a world-readable temp directory before handing it to apt.
#
# Usage: scripts/install-deb.sh [path/to/rmsync_*.deb]
#   With no argument, installs the newest package in target/debian/.

set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
crate_dir="$(dirname "$script_dir")"

if [ $# -ge 1 ]; then
  deb="$1"
  [ -f "$deb" ] || { echo "error: no such file: $deb" >&2; exit 1; }
else
  deb="$(ls -t "$crate_dir"/target/debian/rmsync_*.deb 2>/dev/null | head -n 1 || true)"
  if [ -z "$deb" ]; then
    echo "error: no package found in target/debian/." >&2
    echo "Build one first: scripts/build-deb.sh" >&2
    exit 1
  fi
fi

staging="$(mktemp -d /tmp/rmsync-install.XXXXXX)"
trap 'rm -rf "$staging"' EXIT
chmod 755 "$staging"
install -m 644 "$deb" "$staging/"

echo "Installing $(basename "$deb")..."
sudo apt install -y "$staging/$(basename "$deb")"
