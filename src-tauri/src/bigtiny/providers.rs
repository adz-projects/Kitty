//! Provider plumbing for the BigTiny backend: keep the daemon's provider
//! registry in step with Kitty's provider cards over REST, and pin sessions
//! to a card.

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::bigtiny::client::ensure_client;
use crate::config::providers::{get_secret_async, ProviderProfile};
use crate::state::AppState;

/// Pure: map a Kitty provider profile onto BigTiny's `(provider_type,
/// base_url)` pair. BigTiny's OpenAI-compatible client appends
/// `/v1/chat/completions` itself, so a base URL that already ends in `/v1`
/// (OpenRouter's canonical base, some custom endpoints) must be stripped —
/// the same doubled-path failure goosed's env plumbing hit (see
/// `config/providers/env.rs`).
pub(crate) fn bigtiny_provider_target(profile: &ProviderProfile) -> (String, String) {
    if profile.provider_type == "anthropic" {
        return ("anthropic".to_string(), normalize_scheme(&profile.base_url));
    }
    let base = normalize_scheme(&profile.base_url);
    let base = base.trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    ("openai_compat".to_string(), base.to_string())
}

/// Prepend `http://` to a scheme-less base URL. Kitty's own connection probe
/// tolerates a bare `host:port` by fanning out https-then-http
/// (`config/providers/endpoint::candidates`), but the daemon's HTTP client
/// gets one URL and can't build a request from a scheme-less authority — a
/// self-hosted `192.168.1.199:8081` would silently never connect. `http` (not
/// `https`) for a bare host:port: these are LAN/self-hosted endpoints; a user
/// wanting TLS types `https://` explicitly.
fn normalize_scheme(base_url: &str) -> String {
    let trimmed = base_url.trim();
    if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

/// Every provider card, as the daemon should know it.
///
/// **All** usable profiles are registered, not just the default: a chat pinned
/// to a non-default card, a specialist told to prefer one, and failover all
/// need the daemon to know it. Kitty's rows in the daemon are kept in exact
/// step with the cards: a card's edit reaches its row (every setting sent
/// explicitly, so a cleared value clears), a deleted or disabled card's row
/// is removed (with its key), and the default card is set as Kitty's
/// per-app default.
///
/// Called at every attach and after any card is added, edited, duplicated or
/// deleted.
pub async fn sync_all_providers(app: &AppHandle) -> Result<(), String> {
    let (profiles, default_id) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        (
            cfg.providers
                .iter()
                .filter(|p| p.is_usable())
                .cloned()
                .collect::<Vec<_>>(),
            cfg.active_provider_id.clone(),
        )
    };
    let catalog = {
        let state = app.state::<AppState>();
        crate::openrouter::catalog::ensure_catalog_fresh(&state).await;
        let catalog = state.openrouter_catalog.lock().unwrap().clone();
        catalog
    };
    let mut cards = Vec::with_capacity(profiles.len());
    for profile in profiles {
        let api_key = get_secret_async(&profile.id).await;
        let body = provider_body(&profile, catalog.as_ref(), api_key);
        cards.push(body);
    }
    let client = ensure_client(app)?;
    sync_cards(&client, &cards, default_id.as_deref()).await
}

/// One card, ready to send.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProviderCard {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    pub config: Value,
    pub api_key: Option<String>,
}

