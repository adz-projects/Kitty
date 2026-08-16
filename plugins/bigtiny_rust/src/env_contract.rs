//! The `BIGTINY_*` environment-variable contract, and the path resolution
//! that goes with it.
//!
//! **Lives in the library, not in `bin/bigtiny_daemon.rs`, because it has two
//! callers.** The CLI is one; an *embedding* host that links this crate and
//! calls [`crate::run`] directly is the other — which is how the daemon runs
//! on Android, where there is no separate executable to spawn (D8, §2.3).
//! Several comments below warn that Kitty's spawn code is "the other half of
//! this contract and must stay in lockstep"; keeping one copy is what makes
//! that a two-way agreement rather than a three-way one.
//!
//! This is deliberately **not** a general nested-env deserializer. It honors
//! exactly the variables a host actually sets, so an unrecognised
//! `BIGTINY_*` name is an error the author can see rather than a silent
//! no-op.

use std::path::PathBuf;

use crate::config::BigTinyConfig;

/// `BIGTINY_DATA_DIR` env var, or `~/.bigtiny` — matches
/// `plugins/bigtiny/bigtiny/paths.py::data_dir()` exactly, since Kitty's
/// `bigtiny_proc.rs::spawn` points this at `%APPDATA%/Kitty/bigtiny/`.
pub fn resolve_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("BIGTINY_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs_home().join(".bigtiny")
}

pub fn dirs_home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Expand a leading `~/` in a config-supplied path against the resolved
/// home directory, matching Python's `Path(...).expanduser()`.
pub fn shellexpand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => dirs_home().join(rest),
        None => PathBuf::from(path),
    }
}

