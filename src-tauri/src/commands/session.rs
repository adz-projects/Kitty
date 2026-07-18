//! Session lifecycle commands: create/send/cancel/approve/resume/fork/delete,
//! plus the mode-override and private-chat-folder helpers that only exist to
//! support them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::config;
use crate::config::providers;
use crate::goosed::api;
use crate::notifications;
use crate::state::AppState;

/// Store the active session (raw JSON) so the full window can adopt it on Expand.
/// Emits `session://active` so an *already-open* main window re-adopts the newly
/// handed-off session (its mount-time `getActiveSession` only runs once).
#[tauri::command]
pub fn set_active_session(app: AppHandle, info: Value) -> Result<(), String> {
    *app.state::<AppState>().active_session.lock().unwrap() = Some(info.clone());
    let _ = app.emit("session://active", info);
    Ok(())
}

/// Read the active session, if any (the full window calls this on mount).
#[tauri::command]
pub fn get_active_session(state: tauri::State<'_, AppState>) -> Result<Option<Value>, String> {
    Ok(state.active_session.lock().unwrap().clone())
}

/// Details returned when a session is created, for the chat UI.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: String,
    pub current_mode: String,
    pub available_modes: Vec<ModeInfo>,
    /// `None` when the active model doesn't support effort control at all —
    /// see `parse_thinking_effort`'s doc comment (Round-7 Feature).
    pub thinking_effort: Option<ThinkingEffort>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffortOption {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingEffort {
    pub current_value: String,
    pub options: Vec<EffortOption>,
}

/// Both `session/new` and `session/load`'s raw ACP result carry a top-level
/// `configOptions: [...]` array (live-probed, `docs/acp-protocol.md`) with
/// entries for `provider`/`mode`/`model`/`thinking_effort` — Kitty only needs
/// the last one here (the others are already managed via their own
/// mechanisms: `ProviderBadge`, `ModeToggle`/`ModeBadge`, the Providers model
/// picker). goose's own option set can include `off`/`max` alongside
/// `low`/`medium`/`high` (owner decision: only the three standard levels are
/// worth exposing in the UI — `off` and `max` are dropped here, not just
/// hidden client-side, so there's one source of truth for what's selectable).
/// `thinking_effort.options` is otherwise model-dependent: a model with no
/// extended-thinking support offers just `[{name:"off",value:"off"}]` — after
/// dropping `off`, that's zero low/medium/high options — so treat fewer than
/// 2 surviving options as "no effort control for this model" and return
/// `None` rather than a useless dropdown.
fn parse_thinking_effort(result: &Value) -> Option<ThinkingEffort> {
    const RANK: [&str; 3] = ["low", "medium", "high"];
    let entry = result
        .get("configOptions")?
        .as_array()?
        .iter()
        .find(|c| c.get("id").and_then(|v| v.as_str()) == Some("thinking_effort"))?;
    let current_value = entry.get("currentValue")?.as_str()?.to_string();
    let mut options: Vec<EffortOption> = entry
        .get("options")?
        .as_array()?
        .iter()
        .filter_map(|o| serde_json::from_value::<EffortOption>(o.clone()).ok())
        .filter(|o| RANK.contains(&o.value.to_lowercase().as_str()))
        .collect();
    options.sort_by_key(|o| RANK.iter().position(|r| *r == o.value.to_lowercase()));
    if options.len() < 2 {
        return None;
    }
    // The model's actual current value may be a dropped choice (e.g. `off`,
    // its natural resting default) — fall back to the first surviving option
    // for display purposes only; nothing is sent to goosed until the user
    // actually picks one via the dropdown.
    let current_value = if options.iter().any(|o| o.value == current_value) {
        current_value
    } else {
        options[0].value.clone()
    };
    Some(ThinkingEffort {
        current_value,
        options,
    })
}

/// Prefix every no-explicit-folder session's private chat folder lives under
/// (Round-3 item 25). `delete_session` only ever removes a directory under
/// this prefix — never a user-chosen custom working directory.
pub const CHATS_DIR_NAME: &str = "chats";

/// Base directory that holds every chat's own context folder. The user's choice
/// (`default_context_folder`, set in Settings) when non-empty, else the default
/// `~/Documents/Kitty`. Each chat then lives in `<base>/chats/<id>/`, so the
/// setting is a *base for per-chat folders*, not one shared working directory.
fn chats_base_dir(app: &AppHandle) -> PathBuf {
    let configured = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.default_context_folder.clone()
    };
    configured
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::document_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Kitty")
        })
}

