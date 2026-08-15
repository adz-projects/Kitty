# Adaptive Pathway — Rust ↔ sidecar HTTP contract

> **SUPERSEDED — historical.** Everything below describes the Python HTTP
> sidecar, which is **retired**: no longer built, bundled, spawned or
> supervised. The behavioral-memory engine is now
> `plugins/adaptive-pathway_rust/`, a path dependency statically linked into
> the BigTiny daemon, so recall is an in-process call on the agent loop rather
> than an HTTP hop. Kitty reaches it through the daemon
> (`src-tauri/src/bigtiny/pathway.rs`), not a socket of its own, and
> embeddings come from the daemon's own local engine via the `SemanticEmbedder`
> trait — there is no `AP_EMBED_OLLAMA_*` configuration and no Ollama
> anywhere in this path. `src-tauri/src/adaptive_pathway/` and
> `lifecycle/adaptive_pathway_proc.rs` are both deleted.
>
> Kept as the record of the contract the Rust port was verified against; see
> `docs/PLUGINS.md` and `CLAUDE.md` for how it actually works now.

`plugins/adaptive-pathway/` ships a FastAPI/uvicorn HTTP sidecar
(`src/adaptive_pathway/integrations/sidecar/server.py`) that Kitty spawns and
monitors directly (`src-tauri/src/lifecycle/adaptive_pathway_proc.rs`) —
unlike `replacement-mcp`, this plugin is **not** a goosed-spawned extension;
it's a plain child process Kitty owns for its lifetime.

Two separate things both call the sidecar:

1. **Kitty's Rust client** (`src-tauri/src/adaptive_pathway/mod.rs`) — plain
   REST calls, used by Settings UI (Graph Health, Domain Profiles, ensemble
   weight sliders) and the hint-feedback buttons in chat.
2. **The `adaptive-pathway` Goose MCP extension** — a *separate* process
   spawned by goosed (not documented here; it talks to the model via MCP
   tools like `decide`/`record_outcome`, not this HTTP surface). The two
   share the sidecar's embedding-model tag (`AP_EMBED_OLLAMA_MODEL`/
   `AP_EMBED_OLLAMA_URL`) so their vectors stay in the same embedding space —
   see `config::providers::env::goosed_env`'s doc comment.

This file documents only the first: the routes Kitty's Rust client actually
calls, as **verified against `adaptive_pathway/mod.rs` itself** — treat this
as the source of truth over `plugins/adaptive-pathway/docs/acp-endpoints.md`
(that file describes a broader, partly-aspirational ACP-shaped design; several
of its entries — `set_mode`, `set_preferences` — are explicitly **not
implemented** anywhere in the sidecar).

## Base URL

`http://127.0.0.1:{port}` — `port` defaults to `8700`
(`Config::adaptive_pathway_port`), overridable in Settings.

## Endpoints

All request/response bodies are opaque `serde_json::Value` on the Rust side
(no typed structs) — the sidecar is the schema's source of truth. `Ok(())` in
the table below means the Rust wrapper discards the body and only checks the
HTTP status.

| Rust fn | Method | Path | Used by |
|---|---|---|---|
| `get_state` | GET | `/state` | Settings status card, ensemble-weight sliders, Graph Health |
| `get_metrics` | GET | `/metrics` | Graph Health's `exploration_health` block (`/state` doesn't carry this) |
| `health` | GET | `/health` | Graph Health tab's issue list |
| `list_domains` | GET | `/domains` | Domain Profiles tab |
| `update_domain` | PUT | `/domains/{domain_id}` | Domain Profiles tab's edit action |
| `get_edge` | GET | `/edges/{edge_id}` | "Why was this suggested" hint detail |
| `record_annotation` | POST | `/annotation` | 👍👎💡🔄 feedback buttons |
| `record_outcome` | POST | `/outcome` | Not a `commands/adaptive_pathway.rs` command — called directly by `goosed::stream::track_and_maybe_record_outcome`, the best-effort backstop that auto-records an outcome from the ACP tool-call stream if the model itself never calls the `decide` extension's `record_outcome` MCP tool |
| `toggle_suggestions` | POST | `/suggestions/toggle` | Pause/resume header toggle |
| `get_schism` | GET | `/schism` | Schism Resolution modal detail |
| `resolve_schism` | POST | `/schism/resolve` | Schism Resolution modal actions (`keep_faction`: `"a"` \| `"b"` \| `"both"`) |
| `update_ensemble_weights` | PUT | `/config/ensemble` | Ensemble-weight sliders (`ig_weight_min`/`ig_weight_max`/`pc_weight`, all optional) |
| `accept_nudge` | POST | `/nudge/accept` | Exploration-consent prompt's Accept button |
| `dismiss_nudge` | POST | `/nudge/dismiss` | Exploration-consent prompt's Not now button |
| `get_session_reflection` | GET | `/session_reflection` | "See the roads not taken?" session-footer link |

Every command in `commands/adaptive_pathway.rs` gates on
`require_ok()` first — a dead/starting sidecar short-circuits with a plain
"Adaptive Pathway isn't running" error rather than a confusing connect
timeout, so a caller never needs to check the sidecar's status itself.

## Where the deeper design lives

`plugins/adaptive-pathway/docs/`:
- `acp-endpoints.md` — the broader conceptual endpoint set (includes
  not-yet-implemented ones; verify against the table above and
  `sidecar/server.py` before relying on anything there).
- `api-reference.md` — the underlying Python API (`AdaptivePathway` class)
  both the sidecar and the MCP extension wrap.
- `KNOWN_ISSUES.md` — open issues in the vendored plugin, including the
  `edge_id`/`attribution_id` passthrough quirk referenced in
  `src/stores/chat/types.ts`'s `AdaptivePathwayHint` doc comment.