/// Applies the specific `BIGTINY_SUMMARIZER__*`/`BIGTINY_TOKEN_MANAGEMENT__*`/
/// `BIGTINY_MEMORY__*` env vars Kitty's spawn code sets — see this file's
/// module doc for why only these, not a general nested-env-var deserializer.
pub fn apply_env_overrides(config: &mut BigTinyConfig) {
    if let Ok(v) = std::env::var("BIGTINY_SUMMARIZER__ENABLED") {
        config.summarizer.enabled = v.eq_ignore_ascii_case("true") || v == "1";
    }
    // `MODEL`/`KEEP_ALIVE` are gone: those named an Ollama tag and an
    // Ollama-native `keep_alive` value for the now-deleted Ollama-only
    // `SummarizerClient`. The local summarizer's model comes from
    // `BIGTINY_LOCAL__MODEL_PATH` instead, and residency is the slot
    // manager's job, not a per-call keep-alive knob.
    if let Ok(v) = std::env::var("BIGTINY_SUMMARIZER__FALLBACK") {
        config.summarizer.fallback = v;
    }
    if let Some(n) = std::env::var("BIGTINY_TOKEN_MANAGEMENT__MAX_CONTEXT_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.token_management.max_context_tokens = n;
    }
    if let Some(n) = std::env::var("BIGTINY_TOKEN_MANAGEMENT__MAX_LIVE_TAIL_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.token_management.max_live_tail_tokens = n;
    }
    if let Some(n) = std::env::var("BIGTINY_TOKEN_MANAGEMENT__MESSAGE_MASK_HEAD_LINES")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.token_management.message_mask_head_lines = n;
    }
    if let Some(n) = std::env::var("BIGTINY_TOKEN_MANAGEMENT__MESSAGE_MASK_TAIL_LINES")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.token_management.message_mask_tail_lines = n;
    }
    if let Ok(v) = std::env::var("BIGTINY_MEMORY__PREFLIGHT_ENABLED") {
        config.memory.preflight_enabled = v.eq_ignore_ascii_case("true") || v == "1";
    }
    if let Some(n) = std::env::var("BIGTINY_MEMORY__BM25_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.memory.bm25_threshold = Some(n);
    }
    if let Some(n) = std::env::var("BIGTINY_MEMORY__PREFLIGHT_RESULTS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.memory.preflight_results = n;
    }
    if let Some(n) = std::env::var("BIGTINY_MEMORY__ARTIFACTS_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.memory.artifacts_max_tokens = n;
    }
    // `PathwayConfig::enabled` defaults to `false` and, unlike every other
    // config section above, previously had NO env override at all. Since
    // Kitty (like every host) never passes a `--config` YAML, that made the
    // in-process behavioral-memory engine permanently dead in every real
    // deployment regardless of anything the host does -- this is the actual
    // toggle a host needs to opt in, mirroring `BIGTINY_SUMMARIZER__*`.
    if let Ok(v) = std::env::var("BIGTINY_PATHWAY__ENABLED") {
        config.pathway.enabled = v.eq_ignore_ascii_case("true") || v == "1";
    }
    if let Some(n) = std::env::var("BIGTINY_PATHWAY__LEARN_EVERY_N")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.pathway.learn_every_n = n;
    }
    // The in-process **LiteRT** engine (docs plan "Replace llama.cpp with
    // LiteRT"). Same reasoning as `BIGTINY_PATHWAY__ENABLED` above: no host
    // passes `--config`, so without these the engine can only ever be off.
    // Kitty's `lifecycle/bigtiny_env.rs` is the other half of this contract and
    // must stay in lockstep.
    //
    // Paths, not model names: the daemon has no idea where a host keeps its
    // models, and resolving on this side would duplicate that knowledge. (The
    // retired llama.cpp engine's `BIGTINY_LOCAL__*` block lived here.)
    if let Ok(v) = std::env::var("BIGTINY_LITERT__ENABLED") {
        config.litert.enabled = v.eq_ignore_ascii_case("true") || v == "1";
    }
    if let Ok(v) = std::env::var("BIGTINY_LITERT__LIB_PATH") {
        config.litert.lib_path = v;
    }
    if let Ok(v) = std::env::var("BIGTINY_LITERT__EMBED_MODEL_PATH") {
        config.litert.embed_model_path = v;
    }
    if let Ok(v) = std::env::var("BIGTINY_LITERT__TOKENIZER_PATH") {
        config.litert.tokenizer_path = v;
    }
    if let Ok(v) = std::env::var("BIGTINY_LITERT__SUMMARIZER_MODEL_PATH") {
        config.litert.summarizer_model_path = v;
    }

    // Env overrides write the same fields `BigTinyConfig::load` sanitizes,
    // but bypassed the clamp — e.g. `..._MASK_HEAD_LINES=-5` reached the
    // `as usize` slicing sites in compaction as a huge index. Close the gap.
    config.token_management.sanitize();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env vars are process-global, so every override this contract honors
    /// is exercised from ONE test (parallel tests setting the same vars
    /// would race), and each var is removed again on the way out.
    #[test]
    fn env_overrides_are_sanitized_and_booleans_parse_leniently() {
        // 815bugs #98: overrides bypassed `TokenManagementConfig::sanitize()`.
        std::env::set_var("BIGTINY_TOKEN_MANAGEMENT__MESSAGE_MASK_HEAD_LINES", "-5");
        std::env::set_var("BIGTINY_TOKEN_MANAGEMENT__MESSAGE_MASK_TAIL_LINES", "-3");
        // 815bugs #99: `=1` (and `=FALSE`) were silently ignored by the strict
        // bool parse every neighbor doesn't use. Exercised here on the LiteRT
        // enable flag (the retired `BIGTINY_LOCAL__TOOL_CALLS` used to cover it).
        std::env::set_var("BIGTINY_LITERT__ENABLED", "1");

        let mut config = BigTinyConfig::default();
        apply_env_overrides(&mut config);

        assert_eq!(config.token_management.message_mask_head_lines, 0);
        assert_eq!(config.token_management.message_mask_tail_lines, 0);
        assert!(config.litert.enabled);

        std::env::set_var("BIGTINY_LITERT__ENABLED", "FALSE");
        let mut config = BigTinyConfig::default();
        apply_env_overrides(&mut config);
        assert!(!config.litert.enabled, "=FALSE must mean false, not be ignored");

        std::env::remove_var("BIGTINY_TOKEN_MANAGEMENT__MESSAGE_MASK_HEAD_LINES");
        std::env::remove_var("BIGTINY_TOKEN_MANAGEMENT__MESSAGE_MASK_TAIL_LINES");
        std::env::remove_var("BIGTINY_LITERT__ENABLED");
    }
}