/// A fresh per-chat folder `<base>/chats/<timestamp>-<short-rand>/`. The
/// `<timestamp>-<rand>` is the chat's own id — goose's session id isn't known
/// until `session/new` returns (the cwd is passed *into* it), so a client-side
/// id names the folder instead; `session/list.cwd` maps back to it later.
fn new_chat_folder(base: &Path) -> PathBuf {
    use rand::Rng;
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let suffix: String = {
        let mut rng = rand::thread_rng();
        (0..6)
            .map(|_| format!("{:x}", rng.gen_range(0u8..16)))
            .collect()
    };
    base.join(CHATS_DIR_NAME).join(format!("{ts}-{suffix}"))
}

/// The working directory a new session starts in: a fresh per-chat folder under
/// the (configurable) chats base, created if missing. Same for both modes.
/// `create_dir_all` runs on a blocking thread — this is user-triggered
/// (every "New Session"), so a slow disk shouldn't stall the tokio worker
/// other requests are running on.
async fn resolve_cwd(app: &AppHandle) -> String {
    let path = new_chat_folder(&chats_base_dir(app));
    let path_for_blocking = path.clone();
    let _ = tokio::task::spawn_blocking(move || std::fs::create_dir_all(&path_for_blocking)).await;
    path.to_string_lossy().replace('\\', "/")
}

/// Start a new goosed session (ACP `session/new`). Connects the ACP client on
/// first use. An explicit `cwd` (e.g. a dropped folder) overrides the default.
#[tauri::command]
pub async fn new_session(app: AppHandle, cwd: Option<String>) -> Result<SessionInfo, String> {
    let client = api::ensure_client(&app).await?;
    let cwd = match cwd {
        Some(c) if !c.trim().is_empty() => {
            let c_for_blocking = c.clone();
            let _ =
                tokio::task::spawn_blocking(move || std::fs::create_dir_all(&c_for_blocking)).await;
            c.replace('\\', "/")
        }
        _ => resolve_cwd(&app).await,
    };
    let result = client
        .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
        .await?;

    let session_id = result
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or("goosed did not return a session id")?
        .to_string();

    // Web search + artifacts should work in EVERY session regardless of the
    // provider's chat-only flag (Round-2 item 14a). The `computercontroller`
    // builtin is keyless and provides web search/fetch; best-effort + idempotent.
    // (Dedicated Brave search additionally needs the mcp-brave-search extension
    // and a BRAVE_API_KEY — see docs/acp-protocol.md.) Spawned rather than
    // awaited (Round-7): its result was already discarded, so awaiting it here
    // only added a second full ACP round trip to the critical path the
    // frontend blocks on before `new_session` visibly manifests.
    let seed_client = client.clone();
    let seed_session_id = session_id.clone();
    tauri::async_runtime::spawn(async move {
        let _ = seed_client
            .request(
                "_goose/unstable/session/extensions/add",
                json!({
                    "sessionId": &seed_session_id,
                    "extension": { "type": "builtin", "name": "computercontroller" }
                }),
            )
            .await;
    });

    let current_mode = result
        .pointer("/modes/currentModeId")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let available_modes = result
        .pointer("/modes/availableModes")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(parse_mode).collect())
        .unwrap_or_default();

    // Cross-window live-update (Round-4 item 6): overlay and main each own an
    // independent zustand store, so nothing else tells the other window's
    // session list/recents dropdown a new session now exists.
    let _ = app.emit("session://created", json!({ "sessionId": session_id }));

    let thinking_effort = parse_thinking_effort(&result);

    Ok(SessionInfo {
        session_id,
        cwd,
        current_mode,
        available_modes,
        thinking_effort,
    })
}

