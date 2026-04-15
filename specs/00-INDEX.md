# rmSync — Spec Index & Dependency Map

## How to Use These Specs

Each spec is a self-contained thin slice designed to be fed directly into Claude Code as a prompt. They are numbered by dependency order — complete them in sequence within each layer. Layers can overlap once their dependencies are met.

## Dependency Graph

```
LAYER 0 — Foundation (no dependencies, parallelizable)
  01-project-scaffolding ─────────────────────────┐
  02-metadata-parser ──────────────────────────────┤
  03-rm-binary-parser ─── 04-svg-renderer          │
  05-sqlite-state-schema ──────────────────────────┤
                                                   │
LAYER 1 — Connectivity (depends on 01)             │
  06-ssh-sftp-module ──── 08-remote-file-listing   │
  07-udev-device-monitor ─────────────────────────┤
                                                   │
LAYER 2 — Local Infrastructure (depends on 01, 05) │
  09-local-file-scanner                            │
  10-config-persistence                            │
                                                   │
LAYER 3 — Sync Core (depends on 05, 06, 08, 09)   │
  11-three-state-diff ─── 12-pull-sync             │
                     └─── 13-push-sync             │
                     └─── 14-conflict-resolution   │
                                                   │
LAYER 4 — UI Shell (depends on 01, 02)             │
  15-gtk-main-window                               │
  16-folder-browser ──────────────────────────────┤
  17-sync-folder-selector                          │
  18-device-status-indicator                       │
  19-sync-button-progress                          │
                                                   │
LAYER 5 — Viewer (depends on 03, 04, 15)           │
  20-single-page-viewer                            │
  21-multipage-scrollable-viewer                   │
                                                   │
LAYER 6 — Integration (depends on all above)       │
  22-wire-sync-to-ui                               │
  23-wire-device-monitor-to-ui                     │
  24-end-to-end-tests                              │
                                                   │
LAYER 7 — Packaging                                │
  25-deb-packaging ────────────────────────────────┘
```

## Parallelization Opportunities

These specs can run in parallel Claude Code sessions:

- **01** + **02** + **03** + **05** (all Layer 0, no interdependencies)
- **06** + **07** (both only depend on 01)
- **09** + **10** (both only depend on 01 + 05)
- **15** + **16** + **17** + **18** + **19** (UI shell, all depend on 01 + 02)
- **12** + **13** + **14** (all depend on 11)

## Estimated Effort Per Spec

Most specs target **1–3 hours** of Claude Code session time. Specs 03 (binary parser) and 11 (diff engine) are the densest and may take longer.
