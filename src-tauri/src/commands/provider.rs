//! Provider commands. Settings → Providers lists, saves and deletes
//! *providers* (a service, its key, and the models the user checked); the
//! rest of Kitty works with *model cards*, one per checked model
//! (`ProviderProfile::cards`): the model list a chat, a scheduled task or a
//! specialist picks from, and the default model. Every change is mirrored to
//! the engine by `bigtiny::providers::sync_all_providers`.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::ai_models::{self, ModelInfo};
use crate::config;
use crate::config::providers::{self, NetworkTier, ProviderProfile};
use crate::config::Config;
use crate::openrouter;
use crate::openrouter::catalog;
use crate::state::AppState;

/// One model card plus derived fields the UI needs: an entry of the user's
/// model list.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderView {
    #[serde(flatten)]
    pub profile: ProviderProfile,
    pub network_tier: NetworkTier,
    pub has_secret: bool,
    /// The default model (new chats start on it).
    pub active: bool,
    /// Resolved image support for this card's model: the user's vision mark,
    /// else what `bigtiny::vision` discovered and remembered. `None` means no
    /// evidence either way, and the frontend falls back to the name patterns
    /// in `vision_models.ts`.
    pub accepts_images: Option<bool>,
    /// Whether this card's model can call tools: the `supports_tools`
    /// override, else detected (`ProviderProfile::tools_supported`).
    pub tools_supported: bool,
    /// This card's detected reasoning ladder (`bigtiny::effort`).
    pub effort: crate::bigtiny::effort::EffortSummary,
    /// The model, described and priced from OpenRouter's catalog.
    pub info: ModelInfo,
}

/// A provider as Settings → Providers shows it: its whole model list,
/// described and ranked.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderAccountView {
    #[serde(flatten)]
    pub profile: ProviderProfile,
    pub network_tier: NetworkTier,
    pub has_secret: bool,
    pub requires_key: bool,
    /// Every listed model, checked or not, ranked.
    pub model_infos: Vec<ModelInfo>,
}

/// Async — `has_secret` is a blocking Windows Credential Manager IPC call per
/// provider, so this runs off the main thread and offloads each lookup
/// through `get_secret_async`.
async fn card_views(
    cfg: &Config,
    catalog: Option<&catalog::OpenRouterCatalog>,
    detecting: &std::collections::HashSet<String>,
) -> Vec<ProviderView> {
    let entries = catalog.map(|c| c.entries.as_slice()).unwrap_or(&[]);
    let now = ai_models::now_unix();
    let mut views = Vec::new();
    for provider in &cfg.providers {
        let has_secret = providers::get_secret_async(&provider.id).await.is_some();
        for card in provider.cards() {
            let mut info = ai_models::describe(provider, card.model(), entries);
            info.card_id = Some(card.id.clone());
            views.push(ProviderView {
                network_tier: card.network_tier(),
                has_secret,
                active: cfg.active_provider_id.as_deref() == Some(&card.id),
                accepts_images: crate::bigtiny::vision::vision_for_profile(cfg, &card),
                tools_supported: card.tools_supported(catalog),
                effort: crate::bigtiny::effort::summary_for_profile(cfg, &card, detecting),
                info,
                profile: card,
            });
        }
    }
    let mut infos: Vec<ModelInfo> = views.iter().map(|v| v.info.clone()).collect();
    ai_models::rank(&mut infos, now);
    let order = |v: &ProviderView| {
        infos
            .iter()
            .position(|i| i.card_id == v.info.card_id)
            .unwrap_or(usize::MAX)
    };
    views.sort_by_key(order);
    views
}