/// Add one recipe-declared extension to a live session, best-effort — mirrors
/// `new_session`'s own `computercontroller` add above. Real Goose recipe
/// extension types (`stdio`/`builtin`/`platform`/`streamable_http`/`frontend`/
/// `inline_python`) don't line up with what ACP's `extensions/add` accepts
/// (`builtin`/`platform`/`mcp`, confirmed in `docs/acp-protocol.md`), so
/// `stdio` maps to the ACP `mcp` shape (env resolved to literal `KEY=VALUE`
/// strings from Kitty's own process env — never goosed's — matching the
/// confirmed `server.env` bare-string-array shape) and `builtin`/`platform`
/// pass straight through. The remaining three have no ACP equivalent at all —
/// silently skipped, never a hard failure, since an extension type ACP can't
/// represent must not break a recipe invocation.
#[tauri::command]
pub async fn add_recipe_extension(
    app: AppHandle,
    session_id: String,
    extension: crate::config::recipes::RecipeExtension,
) -> Result<(), String> {
    let payload = match extension.ext_type.as_str() {
        "builtin" => json!({ "type": "builtin", "name": extension.name }),
        "platform" => json!({ "type": "platform", "name": extension.name }),
        "stdio" => {
            let env: Vec<String> = extension
                .env_keys
                .iter()
                .filter_map(|k| std::env::var(k).ok().map(|v| format!("{k}={v}")))
                .collect();
            json!({
                "type": "mcp",
                "server": {
                    "name": extension.name,
                    "command": extension.cmd.clone().unwrap_or_default(),
                    "args": extension.args,
                    "env": env,
                },
            })
        }
        _ => return Ok(()),
    };
    let client = api::ensure_client(&app).await?;
    let _ = client
        .request(
            "_goose/unstable/session/extensions/add",
            json!({ "sessionId": session_id, "extension": payload }),
        )
        .await;
    Ok(())
}

fn parse_mode(v: &Value) -> ModeInfo {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    ModeInfo {
        id: s("id"),
        name: s("name"),
        description: s("description"),
    }
}

/// An image attached to a chat turn (Round-3 item 17). `data_url` is a
/// `data:<mime>;base64,<...>` string as produced by `read_file_any`.
#[derive(Debug, Clone, Deserialize)]
pub struct ImageAttachment {
    pub mime: String,
    pub data_url: String,
}

/// Send a user turn (ACP `session/prompt`). Returns immediately; streamed
/// output arrives via `chat://*` events, and completion via `chat://complete`.
/// `images`, when present, are appended as native ACP image content blocks
/// (`{type:"image", data, mimeType}`, confirmed live — see acp-protocol.md)
/// instead of relying on a filesystem tool to open a path — this is what fixes
/// the "file not found" failure untrusted/remote providers hit on a bare path
/// reference (Round-3 item 17).
#[tauri::command]
pub async fn send_prompt(
    app: AppHandle,
    session_id: String,
    text: String,
    images: Option<Vec<ImageAttachment>>,
) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    // Per-provider override for how long `session/prompt` tolerates silence
    // before giving up (Settings → Providers → Advanced) — falls back to the
    // shared default. Resolved once up front since it's a plain config read.
    let idle_secs = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.active_provider_id
            .as_ref()
            .and_then(|id| cfg.providers.iter().find(|p| &p.id == id))
            .and_then(|p| p.prompt_idle_timeout_secs)
            .map(u64::from)
            .unwrap_or(api::DEFAULT_PROMPT_IDLE_SECS)
    };
    let app_bg = app.clone();
    let sid = session_id.clone();
    let mut prompt = vec![json!({ "type": "text", "text": text })];
    for img in images.unwrap_or_default() {
        // Strip a "data:<mime>;base64," prefix if present; ACP wants raw base64.
        let data = img
            .data_url
            .split_once(",")
            .map(|(_, b64)| b64)
            .unwrap_or(&img.data_url);
        prompt.push(json!({ "type": "image", "data": data, "mimeType": img.mime }));
    }
    app.state::<AppState>()
        .in_flight_sessions
        .lock()
        .unwrap()
        .insert(sid.clone());
    tauri::async_runtime::spawn(async move {
        let params = json!({ "sessionId": sid, "prompt": prompt });
        let mut res = client
            .request_session_prompt(&sid, params.clone(), idle_secs)
            .await;

        // A silent, single retry — specifically for goosed's generic
        // "Internal error" (the JSON-RPC catch-all code, confirmed via a real
        // report: a correctly-configured custom-OpenAI provider reached over
        // Tailscale "works most of the time" but fails intermittently with
        // exactly this). This is goosed *responding* (not a dead connection —
        // that surfaces as "ACP connection closed"/"ACP request cancelled",
        // different messages, not retried here), so the local ACP link is
        // fine; the failure is goosed's own upstream call to the remote
        // provider hitting a transient hiccup. Resending the identical prompt
        // once gives that upstream call a second chance before making the
        // user manually resend or restart goosed for what's often just one
        // bad round trip.
        if let Err(message) = &res {
            if message.eq_ignore_ascii_case("internal error") {
                res = client.request_session_prompt(&sid, params, idle_secs).await;
            }
        }

        match res {
            Ok(result) => {
                let _ = app_bg.emit(
                    "chat://complete",
                    json!({ "session_id": sid, "result": result }),
                );
                notifications::notify_if_hidden(
                    &app_bg,
                    notifications::Event::TaskComplete,
                    "Kitty finished",
                    "Your task is complete.",
                );
                providers::emit_health_from_send_result(&app_bg, true);
            }
            Err(message) => {
                // A failed round trip is a signal the shared ACP connection
                // may no longer be good (e.g. a plain "Invalid params" right
                // after a provider switch, or a genuine timeout) — drop
                // Kitty's client reference so the next attempt reconnects.
                // No goosed restart here: the previous idle-reset timeout had
                // a real bug (a stale activity timestamp from the *previous*
                // turn could make a fresh send time out instantly, regardless
                // of connection health — now fixed at the source in
                // `request_session_prompt`), so a genuine timeout reaching
                // here should be rare and doesn't warrant disrupting every
                // other session sharing this goosed process.
                *app_bg.state::<AppState>().acp.lock().await = None;
                let _ = app_bg.emit(
                    "chat://error",
                    json!({ "session_id": sid, "message": &message }),
                );
                notifications::notify_if_hidden(
                    &app_bg,
                    notifications::Event::TaskFailed,
                    "Kitty ran into a problem",
                    &message,
                );
                providers::emit_health_from_send_result(&app_bg, false);
            }
        }
        // A finished turn clears any pending-approval tray state.
        notifications::set_tray_pending(&app_bg, false);
        app_bg
            .state::<AppState>()
            .in_flight_sessions
            .lock()
            .unwrap()
            .remove(&sid);
    });
    Ok(())
}

