# Plugins vs MCP servers

BigTiny V2 hosts two kinds of extension. They look similar from outside — both
end up giving the model new capabilities — and they are built and hosted
completely differently. Getting this wrong is invisible until two apps run at
once, so decide which one you are writing before you start.

| | **Plugin** | **MCP server** |
|---|---|---|
| Example | adaptive pathway | `kitty-tools`, `kitty-web`, `kitty-wasm` |
| Integration | hooks the agent loop: context injection, per-turn recall, post-turn learning, background maintenance, its own routes | tools only, across the MCP boundary |
| Also exposes tools? | yes (`record`/`forget`) — but incidentally | that is the whole job |
| Lives in | `daemon/src/plugins/`, compiled in | a row in `mcp_servers`, any transport |
| Per-app model | a per-app **instance** | per-app **selection** |
| Configured by | `PUT /api/apps/me/plugins/{plugin}` | `POST /api/mcp/servers` |

## The line is loop integration, not statefulness

This is the distinction people get wrong. `kitty-tools` is stateful — it owns a
scratchpad and an extract-once document cache — and it is still only an MCP
server, because it never runs inside a turn. Pathway is a plugin because it
does: recall before the LLM call, learning after it, a background sweep
between.

If your extension only answers `tools/call`, it is an MCP server no matter how
much state it keeps.

## Writing a plugin

A plugin gets a **per-app instance**, because its state is memory about how one
app is used. Merging two apps' state would be both a privacy leak and a quality
regression — beliefs blended across two unrelated usage patterns describe
nobody in particular.

Three rules, learned from pathway:

1. **Instances are lazy and closable.** `PluginHost` opens one on first use and
   closes it on request. A registered app that never sends a turn must cost
   neither a file nor a task. V1 spawned exactly one background sweep because
   there was exactly one engine; carrying that forward naively gives you one
   sweep per registered app, forever.
2. **Share the expensive parts explicitly.** The embedding model is one loaded
   `Arc<dyn SemanticEmbedder>` shared by every instance. Per-app would multiply
   RAM by app count *and* put each app's vectors in an incomparable space.
   Decide, per resource, whether it is per-app state or a machine resource.
3. **Off must cost nothing.** Disabling a plugin means no instance, no
   background work, no files — not merely hidden tools. And disabling must
   *close* a live instance, or it keeps running until restart.

Add the name to `KNOWN_PLUGINS` in `daemon/src/routes/plugins.rs` so an unknown
name stays a 404 rather than a stored preference nothing reads.

## Writing an MCP server

Nothing new is needed: register it against `/api/mcp/servers` with an `app_id`.
Visibility and mutability are deliberately different questions:

|                        | own private row | shared (`app_id IS NULL`) | another app's |
|------------------------|-----------------|---------------------------|---------------|
| list / use             | yes             | yes                       | no (404)      |
| patch / connect / delete | yes           | **no (403)**              | no (404)      |

A shared server's `command` is what *every* app's agent loop executes, so
letting one app repoint it would be a code-execution vector against the others.
Shared rows are immutable through the API, including to the app that created
them.

### Per-app state in an MCP server

`kitty-tools` resolves its home **once per process** from `KITTY_PLUGIN_HOME`
(`plugins/kitty-tools/src/paths.rs`), and its scratchpad and document cache
hang off that. Two apps sharing one connection would share a scratchpad.

On Windows, `mcp::manager::scoped_env` gives each app's stdio child
`apps/<app_id>/plugin-home`, which works because a stdio server is a separate
process per row. An explicit `KITTY_PLUGIN_HOME` in the row's own `env` wins —
that is the operator speaking.

**Android is exempt by design.** Its in-process transport is one process with
one environment, so this cannot work there — but Android hosts exactly one app,
so process-global *is* app-scoped. That is a platform difference, not deferred
work.

The content-addressed download cache (`~/.cache/lean-goose-mcp`) stays shared
on both platforms: it is keyed by content, so sharing it is safe and useful.

## Android packaging

Android compiles everything into one binary and hosts it in-process, so the
*packaging* distinction collapses. The architectural one does not. The phone
simply instantiates both kinds in one process; do not let that tempt you into
merging the two models in code.