/// The user's model list: every checked model of every provider, as cards,
/// ranked by intelligence and age.
#[tauri::command]
pub async fn list_providers(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ProviderView>, String> {
    // Snapshot the config out of the lock — the async secret lookups below
    // must not hold the global config Mutex across their awaited OS calls.
    let cfg = state.config.lock().unwrap().clone();
    let catalog = state.openrouter_catalog.lock().unwrap().clone();
    let detecting = state.effort_detecting.lock().unwrap().clone();
    Ok(card_views(&cfg, catalog.as_ref(), &detecting).await)
}

/// The providers themselves (Settings → Providers), each with its ranked
/// model list.
#[tauri::command]
pub async fn list_provider_accounts(app: AppHandle) -> Result<Vec<ProviderAccountView>, String> {
    let catalog = ai_models::catalog(&app).await;
    let cfg = app.state::<AppState>().config.lock().unwrap().clone();
    let now = ai_models::now_unix();
    let mut out = Vec::with_capacity(cfg.providers.len());
    for p in &cfg.providers {
        out.push(ProviderAccountView {
            network_tier: p.network_tier(),
            has_secret: providers::get_secret_async(&p.id).await.is_some(),
            requires_key: p.requires_key(),
            model_infos: ai_models::provider_models(p, &catalog, now),
            profile: p.clone(),
        });
    }
    Ok(out)
}

/// Create or update a provider. `secret`, when present, is stored in the
/// keyring only (never in config.json). Returns the saved provider (with id
/// and its models' card ids).
///
/// Every model card is mirrored to the engine at once
/// (`sync_all_providers`), so a change takes effect on the next turn.
/// Best-effort on the engine round-trip: the provider is already saved, and
/// the next attach syncs again.
#[tauri::command]
pub async fn upsert_provider(
    app: AppHandle,
    mut profile: ProviderProfile,
    secret: Option<String>,
) -> Result<ProviderProfile, String> {
    if profile.id.trim().is_empty() {
        profile.id = format!("prof_{}", chrono::Utc::now().timestamp_millis());
    }
    if profile.created_at.trim().is_empty() {
        profile.created_at = chrono::Utc::now().to_rfc3339();
    }
    if profile.provider_type == "local" {
        return Err("The \"On this device\" provider type is no longer supported.".into());
    }
    // Only a stored provider is saved here, never a card.
    profile.provider_id = None;
    profile.supports_vision = false;
    if let Some(s) = secret {
        if !s.is_empty() {
            // Async write: `set_secret` is blocking Windows Credential
            // Manager IPC, which must not run on a tokio worker.
            providers::set_secret_async(&profile.id, &s).await?;
        }
    }
    let catalog = ai_models::catalog(&app).await;
    let now = ai_models::now_unix();
    let (saved, detect) = {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        let old_cards = cfg
            .providers
            .iter()
            .find(|p| p.id == profile.id)
            .map(|p| p.cards())
            .unwrap_or_default();
        let index = match cfg.providers.iter().position(|p| p.id == profile.id) {
            Some(i) => {
                // Card ids are the config's, never the form's.
                profile.model_ids = cfg.providers[i].model_ids.clone();
                cfg.providers[i] = profile.clone();
                i
            }
            None => {
                profile.model_ids.clear();
                cfg.providers.push(profile.clone());
                cfg.providers.len() - 1
            }
        };
        providers::assign_model_ids(&mut cfg.providers, index);
        let saved = cfg.providers[index].clone();
        // Re-detect the reasoning ladder of a card whose endpoint changed, or
        // that has none yet.
        let detect: Vec<String> = saved
            .cards()
            .into_iter()
            .filter(|card| {
                let has_caps = cfg
                    .effort_caps
                    .contains_key(&crate::bigtiny::effort::effort_key(&card.id, card.model()));
                let moved = old_cards
                    .iter()
                    .find(|o| o.id == card.id)
                    .map_or(true, |o| {
                        o.provider_type != card.provider_type || o.base_url != card.base_url
                    });
                !has_caps || moved
            })
            .map(|card| card.id)
            .collect();
        // No default yet (the first provider), or the default was unchecked:
        // the provider's suggested model takes over.
        let suggested = ai_models::defaults::pick(
            &saved.provider_type,
            &saved.base_url,
            &ai_models::provider_models(&saved, &catalog, now),
            true,
            now,
        )
        .and_then(|m| saved.model_ids.get(&m).cloned());
        cfg.repair_default(suggested);
        if cfg.active_provider_id.is_some() {
            cfg.needs_default_provider = false;
        }
        config::save(&cfg).map_err(|e| e.to_string())?;
        (saved, detect)
    };
    for id in &detect {
        // Background: detection may probe a self-hosted server or HuggingFace,
        // and saving must not wait on that. Settings and open chats refresh
        // on `provider://effort-detected`.
        crate::bigtiny::effort::spawn_detect(&app, id);
    }
    if let Err(e) = crate::bigtiny::providers::sync_all_providers(&app).await {
        tracing::warn!(
            "provider {} saved but failed to sync to the engine: {e}",
            saved.id
        );
    }
    // Probe image support now rather than waiting for the first turn, so the
    // composer offers (or hides) the attach controls correctly from the
    // moment the model is checked. Best-effort and cached after the first
    // success.
    for card in saved.cards() {
        crate::bigtiny::vision::ensure_vision_cached(&app, &card.id).await;
    }
    Ok(saved)
}

/// Re-detect one card's reasoning levels. The old ladder stays until the new
/// detection succeeds.
#[tauri::command]
pub async fn redetect_effort(app: AppHandle, provider_id: String) -> Result<(), String> {
    crate::bigtiny::effort::detect_card(&app, &provider_id).await;
    Ok(())
}

/// Re-detect every card's reasoning levels (Settings → Providers →
/// Re-detect reasoning).
#[tauri::command]
pub async fn redetect_effort_all(app: AppHandle) -> Result<(), String> {
    crate::bigtiny::effort::detect_all(&app, false).await;
    Ok(())
}

/// Delete a provider: its key, its config entry and its models' rows in the
/// engine. When the default model was one of its models, the next usable
/// model takes over; returns the default's card id, so the UI can say which
/// one it is now.
///
/// `async` for a platform reason, not a performance one: a *synchronous*
/// `#[tauri::command]` runs on the main thread, and on Android the secret
/// store is reached by posting to the main looper and waiting for the reply
/// (`android::secrets`). A sync command touching a secret therefore deadlocks
/// the app.
#[tauri::command]
pub async fn delete_provider(app: AppHandle, id: String) -> Result<Option<String>, String> {
    providers::delete_secret(&id);
    let promoted = {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        let Some(index) = cfg.providers.iter().position(|p| p.id == id) else {
            return Ok(cfg.active_provider_id.clone());
        };
        cfg.providers.remove(index);
        cfg.repair_default(None);
        cfg.needs_default_provider = false;
        config::save(&cfg).map_err(|e| e.to_string())?;
        cfg.active_provider_id.clone()
    };
    // Removes the rows (and the keys the engine held for them) and sets the
    // new default.
    if let Err(e) = crate::bigtiny::providers::sync_all_providers(&app).await {
        tracing::warn!("deleted provider {id} but failed to sync the engine: {e}");
    }
    Ok(promoted)
}

/// Best-effort context-length lookup for OpenRouter models, for the Providers
/// form's auto-suggest (Round-6 Feature 1). `Ok(None)` (not `Err`) when the
/// model isn't found in the list — this is a suggestion, not a required value.
#[tauri::command]
pub async fn openrouter_context_length(model: String) -> Result<Option<u32>, String> {
    let models = openrouter::list_models().await?;
    Ok(openrouter::context_length_for(&models, &model))
}

/// Best-effort context-length lookup for a **remote** Ollama server, via
/// `POST /api/show`. The Ollama case lost its live lookup when managed Ollama
/// was retired (commit `8c5fbef`); this restores it for the endpoint the user
/// runs themselves. `Ok(None)` (never `Err` on a shape we don't recognize)
/// keeps the field manually editable — this only ever *suggests* a value.
#[tauri::command]
pub async fn ollama_context_length(base_url: String, model: String) -> Result<Option<u32>, String> {
    let url = format!("{}/api/show", base_url.trim_end_matches('/'));
    let resp = crate::util::http_client()
        .post(url)
        .json(&serde_json::json!({ "model": model }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("could not reach Ollama: {e}"))?;
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(context_length_from_show(&json))
}

/// Best-effort vision probe for a **remote** Ollama server, via the same
/// `POST /api/show` call `ollama_context_length` already makes.
///
/// Ollama reports a top-level `capabilities` array (`["completion","vision"]`,
/// or `"multimodal"` on some builds). Kitty was guessing vision support from
/// the model *name* while this authoritative answer sat one field away in a
/// response it was already fetching for the context length — the `qwen3.6`
/// entry in `vision_models.ts` even documents someone reading this array by
/// hand and hardcoding the result.
///
/// `Ok(None)` rather than `Err` on an unrecognized shape, matching every other
/// probe here: an absent capabilities array (older Ollama) must stay
/// distinguishable from a definite "no vision", because only the latter is
/// allowed to narrow what the UI offers.
#[tauri::command]
pub async fn ollama_accepts_images(
    base_url: String,
    model: String,
) -> Result<Option<bool>, String> {
    let url = format!("{}/api/show", base_url.trim_end_matches('/'));
    let resp = crate::util::http_client()
        .post(url)
        .json(&serde_json::json!({ "model": model }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("could not reach Ollama: {e}"))?;
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(vision_from_show(&json))
}

/// Read the `capabilities` array out of an Ollama `/api/show` response. Split
/// out pure so it is testable without a live server, like
/// `context_length_from_show` beside it.
fn vision_from_show(json: &serde_json::Value) -> Option<bool> {
    let caps = json.get("capabilities")?.as_array()?;
    Some(
        caps.iter()
            .filter_map(|c| c.as_str())
            .any(|c| c.eq_ignore_ascii_case("vision") || c.eq_ignore_ascii_case("multimodal")),
    )
}

/// Pull `<arch>.context_length` out of an Ollama `/api/show` `model_info`
/// block. The architecture prefix varies per model (`llama.`, `qwen2.`,
/// `gemma3.`, …), so match on the `.context_length` suffix rather than
/// hardcoding a family. Split out pure so it's unit-testable without a live
/// server, mirroring `providers::connection::tags_response_has_tag`.
fn context_length_from_show(json: &serde_json::Value) -> Option<u32> {
    json.get("model_info")
        .and_then(|m| m.as_object())
        .and_then(|obj| {
            obj.iter()
                .find(|(k, _)| k.ends_with(".context_length"))
                .and_then(|(_, v)| v.as_u64())
        })
        .and_then(|n| u32::try_from(n).ok())
}

/// Best-effort context-length lookup for a `custom_openai` server (release-
/// fixes item 15) — unlike Ollama there's no one API every such server
/// implements, so this tries two shapes in order and returns `Ok(None)`
/// (never `Err`) the moment neither one is recognized, keeping the field
/// manually editable exactly like the Ollama/OpenRouter suggestions above:
///
/// 1. `GET /props` — llama.cpp server's own endpoint; several field names
///    have been used across versions (`n_ctx`, `n_ctx_train`,
///    `default_generation_settings.n_ctx`), so all three are tried.
/// 2. `GET /v1/models` — the OpenAI-compatible listing. Real OpenAI carries
///    no context-length field, but several self-hosted servers (LM Studio
///    and others) add a non-standard `context_length` per entry in `data`.
#[tauri::command]
pub async fn custom_openai_context_length(
    base_url: String,
    model: String,
) -> Result<Option<u32>, String> {
    let base = base_url.trim_end_matches('/');
    let client = crate::util::http_client();

    if let Ok(resp) = client
        .get(format!("{base}/props"))
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        if let Ok(json) = resp.json::<serde_json::Value>().await {
            if let Some(n) = context_length_from_props(&json) {
                return Ok(Some(n));
            }
        }
    }

    if let Ok(resp) = client
        .get(format!("{base}/v1/models"))
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        if let Ok(json) = resp.json::<serde_json::Value>().await {
            if let Some(n) = context_length_from_models_list(&json, &model) {
                return Ok(Some(n));
            }
        }
    }

    Ok(None)
}

fn context_length_from_props(json: &serde_json::Value) -> Option<u32> {
    for path in [
        &["n_ctx"][..],
        &["n_ctx_train"][..],
        &["default_generation_settings", "n_ctx"][..],
    ] {
        let mut cur = json;
        let mut ok = true;
        for key in path {
            match cur.get(key) {
                Some(v) => cur = v,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            if let Some(n) = cur.as_u64().and_then(|n| u32::try_from(n).ok()) {
                return Some(n);
            }
        }
    }
    None
}

fn context_length_from_models_list(json: &serde_json::Value, model: &str) -> Option<u32> {
    json.get("data")?.as_array()?.iter().find_map(|entry| {
        let id = entry.get("id").and_then(|v| v.as_str())?;
        if id != model {
            return None;
        }
        entry
            .get("context_length")
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok())
    })
}

/// Check an OpenRouter provider profile's current credit balance/usage.
/// Reads the key from the keyring (never sent to/stored by Kitty otherwise)
/// — errors if the profile has no stored secret or isn't an OpenRouter profile.
#[tauri::command]
pub async fn openrouter_credits(
    state: tauri::State<'_, AppState>,
    provider_id: String,
) -> Result<serde_json::Value, String> {
    // A provider's id, or one of its cards'.
    let profile = {
        let cfg = state.config.lock().unwrap();
        cfg.providers
            .iter()
            .find(|p| p.id == provider_id)
            .or_else(|| cfg.provider_of_card(&provider_id))
            .cloned()
            .ok_or("no such provider profile")?
    };
    let provider_id = profile.id.clone();
    if profile.provider_type != "openrouter" {
        return Err("not an OpenRouter profile".into());
    }
    let key = providers::get_secret_async(&provider_id)
        .await
        .ok_or("no API key stored for this profile — edit it and add one")?;
    openrouter::get_credits(&key).await
}

/// Check a provider's key and list the models it offers, for its checklist
/// (the form then describes and ranks them: `describe_provider_models`).
/// `secret` is the raw, unsaved form value; when it is blank and the
/// provider is saved (`provider_id`), the stored key is used. Never logged.
/// Never fabricates entries.
#[tauri::command]
pub async fn discover_provider_models(
    state: tauri::State<'_, AppState>,
    provider_type: String,
    base_url: String,
    provider_id: Option<String>,
    secret: Option<String>,
) -> Result<Vec<String>, String> {
    let typed = secret
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let key = match (typed, provider_id.filter(|id| !id.is_empty())) {
        (Some(k), _) => Some(k),
        (None, Some(id)) => providers::get_secret_async(&id).await,
        (None, None) => None,
    };
    let mut models = discover_models(&state, &provider_type, &base_url, key.as_deref()).await?;
    models.sort();
    models.dedup();
    if models.is_empty() {
        return Err("The provider listed no models.".into());
    }
    Ok(models)
}

/// Per-type dispatch behind [`discover_provider_models`]. Starts by
/// refreshing the OpenRouter catalog cache if it is stale — opening a
/// provider's form is what keeps the catalog from going stale over a
/// long-running session, not a background timer.
async fn discover_models(
    state: &AppState,
    provider_type: &str,
    base_url: &str,
    key: Option<&str>,
) -> Result<Vec<String>, String> {
    catalog::ensure_catalog_fresh(state).await;
    let need_key = || key.ok_or_else(|| "Enter the provider's API key first.".to_string());
    match provider_type {
        "openrouter" => {
            // The credits endpoint is the key check — the catalog itself
            // needs no key, so it is the model list.
            openrouter::get_credits(need_key()?).await?;
            let ids: Vec<String> = state
                .openrouter_catalog
                .lock()
                .unwrap()
                .as_ref()
                .map(|c| c.entries.iter().map(|e| e.id.clone()).collect())
                .unwrap_or_default();
            if ids.is_empty() {
                return Err("OpenRouter's model list could not be loaded — try again.".into());
            }
            Ok(ids)
        }
        "anthropic" => {
            let resp = crate::util::http_client()
                .get(format!(
                    "{}/v1/models?limit=1000",
                    base_url.trim_end_matches('/')
                ))
                .header("x-api-key", need_key()?)
                .header("anthropic-version", "2023-06-01")
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await
                .map_err(|e| format!("could not reach Anthropic: {e}"))?;
            if !resp.status().is_success() {
                return Err(classify_key_error(resp.status(), "Anthropic"));
            }
            let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
            Ok(ids_of(&json["data"], "id"))
        }
        "ollama" => {
            let resp = crate::util::http_client()
                .get(format!("{}/api/tags", base_url.trim_end_matches('/')))
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
                .map_err(|e| format!("could not reach Ollama: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("Ollama answered {}", resp.status()));
            }
            let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
            Ok(ids_of(&json["models"], "name"))
        }
        // OpenAI and every OpenAI-compatible service: `GET /models`. A
        // schemeless self-hosted entry fans out to https-then-http.
        _ => {
            let mut attempts: Vec<(String, String)> = Vec::new();
            for base in providers::endpoint::candidates(base_url) {
                let mut req = crate::util::http_client()
                    .get(format!("{base}/models"))
                    .timeout(std::time::Duration::from_secs(15));
                if let Some(k) = key {
                    req = req.bearer_auth(k);
                }
                match req.send().await {
                    Ok(resp) if resp.status().is_success() => {
                        let json: serde_json::Value =
                            resp.json().await.map_err(|e| e.to_string())?;
                        return Ok(ids_of(&json["data"], "id"));
                    }
                    // A reachable endpoint that refuses us: a key problem,
                    // which the other scheme will not fix.
                    Ok(resp) => return Err(classify_key_error(resp.status(), "The provider")),
                    Err(e) => attempts.push((base, providers::endpoint::humanize(&e))),
                }
            }
            Err(providers::endpoint::unreachable_message(&attempts))
        }
    }
}

fn ids_of(list: &serde_json::Value, field: &str) -> Vec<String> {
    list.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m[field].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn classify_key_error(status: reqwest::StatusCode, who: &str) -> String {
    if status.as_u16() == 401 || status.as_u16() == 403 {
        format!("{who} rejected that API key — check it and try again.")
    } else {
        format!("{who} returned {status}")
    }
}

/// Check that a provider card works: reachable, and its key accepted. Used by
/// the provider form's "Test connection", the chat's offline banner while it
/// waits for a provider to come back, and before a card is made the default.
#[tauri::command]
pub async fn test_provider_connection(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let profile = {
        let cfg = state.config.lock().unwrap();
        cfg.card(&id)
    };
    let profile = profile.ok_or("That model is no longer available.")?;
    providers::test_connection(&profile).await
}

/// "Test connection" in the provider form, for any type (#66): the card as
/// edited, with the key typed in the form or, when that is blank, the one
/// already saved for it.
#[tauri::command]
pub async fn test_provider_draft(
    profile: ProviderProfile,
    secret: Option<String>,
) -> Result<(), String> {
    let typed = secret
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let key = match typed {
        Some(k) => Some(k),
        None if !profile.id.is_empty() => {
            providers::get_secret_async(profile.secret_account()).await
        }
        None => None,
    };
    providers::test_connection_with_key(&profile, key).await
}

/// [`test_provider_connection`] for the default card. `Ok(())` when there is
/// no default: nothing to check.
#[tauri::command]
pub async fn test_active_provider_connection(
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let profile = {
        let cfg = state.config.lock().unwrap();
        cfg.active_provider_id.as_ref().and_then(|id| cfg.card(id))
    };
    match profile {
        Some(p) => providers::test_connection(&p).await,
        None => Ok(()),
    }
}

fn usable_profile(app: &AppHandle, id: &str) -> Result<ProviderProfile, String> {
    let state = app.state::<AppState>();
    let cfg = state.config.lock().unwrap();
    let profile = cfg
        .card(id)
        .ok_or("That model is no longer available — check it under Settings → Providers.")?;
    if !profile.is_usable() {
        return Err(format!(
            "{} can no longer be used; choose another provider.",
            profile.name
        ));
    }
    Ok(profile)
}

/// Make a card the default: the one new chats start on. Settings only - a
/// chat's own card is [`set_chat_provider`], which never changes this.
///
/// Gated on a connection test, so a card that does not work cannot become
/// what every new chat uses; the old default stays on failure.
#[tauri::command]
pub async fn set_default_provider(app: AppHandle, id: String) -> Result<(), String> {
    let profile = usable_profile(&app, &id)?;
    providers::test_connection(&profile)
        .await
        .map_err(|e| format!("Can't make {} the default — {e}", profile.name))?;
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.active_provider_id = Some(id.clone());
        cfg.needs_default_provider = false;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    crate::bigtiny::providers::sync_all_providers(&app).await?;
    let _ = app.emit(
        "provider://activated",
        serde_json::json!({ "session_id": null, "provider_id": id, "model": null }),
    );
    Ok(())
}

/// Put one chat on a card (the chat badge, on a chat with no messages yet).
/// Only that chat changes: the default and every other chat keep theirs
/// (decision #50). The chat's system prompt follows the card.
///
/// A chat that already has history moves to another card by branching
/// instead (`branch_to_provider`), which keeps the original intact.
#[tauri::command]
pub async fn set_chat_provider(
    app: AppHandle,
    session_id: String,
    id: String,
) -> Result<(), String> {
    let profile = usable_profile(&app, &id)?;
    providers::test_connection(&profile)
        .await
        .map_err(|e| format!("Can't switch to {} — {e}", profile.name))?;
    let model = profile.model().to_string();
    crate::bigtiny::providers::set_session_provider(&app, &session_id, &profile.id, &model).await;
    if let Err(e) = crate::bigtiny::sessions::update_persona_override(
        &app,
        &session_id,
        &providers::system_prompt_for(&profile),
    )
    .await
    {
        tracing::warn!("could not set {session_id}'s system prompt: {e}");
    }
    let _ = app.emit(
        "provider://activated",
        serde_json::json!({ "session_id": session_id, "provider_id": id, "model": model }),
    );
    Ok(())
}

/// The pre-v1 single entry point, kept while the frontend moves to the two
/// above: with a session it changes that chat only, without one the default.
#[tauri::command]
pub async fn activate_provider(
    app: AppHandle,
    id: Option<String>,
    session_id: Option<String>,
) -> Result<(), String> {
    let id = id.ok_or("A provider must be active — add one in Settings → Providers.")?;
    match session_id {
        Some(session_id) => set_chat_provider(app, session_id, id).await,
        None => set_default_provider(app, id).await,
    }
}

/// Stamp a single session with a specific provider/model (`PATCH
/// /api/chat/{id}/config`) without touching the default or any other
/// session. Used when resuming a session that should keep its own card.
#[tauri::command]
pub async fn set_session_provider(
    app: AppHandle,
    session_id: String,
    provider_id: String,
    model: Option<String>,
) -> Result<(), String> {
    crate::bigtiny::providers::set_session_provider(
        &app,
        &session_id,
        &provider_id,
        model.as_deref().unwrap_or(""),
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_length_from_show_reads_the_arch_prefixed_key() {
        let body = json!({
            "model_info": {
                "general.architecture": "llama",
                "llama.context_length": 131072,
                "llama.embedding_length": 4096
            }
        });
        assert_eq!(context_length_from_show(&body), Some(131072));
    }

    #[test]
    fn context_length_from_show_handles_a_different_family_prefix() {
        // The prefix is the model's architecture, not always "llama".
        let body = json!({ "model_info": { "qwen2.context_length": 32768 } });
        assert_eq!(context_length_from_show(&body), Some(32768));
    }

    #[test]
    fn context_length_from_show_none_when_field_absent() {
        let body = json!({ "model_info": { "general.architecture": "llama" } });
        assert_eq!(context_length_from_show(&body), None);
    }

    #[test]
    fn context_length_from_show_none_on_unexpected_shape() {
        // A server answering with something other than Ollama's shape must
        // read as "unknown", not panic.
        let body = json!({ "unexpected": "shape" });
        assert_eq!(context_length_from_show(&body), None);
    }

    /// The signal Kitty was guessing at from model names, sitting in a
    /// response it already made for the context length. Both spellings are
    /// real: `"vision"` is current Ollama, `"multimodal"` appears on some
    /// builds (the `qwen3.6` note in `vision_models.ts` records seeing it).
    #[test]
    fn vision_from_show_reads_the_capabilities_array() {
        for caps in [
            serde_json::json!(["completion", "vision"]),
            serde_json::json!(["completion", "multimodal"]),
            serde_json::json!(["VISION"]),
        ] {
            let body = serde_json::json!({ "capabilities": caps });
            assert_eq!(vision_from_show(&body), Some(true), "{caps}");
        }
        let text_only = serde_json::json!({"capabilities": ["completion", "tools"]});
        assert_eq!(vision_from_show(&text_only), Some(false));
    }

    /// A missing array is "nobody answered", NOT "no vision" — only the
    /// latter is allowed to turn image affordances off, so collapsing the two
    /// would silently disable images on every older Ollama build.
    #[test]
    fn a_missing_capabilities_array_is_unknown_not_a_negative() {
        assert_eq!(vision_from_show(&serde_json::json!({})), None);
        assert_eq!(
            vision_from_show(&serde_json::json!({"capabilities": "vision"})),
            None
        );
    }

    #[test]
    fn context_length_from_show_none_when_value_overflows_u32() {
        // Implausible, but a garbage huge number must degrade to "unknown"
        // rather than wrapping to a small one.
        let body = json!({ "model_info": { "llama.context_length": 5_000_000_000u64 } });
        assert_eq!(context_length_from_show(&body), None);
    }
}