/// Cancel the in-flight turn for a session (ACP `session/cancel` notification).
/// goosed resolves the pending prompt with a `cancelled` stop reason.
#[tauri::command]
pub async fn cancel_prompt(app: AppHandle, session_id: String) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    client
        .notify("session/cancel", json!({ "sessionId": session_id }))
        .await;
    Ok(())
}

/// Whether `session_id` currently has a `session/prompt` in flight — checked
/// fresh (not a client-cached snapshot) so a window adopting the session
/// (Expand mid-stream, or just resuming one another window/process is
/// actively driving) can correctly show "still working" instead of looking
/// stalled just because `session/load`'s replay doesn't reliably convey an
/// in-progress turn.
#[tauri::command]
pub fn is_session_busy(state: tauri::State<'_, AppState>, session_id: String) -> bool {
    state
        .in_flight_sessions
        .lock()
        .unwrap()
        .contains(&session_id)
}

/// Respond to a deferred tool-approval prompt. `option_id` = the chosen ACP
/// option (e.g. `allow_once`, `reject_once`); `None` cancels.
#[tauri::command]
pub async fn respond_permission(
    app: AppHandle,
    tool_call_id: String,
    option_id: Option<String>,
) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    let id = client
        .take_permission(&tool_call_id)
        .await
        .ok_or("that approval request is no longer pending")?;

    let outcome = match option_id {
        Some(opt) => json!({ "outcome": { "outcome": "selected", "optionId": opt } }),
        None => json!({ "outcome": { "outcome": "cancelled" } }),
    };
    client.respond(id, outcome).await;
    notifications::set_tray_pending(&app, false);
    Ok(())
}

/// Switch the session's approval mode (`auto` / `approve` / `smart_approve`).
#[tauri::command]
pub async fn set_mode(app: AppHandle, session_id: String, mode_id: String) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    client
        .request(
            "session/set_mode",
            json!({ "sessionId": session_id, "modeId": mode_id }),
        )
        .await?;
    Ok(())
}

/// List past sessions (raw ACP session objects; the frontend parses them).
#[tauri::command]
pub async fn list_sessions(app: AppHandle) -> Result<Vec<Value>, String> {
    let client = api::ensure_client(&app).await?;
    let result = client.request("session/list", json!({})).await?;
    Ok(result
        .get("sessions")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default())
}

/// Resume a session (ACP `session/load`). The conversation replays as
/// `chat://*` events during the call; returns the session's mode info.
#[tauri::command]
pub async fn load_session(
    app: AppHandle,
    session_id: String,
    cwd: String,
) -> Result<SessionInfo, String> {
    let client = api::ensure_client(&app).await?;
    let result = client
        .request(
            "session/load",
            json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] }),
        )
        .await?;

    let current_mode = result
        .pointer("/modes/currentModeId")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let available_modes = result
        .pointer("/modes/availableModes")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(parse_mode).collect())
        .unwrap_or_default();

    let thinking_effort = parse_thinking_effort(&result);

    Ok(SessionInfo {
        session_id,
        cwd,
        current_mode,
        available_modes,
        thinking_effort,
    })
}

