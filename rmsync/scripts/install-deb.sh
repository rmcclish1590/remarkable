#!/bin/bash
# Build rmSync from the current source and install the result via apt.
#
# Every run builds a fresh package (release build, full test suite,
# cargo deb) straight into a new staging directory and installs exactly
# that file, so a package left over from an earlier build can never be
# picked up by mistake. If any build step fails, the script stops before
# apt is invoked and the existing installation is left untouched.
#
# apt's sandboxed `_apt` user cannot read files under a private home
# directory (e.g. /home/<user> with mode 750), which makes local installs
# emit "N: Download unsandboxed as root ... Permission denied". The
# staging directory lives under /tmp for that reason; it is created with
# mktemp so its name is not predictable (see build-deb.sh).
#
# Usage: scripts/install-deb.sh [path/to/rmsync_*.deb]
#   With no argument, builds a new package and installs it.
#   With a path, skips the build and installs that package as given.
#
# User data is not part of the package and survives a reinstall:
# ~/.config/rmsync (config, known_hosts, SSH key), ~/.local/share/rmsync
# (logs) and the sync directory with its .rmsync/ state and backups.

set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
crate_dir="$(dirname "$script_dir")"

staging="$(mktemp -d /tmp/rmsync-install.XXXXXX)"
trap 'rm -rf "$staging"' EXIT
chmod 755 "$staging"

if [ $# -ge 1 ]; then
  deb="$1"
  [ -f "$deb" ] || { echo "error: no such file: $deb" >&2; exit 1; }
  install -m 644 "$deb" "$staging/"
  deb="$staging/$(basename "$deb")"
  source_desc="$(basename "$1") (prebuilt, not rebuilt)"
else
  if ! "$script_dir/build-deb.sh" --out-dir "$staging"; then
    echo >&2
    echo "error: build failed; nothing was installed." >&2
    echo "The existing rmSync installation (if any) is unchanged." >&2
    exit 1
  fi
  shopt -s nullglob
  debs=("$staging"/rmsync_*.deb)
  shopt -u nullglob
  if [ ${#debs[@]} -ne 1 ]; then
    echo "error: expected one freshly built package in $staging, found ${#debs[@]}." >&2
    exit 1
  fi
  deb="${debs[0]}"
  chmod 644 "$deb"

  commit="$(git -C "$crate_dir" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  if [ "$commit" != unknown ] && [ -n "$(git -C "$crate_dir" status --porcelain 2>/dev/null)" ]; then
    commit="$commit with uncommitted changes"
  fi
  source_desc="built from commit $commit"
fi

previous="$(dpkg-query -W -f='${Version}' rmsync 2>/dev/null || true)"

echo
echo "Installing $(basename "$deb")..."
# --reinstall: a rebuild with an unchanged version number is still new code, and
# without it apt reports "already the newest version" and installs nothing.
sudo apt install --reinstall -y "$deb"

installed="$(dpkg-query -W -f='${Version}' rmsync 2>/dev/null || echo unknown)"
echo
echo "rmSync $installed installed ($source_desc)."
if [ -n "$previous" ]; then
  echo "  previously installed: $previous"
fi
