//! Discovering, remembering, and confirming each model's context window.
//!
//! The daemon's wrap-up valve (`agent/loop_.rs`) withdraws tools when a turn is
//! close to the model's context limit, and it reads that limit from the
//! provider row Kitty pushes — falling back to the daemon-wide
//! `max_context_tokens` (64k) when the row has none. On a 200k model with the
//! field unset that withdraws tools at roughly 49k used while 150k of window
//! sits unused, so the whole feature is only as good as this number.
//!
//! Kitty could already *suggest* the value: `commands::provider` has a live
//! lookup per provider type, wired to the Providers form's auto-suggest. But it
//! was only ever a suggestion — a profile whose owner never opened that form
//! kept `context_length: None` forever. This module makes discovery automatic.
//!
//! **It deliberately mirrors `bigtiny::effort`'s design**, because it is the
//! same problem: a per-model property that has to be discovered once, survive
//! across sessions, and be reconciled with what the daemon believes at a
//! predictable moment. Same key shape, same first-turn confirmation, same
//! write-only-when-changed rule, and it rides the same
//! `AppState::effort_confirmed_sessions` gate so a turn pays for at most one
//! confirmation pass rather than two.
//!
//! Resolution order, highest first:
//!   1. An explicit `context_length` on the profile — the user typed it, so it
//!      wins and is never overwritten by discovery.
//!   2. The remembered value for this exact provider+model.
//!   3. Live discovery (below).
//!   4. Nothing — the daemon keeps its own default.

use tauri::{AppHandle, Manager};

use crate::state::AppState;

/// Cache/memory key: provider **and** model, matching
/// `effort::effort_cache_key`. The same model id served by two endpoints can
/// genuinely have different windows — a self-hosted llama-server is built with
/// whatever `n_ctx` its operator chose, which has nothing to do with what the
/// hosted version of that model offers.
fn model_key(provider_id: &str, model: &str) -> String {
    format!("{provider_id}\u{0}{model}")
}

/// A plausible context window. Anything outside this is a misparse, not a
/// model: below 1k no chat is possible, and above 10M is past any shipping
/// model by an order of magnitude. Rejecting here keeps a garbage value from
/// becoming a reserve that either never fires or fires on every turn.
fn plausible(n: u32) -> Option<u32> {
    (1_024..=10_000_000).contains(&n).then_some(n)
}

/// The active provider's `(id, provider_type, base_url, model, explicit)`, or
/// `None` when there is no usable active provider. Split out so the config lock
/// is never held across an `await`.
fn active_provider(app: &AppHandle) -> Option<(String, String, String, String, Option<u32>)> {
    let state = app.state::<AppState>();
    let cfg = state.config.lock().unwrap();
    let active_id = cfg.active_provider_id.as_deref()?;
    let p = cfg.providers.iter().find(|p| p.id == active_id)?;
    Some((
        p.id.clone(),
        p.provider_type.clone(),
        p.base_url.clone(),
        p.models.first().cloned().unwrap_or_default(),
        p.context_length,
    ))
}

/// Live lookup for one provider+model. Best-effort throughout: every source
/// answers `None` rather than erroring, exactly like the auto-suggest commands
/// this reuses, because an undiscoverable window is not a failure — it just
/// leaves the daemon on its default.
async fn discover(
    app: &AppHandle,
    provider_type: &str,
    base_url: &str,
    model: &str,
) -> Option<u32> {
    if model.trim().is_empty() {
        return None;
    }
    let found = match provider_type {
        // The endpoint knows its own build. `n_ctx` from a llama-server is the
        // authoritative answer for that server, and no catalog can substitute
        // for it.
        "ollama" => crate::commands::ollama_context_length(base_url.to_string(), model.to_string())
            .await
            .ok()
            .flatten(),
        "custom_openai" | "local" => {
            crate::commands::custom_openai_context_length(base_url.to_string(), model.to_string())
                .await
                .ok()
                .flatten()
        }
        // Hosted providers: the OpenRouter catalog is already the app's
        // universal capability source for every provider type, and it is
        // already cached in `AppState`.
        _ => catalog_context_length(app, model).await,
    };
    found.and_then(plausible)
}

/// The catalog lookup, via `match_in_catalog` rather than
/// `openrouter::context_length_for`.
///
/// That distinction matters for direct Anthropic/OpenAI profiles:
/// `context_length_for` compares ids for exact equality, so a profile on
/// `claude-sonnet-4-20250514` would never match the catalog's
/// `anthropic/claude-sonnet-4` and every such provider would silently fall
/// through to the daemon default. `match_in_catalog` normalizes first — drops
/// the vendor prefix, lowercases, strips the `-YYYYMMDD` snapshot suffix — so
/// the dated id resolves.
async fn catalog_context_length(app: &AppHandle, model: &str) -> Option<u32> {
    let state = app.state::<AppState>();
    crate::openrouter::catalog::ensure_catalog_fresh(&state).await;
    let guard = state.openrouter_catalog.lock().unwrap();
    let catalog = guard.as_ref()?;
    crate::openrouter::catalog::match_in_catalog(model, &catalog.entries)
        .and_then(|e| e.context_length)
}