/// Fork a session (ACP `session/fork`), optionally truncating the copy to a
/// branch point. Powers "Branch from here" and "Regenerate" (Phase 9).
#[tauri::command]
pub async fn fork_session(
    app: AppHandle,
    session_id: String,
    cwd: String,
    truncate_from: Option<i64>,
) -> Result<SessionInfo, String> {
    let client = api::ensure_client(&app).await?;
    let result = client
        .request(
            "session/fork",
            json!({ "sessionId": session_id, "cwd": cwd }),
        )
        .await?;
    let new_id = result
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or("fork did not return a session id")?
        .to_string();

    if let Some(n) = truncate_from {
        let _ = client
            .request(
                "_goose/unstable/session/conversation/truncate",
                json!({ "sessionId": new_id, "truncateFrom": n }),
            )
            .await;
    }

    let current_mode = result
        .pointer("/modes/currentModeId")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let available_modes = result
        .pointer("/modes/availableModes")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(parse_mode).collect())
        .unwrap_or_default();
    let _ = app.emit("session://created", json!({ "sessionId": new_id }));
    let thinking_effort = parse_thinking_effort(&result);
    Ok(SessionInfo {
        session_id: new_id,
        cwd,
        current_mode,
        available_modes,
        thinking_effort,
    })
}

/// Set the active session's thinking/reasoning effort (ACP
/// `session/set_config_option`, live-probed — `configId`, not `key`/`option`,
/// is the required field; see docs/acp-protocol.md). Live, per-session, no
/// goosed restart needed — unlike provider/temperature/model, which are
/// spawn-time env vars.
#[tauri::command]
pub async fn set_thinking_effort(
    app: AppHandle,
    session_id: String,
    value: String,
) -> Result<Option<ThinkingEffort>, String> {
    let client = api::ensure_client(&app).await?;
    let result = client
        .request(
            "session/set_config_option",
            json!({ "sessionId": session_id, "configId": "thinking_effort", "value": value }),
        )
        .await?;
    Ok(parse_thinking_effort(&result))
}

/// Hot-rebind an *already-open* session onto the currently-active provider's
/// model, via the same `session/set_config_option` mechanism (confirmed live,
/// `docs/acp-protocol.md`: `configOptions` includes `provider`/`model` select
/// entries, settable exactly like `thinking_effort` above).
///
/// Switching providers today only respawns goosed with new env vars —
/// correct for a brand-new session (`GOOSE_PROVIDER`/`GOOSE_MODEL` become its
/// default), but confirmed real bug: an *already-loaded* session keeps its
/// own previously-bound model, so continuing to chat in the same session
/// after switching sent the OLD provider's model id to the NEW provider
/// ("... is not a valid model ID"). This call is best-effort and swallows its
/// own failures — the `session/set_config_option` value format for
/// `provider`/`model` isn't independently live-probed beyond the
/// `thinking_effort` precedent, so if it's ever rejected, the worst case is
/// simply no rebind (today's existing behavior), never a new visible error.
#[tauri::command]
pub async fn rebind_session_provider(app: AppHandle, session_id: String) {
    let (provider_value, model_value) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        let active = cfg
            .active_provider_id
            .as_ref()
            .and_then(|id| cfg.providers.iter().find(|p| &p.id == id));
        match active {
            Some(p) => (
                Some(providers::goose_provider_name(&p.provider_type).to_string()),
                p.models.first().cloned(),
            ),
            None => (None, None),
        }
    };
    let Some(provider_value) = provider_value else {
        return;
    };
    let Ok(client) = api::ensure_client(&app).await else {
        return;
    };
    let _ = client
        .request(
            "session/set_config_option",
            json!({ "sessionId": session_id, "configId": "provider", "value": provider_value }),
        )
        .await;
    if let Some(model_value) = model_value {
        let _ = client
            .request(
                "session/set_config_option",
                json!({ "sessionId": session_id, "configId": "model", "value": model_value }),
            )
            .await;
    }
}

