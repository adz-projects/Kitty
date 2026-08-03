//! Provider plumbing for the BigTiny backend: sync Kitty's active provider
//! profile into BigTiny's runtime provider registry over REST — no daemon
//! restart, unlike the goosed path's spawn-time env vars.

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
        return ("anthropic".to_string(), profile.base_url.clone());
    }
    let base = profile.base_url.trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    ("openai_compat".to_string(), base.to_string())
}

/// Ensure Kitty's active provider profile exists (and is current) in
/// BigTiny's registry; returns the BigTiny provider id, or `None` when no
/// profile is active. Matched by profile name — BigTiny assigns its own ids.
pub async fn sync_active_provider(app: &AppHandle) -> Result<Option<String>, String> {
    let profile = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.active_provider_id
            .as_ref()
            .and_then(|id| cfg.providers.iter().find(|p| &p.id == id).cloned())
    };
    let Some(profile) = profile else {
        return Ok(None);
    };

    let (provider_type, base_url) = bigtiny_provider_target(&profile);
    let api_key = get_secret_async(&profile.id).await;
    let model = profile.models.first().cloned();
    let mut config = json!({});
    if let Some(m) = &model {
        config["model"] = Value::String(m.clone());
    }
    // `provider_type` above is BigTiny's wire-format column (`openai_compat`
    // | `anthropic`, DB-constrained) — it collapses ollama/openai/openrouter/
    // custom_openai together, so it can't tell a self-hosted endpoint apart
    // from a hosted one. `provider_dialect` carries Kitty's original,
    // granular `profile.provider_type` through the unconstrained `config`
    // blob instead; BigTiny's router reads it back
    // (`ProviderRouter::register_from_row`) to decide which providers get a
    // repetition-safe sampling floor (`provider::sampling::defaults_for`)
    // and which llama.cpp/Ollama-only fields (`top_k`/`min_p`) are safe to
    // put on the wire.
    config["provider_dialect"] = Value::String(profile.provider_type.clone());
    if let Some(t) = profile.temperature {
        config["temperature"] = json!(t);
    }
    if let Some(p) = profile.top_p {
        config["top_p"] = json!(p);
    }
    if let Some(k) = profile.top_k {
        config["top_k"] = json!(k);
    }
    if let Some(p) = profile.min_p {
        config["min_p"] = json!(p);
    }
    if let Some(p) = profile.presence_penalty {
        config["presence_penalty"] = json!(p);
    }
    if let Some(f) = profile.frequency_penalty {
        config["frequency_penalty"] = json!(f);
    }
    if let Some(m) = profile.max_tokens {
        config["max_tokens"] = json!(m);
    }
    if let Some(c) = profile.context_length {
        config["context_length"] = json!(c);
    }
    if let Some(n) = profile.parallel_slots {
        config["parallel_slots"] = json!(n);
    }

    let client = ensure_client(app)?;
    let existing = client.get_json("/api/providers").await?;
    let found: Option<String> = existing
        .get("providers")
        .and_then(|p| p.as_array())
        .and_then(|rows| {
            rows.iter()
                .find(|r| r.get("name").and_then(|n| n.as_str()) == Some(profile.name.as_str()))
        })
        .and_then(|r| r.get("id").and_then(|i| i.as_str()).map(String::from));

    let id = match found {
        Some(id) => {
            let mut body =
                json!({ "base_url": base_url, "config": config, "fallback_priority": 1 });
            // Explicit `null`, not an omitted field, when there's no key —
            // BigTiny's `merge_config` treats an omitted `api_key` as "leave
            // whatever's already stored alone" (needed so this same PATCH,
            // sent on every activation, doesn't require repeating an
            // unchanged key every time). Omitting it here for the "key was
            // deleted from Credential Manager" case meant a removed key kept
            // working forever server-side; `null` tells BigTiny to actually
            // clear it.
            body["api_key"] = api_key.as_deref().map(Value::from).unwrap_or(Value::Null);
            client
                .patch_json(&format!("/api/providers/{id}"), &body)
                .await?;
            id
        }
        None => {
            let mut body = json!({
                "name": profile.name,
                "provider_type": provider_type,
                "base_url": base_url,
                "fallback_priority": 1,
                "config": config,
            });
            if let Some(key) = &api_key {
                body["api_key"] = Value::String(key.clone());
            }
            let created = client.post_json("/api/providers", &body).await?;
            created
                .get("id")
                .and_then(|i| i.as_str())
                .ok_or("BigTiny did not return a provider id")?
                .to_string()
        }
    };

    // Every profile Kitty has ever activated stays registered in BigTiny (a
    // feature — instant switching), but the router picks by priority, so the
    // active one must be unambiguous: demote everything else.
    if let Some(rows) = existing.get("providers").and_then(|p| p.as_array()) {
        for row in rows {
            let Some(other_id) = row.get("id").and_then(|i| i.as_str()) else {
                continue;
            };
            let priority = row.get("fallback_priority").and_then(|p| p.as_i64());
            if other_id != id && priority != Some(100) {
                let _ = client
                    .patch_json(
                        &format!("/api/providers/{other_id}"),
                        &json!({ "fallback_priority": 100 }),
                    )
                    .await;
            }
        }
    }
    Ok(Some(id))
}

/// Best-effort: rebind an already-open session onto the currently-active
/// provider/model (`PATCH /api/chat/{id}/config`) — the BigTiny equivalent of
/// the goosed path's `session/set_config_option` hot-rebind. Swallows its own
/// failures, same contract as `rebind_session_provider`.
pub async fn rebind_session(app: &AppHandle, session_id: &str) {
    let Ok(Some(provider_id)) = sync_active_provider(app).await else {
        return;
    };
    let model = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.active_provider_id
            .as_ref()
            .and_then(|id| cfg.providers.iter().find(|p| &p.id == id))
            .and_then(|p| p.models.first().cloned())
    };
    let Ok(client) = ensure_client(app) else {
        return;
    };
    let body = json!({
        "provider": provider_id,
        // Empty string clears a stale override when the profile has no model.
        "model": model.unwrap_or_default(),
    });
    let _ = client
        .patch_json(&format!("/api/chat/{session_id}/config"), &body)
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
            strip_reasoning: false,
            system_prompt: None,
            prompt_idle_timeout_secs: None,
            parallel_slots: None,
            created_at: String::new(),
        }
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
}
