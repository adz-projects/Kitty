//! Discovering, remembering, and confirming whether each model accepts images.
//!
//! Kitty decided this from the model *name* — a 24-entry regex table in
//! `src/lib/vision_models.ts` whose own header admits "the backend exposes no
//! reliable per-model capability signal for this". Two failure modes followed
//! from that: a self-hosted or renamed vision model had its image affordances
//! hidden with no recourse except a manual override, and a text-only model
//! with a vision-ish name offered an attach button that produced a failed
//! turn.
//!
//! The signal was in fact already there, in responses Kitty was already
//! fetching. Ollama's `POST /api/show` — the very call `context_window` makes
//! for the context length — returns a `capabilities` array; OpenRouter's model
//! list carries `architecture.input_modalities`, which the catalog parser
//! simply dropped. The `qwen3.6` entry in `vision_models.ts` documents someone
//! reading that array by hand and hardcoding the answer, which is the clearest
//! possible sign this belonged in code.
//!
//! **Deliberately mirrors `bigtiny::context_window`**, which mirrors
//! `bigtiny::effort`: same `provider\0model` key, same first-turn
//! confirmation, same write-only-when-changed rule, same ride on
//! `AppState::effort_confirmed_sessions` so a turn pays for at most one
//! confirmation pass across all three.
//!
//! Resolution order, highest first:
//!   1. The profile's `supports_vision` flag when set — the user ticked it, so
//!      it wins.
//!   2. The remembered verdict for this exact provider+model.
//!   3. Live discovery (below).
//!   4. Nothing — the frontend falls back to name-pattern detection.
//!
//! Note step 2/3 may answer a definite **false**, and that is allowed to turn
//! image affordances off for a model the name patterns match. A provider that
//! reports its own capabilities is better evidence than a regex over its name,
//! and the whole point is to stop wasting turns on attachments that cannot
//! work. The manual flag still only ever widens.

use tauri::{AppHandle, Manager};

use crate::state::AppState;

/// Cache key, identical in shape to `context_window::model_key` and
/// `effort::effort_cache_key`: the same model id behind two endpoints can
/// genuinely differ, since a self-hosted build may or may not have the
/// projector weights the hosted one ships with.
fn model_key(provider_id: &str, model: &str) -> String {
    format!("{provider_id}\u{0}{model}")
}

/// The active provider's `(id, provider_type, base_url, model, explicit)`.
/// Split out so the config lock is never held across an `await` — same reason
/// as its twin in `context_window`.
fn active_provider(app: &AppHandle) -> Option<(String, String, String, String, bool)> {
    let state = app.state::<AppState>();
    let cfg = state.config.lock().unwrap();
    let active_id = cfg.active_provider_id.as_deref()?;
    let p = cfg.providers.iter().find(|p| p.id == active_id)?;
    Some((
        p.id.clone(),
        p.provider_type.clone(),
        p.base_url.clone(),
        p.models.first().cloned().unwrap_or_default(),
        p.supports_vision,
    ))
}

/// Live lookup for one provider+model. Best-effort: every source answers
/// `None` rather than erroring, because an undiscoverable capability is not a
/// failure — it just leaves the frontend on name-pattern detection.
async fn discover(
    app: &AppHandle,
    provider_type: &str,
    base_url: &str,
    model: &str,
) -> Option<bool> {
    if model.trim().is_empty() {
        return None;
    }
    match provider_type {
        "ollama" => crate::commands::ollama_accepts_images(base_url.to_string(), model.to_string())
            .await
            .ok()
            .flatten(),
        // A llama.cpp/LM Studio server exposes no agreed capability field, and
        // guessing from `/props` would be inventing a signal rather than
        // reading one. These are exactly the profiles the manual override
        // exists for.
        "custom_openai" | "local" => None,
        // Hosted providers go through the OpenRouter catalog, the app's
        // existing universal capability source — already cached in `AppState`.
        _ => catalog_accepts_images(app, model).await,
    }
}

/// Catalog lookup via `match_in_catalog`, not id equality — the same
/// distinction `context_window::catalog_context_length` documents: a direct
/// Anthropic profile on `claude-sonnet-4-20250514` has to resolve against the
/// catalog's `anthropic/claude-sonnet-4`, which needs the vendor-prefix,
/// case, and `-YYYYMMDD`-suffix normalization that function does.
async fn catalog_accepts_images(app: &AppHandle, model: &str) -> Option<bool> {
    let state = app.state::<AppState>();
    crate::openrouter::catalog::ensure_catalog_fresh(&state).await;
    let guard = state.openrouter_catalog.lock().unwrap();
    let catalog = guard.as_ref()?;
    crate::openrouter::catalog::match_in_catalog(model, &catalog.entries)
        .and_then(|e| e.accepts_images)
}

/// Discover this provider+model's vision support once and remember it.
///
/// Skipped entirely when the profile already carries `supports_vision` — that
/// is a deliberate override and discovery must not second-guess it.
pub async fn ensure_vision_cached(app: &AppHandle) {
    let Some((provider_id, provider_type, base_url, model, explicit)) = active_provider(app) else {
        return;
    };
    if explicit {
        return;
    }
    let key = model_key(&provider_id, &model);
    {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        if cfg.model_vision.contains_key(&key) {
            return;
        }
    }

    let Some(found) = discover(app, &provider_type, &base_url, &model).await else {
        // Not memoized as "unknown", for the same reason `context_window`
        // does not memoize a miss: the usual cause is transient (the local
        // server is down, the catalog fetch failed), and retrying next turn
        // costs one map lookup.
        tracing::debug!(provider_id, model, "no vision capability discovered");
        return;
    };

    let state = app.state::<AppState>();
    let mut cfg = state.config.lock().unwrap();
    cfg.model_vision.insert(key, found);
    if let Err(e) = crate::config::save(&cfg) {
        tracing::warn!("failed to persist discovered vision capability: {e}");
    }
    tracing::info!(provider_id, model, found, "discovered model vision support");
}

/// The verdict for one profile, read from an already-taken `Config` snapshot.
///
/// Takes the config rather than the `AppHandle` because its caller
/// (`provider_views`) is building a list and already holds a clone — a
/// per-entry `state.config.lock()` there would be both wasteful and, since
/// that function awaits between entries, a lock-across-await hazard.
pub fn vision_for_profile(
    cfg: &crate::config::Config,
    profile: &crate::config::providers::ProviderProfile,
) -> Option<bool> {
    if profile.supports_vision {
        return Some(true);
    }
    let model = profile.models.first()?;
    cfg.model_vision
        .get(&model_key(&profile.id, model))
        .copied()
}

/// First-turn confirmation, called from `send_prompt` alongside
/// `effort::confirm_model_effort` and
/// `context_window::confirm_model_context_length`.
///
/// Unlike those two there is nothing to push to the daemon — the daemon does
/// not gate on vision; Kitty's own UI does. So this only has to make sure the
/// verdict is discovered and cached before the user reaches for an attach
/// button.
pub async fn confirm_model_vision(app: &AppHandle) {
    ensure_vision_cached(app).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same NUL-separated shape as its two siblings, including the collision
    /// case a plain concatenation would get wrong.
    #[test]
    fn the_key_separates_the_same_model_on_different_providers() {
        assert_ne!(
            model_key("prof_hosted", "qwen3.6-27b"),
            model_key("prof_selfhosted", "qwen3.6-27b")
        );
        assert_ne!(model_key("a", "bc"), model_key("ab", "c"));
    }
}
