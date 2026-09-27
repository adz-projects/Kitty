//! Provider card commands: list/create/update/duplicate/delete, choosing the
//! default card (Settings) and a chat's card (the chat badge), and the
//! connection test. Every change is mirrored to the engine by
//! `bigtiny::providers::sync_all_providers`.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::config;
use crate::config::providers::{self, NetworkTier, ProviderProfile};
use crate::config::Config;
use crate::openrouter;
use crate::openrouter::catalog;
use crate::state::AppState;

/// A provider profile plus derived fields the UI needs.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderView {
    #[serde(flatten)]
    pub profile: ProviderProfile,
    pub network_tier: NetworkTier,
    pub has_secret: bool,
    pub active: bool,
    /// Resolved image support for this profile's model: the manual
    /// `supports_vision` override, else what `bigtiny::vision` discovered and
    /// remembered. `None` means no evidence either way, and the frontend
    /// falls back to the name patterns in `vision_models.ts`.
    ///
    /// Distinct from the flattened `supports_vision` field, which is only the
    /// user's manual override — a `Some(false)` here is a real negative from
    /// the provider and does turn image affordances off.
    pub accepts_images: Option<bool>,
    /// Whether this card's model can call tools: the `supports_tools`
    /// override, else detected (`ProviderProfile::tools_supported`).
    pub tools_supported: bool,
}

/// Async — `has_secret` is a blocking Windows Credential Manager IPC call per
/// profile, so this runs off the main thread and offloads each lookup through
/// `get_secret_async` (a `list_providers` call with many profiles would
/// otherwise block the command thread for the full round of OS dialogs).
async fn provider_views(
    cfg: &Config,
    catalog: Option<&catalog::OpenRouterCatalog>,
) -> Vec<ProviderView> {
    let mut views = Vec::with_capacity(cfg.providers.len());
    for p in &cfg.providers {
        let has_secret = providers::get_secret_async(&p.id).await.is_some();
        views.push(ProviderView {
            network_tier: p.network_tier(),
            has_secret,
            active: cfg.active_provider_id.as_deref() == Some(&p.id),
            accepts_images: crate::bigtiny::vision::vision_for_profile(cfg, p),
            tools_supported: p.tools_supported(catalog),
            profile: p.clone(),
        });
    }
    views
}

/// List provider profiles with derived tier / secret / active flags.
#[tauri::command]
pub async fn list_providers(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ProviderView>, String> {
    // Snapshot the config out of the lock — the async secret lookups below
    // must not hold the global config Mutex across their awaited OS calls.
    let cfg = state.config.lock().unwrap().clone();
    let catalog = state.openrouter_catalog.lock().unwrap().clone();
    Ok(provider_views(&cfg, catalog.as_ref()).await)
}

/// Create or update a provider card. `secret`, when present, is stored in the
/// keyring only (never in config.json). Returns the saved card (with id).
///
/// Every card is mirrored to the engine at once (`sync_all_providers`), so an
/// edit to any card - not only the default - takes effect on its next turn.
/// Best-effort on the engine round-trip: the card is already saved, and the
/// next attach syncs again.
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
    if let Some(s) = secret {
        if !s.is_empty() {
            // Async write: `set_secret` is blocking Windows Credential
            // Manager IPC, which must not run on a tokio worker.
            providers::set_secret_async(&profile.id, &s).await?;
        }
    }
    let is_default;
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        match cfg.providers.iter_mut().find(|p| p.id == profile.id) {
            Some(existing) => *existing = profile.clone(),
            None => cfg.providers.push(profile.clone()),
        }
        config::save(&cfg).map_err(|e| e.to_string())?;
        is_default = cfg.active_provider_id.as_deref() == Some(profile.id.as_str());
    }

    if let Err(e) = crate::bigtiny::providers::sync_all_providers(&app).await {
        tracing::warn!(
            "provider {} saved but failed to sync to the engine: {e}",
            profile.id
        );
    }
    if is_default {
        // Probe image support now rather than waiting for the first turn, so
        // the composer offers (or hides) the attach controls correctly from
        // the moment the profile is saved. Best-effort and already cached
        // after the first success.
        crate::bigtiny::vision::ensure_vision_cached(&app).await;
    }
    Ok(profile)
}

