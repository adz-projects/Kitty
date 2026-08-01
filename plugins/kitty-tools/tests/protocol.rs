//! Protocol test — pins the exact tool surface `kitty-tools` exposes.
//!
//! This is the test protecting adaptive-pathway's bandit state (see the base
//! plan's "Critical constraint: tool names are load-bearing" section): the
//! Thompson bandit buckets tools by the literal name string
//! (`mmh3.hash(name, seed=137) % 64`), and both `edges.semantic_primitive`
//! and `action_history.action_name` key on it too. Renaming any entry below
//! silently orphans everything learned about that tool. Do not "tidy" this
//! array — a hard-coded sorted list is the point.

use kitty_tools::server::KittyToolsServer;

/// All 18 always-on tools this server exposes, sorted, plus 2 more
/// (`generate_accessible_table`/`generate_accessible_svg`) when
/// `KITTY_VIZ_ENABLED=1`. `lean_word_*` from the Word-only split, plus
/// shell, workspace, 5 file tools, 4 cache tools, 4 scratchpad tools.
///
/// Web search (`brave_mcp_search`, formerly gated here on `BRAVE_API_KEY`)
/// moved to the Python `kitty-docs-web` process as the merged, count-tiered
/// `lean_web_search`/`lean_web_search_read_chunk` — see `docs/VERSIONS.md`.
/// Along with the retirement of `lean_fallback_web_search`, this resets
/// adaptive-pathway's learned bandit state for those two old tool names —
/// expected fallout of the merge, not a regression (see this file's header
/// note on why tool names are load-bearing).
const ALWAYS_ON_TOOLS: &[&str] = &[
    "lean_analyze_workspace",
    "lean_cache_clear",
    "lean_cache_delete",
    "lean_cache_list",
    "lean_cache_view",
    "lean_file_append",
    "lean_file_read",
    "lean_file_replace_lines",
    "lean_file_replace_str",
    "lean_file_write",
    "lean_scratchpad_delete",
    "lean_scratchpad_get",
    "lean_scratchpad_list",
    "lean_scratchpad_set",
    "lean_shell",
    "lean_word_read_outline",
    "lean_word_read_text",
    "lean_word_write_doc",
];

// Only one env-gated axis remains (`KITTY_VIZ_ENABLED`) now that Brave search
// left this crate, so the previous brave+viz race (splitting this into two
// `#[test]` fns used to leak one test's env change into the other's
// assertion window) no longer applies — kept as a single test anyway since
// there's no benefit to splitting it further.
#[test]
fn tool_surface_matches_env_gating() {
    unsafe {
        std::env::remove_var("KITTY_VIZ_ENABLED");
    }

    let server = KittyToolsServer::new();
    let mut always_on: Vec<String> = ALWAYS_ON_TOOLS.iter().map(|s| s.to_string()).collect();
    always_on.sort();
    assert_eq!(server.tool_names(), always_on, "always-on tools must be exactly ALWAYS_ON_TOOLS with no env set");

    unsafe {
        std::env::set_var("KITTY_VIZ_ENABLED", "1");
    }

    let server = KittyToolsServer::new();
    let mut with_extras = always_on.clone();
    with_extras.push("generate_accessible_table".to_string());
    with_extras.push("generate_accessible_svg".to_string());
    with_extras.sort();
    assert_eq!(server.tool_names(), with_extras, "viz tools must join once KITTY_VIZ_ENABLED is set");

    unsafe {
        std::env::remove_var("KITTY_VIZ_ENABLED");
    }
}
