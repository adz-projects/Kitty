# Backlog

Deferred items. Per CLAUDE.md's Definition of Done, code carries no `TODO`s —
open an item here instead.

## Open

- **Recipes / skills (Round-2 item 16).** Goose recipes are file-based YAML run via
  the CLI `goose run --recipe <name|path> [--params K=V]` (confirmed by the Batch-0
  ACP probe — `session/new` silently ignores recipe params; there is no ACP method
  to launch a recipe). Integrating them means either a recipe-file CRUD editor in
  Settings (read/write `.yaml`, list via `goose recipe list`, "launch" hands off to
  Goose Desktop / copies the run command) or a new managed child-process path that
  spawns `goose run --recipe` outside the shared `goose serve` (heavier; unconfirmed
  whether such a run surfaces in normal `session/list` history). Deferred by owner
  decision on 2026-07-05 ("skip recipes for now"). Goose also has a separate
  `goose skills list` concept that was folded into "recipes" per the same decision.

## Noted for later

- ChatML **import** path — Phase 11 export is one-way (`.chatml` + `.meta.json`).
  Re-import (reconstruct a session from the pair) is out of scope for v1.
- Per-turn provider/model in the export: we only track the session's current
  model in render state, so every turn is tagged with it. Capture true per-turn
  model if goosed later exposes it in `session/load` replay metadata.
- Cross-platform support (Windows-only for v1 per project description §11).
