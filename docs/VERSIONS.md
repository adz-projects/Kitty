# Pinned Versions & Verified API Surface

This file is the single source of truth for external-dependency versions and any
API-path / process-name assumptions the app relies on. A dependency bump is not
done until this file is updated and the affected code (`goosed/api.rs`,
`conflict.rs`, `starter_models.ts`, etc.) is re-verified against it.

## Goose

- **Pinned version:** _TBD (Phase 0 — pick one released `goose`/`goosed` version; do not chase `main`)._
- **goosed binary location(s) checked:** _TBD._
- **Install method used for dev:** _TBD (native Windows installer vs. WSL — record which)._
- **Vendored OpenAPI spec:** `docs/goosed-openapi.json` (generate the typed TS client from this).
- **Verified route families** (fill in exact paths from the vendored spec):
  - Agent lifecycle (`/agent/...`): _TBD_
  - Streaming reply (`/reply` family): _TBD_
  - Sessions (list / get / fork / delete / insights) (`/sessions/...`): _TBD_
  - Config management (`/config/...`): _TBD_
  - Tool approval confirm/deny: _TBD_
- **Streaming/reasoning surface:** _TBD (does this version emit a distinct reasoning/thought event, or does reasoning arrive inline needing `<think>` splitting?)._

## Ollama

- **Pinned/tested version:** _TBD._
- **Binary location:** `C:\Users\azolkover\AppData\Local\Programs\Ollama\ollama.exe` (detected).
- **Endpoints used:** `GET /api/version`, `GET /api/tags`, `POST /api/pull` (NDJSON), `DELETE /api/delete`.

## Stock Goose Desktop detection (Phase 1 `conflict.rs`)

- **Process name(s) to match:** _TBD (record exact names after checking the pinned release)._

## File-writing tool names (Phase 4 artifacts heuristic)

- **Tool-name patterns treated as artifact producers:** _TBD (developer extension write/edit tools)._

## Installer URLs & hashes (Phase 7)

- **Ollama Windows installer:** _TBD (URL, silent-install flag, published size/hash)._
- **Goose installer:** _TBD._

## Starter models (Phase 7 `starter_models.ts`)

- **Curated ≤4B list:** _TBD (verify tags exist on ollama.com at implementation time)._

## Reasoning-capable models (Phase 10 `reasoning_models.ts`)

- **Name patterns → supports-reasoning:** _TBD._