/// Map a profile onto the daemon's provider row. Every optional setting is
/// present, `null` when unset: the daemon merges `config` key by key, so an
/// omitted key would leave a stale value behind.
pub(crate) fn provider_body(
    profile: &ProviderProfile,
    catalog: Option<&crate::openrouter::catalog::OpenRouterCatalog>,
    api_key: Option<String>,
) -> ProviderCard {
    let (provider_type, base_url) = bigtiny_provider_target(profile);
    let entry = profile.models.first().and_then(|model| {
        catalog.and_then(|c| crate::openrouter::catalog::match_in_catalog(model, &c.entries))
    });
    // Delegate-host hints. `subagent_role` is the user's own setting; the other
    // two are folded from the OpenRouter catalog here rather than in the
    // daemon, because the catalog lives in this process and the daemon serves
    // apps other than Kitty — it must not acquire a dependency it cannot
    // refresh. All three are commonly absent for a self-hosted model the
    // catalog cannot match, which is the ordinary case.
    let cost_tier = entry.and_then(|e| e.cost_tier).map(|tier| match tier {
        crate::openrouter::catalog::CostTier::Economy => "economy",
        crate::openrouter::catalog::CostTier::Moderate => "moderate",
        crate::openrouter::catalog::CostTier::Premium => "premium",
    });
    // One number out of three indices: a delegate's work is agentic
    // tool-driving, so that index leads, with coding and general intelligence
    // as fallbacks when a model has not been measured on it.
    let capability_rank = entry
        .and_then(|e| e.agentic_index.or(e.coding_index).or(e.intelligence_index))
        .map(|rank| rank.clamp(0.0, 100.0) as i32);
    let config = json!({
        "model": profile.models.first(),
        // `provider_type` is BigTiny's wire-format column (`openai_compat` |
        // `anthropic`, DB-constrained) — it collapses ollama/openai/openrouter/
        // custom_openai together. `provider_dialect` carries Kitty's granular
        // type through the unconstrained `config` blob; BigTiny's router reads
        // it back to decide which providers get a repetition-safe sampling
        // floor and which llama.cpp/Ollama-only fields are safe to send.
        "provider_dialect": profile.provider_type,
        "temperature": profile.temperature,
        "top_p": profile.top_p,
        "top_k": profile.top_k,
        "min_p": profile.min_p,
        "presence_penalty": profile.presence_penalty,
        "frequency_penalty": profile.frequency_penalty,
        "max_tokens": profile.max_tokens,
        "context_length": profile.context_length,
        "parallel_slots": profile.parallel_slots,
        "subagent_role": profile.subagent_role.as_deref().filter(|r| !r.is_empty()),
        "cost_tier": cost_tier,
        "capability_rank": capability_rank,
        // Negated so a row written before this existed reads as "supports
        // tools" (see the daemon's `ProviderConfig::tools_unsupported`).
        "tools_unsupported": !profile.tools_supported(catalog),
    });
    ProviderCard {
        id: profile.id.clone(),
        name: profile.name.clone(),
        provider_type,
        base_url,
        config,
        api_key,
    }
}

/// Bring the daemon's rows in line with `cards`, then set `default_id` as the
/// app default. Split from the `AppHandle` half so it runs against a mock
/// daemon in tests.
pub(crate) async fn sync_cards(
    client: &super::client::BigTinyClient,
    cards: &[ProviderCard],
    default_id: Option<&str>,
) -> Result<(), String> {
    let existing = client.get_json("/api/providers").await?;
    let rows: Vec<Value> = existing
        .get("providers")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let row_of = |id: &str| rows.iter().find(|r| r["id"].as_str() == Some(id));

    let mut first_error: Option<String> = None;
    for card in cards {
        let result = match row_of(&card.id) {
            // The wire type cannot be changed in place; replace the row.
            Some(row) if row["provider_type"].as_str() != Some(card.provider_type.as_str()) => {
                match client.delete(&format!("/api/providers/{}", card.id)).await {
                    Ok(_) => create_row(client, card).await,
                    Err(e) => Err(e),
                }
            }
            Some(_) => {
                let body = json!({
                    "name": card.name,
                    "base_url": card.base_url,
                    "config": card.config,
                    "fallback_priority": 1,
                    // Explicit `null`, not an omitted field, when there's no
                    // key: the daemon reads an omitted `api_key` as "leave the
                    // stored one alone", so a key removed here would otherwise
                    // keep working there forever.
                    "api_key": card.api_key,
                });
                client
                    .patch_json(&format!("/api/providers/{}", card.id), &body)
                    .await
                    .map(|_| ())
            }
            None => create_row(client, card).await,
        };
        if let Err(e) = result {
            tracing::warn!("could not sync provider {} to the engine: {e}", card.id);
            first_error.get_or_insert(e);
        }
    }

    for stale in stale_rows(&rows, cards) {
        if let Err(e) = client.delete(&format!("/api/providers/{stale}")).await {
            tracing::warn!("could not remove provider {stale} from the engine: {e}");
        }
    }

    if let Some(id) = default_id.filter(|id| cards.iter().any(|c| c.id == *id)) {
        set_app_default(client, id).await;
    }
    first_error.map_or(Ok(()), Err)
}

async fn create_row(
    client: &super::client::BigTinyClient,
    card: &ProviderCard,
) -> Result<(), String> {
    // The row's id is the card's id, so a session's `metadata.provider`
    // stamp (also the card id) resolves to it.
    let mut body = json!({
        "id": card.id,
        "name": card.name,
        "provider_type": card.provider_type,
        "base_url": card.base_url,
        "fallback_priority": 1,
        "config": card.config,
    });
    if let Some(key) = &card.api_key {
        body["api_key"] = Value::String(key.clone());
    }
    client.post_json("/api/providers", &body).await.map(|_| ())
}

/// Kitty's rows that no card accounts for any more: deleted and disabled
/// cards, and rows from before a card's row shared its id. Only rows this
/// app owns - never another app's, and never one shared with every app.
fn stale_rows(rows: &[Value], cards: &[ProviderCard]) -> Vec<String> {
    rows.iter()
        .filter(|r| r["app_id"].as_str() == Some(crate::lifecycle::bigtiny_app_key::APP_ID))
        .filter_map(|r| r["id"].as_str())
        .filter(|id| !cards.iter().any(|c| c.id == *id))
        .map(str::to_string)
        .collect()
}

