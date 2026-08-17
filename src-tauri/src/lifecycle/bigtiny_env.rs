//! Kitty's half of the `BIGTINY_*` environment contract.
//!
//! Kitty never writes a `--config` YAML for the daemon, so this is the *only*
//! channel these settings reach it by. The daemon's half is
//! `bigtiny_rust::env_contract::apply_env_overrides`; the two must stay in
//! lockstep, and a variable set here that isn't read there (or vice versa) is
//! silently ignored rather than reported.
//!
//! **Built as pairs rather than applied to a `Command`, because there are two
//! hosts.** Desktop spawns the daemon and passes these as child-process env
//! (`bigtiny_proc::spawn`). Android links the daemon in and has no child to
//! configure, so it sets the same pairs on its *own* process before calling
//! `bigtiny_rust::run` (`bigtiny_embedded::start`). Returning data keeps one
//! definition serving both; two copies of a ~40-variable list would drift on
//! the first change and fail as a silently-unapplied setting.

/// Locate the bundled LiteRT resources (Gemma `tokenizer.json` + the runtime
/// DLLs) and return `(tokenizer_path, litert_lib_dir)`.
///
/// **The nuance that broke 0.7.0/0.7.1's first cut of this**: Tauri's
/// `resource_dir()` on Windows resolves to the **executable's own directory**,
/// not an `<exe_dir>/resources` subfolder — see
/// `tauri_utils::platform::resource_dir_from`, which special-cases
/// `cfg!(target_os = "windows")` to just return `exe_dir`. But
/// `bundle.resources` entries in `tauri.conf.json` are declared as
/// `"resources/libLiteRt.dll"` etc. (matching this repo's `src-tauri/resources/`
/// source layout), and NSIS preserves that relative path, so the files
/// actually land at `<exe_dir>/resources/*.dll` on disk. Joining a **bare**
/// filename onto `resource_dir()` therefore looked in `<exe_dir>` itself —
/// once removed from where the DLLs really are — silently found nothing, and
/// the daemon shipped with no tokenizer and a `PATH` entry pointing at the
/// wrong directory, hence the "litert-lm.dll was not found" popup even though
/// the DLL was sitting right there in `resources/`.
///
/// The fix is to join the **same relative path this crate declared in
/// `bundle.resources`** (`"resources"`), not a bare filename — mirroring what
/// `app.path().resolve("resources/…", BaseDirectory::Resource)` would do.
///
/// On Android, `resource_dir()` returns a `asset://` URI, not a filesystem
/// path, so `is_file()` is false and the tokenizer path comes back empty
/// (`daemon_env` then falls back to the models dir — the `.so` there comes
/// from the APK `jniLibs`, not from here).
pub fn locate_litert_resources(app: &tauri::AppHandle) -> (String, String) {
    use tauri::Manager;
    match app.path().resource_dir() {
        Ok(exe_or_resource_dir) => {
            // See the doc comment above: on Windows this is the exe's own
            // directory, and the bundled resources actually live one level
            // down, at the same relative path they were declared with.
            let res = exe_or_resource_dir.join("resources");
            let tok = res.join("tokenizer.json");
            let tok = if tok.is_file() {
                tok.to_string_lossy().into_owned()
            } else {
                String::new()
            };
            (tok, res.to_string_lossy().into_owned())
        }
        Err(_) => (String::new(), String::new()),
    }
}

