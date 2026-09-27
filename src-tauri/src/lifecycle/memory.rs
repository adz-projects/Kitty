//! Whether the two memory engines run, applied live.
//!
//! Adaptive Pathway (behavioural memory) and Memorabilia (factual memory) are
//! both optional, and both need the EmbeddingGemma model to be worth running:
//! without it the daemon falls back to lexical hashing, which recalls poorly
//! enough that Kitty treats memory as off instead. So each engine's effective
//! state is
//!
//! > the user's toggle **and** the running engine has semantic embeddings
//!
//! (and Memorabilia never on Android). It is applied through the daemon's
//! per-app switch, `PUT /api/apps/me/plugins/{plugin}`, which takes effect at
//! once - the engine closes or opens and its tools come and go with it - so a
//! toggle no longer restarts anything.
//!
//! The embedding model itself is loaded when the daemon starts, so a model
//! downloaded (or deleted) since then only takes effect after a restart. When
//! the model on disk and the running engine disagree, and memory is wanted,
//! this schedules one (`engine_restart`), and the engines follow once the
//! restarted daemon is attached (`lifecycle::install_handle` calls
//! [`apply_memory_plugins`]).
//!
//! Called after every attach, after either toggle, and after a model download
//! or delete.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

/// The daemon's plugin ids.
const PATHWAY: &str = "pathway";
const MEMORABILIA: &str = "memorabilia";

/// Where memory stands, for Settings (`get_memory_status`) and the
/// `memory://status` event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MemoryStatus {
    /// The embedding model and its tokenizer are on disk.
    pub model_installed: bool,
    /// The running engine loaded it. `false` with `model_installed` means a
    /// restart is still to come.
    pub engine_has_embeddings: bool,
    /// What the engines are actually doing now.
    pub pathway_active: bool,
    pub memorabilia_active: bool,
    /// Whether Memorabilia exists on this platform at all.
    pub memorabilia_supported: bool,
}

/// What the user wants and what the engine can do, reduced to what each
/// engine should be. Pure, so the policy is testable.
fn effective(
    pathway_wanted: bool,
    memorabilia_wanted: bool,
    engine_has_embeddings: bool,
    memorabilia_supported: bool,
) -> (bool, bool) {
    (
        pathway_wanted && engine_has_embeddings,
        memorabilia_wanted && engine_has_embeddings && memorabilia_supported,
    )
}

/// Whether the engine needs a restart to match the disk: memory is wanted
/// and the model has appeared or gone since the daemon started.
fn restart_needed(any_wanted: bool, model_installed: bool, engine_has_embeddings: bool) -> bool {
    any_wanted && model_installed != engine_has_embeddings
}

/// Bring both engines in line with the toggles and the model. Best-effort:
/// failures are logged and leave the engines as they were.
pub async fn apply_memory_plugins(app: &AppHandle) {
    match apply(app).await {
        Ok(status) => {
            let changed = {
                let state = app.state::<AppState>();
                let mut cur = state.memory_status.lock().unwrap();
                let changed = *cur != status;
                *cur = status.clone();
                changed
            };
            if changed {
                let _ = app.emit("memory://status", status);
                // The memory tool rows mirror this state; bring them along.
                crate::bigtiny::mcp::self_heal_builtin_servers(app).await;
            }
        }
        Err(e) => tracing::warn!("could not apply the memory settings: {e}"),
    }
}

async fn apply(app: &AppHandle) -> Result<MemoryStatus, String> {
    let client = crate::bigtiny::client::ensure_client(app)?;
    let (pathway_wanted, memorabilia_wanted, model) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        (
            cfg.adaptive_pathway_enabled,
            cfg.memorabilia_enabled,
            cfg.adaptive_pathway_embedding_model.clone(),
        )
    };
    let memorabilia_supported = !cfg!(target_os = "android");
    // The embedder needs both the model and the tokenizer beside it.
    let model_installed =
        crate::models::resolve(&model).is_some() && crate::models::tokenizer_path().is_some();

    let listing = client.get_json("/api/apps/me/plugins").await?;
    let engine_has_embeddings = listing
        .get("semantic_embeddings")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let current = |name: &str| {
        listing
            .get("plugins")
            .and_then(|p| p.as_array())
            .and_then(|ps| ps.iter().find(|p| p["plugin"] == name))
            .map(|p| (p["enabled"].as_bool(), p["explicit"].as_bool()))
    };

    let (pathway_on, memorabilia_on) = effective(
        pathway_wanted,
        memorabilia_wanted,
        engine_has_embeddings,
        memorabilia_supported,
    );
    for (plugin, on) in [(PATHWAY, pathway_on), (MEMORABILIA, memorabilia_on)] {
        // Always explicit, never the daemon default: the default belongs to
        // whichever app started the daemon.
        if current(plugin) != Some((Some(on), Some(true))) {
            client
                .request(
                    reqwest::Method::PUT,
                    &format!("/api/apps/me/plugins/{plugin}"),
                )
                .json(&serde_json::json!({ "enabled": on }))
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| format!("setting {plugin}: {e}"))?;
        }
    }

    if restart_needed(
        pathway_wanted || (memorabilia_wanted && memorabilia_supported),
        model_installed,
        engine_has_embeddings,
    ) {
        // Once per state of the disk. If the restarted engine still disagrees
        // (the model is there but would not load), restarting again would
        // loop forever; the status says what is wrong instead.
        let already = {
            let state = app.state::<AppState>();
            let mut last = state.memory_restart_for.lock().unwrap();
            let already = *last == Some(model_installed);
            *last = Some(model_installed);
            already
        };
        if already {
            tracing::warn!(
                model_installed,
                engine_has_embeddings,
                "the engine still does not match the embedding model after a restart; not \
                 restarting again"
            );
        } else {
            tracing::info!(
                model_installed,
                engine_has_embeddings,
                "the embedding model changed since the engine started; scheduling a restart"
            );
            super::engine_restart::schedule(app);
        }
    } else {
        *app.state::<AppState>().memory_restart_for.lock().unwrap() = None;
    }

    Ok(MemoryStatus {
        model_installed,
        engine_has_embeddings,
        pathway_active: pathway_on,
        memorabilia_active: memorabilia_on,
        memorabilia_supported,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decision #1: without semantic embeddings neither engine runs, whatever
    /// the toggles say.
    #[test]
    fn no_embeddings_means_no_memory() {
        assert_eq!(effective(true, true, false, true), (false, false));
        assert_eq!(effective(true, true, true, true), (true, true));
        assert_eq!(effective(false, true, true, true), (false, true));
    }

    #[test]
    fn memorabilia_stays_off_where_it_is_unsupported() {
        assert_eq!(effective(true, true, true, false), (true, false));
    }

    /// A restart is scheduled only when memory is wanted and the disk and the
    /// running engine disagree.
    #[test]
    fn a_restart_follows_a_model_change_only_when_memory_is_wanted() {
        assert!(restart_needed(true, true, false), "a model arrived");
        assert!(restart_needed(true, false, true), "the model went");
        assert!(!restart_needed(true, true, true));
        assert!(!restart_needed(false, true, false), "nobody wants memory");
    }
}
