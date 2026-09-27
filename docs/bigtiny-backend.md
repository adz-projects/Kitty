# BigTiny backend

Kitty is driven by **BigTiny V2** (`BigTinyV2/daemon`, binary
`bigtiny2-daemon`), a chat-first REST/SSE engine and Kitty's only backend. It
is multi-app: on desktop Kitty is one *client* of a shared engine, not its
owner. On Android the same engine is linked in and hosted in-process. V1
(`plugins/bigtiny_rust/`) is frozen and unused; a V1 install's data is
imported from the hub (`commands/v1_import.rs` → `POST /api/apps/me/import-v1`).

`BigTinyV2/API.md` is the source of truth for routes. This page is the contract
from Kitty's side.

## Finding the engine

- **Desktop** (`lifecycle/bigtiny_v2.rs`, over `bigtiny2-client`): read
  `%APPDATA%\BigTinyV2\daemon.json`, check the process behind it is a V2 engine
  with API version ≥ 2, and spawn one under a lock only when none is running.
  `bigtiny_command` / `bigtiny_args` in `config.json` default to the bundled
  `bigtiny2-daemon.exe`; in a source checkout with no bundled exe they fall
  back to `cargo run --manifest-path <repo>/BigTinyV2/daemon/Cargo.toml --bin
  bigtiny2-daemon`. The engine picks its own port and keeps its own data
  directory (`%APPDATA%\BigTinyV2`) and at-rest key (DPAPI-protected).
- **Android** (`lifecycle/bigtiny_embedded.rs`): started in-process on a
  loopback port, with its data under the app's private directory and its
  at-rest key supplied from the SecretStore.
- **Identity** (`lifecycle/bigtiny_app_key.rs`): Kitty registers once as the
  app `kitty` and keeps the issued key in the secret store, sending it as
  `X-API-Key`. At attach the stored key is checked (`GET /api/apps/me`); a
  lost key is reclaimed with the handshake's registration token
  (`POST /api/apps/reclaim`), keeping Kitty's data.
- **Health** (`lifecycle/health.rs`): `GET /api/health` (open, by design).
  When the engine stops answering, Kitty re-reads the handshake and re-attaches
  if the engine came back elsewhere.
- **Restart** (`lifecycle/engine_restart.rs`): Kitty never kills the engine.
  When a start-up setting changes it calls `POST /api/admin/restart`; the
  engine refuses while another app is attached or busy and names it, and
  `force` overrides only the other-app check. Android applies such settings
  the next time Kitty starts.
- **Uninstall** (`uninstall.rs`): `kitty.exe --uninstall-cleanup` calls
  `DELETE /api/apps/me?purge=true`, which removes only Kitty's data.

## What the Rust layer does (`src-tauri/src/bigtiny/`)

- **Sessions** (`sessions.rs`): create, page (`offset`/`limit` with a total),
  full-text search (`/api/search`), load, fork, rename, delete. `load` replays
  history as `chat://user-message` (with attachment chips, scaffolding
  stripped — `turn_text.rs`), `chat://reasoning-delta` (stored reasoning),
  `chat://message-delta` and `chat://tool-call` events.
- **Streaming** (`stream.rs`): `POST /api/chat/{id}/send` SSE frames →
  `chat://message-delta`, `chat://reasoning-delta`, `chat://tool-call`
  (`is_error` honoured; results over 100 KB truncated, fetchable in full),
  `chat://notice` (model failover, step limit), `chat://complete`,
  `chat://error` with a typed `error_type`. 15 s keepalives reset the idle
  timeout, which is the chat's own provider's. `/api/chat/{id}/stream` follows
  a turn running elsewhere (a specialist being watched).
- **App events** (`lifecycle/app_events.rs`): `GET /api/apps/me/events` — HITL
  pauses and resolutions for any of Kitty's chats, schedule runs, titles.
  Approvals are decided in `approvals.rs` and answered with
  `POST /api/chat/{id}/approve` (with an `args_pattern` for a scoped "always
  allow"); `GET /api/apps/me/pending` recovers any that paused while Kitty was
  not listening.
- **Providers** (`providers.rs`): every card is synced into the engine
  (`POST/PATCH/DELETE /api/providers`), with every optional setting sent
  (null when unset) so a cleared value clears, plus `supports_tools` and the
  delegate-host hints. The default card is Kitty's per-app default.
- **MCP servers** (`mcp.rs`): CRUD over `/api/mcp/servers`, and
  `ensure_builtin_servers`, which keeps the bundled servers registered against
  the current install and reports what it could not sync.
- **Specialists, schedules, memory**: thin wrappers over `/api/specialists`,
  `/api/schedules`, `/api/pathway/*`, `/api/memorabilia/*`; the memory engines
  are switched per app with `PUT /api/apps/me/plugins/{plugin}`.

## Deliberately different from the old goosed/ACP path

- **No approval modes**: one policy, decided by Kitty (see `approvals.rs`),
  with scoped "always allow" rules the user can revoke.
- **Specialists replaced recipes**: the model delegates; there is no `/slug`.
- **No context-strategy setting**: compaction is the engine's, with the local
  summarizer (Windows, optional) or the chat's provider.
