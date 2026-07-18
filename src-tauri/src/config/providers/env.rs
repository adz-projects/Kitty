//! Environment assembly for `goose serve`, built from the active provider
//! profile + global model params.

use super::keyring::get_secret;
use crate::config::Config;

/// Map our provider_type to Goose's `GOOSE_PROVIDER` value. `pub(crate)` so
/// `commands::session::rebind_session_provider` can reuse the exact same
/// mapping when hot-rebinding an already-open session's provider/model via
/// `session/set_config_option`, rather than duplicating it.
pub(crate) fn goose_provider_name(provider_type: &str) -> &str {
    match provider_type {
        "custom_openai" => "openai",
        other => other,
    }
}

/// Build the environment for `goose serve` from the active provider profile +
/// model params. Empty when no profile is active (goosed uses its own config).
pub fn goosed_env(config: &Config) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();

    if let Some(active) = config
        .active_provider_id
        .as_ref()
        .and_then(|id| config.providers.iter().find(|p| &p.id == id))
    {
        env.push((
            "GOOSE_PROVIDER".into(),
            goose_provider_name(&active.provider_type).into(),
        ));
        if let Some(model) = active.models.first() {
            env.push(("GOOSE_MODEL".into(), model.clone()));
        }
        let secret = get_secret(&active.id);
        match active.provider_type.as_str() {
            "ollama" => env.push(("OLLAMA_HOST".into(), active.base_url.clone())),
            "openrouter" => {
                // OpenRouter has no dedicated native client in goosed — it's
                // an OpenAI-compatible endpoint, and goosed's OpenAI-family
                // client is what actually sends the request regardless of
                // `GOOSE_PROVIDER`'s label. Confirmed by a real failure:
                // switching to an OpenRouter profile mid-chat sent the
                // request to the OpenAI client's own *default* base URL
                // (`api.openai.com`) with no key at all — i.e. goosed read
                // neither `OPENROUTER_API_KEY` nor anything OpenRouter-
                // specific for the actual HTTP call; it needed the standard
                // `OPENAI_*` names. Set both families so this works
                // regardless of which one goosed's OpenRouter handling
                // actually reads.
                if let Some(s) = secret {
                    env.push(("OPENROUTER_API_KEY".into(), s.clone()));
                    env.push(("OPENAI_API_KEY".into(), s));
                }
                // goosed's OpenAI-compatible client appends `/v1/chat/completions`
                // onto whatever base URL it's given — but OpenRouter's own
                // canonical base URL (`DEFAULT_URL.openrouter` /
                // https://openrouter.ai/api/v1) already ends in `/v1`.
                // Passing it through unstripped produced a real, confirmed
                // failure: a doubled path, `.../api/v1/v1/chat/completions`
                // (404). Strip the trailing `/v1` before handing it to the
                // OPENAI_* vars, which expect a bare base with no `/v1` of
                // its own.
                let openai_base = active.base_url.trim_end_matches('/');
                let openai_base = openai_base.strip_suffix("/v1").unwrap_or(openai_base);
                env.push(("OPENAI_BASE_URL".into(), openai_base.to_string()));
                env.push(("OPENAI_HOST".into(), openai_base.to_string()));
            }
            "anthropic" => {
                if let Some(s) = secret {
                    env.push(("ANTHROPIC_API_KEY".into(), s));
                }
            }
            "openai" | "custom_openai" => {
                if let Some(s) = secret {
                    env.push(("OPENAI_API_KEY".into(), s));
                }
                env.push(("OPENAI_BASE_URL".into(), active.base_url.clone()));
                env.push(("OPENAI_HOST".into(), active.base_url.clone()));
            }
            _ => {}
        }

        // Per-provider sampling params (Round-2 item 27; None -> Goose default).
        if let Some(t) = active.temperature {
            env.push(("GOOSE_TEMPERATURE".into(), t.to_string()));
        }
        if let Some(c) = active.context_length {
            env.push(("GOOSE_CONTEXT_LIMIT".into(), c.to_string()));
        }
        if let Some(p) = active.top_p {
            env.push(("GOOSE_TOP_P".into(), p.to_string()));
        }
    }

    // Global (not per-provider) context-management strategy (Round-4 item 3).
    env.push((
        "GOOSE_CONTEXT_STRATEGY".into(),
        config.context_strategy.clone(),
    ));

    // Round-5: nudge the model to save generated files into the session's own
    // working directory (the per-chat `Documents/Kitty/chats/<id>/` folder,
    // which goose already sets as the shell cwd) instead of writing to an
    // absolute path like goose's built-in `~/Documents/Goose` default. Injected
    // every turn via the bundled `tom` (Top Of Mind) platform extension's
    // `GOOSE_MOIM_MESSAGE_TEXT` env. Relative-path writes already land in the
    // chat folder; this is a soft nudge to make the model prefer them for
    // documents/spreadsheets it would otherwise dump in ~/Documents/Goose.
    //
    // `GOOSE_MOIM_MESSAGE_TEXT` is a single scalar env (goosed has no way to
    // combine multiple `tom` messages), so every "nudge the model every turn"
    // need has to append here rather than push its own separate env entry —
    // a second `env.push(("GOOSE_MOIM_MESSAGE_TEXT", ...))` would just
    // silently clobber this one.
    let mut moim_message = String::from(
        "When you create, generate, or export any file (documents, spreadsheets, scripts, \
         data, etc.), always save it into the current working directory using a relative \
         path such as `report.docx`. Do not write to an absolute path such as \
         ~/Documents/Goose — the working directory is already set to the correct folder \
         for this conversation.",
    );

    // Adaptive Pathway's `decide`/`record_outcome` MCP tools rely entirely on
    // the model reading `decide`'s own tool description ("call this before
    // selecting any tool") and choosing to comply on its own — there's no ACP
    // mechanism to inject the extension's `adaptive_instructions` MCP prompt
    // (session/new silently drops systemPrompt/instructions, confirmed in
    // docs/acp-protocol.md's "Recipes / skills" section). In practice this
    // signal is too easy to miss for a small/local model choosing among many
    // other enabled extensions' tools — confirmed live: a real Kitty install
    // with several genuine conversations had a completely empty
    // adaptive-pathway database (zero rows in every table) despite the
    // extension being enabled and registered. Fold the same reminder into the
    // `tom` nudge every turn instead of leaving it to chance.
    if config.adaptive_pathway_enabled {
        // `AP_EMBED_OLLAMA_MODEL` lives on goosed's own process env here so the
        // extension registration in `AdaptivePathway.tsx` can declare it as an
        // `env_keys` entry (goose forwards named env vars from *its own*
        // environment to the spawned MCP child — the extension-registration
        // `env` param only declares names, it cannot carry a literal value
        // directly). This is how the sidecar (spawned separately by Kitty,
        // see `adaptive_pathway_proc::ensure_running`) and this MCP extension
        // (spawned by goosed) end up pinned to the same embedding model tag
        // without either one's config drifting from the other.
        env.push((
            "AP_EMBED_OLLAMA_MODEL".into(),
            config.adaptive_pathway_embedding_model.clone(),
        ));
        env.push(("AP_EMBED_OLLAMA_URL".into(), config.ollama_base_url.clone()));

        // Kept short and load-bearing-instruction-first deliberately: this
        // whole block is one shared scalar re-injected into every turn (see
        // the file-path nudge above), and a small/local model is more likely
        // to skip an instruction buried after several paragraphs of preamble
        // than one it sees immediately. Kitty also auto-records a
        // best-effort `record_outcome` from the ACP tool-call stream now
        // (see `goosed::stream::track_and_maybe_record_outcome`) as a
        // backstop for exactly this — but the model isn't told that, since
        // its own call carries real `context` the backstop can't provide,
        // and knowing a backstop exists would only invite skipping the
        // higher-quality signal.
        moim_message.push_str(
            " Before choosing a tool, call the adaptive-pathway extension's `decide` tool \
             with the tool(s) you're considering, and pass a `context` parameter: a short \
             one-sentence summary of what this conversation is about (e.g. \"reviewing a \
             novel draft\") — this keeps what you learn in one topic from leaking into \
             suggestions for a different one. After using a tool, call `record_outcome` \
             with the tool and reward (1.0 success, -1.0 failure, 0.0 neutral) — every time, \
             not just occasionally.",
        );

        // Adaptive Pathway also learns general response-style preferences, not
        // just tool selection — e.g. what kind of writing critique the user
        // finds useful. `decide`/`record_annotation` are fully generic (plain
        // string labels, no tool-call assumption), and Kitty's own hint-badge
        // UI already renders 👍👎 feedback buttons for any `decide` call that
        // returns hints, regardless of what the labels mean — so this reuses
        // the exact same mechanism as the tool nudge above, just with
        // different labels and record_annotation instead of record_outcome
        // (a style preference isn't a pass/fail execution result).
        moim_message.push_str(
            " Adaptive Pathway also learns HOW you respond, not just which tool you use. \
             Before a substantive response with a real choice of approach — critique, \
             explanations, brainstorming, planning (not simple factual answers) — call \
             `decide` with a few candidate `style:...` labels as the tools you're \
             considering, e.g. `style:critique:structural`, `style:critique:direct-tone`, \
             `style:explain:concise`, `style:proactive:ask-first` — invent new ones as \
             needed. Use the top hint to shape your response. Afterward, watch the user's \
             next message for a reaction and call `record_annotation` on the label you \
             used: clear positive signals (thanks, building on it, direct confirmation, \
             acting on it without pushback) mean `keep_this` (or `micro_positive` if \
             milder); clear negative signals (correction, re-asking, pushback, redoing it \
             themselves) mean `dont_do_again` (or `micro_negative` if milder). No clear \
             reaction — don't annotate.",
        );

        // Scenario this guards against: a response covers two angles (e.g. a
        // mainstream, well-supported option and an alternative one); the user
        // reacts positively to just the mainstream one, but without this
        // instruction the model could annotate BOTH as liked, rewarding an
        // option the user never actually endorsed. Merged with the intensity
        // rule below since both are short caveats on the same mechanism.
        moim_message.push_str(
            " Attribution matters: only annotate the specific label your reaction was \
             about — if a response combined multiple approaches, do not also annotate the \
             others just because they appeared in the same response; skip annotating if \
             you're not sure which one it was about. For `dont_do_again`, use intensity 0.9 \
             (not the normal ~0.5) when the user clearly wants a topic stopped going \
             forward (e.g. \"stop suggesting that\") — that suppresses it from suggestions \
             for about a month instead of a soft nudge.",
        );
    }

    env.push(("GOOSE_MOIM_MESSAGE_TEXT".into(), moim_message));

    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::providers::ProviderProfile;

    fn moim_text(env: &[(String, String)]) -> &str {
        env.iter()
            .find(|(k, _)| k == "GOOSE_MOIM_MESSAGE_TEXT")
            .map(|(_, v)| v.as_str())
            .expect("GOOSE_MOIM_MESSAGE_TEXT must always be present")
    }

    #[test]
    fn goosed_env_sets_single_moim_message_text_entry() {
        // A second env.push(("GOOSE_MOIM_MESSAGE_TEXT", ...)) would silently
        // clobber the first when goosed reads its env — every "nudge the
        // model every turn" need must append to one shared string instead.
        let cfg = Config::default();
        let env = goosed_env(&cfg);
        let count = env
            .iter()
            .filter(|(k, _)| k == "GOOSE_MOIM_MESSAGE_TEXT")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn goosed_env_openrouter_also_sets_openai_base_url() {
        // Goose has no dedicated native client for OpenRouter — it's the
        // OpenAI-compatible client under the hood, which silently falls back
        // to api.openai.com when OPENAI_BASE_URL isn't set. Confirmed by a
        // real failure: switching to an OpenRouter profile mid-chat sent the
        // request to OpenAI's own default endpoint with no key at all.
        let cfg = Config {
            providers: vec![ProviderProfile {
                id: "p1".into(),
                name: "OpenRouter".into(),
                provider_type: "openrouter".into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                models: vec!["some/model".into()],
                is_trusted: true,
                temperature: None,
                top_p: None,
                context_length: None,
                strip_reasoning: false,
                system_prompt: None,
                prompt_idle_timeout_secs: None,
                created_at: String::new(),
            }],
            active_provider_id: Some("p1".into()),
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        // The trailing `/v1` must be stripped — goosed's OpenAI-compatible
        // client appends `/v1/chat/completions` itself, and passing the raw
        // (already-`/v1`-suffixed) base through unstripped produced a real,
        // confirmed doubled-path 404: `.../api/v1/v1/chat/completions`.
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "OPENAI_BASE_URL")
                .map(|(_, v)| v.as_str()),
            Some("https://openrouter.ai/api")
        );
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "OPENAI_HOST")
                .map(|(_, v)| v.as_str()),
            Some("https://openrouter.ai/api")
        );
    }

    #[test]
    fn goosed_env_openrouter_strips_v1_regardless_of_trailing_slash() {
        let cfg = Config {
            providers: vec![ProviderProfile {
                id: "p1".into(),
                name: "OpenRouter".into(),
                provider_type: "openrouter".into(),
                base_url: "https://openrouter.ai/api/v1/".into(),
                models: vec!["some/model".into()],
                is_trusted: true,
                temperature: None,
                top_p: None,
                context_length: None,
                strip_reasoning: false,
                system_prompt: None,
                prompt_idle_timeout_secs: None,
                created_at: String::new(),
            }],
            active_provider_id: Some("p1".into()),
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "OPENAI_BASE_URL")
                .map(|(_, v)| v.as_str()),
            Some("https://openrouter.ai/api")
        );
    }

    #[test]
    fn moim_message_includes_adaptive_pathway_nudge_when_enabled() {
        let cfg = Config {
            adaptive_pathway_enabled: true,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        assert!(
            text.contains("relative"),
            "file-path nudge must still be present"
        );
        assert!(text.contains("decide"));
        assert!(text.contains("record_outcome"));
    }

    #[test]
    fn goosed_env_sets_embedding_model_when_adaptive_pathway_enabled() {
        // The MCP extension (spawned by goosed) can't receive a literal env
        // value straight from `add_extension` — only `env_keys` naming a var
        // goosed already has in *its own* environment. This entry is that var.
        let cfg = Config {
            adaptive_pathway_enabled: true,
            adaptive_pathway_embedding_model: "qwen3-embedding:0.6b".into(),
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "AP_EMBED_OLLAMA_MODEL")
                .map(|(_, v)| v.as_str()),
            Some("qwen3-embedding:0.6b")
        );
    }

    #[test]
    fn goosed_env_omits_embedding_model_when_adaptive_pathway_disabled() {
        let cfg = Config {
            adaptive_pathway_enabled: false,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        assert!(!env.iter().any(|(k, _)| k == "AP_EMBED_OLLAMA_MODEL"));
    }

    #[test]
    fn goosed_env_sets_embedding_url_when_adaptive_pathway_enabled() {
        let cfg = Config {
            adaptive_pathway_enabled: true,
            ollama_base_url: "http://localhost:11434".into(),
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "AP_EMBED_OLLAMA_URL")
                .map(|(_, v)| v.as_str()),
            Some("http://localhost:11434")
        );
    }

    #[test]
    fn goosed_env_omits_embedding_url_when_adaptive_pathway_disabled() {
        let cfg = Config {
            adaptive_pathway_enabled: false,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        assert!(!env.iter().any(|(k, _)| k == "AP_EMBED_OLLAMA_URL"));
    }

    #[test]
    fn moim_message_omits_adaptive_pathway_nudge_when_disabled() {
        let cfg = Config {
            adaptive_pathway_enabled: false,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        assert!(
            text.contains("relative"),
            "file-path nudge must still be present"
        );
        assert!(!text.contains("decide"));
        assert!(!text.contains("record_outcome"));
    }

    #[test]
    fn moim_message_includes_response_style_nudge_when_enabled() {
        let cfg = Config {
            adaptive_pathway_enabled: true,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        assert!(text.contains("style:critique:structural"));
        assert!(text.contains("record_annotation"));
        assert!(text.contains("keep_this"));
        assert!(text.contains("dont_do_again"));
        // Both nudges must coexist in the single scalar env, not clobber each other.
        assert!(
            text.contains("record_outcome"),
            "tool nudge must still be present"
        );
    }

    #[test]
    fn moim_message_omits_response_style_nudge_when_disabled() {
        let cfg = Config {
            adaptive_pathway_enabled: false,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        assert!(!text.contains("style:critique:structural"));
        assert!(!text.contains("record_annotation"));
    }

    #[test]
    fn moim_message_includes_context_param_instruction_when_enabled() {
        let cfg = Config {
            adaptive_pathway_enabled: true,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        assert!(text.contains("`context` parameter"));
        // Without this, preferences bleed across unrelated topics purely
        // from call frequency (the frequency-obsession scenario).
        assert!(text.contains("leaking into"));
    }

    #[test]
    fn moim_message_includes_attribution_precision_instruction_when_enabled() {
        let cfg = Config {
            adaptive_pathway_enabled: true,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        // Guards the mixed-response misattribution scenario: liking one of
        // several angles a response covered must not reward all of them.
        assert!(text.contains("Attribution matters"));
        assert!(text.contains("do not also annotate the others"));
    }

    #[test]
    fn moim_message_omits_new_instructions_when_disabled() {
        let cfg = Config {
            adaptive_pathway_enabled: false,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        assert!(!text.contains("`context` parameter"));
        assert!(!text.contains("Attribution matters"));
        assert!(!text.contains("intensity 0.9"));
    }

    #[test]
    fn moim_message_includes_dont_do_again_intensity_guidance_when_enabled() {
        let cfg = Config {
            adaptive_pathway_enabled: true,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        // Guards the topic-lock-in scenario: a clear "stop showing me this"
        // must use high intensity so the engine's TTL suppression fires.
        assert!(text.contains("intensity 0.9"));
        assert!(text.contains("suppresses it from suggestions for about a month"));
    }
}