/// Every `BIGTINY_*` variable the daemon should see, in a stable order.
///
/// `secret` and `encryption_key` are separate parameters rather than config
/// fields because they have different lifetimes and sources: the secret is
/// regenerated every launch, while the encryption key is stable across
/// restarts (rotating it would make previously-encrypted rows in BigTiny's
/// own DB undecryptable).
#[allow(clippy::too_many_arguments)]
pub fn daemon_env(
    secret: &str,
    encryption_key: &str,
    summarizer: &crate::config::SummarizerSettings,
    token_management: &crate::config::TokenManagementSettings,
    memory: &crate::config::MemorySettings,
    local: &crate::config::LocalModelSettings,
    pathway_enabled: bool,
    pathway_embedding_model: &str,
    // Absolute path to the bundled Gemma `tokenizer.json`, resolved by the
    // caller from `resource_dir()` (it ships as an app resource, not in the
    // models dir). Empty falls back to `models::resolve("tokenizer.json")` so a
    // dev run that dropped the file into the models dir still works.
    tokenizer_path: &str,
) -> Vec<(String, String)> {
    let b = |v: bool| if v { "true" } else { "false" }.to_string();
    let mut env: Vec<(String, String)> = vec![
        ("BIGTINY_SECRET".into(), secret.to_string()),
        ("BIGTINY_ENCRYPTION_KEY".into(), encryption_key.to_string()),
        (
            "BIGTINY_SUMMARIZER__ENABLED".into(),
            b(summarizer.enabled),
        ),
        (
            "BIGTINY_TOKEN_MANAGEMENT__MAX_CONTEXT_TOKENS".into(),
            token_management.max_context_tokens.to_string(),
        ),
        (
            "BIGTINY_TOKEN_MANAGEMENT__MAX_LIVE_TAIL_TOKENS".into(),
            token_management.max_live_tail_tokens.to_string(),
        ),
        (
            "BIGTINY_TOKEN_MANAGEMENT__MESSAGE_MASK_HEAD_LINES".into(),
            token_management.message_mask_head_lines.to_string(),
        ),
        (
            "BIGTINY_TOKEN_MANAGEMENT__MESSAGE_MASK_TAIL_LINES".into(),
            token_management.message_mask_tail_lines.to_string(),
        ),
        // `PathwayConfig::enabled` defaults to `false` inside BigTiny and
        // (unlike every other section) has no other override path, so without
        // this the behavioral-memory engine can never turn on at all.
        ("BIGTINY_PATHWAY__ENABLED".into(), b(pathway_enabled)),
    ];

    // Model paths are resolved here rather than in the daemon, so the daemon
    // never needs to know where Kitty keeps models. An unresolvable name
    // yields an empty value, which the daemon reads as "that slot is
    // unconfigured" — chat falls back to the active provider, embeddings to
    // lexical hashing. Both are degradations, neither is an error.
    // LiteRT engine paths (successor to the llama.cpp `BIGTINY_LOCAL__*` block).
    // Resolved host-side because the daemon has no idea where Kitty keeps
    // models. `summarizer.model` now names the generative `.litertlm`;
    // `pathway_embedding_model` names the EmbeddingGemma `.tflite`; the Gemma
    // `tokenizer.json` is a bundled resource placed in the models dir. An empty
    // value means that slot is unconfigured, which the daemon degrades on
    // (embeddings -> lexical hashing, summarizer -> remote session model) rather
    // than failing to start. The llama-only knobs (`n_ctx`, `n_gpu_layers`,
    // `backend`, cache types) are no longer sent — LiteRT has no equivalent.
    let embed_tflite = crate::models::resolve(pathway_embedding_model)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Prefer the caller-resolved bundled resource path; fall back to the models
    // dir for a dev run without the packaged resource.
    let tokenizer = if !tokenizer_path.trim().is_empty() {
        tokenizer_path.to_string()
    } else {
        crate::models::resolve("tokenizer.json")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let summarizer_litertlm = crate::models::resolve(&summarizer.model)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Bare filename: loaded via `Library::from_path`, which dlopen/LoadLibrary
    // resolves through the OS search path — the APK `jniLibs` dir on Android and
    // the daemon's own directory on Windows, where each is bundled.
    let lib_name = if cfg!(target_os = "android") {
        "libLiteRt.so"
    } else {
        "libLiteRt.dll"
    };
    // The engine needs both the model and its tokenizer to embed at all.
    let litert_enabled = !embed_tflite.is_empty() && !tokenizer.is_empty();
    let _ = local; // llama.cpp engine knobs are retired; kept in the signature
                   // for now so callers are unchanged during the transition.
    if !litert_enabled {
        tracing::info!(
            embedding_model = %pathway_embedding_model,
            "EmbeddingGemma tflite or tokenizer.json not found; LiteRT engine stays off"
        );
    }

    env.extend([
        ("BIGTINY_LITERT__ENABLED".to_string(), b(litert_enabled)),
        ("BIGTINY_LITERT__LIB_PATH".to_string(), lib_name.to_string()),
        (
            "BIGTINY_LITERT__EMBED_MODEL_PATH".to_string(),
            embed_tflite,
        ),
        ("BIGTINY_LITERT__TOKENIZER_PATH".to_string(), tokenizer),
        (
            "BIGTINY_LITERT__SUMMARIZER_MODEL_PATH".to_string(),
            summarizer_litertlm,
        ),
    ]);

    // Set only when configured. Leaving the variable absent (rather than
    // `""`) is what keeps `None` the daemon's genuinely-unset default.
    if let Some(threshold) = memory.bm25_threshold {
        env.push((
            "BIGTINY_MEMORY__BM25_THRESHOLD".to_string(),
            threshold.to_string(),
        ));
    }
    // Consolidates BigTiny's db / cache-sandbox-root / recipes under Kitty's
    // own data dir instead of its standalone `~/.bigtiny` default.
    // Best-effort: if this can't be resolved the daemon just uses that
    // default rather than failing to start.
    if let Ok(data_dir) = crate::config::bigtiny_data_dir() {
        env.push((
            "BIGTINY_DATA_DIR".to_string(),
            data_dir.to_string_lossy().into_owned(),
        ));
    }

    env
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model name nothing will ever resolve. `daemon_env` calls
    /// `models::resolve`, which reads the *real* models directory — so a test
    /// using the default summarizer model passes or fails depending on
    /// whether the developer happens to have that GGUF downloaded. Naming a
    /// model that cannot exist is what makes these deterministic.
    const NO_SUCH_MODEL: &str = "test-model-that-is-never-installed";

    fn settings() -> (
        crate::config::SummarizerSettings,
        crate::config::TokenManagementSettings,
        crate::config::MemorySettings,
        crate::config::LocalModelSettings,
    ) {
        (
            crate::config::SummarizerSettings {
                model: NO_SUCH_MODEL.to_string(),
                ..Default::default()
            },
            crate::config::TokenManagementSettings::default(),
            crate::config::MemorySettings::default(),
            crate::config::LocalModelSettings::default(),
        )
    }

    fn env_of(pairs: &[(String, String)], key: &str) -> Option<String> {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    /// The secret and the encryption key are the two values that must reach
    /// the daemon or nothing works: no auth, and no decryptable provider
    /// keys.
    #[test]
    fn the_credentials_are_always_present() {
        let (s, t, m, l) = settings();
        let e = daemon_env("sec", "enc", &s, &t, &m, &l, false, "", "");
        assert_eq!(env_of(&e, "BIGTINY_SECRET").as_deref(), Some("sec"));
        assert_eq!(env_of(&e, "BIGTINY_ENCRYPTION_KEY").as_deref(), Some("enc"));
    }

    /// Booleans go over as `true`/`false`, which is one of the two forms
    /// `apply_env_overrides` accepts. Rust's `Display` for `bool` happens to
    /// agree, but relying on that coincidence is how `1`/`0` sneaks in later.
    #[test]
    fn booleans_use_the_word_form_the_daemon_parses() {
        let (s, t, m, l) = settings();
        let on = daemon_env("", "", &s, &t, &m, &l, true, "", "");
        let off = daemon_env("", "", &s, &t, &m, &l, false, "", "");
        assert_eq!(env_of(&on, "BIGTINY_PATHWAY__ENABLED").as_deref(), Some("true"));
        assert_eq!(
            env_of(&off, "BIGTINY_PATHWAY__ENABLED").as_deref(),
            Some("false")
        );
    }

    /// An unresolvable embedding model must leave the LiteRT slot empty and the
    /// engine off — the daemon reads empty as "slot unconfigured" and degrades
    /// (embeddings fall back to lexical hashing), which is the intended
    /// behaviour.
    #[test]
    fn an_unresolvable_model_leaves_the_slot_empty_and_the_engine_off() {
        let (s, t, m, l) = settings();
        let e = daemon_env("", "", &s, &t, &m, &l, false, NO_SUCH_MODEL, "");
        assert_eq!(
            env_of(&e, "BIGTINY_LITERT__EMBED_MODEL_PATH").as_deref(),
            Some("")
        );
        assert_eq!(
            env_of(&e, "BIGTINY_LITERT__ENABLED").as_deref(),
            Some("false")
        );
    }

    /// The LiteRT path variables are always sent (even with nothing resolvable),
    /// so a model downloaded later is picked up without the daemon falling back
    /// to its own hardcoded defaults. The `LIB_PATH` bare name is always set.
    #[test]
    fn the_litert_paths_are_sent_even_with_no_model() {
        let (s, t, m, l) = settings();
        let e = daemon_env("", "", &s, &t, &m, &l, false, NO_SUCH_MODEL, "");
        assert_eq!(
            env_of(&e, "BIGTINY_LITERT__ENABLED").as_deref(),
            Some("false")
        );
        for key in [
            "BIGTINY_LITERT__LIB_PATH",
            "BIGTINY_LITERT__EMBED_MODEL_PATH",
            "BIGTINY_LITERT__TOKENIZER_PATH",
            "BIGTINY_LITERT__SUMMARIZER_MODEL_PATH",
        ] {
            assert!(env_of(&e, key).is_some(), "{key} should always be sent");
        }
    }

    /// An unset bm25 threshold must be *absent*, not empty: the daemon parses
    /// this one, and `""` would fail the parse and leave the default in place
    /// by accident rather than by design.
    #[test]
    fn an_unset_bm25_threshold_is_omitted_rather_than_blank() {
        let (s, t, mut m, l) = settings();
        m.bm25_threshold = None;
        let e = daemon_env("", "", &s, &t, &m, &l, false, "", "");
        assert!(env_of(&e, "BIGTINY_MEMORY__BM25_THRESHOLD").is_none());

        m.bm25_threshold = Some(1.5);
        let e = daemon_env("", "", &s, &t, &m, &l, false, "", "");
        assert_eq!(
            env_of(&e, "BIGTINY_MEMORY__BM25_THRESHOLD").as_deref(),
            Some("1.5")
        );
    }

    /// No duplicate keys. `Command::envs` would silently take the last, and
    /// `set_var` likewise — a duplicate would make the effective value depend
    /// on ordering nobody is looking at.
    #[test]
    fn no_key_is_emitted_twice() {
        let (s, t, m, l) = settings();
        let e = daemon_env("", "", &s, &t, &m, &l, true, "", "");
        let mut keys: Vec<&str> = e.iter().map(|(k, _)| k.as_str()).collect();
        let before = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(before, keys.len(), "duplicate key in the daemon env");
    }
}
