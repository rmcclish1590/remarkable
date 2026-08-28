# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Remarkable is a desktop application for synchronizing data between a computer and a reMarkable tablet. It provides data backup and document visualization capabilities.

The application (`rmsync`) is a GTK4/libadwaita desktop app in Rust targeting Linux Mint and other Debian-based distributions. It talks to the tablet over USB via SSH/SFTP (the tablet exposes a USB network interface at `10.11.99.1`).

## Repository Layout

- `rmsync/` — the Rust crate; all application code lives here. **Run all cargo commands from this directory.**
- `specs/` — 28 numbered design specs plus `00-INDEX.md`, a dependency graph organised into layers (Foundation → Connectivity → Sync Core → UI → Viewer → Integration → Packaging → Real-Device Fixes). Each spec is a self-contained slice. When touching an area, read its spec first — it states the intended contract, not just the current code.

## Commands

All from `rmsync/`:

```bash
cargo build                       # debug build
cargo build --release
cargo run                         # launch the GTK app
cargo test                        # lib + both integration binaries
cargo test --lib                  # unit tests only (fast)
cargo test <name-substring>       # single test, e.g. cargo test folder_path
cargo test --test integration_sync   # one integration binary
cargo clippy --all-targets        # kept clean; CI does not exist, so run it yourself
cargo run --example dump_rm -- <file.rm>   # report what the .rm parser recovers
```

Build dependencies: `build-essential pkg-config libgtk-4-dev libadwaita-1-dev libudev-dev`.

Packaging (`cargo install cargo-deb` first):

```bash
./scripts/build-deb.sh     # release build + tests + cargo deb, stages package in /tmp
./scripts/install-deb.sh   # installs the newest build via apt
```

The scripts stage the `.deb` in a `mktemp -d` directory because apt's sandboxed `_apt` user cannot read files under a mode-750 home directory. Never substitute a fixed `/tmp` path — that path is handed to `sudo apt install`, and a predictable name in world-writable `/tmp` is hijackable.

## The sync directory contract

This is the most important thing to understand before changing anything in `sync/`, `remarkable/`, or the viewer. The user's sync directory (default `~/Documents/reMarkable`, set in config) looks like:

```
<sync_dir>/
  raw/                     byte-for-byte mirror of the tablet's xochitl dir
    <uuid>.metadata        one per document AND per folder
    <uuid>.content
    <uuid>/<page-id>.rm    page files, same nesting as the device
  .rmsync/state.db         SQLite sync state
  .rmsync/cache/<uuid>_<page-id>.svg   rendered pages
```

`raw/` is deliberately **flat and UUID-named** — it mirrors `/home/root/.local/share/remarkable/xochitl`. Its layout is load-bearing in two ways that are easy to break:

- **Push derives remote paths from local relative paths** (`transfer.rs`), so a local path that no longer mirrors the device produces a wrong remote path.
- **`compute_local_hash` hashes the path relative to `raw/`**, and the remote side hashes `<uuid>.metadata` / `<uuid>/<page>.rm`. If the two stop agreeing, the diff sees a mismatch and reports a conflict for *every* document.

Human-readable names never appear in `raw/`; they come from `visibleName` in the metadata. Folder hierarchy is reconstructed in memory, not on disk.

## Architecture

**Sync pipeline** (`sync/engine.rs` orchestrates): `scan_remote` (SFTP walk) + `scan_local` (read `raw/`) + the state DB feed `compute_sync_plan`, which runs a **three-state diff**. Each UUID is classified against the triple `(local, remote, synced)` — the `synced` baseline is what makes a one-sided change distinguishable from a genuine conflict. The resulting `SyncPlan` drives conflict resolution, then pulls, then pushes, then deletes.

**Folder hierarchy** (`remarkable/document.rs`): documents and folders are sibling `<uuid>.metadata` files; a folder is `type: "CollectionType"` and children point at it by `parent` UUID. `DocumentTree::build_from_directory` reassembles the tree and **promotes any item whose parent is absent to the root**. That fallback means missing folder metadata silently degrades into a flat list rather than an error — `scanner.rs` walks the parent chain after each remote scan to make sure ancestor folders are actually fetched.

**`.rm` parsing** (`remarkable/rm_parser.rs` dispatches on version): v3/v5 use the old flat layer/stroke layout handled in `rm_parser.rs`; **v6 (firmware 3.x+) is a completely different format** — a CRDT scene tree of length-prefixed blocks with tagged-value fields, decoded in `rm_v6.rs`. Do not assume "v6" means the flat layout; that mistake made every modern page fail to parse. `rm_v6` skips malformed or deleted items rather than failing the page. Besides strokes, v6 files can carry typed text (root-text block 0x07): a CRDT character sequence that `rm_v6.rs` reassembles into styled paragraphs (`RmText` on `RmPage`). Output feeds `svg_renderer.rs`, which computes the viewBox from the stroke/text bounding box (content uses centre-origin x, so coordinates are routinely negative) and emits typed text as SVG `<text>` elements — the viewer must load system fonts into resvg's fontdb or that text silently disappears.

There is also a Python fallback (`scripts/rm_to_svg.py`, requires `rmscene`) used only when the native parser fails or recovers nothing.

**Device layer** (`device/`): `connection.rs` wraps russh/russh-sftp with trust-on-first-use host-key pinning (`~/.config/rmsync/known_hosts`); first connection asks for the tablet's SSH password and installs an Ed25519 key. `monitor.rs` watches for the tablet appearing/disappearing and broadcasts `DeviceEvent`s.

**Threading** (`app.rs`): GTK owns the main thread. Each sync runs on its own `std::thread` with a current-thread tokio runtime, because `rusqlite` is `!Sync`. A separate shared runtime handles device-monitor subscriptions. Events cross back to the UI through a glib channel — never touch GTK widgets from a sync thread.

## Conventions and traps

- **Tablet-supplied identifiers are untrusted.** Anything used to build a filesystem path (UUIDs, page IDs, remote filenames) must go through `is_safe_uuid` / `is_safe_component` in `transfer.rs`. Path traversal via unvalidated page IDs has been a real vulnerability here.
- **`state_db.rs` migrations**: `migrate()` only runs `CREATE TABLE IF NOT EXISTS`. Adding a column requires a real `ALTER TABLE` path and a `SCHEMA_VERSION` bump — existing databases will not pick it up otherwise.
- Integration tests share `tests/common/mod.rs`, which synthesises `.rm` files and notebook metadata. There is no `DeviceConnection` mock, so anything requiring SSH cannot be integration-tested; keep decision logic in pure functions so it can be tested directly (see `missing_parent_uuids`).
- Rendered SVGs are only regenerated when the cache file is missing or when `RENDER_VERSION` (in `svg_renderer.rs`) doesn't match the cache's `.render-version` marker. **After changing the parser or renderer, bump `RENDER_VERSION`** — otherwise users' already-cached pages keep the old output forever.

## Git and issue workflow

- **Never commit directly to `main`.** Always create a branch, push it, and open a PR.
- Work is tracked in Linear under the McClish Products team (`MCC`), project "Remarkable". Linear supplies a branch name per issue — use it.
- **One issue per branch/PR/merge/delete cycle.** Do not batch multiple issues onto one branch, even when they are related.
- Set the issue to **In Progress when work starts**, before creating the branch. A merged PR whose body says `Closes MCC-NN` moves it to Done — verify that happened rather than assuming.
