//! Loop-integrated plugins, hosted per app.
//!
//! # Plugin vs MCP server
//!
//! These are two different things and this module exists to keep them apart:
//!
//! | | **Plugin** (here) | **MCP server** (`crate::mcp`) |
//! |---|---|---|
//! | Example | adaptive pathway | `kitty-tools`, `kitty-web`, `kitty-wasm` |
//! | Integration | hooks the agent loop — context injection, per-turn recall, post-turn learning, background maintenance, its own routes | tools only, across the MCP boundary |
//! | Also exposes tools | yes (`record`/`forget`) — but incidentally | that is all it does |
//! | Per-app requirement | a per-app **instance**, because its state is memory about that app's usage | per-app **selection** |
//!
//! **The axis is loop integration, not statefulness.** `kitty-tools` is
//! stateful — scratchpad, extract-once doc cache — and is still only an MCP
//! server. Pathway is a plugin because it runs *inside* the turn.
//!
//! On Android everything is compiled into one binary and hosted in-process, so
//! the packaging collapses. The architectural distinction does not, and this
//! module is where it stays visible: the phone simply instantiates both kinds
//! in one process.
//!
//! # Why per-app instances
//!
//! V1 opened exactly one `PathwayEngine` for the whole daemon. That was
//! coherent with one client. With several, a single graph would mix apps'
//! beliefs together — both a privacy leak (a research pipeline's inferences
//! leaking into a chat app's context) and a quality regression, since beliefs
//! blended across two very different usage patterns describe nobody in
//! particular.
//!
//! What is emphatically *not* per-app is the **embedder**: it is a loaded
//! model in memory, and one per app would multiply RAM by app count for no
//! benefit. It is shared by `Arc`, which also keeps every app's vectors in one
//! comparable space.

pub mod host;

pub use host::PluginHost;

/// A `PluginHost` for tests: pathway **off** by default, state under a temp
/// dir.
///
/// Off so no test accidentally opens a belief graph it did not ask for; a test
/// that wants one sets the per-app preference explicitly. Uses a real host
/// rather than a mock, so the per-app instance behaviour under test is the
/// same code the daemon runs.
pub fn test_plugin_host(pool: &sqlx::SqlitePool) -> std::sync::Arc<crate::plugins::PluginHost> {
    let config = crate::config::BigTinyConfig::default();
    let summarizer = std::sync::Arc::new(crate::agent::summarizer_chain::SummarizerChain::new(
        None,
        std::sync::Arc::new(crate::provider::router::ProviderRouter::new(
            config.cache.clone(),
        )),
        config.summarizer.clone(),
    ));
    std::sync::Arc::new(crate::plugins::PluginHost::new(
        pool.clone(),
        std::env::temp_dir().join("bigtiny2-test-plugins"),
        false,
        adaptive_pathway::config::Config::default(),
        None,
        summarizer,
    ))
}