/// Tell the daemon which provider *this app* defaults to.
///
/// Replaces `demote_others`, which existed only because "the active provider"
/// used to be daemon-global: expressing "use mine" meant PATCHing
/// `fallback_priority: 100` onto every row Kitty did not own. With a second
/// app attached that is a cross-tenant stomp — Kitty would be reordering the
/// research pipeline's providers on every profile switch, and the pipeline
/// would be doing the same back.
///
/// V2 has a per-app default (`PATCH /api/apps/me`), so the same intent is one
/// scoped write that cannot touch anyone else's configuration. Kitty's other
/// profiles stay registered and instantly switchable, exactly as before;
/// `fallback_priority` survives only as a tiebreaker *within* one app's own
/// visible set.
///
/// Best-effort: a failure here means the next send falls back to the app's
/// healthiest visible provider, which is the right answer anyway.
async fn set_app_default(client: &super::client::BigTinyClient, active_id: &str) {
    if let Err(e) = client
        .patch_json("/api/apps/me", &json!({ "default_provider_id": active_id }))
        .await
    {
        tracing::warn!("could not set Kitty's default provider to {active_id}: {e}");
    }
}

/// Per-session provider override: PATCH a single session's metadata with the
/// given provider/model (`PATCH /api/chat/{id}/config`). This does NOT sync the global active provider or touch
/// the BigTiny registry defaults — a session keeps the provider it was
/// stamped with, independent of what other windows pick. This is the
/// per-session isolation contract: providers are resolved per session at send
/// time (`loop_.rs` reads `metadata.provider`), so every window can chat on a
/// different provider without flipping each other's open sessions.
/// Best-effort: swallows its own failures.
pub async fn set_session_provider(
    app: &AppHandle,
    session_id: &str,
    provider_id: &str,
    model: &str,
) {
    let Ok(client) = ensure_client(app) else {
        return;
    };
    let _ = client
        .patch_json(
            &format!("/api/chat/{session_id}/config"),
            &json!({
                "provider": provider_id,
                // Empty string clears a stale override when the profile has
                // no model — the daemon's `model_override` filters a blank
                // string back to "no pin" (`loop_.rs`), so this is a clear,
                // not an override that puts an empty `model` on the wire.
                "model": model,
            }),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(provider_type: &str, base_url: &str) -> ProviderProfile {
        ProviderProfile {
            id: "p1".into(),
            name: "Test".into(),
            provider_type: provider_type.into(),
            subagent_role: None,
            base_url: base_url.into(),
            models: vec![],
            is_trusted: true,
            temperature: None,
            top_p: None,
            top_k: None,
            min_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            context_length: None,
            supports_vision: false,
            system_prompt: None,
            prompt_idle_timeout_secs: None,
            parallel_slots: None,
            supports_tools: None,
            disabled_reason: None,
            created_at: String::new(),
        }
    }

    /// A cleared setting has to clear on the engine too: every optional key
    /// is sent, `null` when unset, because the daemon merges `config` key by
    /// key.
    #[test]
    fn every_optional_setting_is_sent_even_when_unset() {
        let mut p = profile("openrouter", "https://openrouter.ai/api/v1");
        p.models = vec!["a/b".into()];
        let card = provider_body(&p, None, None);
        let config = card.config.as_object().unwrap();
        for key in [
            "temperature",
            "top_p",
            "top_k",
            "min_p",
            "presence_penalty",
            "frequency_penalty",
            "max_tokens",
            "context_length",
            "parallel_slots",
            "subagent_role",
            "cost_tier",
            "capability_rank",
        ] {
            assert!(config.contains_key(key), "{key} must be sent");
            assert!(config[key].is_null(), "{key} is unset");
        }
        assert_eq!(config["model"], "a/b");
        assert_eq!(config["tools_unsupported"], false);

        p.supports_tools = Some(false);
        assert_eq!(
            provider_body(&p, None, None).config["tools_unsupported"],
            true
        );
    }

    /// Only Kitty's own rows are ever removed, and only those no card accounts
    /// for.
    #[test]
    fn stale_rows_are_kittys_rows_without_a_card() {
        let rows = vec![
            json!({"id": "keep", "app_id": "kitty"}),
            json!({"id": "gone", "app_id": "kitty"}),
            json!({"id": "theirs", "app_id": "research"}),
            json!({"id": "shared", "app_id": null}),
        ];
        let cards = vec![provider_body(
            &ProviderProfile {
                id: "keep".into(),
                ..profile("anthropic", "x")
            },
            None,
            None,
        )];
        assert_eq!(stale_rows(&rows, &cards), ["gone"]);
    }

    /// Against a mock engine: an existing card is patched, a new one created,
    /// a deleted card's row removed, and the default set.
    #[tokio::test]
    async fn sync_brings_the_engine_in_line_with_the_cards() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/providers")
            .with_body(
                json!({"providers": [
                    {"id": "old", "provider_type": "openai_compat", "app_id": "kitty"},
                    {"id": "edited", "provider_type": "openai_compat", "app_id": "kitty"},
                ]})
                .to_string(),
            )
            .create_async()
            .await;
        let patched = server
            .mock("PATCH", "/api/providers/edited")
            .match_body(mockito::Matcher::PartialJson(
                json!({"name": "Edited", "api_key": null}),
            ))
            .with_body("{}")
            .create_async()
            .await;
        let created = server
            .mock("POST", "/api/providers")
            .match_body(mockito::Matcher::PartialJson(
                json!({"id": "new", "api_key": "sk"}),
            ))
            .with_body(r#"{"id":"new"}"#)
            .create_async()
            .await;
        let removed = server
            .mock("DELETE", "/api/providers/old")
            .with_body("{}")
            .create_async()
            .await;
        let default = server
            .mock("PATCH", "/api/apps/me")
            .match_body(mockito::Matcher::Json(
                json!({"default_provider_id": "new"}),
            ))
            .with_body("{}")
            .create_async()
            .await;

        let edited = ProviderProfile {
            id: "edited".into(),
            name: "Edited".into(),
            ..profile("openrouter", "https://x")
        };
        let new = ProviderProfile {
            id: "new".into(),
            ..profile("custom_openai", "http://box:1")
        };
        let cards = vec![
            provider_body(&edited, None, None),
            provider_body(&new, None, Some("sk".into())),
        ];
        let client = crate::bigtiny::client::BigTinyClient::new(server.url(), None);
        sync_cards(&client, &cards, Some("new")).await.unwrap();

        patched.assert_async().await;
        created.assert_async().await;
        removed.assert_async().await;
        default.assert_async().await;
    }

    #[test]
    fn anthropic_maps_to_native_provider() {
        let (t, url) = bigtiny_provider_target(&profile("anthropic", "https://api.anthropic.com"));
        assert_eq!(t, "anthropic");
        assert_eq!(url, "https://api.anthropic.com");
    }

    #[test]
    fn openrouter_strips_trailing_v1() {
        let (t, url) =
            bigtiny_provider_target(&profile("openrouter", "https://openrouter.ai/api/v1"));
        assert_eq!(t, "openai_compat");
        assert_eq!(url, "https://openrouter.ai/api");
    }

    #[test]
    fn ollama_base_passes_through() {
        let (t, url) = bigtiny_provider_target(&profile("ollama", "http://localhost:11434"));
        assert_eq!(t, "openai_compat");
        assert_eq!(url, "http://localhost:11434");
    }

    #[test]
    fn scheme_less_base_url_gets_http_prepended() {
        let (t, url) = bigtiny_provider_target(&profile("custom_openai", "192.168.1.199:8081"));
        assert_eq!(t, "openai_compat");
        assert_eq!(url, "http://192.168.1.199:8081");
    }

    #[test]
    fn explicit_https_scheme_is_preserved() {
        let (_, url) =
            bigtiny_provider_target(&profile("custom_openai", "https://box.ts.net:8081"));
        assert_eq!(url, "https://box.ts.net:8081");
    }

    #[test]
    fn scheme_less_anthropic_gets_http_prepended() {
        let (t, url) = bigtiny_provider_target(&profile("anthropic", "api.anthropic.com"));
        assert_eq!(t, "anthropic");
        assert_eq!(url, "http://api.anthropic.com");
    }

    // Fireworks/DeepInfra (provider-add redesign) — neither needs its own
    // match arm here, since the else-branch already covers anything that
    // isn't "anthropic"; these just pin that down as a regression guard.
    #[test]
    fn fireworks_maps_to_openai_compat() {
        let (t, url) = bigtiny_provider_target(&profile(
            "fireworks",
            "https://api.fireworks.ai/inference/v1",
        ));
        assert_eq!(t, "openai_compat");
        assert_eq!(url, "https://api.fireworks.ai/inference");
    }

    #[test]
    fn deepinfra_maps_to_openai_compat() {
        let (t, url) =
            bigtiny_provider_target(&profile("deepinfra", "https://api.deepinfra.com/v1/openai"));
        assert_eq!(t, "openai_compat");
        assert_eq!(url, "https://api.deepinfra.com/v1/openai");
    }
}
