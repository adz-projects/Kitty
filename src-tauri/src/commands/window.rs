//! Window-lifecycle and backend-process commands: overlay/settings/main show,
//! stack status, and the BigTiny restart used by "Fix this" + provider switches.

use tauri::{AppHandle, Manager};

use crate::lifecycle;
use crate::state::AppState;
use crate::state::{StackStatus, StartupPhase};
use crate::windows;

/// Show/hide the overlay from the frontend.
#[tauri::command]
pub fn toggle_overlay(app: AppHandle) -> Result<(), String> {
    windows::toggle_overlay(&app).map_err(|e| e.to_string())
}

/// Hide the overlay (Escape handler in the overlay UI calls this).
#[tauri::command]
pub fn hide_overlay(app: AppHandle) -> Result<(), String> {
    windows::hide_overlay(&app).map_err(|e| e.to_string())
}

/// Open settings, optionally deep-linked to a section + highlighted element.
/// Async so window creation dispatches to the main thread (a sync command would
/// deadlock: it holds the main thread while `build()` needs it).
#[tauri::command]
pub async fn open_settings(
    app: AppHandle,
    section: Option<String>,
    highlight: Option<String>,
) -> Result<(), String> {
    windows::open_settings(&app, section, highlight).map_err(|e| e.to_string())
}

/// The `route://goto` target this hub should navigate to on mount, if the
/// call that created it also routed it somewhere.
///
/// **Consumed on read.** A hub asks once at mount; leaving the target in place
/// would send the window back to Settings on every reload (and, in dev, on
/// every hot restart) long after the user had navigated away.
#[tauri::command]
pub fn get_route_target(
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
) -> Result<Option<serde_json::Value>, String> {
    Ok(state.route_targets.lock().unwrap().remove(window.label()))
}

/// Open the full window. Async so window creation dispatches to the main thread.
#[tauri::command]
pub async fn open_main(app: AppHandle) -> Result<(), String> {
    windows::open_main(&app).map_err(|e| e.to_string())
}

/// Allocate a fresh label and open a brand-new chat window (Feature 5) —
/// always creates, never reuses an existing window, unlike `open_main`.
/// `handoff`, if given, is a session snapshot (the same shape the overlay's
/// Expand used to hand to `set_active_session`) stashed for the new window's
/// own one-time mount-time read via `get_pending_handoff` — keyed by this
/// window's specific label so opening several windows in a row can't race
/// each other over the same handoff, unlike the older global
/// `active_session` slot (still used, separately, by the provider
/// context-handoff gate in Settings -> Providers — not touched here).
#[tauri::command]
pub async fn open_new_chat_window(
    app: AppHandle,
    handoff: Option<serde_json::Value>,
) -> Result<(), String> {
    windows::open_new_chat_window(&app, handoff).map_err(|e| e.to_string())
}

/// One-shot read of this specific window's pending Expand handoff, if any —
/// the multi-window analog of `get_active_session`, targeted by the calling
/// window's own label (via Tauri's `Window` extractor) instead of a single
/// global slot. Removes the entry once read, so a later mount of the same
/// label (there isn't one today, since labels aren't reused, but this keeps
/// the contract "consumed exactly once" honest regardless) never re-adopts it.
#[tauri::command]
pub fn get_pending_handoff(
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
) -> Result<Option<serde_json::Value>, String> {
    Ok(state
        .pending_handoffs
        .lock()
        .unwrap()
        .remove(window.label()))
}

/// Called once by every window's frontend right after mounting. Lets the
/// dev-only load watchdog (`windows::spawn_load_watchdog`) tell a window that
/// is still loading apart from one whose first navigation failed and will
/// never load on its own — see `state::AppState::booted_windows`.
#[tauri::command]
pub fn window_ready(window: tauri::Window, state: tauri::State<'_, AppState>) {
    state
        .booted_windows
        .lock()
        .unwrap()
        .insert(window.label().to_string());
}

/// Current stack status (frontend also listens to `stack://status`).
#[tauri::command]
pub fn get_stack_status(state: tauri::State<'_, AppState>) -> Result<StackStatus, String> {
    Ok(*state.stack_status.lock().unwrap())
}

/// Whether a load-time engine setting is waiting on a daemon restart
/// (frontend also listens to `engine://restart-state`). Lets a settings
/// window that opened after the change primed its own chip.
#[tauri::command]
pub fn get_engine_restart_state(
    app: tauri::AppHandle,
) -> Result<crate::lifecycle::engine_restart::EngineRestartState, String> {
    Ok(crate::lifecycle::engine_restart::current(&app))
}

/// One-time startup progress (frontend also listens to `stack://startup-phase`).
/// Lets a window that attaches after `start_stack` began (e.g. a slow overlay
/// mount) prime its initial phase instead of assuming `SpawningGoosed`.
#[tauri::command]
pub fn get_startup_phase(state: tauri::State<'_, AppState>) -> Result<StartupPhase, String> {
    Ok(*state.startup_phase.lock().unwrap())
}

/// Restart the BigTiny daemon (kills our owned process, respawns, re-syncs
/// the active provider registration). "Fix this" and the degraded-state
/// panel call this; `activate_provider` used to call it after switching, but
/// BigTiny switches providers live over REST instead.
#[tauri::command]
pub async fn restart_backend(app: AppHandle) -> Result<(), String> {
    // Take the old process out of the daemon handle so we can drop the
    // `bigtiny` Mutex before killing — `kill_if_owned` blocks on
    // `child.wait()`, and holding the lock across that would stall every
    // other BigTiny client call. Run the wait off the async worker too.
    let old_proc = {
        let state = app.state::<AppState>();
        let mut handle = state.bigtiny.lock().unwrap();
        std::mem::take(&mut handle.process)
    };
    tokio::task::spawn_blocking(move || {
        let mut proc = old_proc;
        proc.kill_if_owned();
    })
    .await
    .map_err(|e| format!("backend kill task panicked: {e}"))?;
    let (
        command,
        args,
        dir,
        summarizer,
        token_management,
        memory,
        local,
        pathway_enabled,
        pathway_embedding_model,
    ) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        (
            cfg.bigtiny_command.clone(),
            cfg.bigtiny_args.clone(),
            cfg.bigtiny_dir.clone(),
            cfg.summarizer.clone(),
            cfg.token_management.clone(),
            cfg.memory.clone(),
            cfg.local.clone(),
            cfg.adaptive_pathway_enabled,
            cfg.adaptive_pathway_embedding_model.clone(),
        )
    };
    let handle = lifecycle::bigtiny_proc::spawn(
        &command,
        &args,
        dir.as_deref(),
        &summarizer,
        &token_management,
        &memory,
        &local,
        pathway_enabled,
        &pathway_embedding_model,
    )
    .await?;
    let (healthy, port) = (handle.healthy, handle.port);
    {
        let state = app.state::<AppState>();
        *state.bigtiny.lock().unwrap() = handle;
    }
    if let Err(e) = crate::bigtiny::providers::sync_active_provider(&app).await {
        tracing::warn!("bigtiny provider sync after restart failed: {e}");
    }
    // See `lifecycle::sync_mcp_once_healthy`: don't give up on the MCP sync
    // after one failed call if the daemon is just slow to finish binding.
    if let Some(port) = port {
        lifecycle::sync_mcp_once_healthy(&app, healthy, port);
    }
    Ok(())
}