/// Copy a provider card — the way to use a second model from the same
/// provider, since each card holds exactly one. The copy gets a fresh id, the
/// source's secret, and a "(copy)" name; it is never the default.
#[tauri::command]
pub async fn duplicate_provider(app: AppHandle, id: String) -> Result<ProviderProfile, String> {
    let mut copy = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.providers
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| "That provider no longer exists.".to_string())?
    };
    copy.id = format!("prof_{}", chrono::Utc::now().timestamp_millis());
    copy.name = format!("{} (copy)", copy.name);
    copy.created_at = chrono::Utc::now().to_rfc3339();
    if let Some(secret) = providers::get_secret_checked(&id).await? {
        providers::set_secret_async(&copy.id, &secret).await?;
    }
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.providers.push(copy.clone());
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    if let Err(e) = crate::bigtiny::providers::sync_all_providers(&app).await {
        tracing::warn!("duplicated provider {} but failed to sync it: {e}", copy.id);
    }
    Ok(copy)
}

/// Delete a provider card: its key, its config entry and its row in the
/// engine. Deleting the default promotes the next usable card (decision #5)
/// and returns its id, so the UI can say which one it is now.
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
        if cfg.active_provider_id.as_deref() == Some(&id) {
            cfg.active_provider_id = providers::next_default(&cfg.providers, index);
            cfg.needs_default_provider =
                cfg.active_provider_id.is_none() && !cfg.providers.is_empty();
        }
        config::save(&cfg).map_err(|e| e.to_string())?;
        cfg.active_provider_id.clone()
    };
    // Removes the row (and the key the engine held for it) and sets the
    // promoted default.
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
    let profile = {
        let cfg = state.config.lock().unwrap();
        cfg.providers
            .iter()
            .find(|p| p.id == provider_id)
            .cloned()
            .ok_or("no such provider profile")?
    };
    if profile.provider_type != "openrouter" {
        return Err("not an OpenRouter profile".into());
    }
    let key = providers::get_secret_async(&provider_id)
        .await
        .ok_or("no API key stored for this profile — edit it and add one")?;
    openrouter::get_credits(&key).await
}

/// One selectable row in the provider-add model picker (mirrors TS
/// `ModelPickerEntry`, `src/lib/types.ts`). `matched: false` means the id
/// came straight from the vendor's own key-validation response (the key
/// really does grant access to it) but didn't cross-reference against the
/// cached OpenRouter catalog — every ranking field is `None` in that case,
/// never fabricated.
#[derive(Debug, Clone, Serialize)]
pub struct ModelPickerEntry {
    pub id: String,
    pub name: String,
    pub cost_tier: Option<catalog::CostTier>,
    pub capability_score: Option<f64>,
    pub price_rank: Option<f64>,
    pub created: Option<i64>,
    pub context_length: Option<u32>,
    pub matched: bool,
}

/// Validate an in-progress (unsaved) provider profile's API key and return
/// its available models, ranked/cost-tagged where possible. Never touches
/// the keyring — the profile doesn't exist yet, so `secret` is the raw,
/// unsaved form value, and it's never logged.
#[tauri::command]
pub async fn discover_provider_models(
    state: tauri::State<'_, AppState>,
    provider_type: String,
    base_url: String,
    secret: String,
) -> Result<Vec<ModelPickerEntry>, String> {
    discover_models_internal(&state, &provider_type, &base_url, &secret).await
}

/// Same as `discover_provider_models`, but for an already-saved profile
/// whose key the user left blank while editing ("leave blank to keep",
/// `ProviderForm.tsx`) — reads `provider_type`/`base_url` from the saved
/// profile and the secret from the keyring instead of taking them as params.
#[tauri::command]
pub async fn discover_provider_models_for_saved(
    state: tauri::State<'_, AppState>,
    provider_id: String,
) -> Result<Vec<ModelPickerEntry>, String> {
    let (provider_type, base_url) = {
        let cfg = state.config.lock().unwrap();
        let p = cfg
            .providers
            .iter()
            .find(|p| p.id == provider_id)
            .ok_or("no such provider profile")?;
        (p.provider_type.clone(), p.base_url.clone())
    };
    let secret = providers::get_secret_async(&provider_id)
        .await
        .ok_or("no API key stored for this profile — edit it and add one")?;
    discover_models_internal(&state, &provider_type, &base_url, &secret).await
}