/// Get a session's persisted chat/agentic mode override, if any (`None` =
/// default to chat — see `Config::session_modes`'s doc comment).
#[tauri::command]
pub fn get_session_mode(
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> Result<Option<String>, String> {
    let cfg = state.config.lock().unwrap();
    Ok(cfg.session_modes.get(&session_id).cloned())
}

/// Set (or clear, via `None`) a session's mode override.
#[tauri::command]
pub fn set_session_mode(
    state: tauri::State<'_, AppState>,
    session_id: String,
    mode: Option<String>,
) -> Result<(), String> {
    let mut cfg = state.config.lock().unwrap();
    match mode {
        Some(m) if !m.trim().is_empty() => {
            cfg.session_modes.insert(session_id, m);
        }
        _ => {
            cfg.session_modes.remove(&session_id);
        }
    }
    config::save(&cfg).map_err(|e| e.to_string())
}

/// Delete a session (ACP `session/delete`). If `cwd` sits under the private
/// `Documents/Kitty/chats/` prefix (i.e. it was never an explicit user-chosen
/// working directory — see `resolve_cwd`), also remove that directory. The
/// prefix check is a hard safety gate: a custom/explicit folder is never
/// touched (Round-3 item 25).
#[tauri::command]
pub async fn delete_session(
    app: AppHandle,
    session_id: String,
    cwd: Option<String>,
) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    client
        .request("session/delete", json!({ "sessionId": session_id }))
        .await?;
    if let Some(cwd) = cwd {
        // Only remove a folder that sits under the chats base's `chats/` dir
        // (a Kitty-created per-chat folder) — never a user's own directory.
        let chats_root = chats_base_dir(&app).join(CHATS_DIR_NAME);
        let chats_root = format!("{}/", chats_root.to_string_lossy().replace('\\', "/"));
        let cwd_norm = cwd.replace('\\', "/");
        if cwd_norm.starts_with(&chats_root) {
            let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&cwd_norm)).await;
        }
    }
    // Cross-window live-update, mirroring `session://created` (Round-4 item 6)
    // — without this, another window's sidebar/recents keeps showing this
    // session until it happens to refresh for some other reason (confirmed
    // real gap: `regenerate()`'s background cleanup of the superseded session
    // it forked away from has no other way to reach a different window).
    let _ = app.emit("session://deleted", json!({ "sessionId": session_id }));
    Ok(())
}

/// Delete every session (Settings → General "Clear all chat history" — a
/// standalone destructive action, unrelated to provider switching). Loops
/// `session/delete` per id since goosed has no bulk method, reusing
/// `delete_session`'s exact "only remove a folder under the chats-root
/// prefix" safety gate for each one's working directory. Also clears
/// `session_folders`/`session_modes` (app-side organization that can't refer
/// to a now-deleted session) and the active-session pointer.
#[tauri::command]
pub async fn clear_all_sessions(app: AppHandle) -> Result<usize, String> {
    let client = api::ensure_client(&app).await?;
    let sessions = {
        let result = client.request("session/list", json!({})).await?;
        result
            .get("sessions")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default()
    };

    let chats_root = chats_base_dir(&app).join(CHATS_DIR_NAME);
    let chats_root = format!("{}/", chats_root.to_string_lossy().replace('\\', "/"));

    let mut deleted = 0usize;
    let mut last_err: Option<String> = None;
    for s in &sessions {
        let Some(sid) = s.get("sessionId").and_then(|v| v.as_str()) else {
            continue;
        };
        match client
            .request("session/delete", json!({ "sessionId": sid }))
            .await
        {
            Ok(_) => {
                deleted += 1;
                if let Some(cwd) = s.get("cwd").and_then(|v| v.as_str()) {
                    let cwd_norm = cwd.replace('\\', "/");
                    if cwd_norm.starts_with(&chats_root) {
                        let _ =
                            tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&cwd_norm))
                                .await;
                    }
                }
            }
            Err(e) => last_err = Some(e), // keep going; one bad id shouldn't abort the rest
        }
    }

    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.session_folders.clear();
        cfg.session_modes.clear();
        config::save(&cfg).map_err(|e| e.to_string())?;
        *state.active_session.lock().unwrap() = None;
    }
    // Deliberately not re-emitting `session://active` with a null payload here
    // — `onActiveSession`'s only consumer (`main/App.tsx`) assumes a real
    // `SessionInfo` and dereferences `info.session_id` unconditionally, so a
    // null payload would throw there. `session://cleared` below is what
    // already-open windows react to instead (SessionList/chatStore both blank
    // their own state directly, the same pattern `handOffToMain` uses).
    let _ = app.emit("session://cleared", json!({ "deleted": deleted }));

    match last_err {
        Some(e) if deleted == 0 => Err(e),
        _ => Ok(deleted),
    }
}