/// Discover this provider+model's window once and remember it, if it isn't
/// already known. Cheap on every call after the first: a hit is a map lookup,
/// and the OpenRouter path is served from the in-memory catalog.
///
/// Does nothing when the profile carries an explicit `context_length` — that is
/// a deliberate override and discovery must never stomp it.
pub async fn ensure_context_length_cached(app: &AppHandle) {
    let Some((provider_id, provider_type, base_url, model, explicit)) = active_provider(app) else {
        return;
    };
    if explicit.is_some() {
        return;
    }
    let key = model_key(&provider_id, &model);
    {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        if cfg.model_context_lengths.contains_key(&key) {
            return;
        }
    }

    let Some(found) = discover(app, &provider_type, &base_url, &model).await else {
        // Deliberately not memoized as "none": unlike an effort-level probe,
        // an undiscoverable window is often transient (the local server is
        // down, the catalog fetch failed), and re-trying next turn costs one
        // cached lookup.
        tracing::debug!(provider_id, model, "no context length discovered");
        return;
    };

    let state = app.state::<AppState>();
    let mut cfg = state.config.lock().unwrap();
    cfg.model_context_lengths.insert(key, found);
    if let Err(e) = crate::config::save(&cfg) {
        tracing::warn!("failed to persist discovered context length: {e}");
    }
    tracing::info!(provider_id, model, found, "discovered model context length");
}

/// The window Kitty believes this provider+model has, applying the resolution
/// order in this module's header.
pub fn resolved_context_length(app: &AppHandle) -> Option<u32> {
    let (provider_id, _, _, model, explicit) = active_provider(app)?;
    if explicit.is_some() {
        return explicit;
    }
    let state = app.state::<AppState>();
    let cfg = state.config.lock().unwrap();
    cfg.model_context_lengths
        .get(&model_key(&provider_id, &model))
        .copied()
}

/// First-turn confirmation, the counterpart to
/// `effort::confirm_model_effort` and called from the same place.
///
/// Discovers the window if it isn't known yet, then makes sure the daemon's
/// provider row actually carries it — because the row is what the wrap-up valve
/// reads, and a value that only exists in Kitty's config protects nothing.
/// Re-registers only when the daemon's value differs, so an unchanged session
/// touches neither the config file nor the daemon.
pub async fn confirm_model_context_length(app: &AppHandle) {
    ensure_context_length_cached(app).await;
    let Some(resolved) = resolved_context_length(app) else {
        return;
    };

    // `providers::sync_active_provider` relays the profile's own
    // `context_length` field, so the remembered value has to be written onto
    // the profile before the daemon can see it. Only when it differs.
    let needs_push = {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        let Some(active_id) = cfg.active_provider_id.clone() else {
            return;
        };
        match cfg.providers.iter_mut().find(|p| p.id == active_id) {
            Some(p) if p.context_length != Some(resolved) => {
                p.context_length = Some(resolved);
                if let Err(e) = crate::config::save(&cfg) {
                    tracing::warn!("failed to persist confirmed context length: {e}");
                }
                true
            }
            _ => false,
        }
    };

    if needs_push {
        if let Err(e) = crate::bigtiny::providers::sync_active_provider(app).await {
            tracing::warn!("failed to push confirmed context length to the daemon: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_separates_the_same_model_on_different_providers() {
        // A self-hosted llama-server is built with whatever `n_ctx` its
        // operator chose, which is unrelated to the hosted version's window —
        // so these must never share a remembered value.
        assert_ne!(
            model_key("prov-a", "qwen3-30b"),
            model_key("prov-b", "qwen3-30b")
        );
        assert_eq!(
            model_key("prov-a", "qwen3-30b"),
            model_key("prov-a", "qwen3-30b")
        );
        // NUL-separated, so a provider id ending in the model's prefix can't
        // collide with a shorter one.
        assert_ne!(model_key("a", "bc"), model_key("ab", "c"));
    }

    #[test]
    fn implausible_windows_are_rejected_rather_than_remembered() {
        // A misparse becomes a reserve that either never fires or fires on
        // every turn, so it must not be stored at all.
        assert_eq!(plausible(0), None);
        assert_eq!(plausible(1), None);
        assert_eq!(plausible(1_023), None);
        assert_eq!(plausible(20_000_000), None);
        // Real windows, from the small end to the large.
        assert_eq!(plausible(1_024), Some(1_024));
        assert_eq!(plausible(8_192), Some(8_192));
        assert_eq!(plausible(128_000), Some(128_000));
        assert_eq!(plausible(1_000_000), Some(1_000_000));
    }
}