/// Shared per-provider-type dispatch behind both commands above. Always
/// starts by refreshing the OpenRouter catalog cache if it's stale — this
/// (opening Add/Edit Provider) is what keeps the catalog from going stale
/// over a long-running session, not a background timer.
async fn discover_models_internal(
    state: &AppState,
    provider_type: &str,
    base_url: &str,
    secret: &str,
) -> Result<Vec<ModelPickerEntry>, String> {
    catalog::ensure_catalog_fresh(state).await;
    let catalog_entries: Vec<catalog::OpenRouterCatalogEntry> = state
        .openrouter_catalog
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.entries.clone())
        .unwrap_or_default();

    match provider_type {
        "openrouter" => {
            // The credits endpoint is the validation step — the catalog
            // itself needs no key at all, so this is purely "does this key
            // work", not a second data source.
            openrouter::get_credits(secret).await?;
            Ok(catalog_entries
                .iter()
                .map(|e| ModelPickerEntry {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    cost_tier: e.cost_tier,
                    capability_score: e.intelligence_index,
                    price_rank: e.price_rank,
                    created: e.created,
                    context_length: e.context_length,
                    matched: true,
                })
                .collect())
        }
        "anthropic" => {
            let client = crate::util::http_client();
            let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
            let resp = client
                .get(url)
                .header("x-api-key", secret)
                .header("anthropic-version", "2023-06-01")
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await
                .map_err(|e| format!("could not reach Anthropic: {e}"))?;
            if !resp.status().is_success() {
                return Err(classify_key_error(resp.status(), "Anthropic"));
            }
            let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
            Ok(merge_with_catalog(
                parse_anthropic_models(&json),
                &catalog_entries,
            ))
        }
        "openai" | "fireworks" | "deepinfra" => {
            let client = crate::util::http_client();
            let url = format!("{}/models", base_url.trim_end_matches('/'));
            let resp = client
                .get(url)
                .bearer_auth(secret)
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await
                .map_err(|e| format!("could not reach the provider: {e}"))?;
            if !resp.status().is_success() {
                return Err(classify_key_error(resp.status(), "The provider"));
            }
            let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
            Ok(merge_with_catalog(
                parse_openai_compat_models(&json),
                &catalog_entries,
            ))
        }
        other => Err(format!("model discovery isn't available for \"{other}\"")),
    }
}

fn classify_key_error(status: reqwest::StatusCode, who: &str) -> String {
    if status.as_u16() == 401 || status.as_u16() == 403 {
        format!("{who} rejected that API key — check it and try again.")
    } else {
        format!("{who} returned {status}")
    }
}

/// `(id, display_name, created_unix)` per model. Anthropic's own timestamp
/// field is `created_at` (RFC3339), not a unix int — parsed opportunistically
/// so an unmatched Anthropic model can still sort under "Newest".
fn parse_anthropic_models(json: &serde_json::Value) -> Vec<(String, Option<String>, Option<i64>)> {
    json.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id").and_then(|v| v.as_str())?.to_string();
                    let name = m
                        .get("display_name")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let created = m
                        .get("created_at")
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.timestamp());
                    Some((id, name, created))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `(id, None, created_unix)` per model — the standard OpenAI-compatible
/// `/models` shape has no human-readable name field, only `id`/`created`/
/// `owned_by`.
fn parse_openai_compat_models(
    json: &serde_json::Value,
) -> Vec<(String, Option<String>, Option<i64>)> {
    json.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id").and_then(|v| v.as_str())?.to_string();
                    let created = m.get("created").and_then(|v| v.as_i64());
                    Some((id, None, created))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Cross-reference each raw `(id, name?, created?)` against the cached
/// OpenRouter catalog. A model with no match still appears — `matched:
/// false`, ranking fields all `None` — never hidden, never fabricated. The
/// vendor's own name/created (when given) always wins over the catalog's.
fn merge_with_catalog(
    raw: Vec<(String, Option<String>, Option<i64>)>,
    catalog_entries: &[catalog::OpenRouterCatalogEntry],
) -> Vec<ModelPickerEntry> {
    raw.into_iter()
        .map(
            |(id, name_opt, created_opt)| match catalog::match_in_catalog(&id, catalog_entries) {
                Some(e) => ModelPickerEntry {
                    name: name_opt.unwrap_or_else(|| e.name.clone()),
                    cost_tier: e.cost_tier,
                    capability_score: e.intelligence_index,
                    price_rank: e.price_rank,
                    created: created_opt.or(e.created),
                    context_length: e.context_length,
                    matched: true,
                    id,
                },
                None => ModelPickerEntry {
                    name: name_opt.unwrap_or_else(|| id.clone()),
                    cost_tier: None,
                    capability_score: None,
                    price_rank: None,
                    created: created_opt,
                    context_length: None,
                    matched: false,
                    id,
                },
            },
        )
        .collect()
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
        cfg.providers.iter().find(|p| p.id == id).cloned()
    };
    let profile = profile.ok_or("That provider no longer exists.")?;
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
        None if !profile.id.is_empty() => providers::get_secret_async(&profile.id).await,
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
        cfg.active_provider_id
            .as_ref()
            .and_then(|id| cfg.providers.iter().find(|p| &p.id == id).cloned())
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
        .providers
        .iter()
        .find(|p| p.id == id)
        .cloned()
        .ok_or("That provider no longer exists.")?;
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
    let model = profile.models.first().cloned().unwrap_or_default();
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
