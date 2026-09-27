//! Restart the daemon when a load-time setting changes (§6.4, D11).
//!
//! Everything in `bigtiny_env::SpawnSnapshot` reaches the daemon as a
//! `BIGTINY_*` env var at spawn, so there is no in-process path to apply one —
//! changing it means restarting the daemon, full stop. That makes the *timing*
//! the whole design:
//!
//! - **Idle → restart immediately.** Nothing is lost.
//! - **Mid-generation → queue it.** Restarting ends every turn the daemon is
//!   running. §4.1's "in-flight streams are never aborted" is not a nicety
//!   here: the user would watch a half-written reply vanish because they
//!   nudged a slider in another window.
//! - **Another app is using the engine → queue it, and say who.** The daemon
//!   is shared (see `bigtiny_v2`). It refuses a restart while another app is
//!   attached or mid-turn (`POST /api/admin/restart`), and names them;
//!   the user can choose "Restart anyway", which overrides only the
//!   attached-apps check, never a running turn.
//!
//! A restart asks the daemon to exit, waits until it has, and attaches again,
//! which spawns a fresh daemon with the current settings. Asking rather than
//! killing is what makes it safe for the other apps: the daemon decides, with
//! the whole picture.
//!
//! Android hosts the daemon in-process and cannot restart it; a changed
//! setting there applies at the next launch, and the state says so.

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::config::Config;
use crate::lifecycle::bigtiny_env::SpawnSnapshot;
use crate::state::AppState;

/// Something that stopped the daemon from restarting: another app attached
/// to it, or a turn still running. From `POST /api/admin/restart`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartBlocker {
    pub app_id: String,
    pub display_name: String,
    /// `attached` or `active_turn`.
    pub reason: String,
}

/// Payload for `engine://restart-state`. Drives the non-blocking
/// "restart pending" banner — never a modal (§6.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct EngineRestartState {
    /// A load-time setting changed and the running daemon no longer matches
    /// the saved config.
    pub reload_required: bool,
    /// The restart is waiting: for Kitty's own generation to finish, or for
    /// the apps in `blocked_by`.
    pub restart_pending: bool,
    /// Who the daemon said is in the way, the last time Kitty asked.
    pub blocked_by: Vec<RestartBlocker>,
}

/// What a restart attempt did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct RestartOutcome {
    /// The daemon was restarted and Kitty is attached to the new one.
    pub restarted: bool,
    /// When it was not: who is in the way.
    pub blocked_by: Vec<RestartBlocker>,
    /// Whether the daemon Kitty is now attached to is one Kitty started. When
    /// another app won the race to start it, it runs with that app's engine
    /// settings.
    pub spawned_by_us: bool,
}

/// True when `new` needs a daemon restart to take effect.
///
/// Compared as a whole snapshot rather than field-by-field: the question is
/// never "did one knob change" but "is the running daemon still consistent
/// with the saved config", and a field added to `SpawnSnapshot` later is
/// load-time by construction.
pub fn needs_restart(old: &Config, new: &Config) -> bool {
    SpawnSnapshot::from_config(old) != SpawnSnapshot::from_config(new)
}

fn emit(app: &AppHandle, state: EngineRestartState) {
    let changed = {
        let s = app.state::<AppState>();
        let mut cur = s.engine_restart.lock().unwrap();
        let changed = *cur != state;
        *cur = state.clone();
        changed
    };
    if changed {
        let _ = app.emit("engine://restart-state", state);
    }
}

/// Current state, for a window that attaches after the event fired.
pub fn current(app: &AppHandle) -> EngineRestartState {
    app.state::<AppState>()
        .engine_restart
        .lock()
        .unwrap()
        .clone()
}

#[cfg(not(target_os = "android"))]
fn kitty_is_generating(app: &AppHandle) -> bool {
    !app.state::<AppState>()
        .in_flight_sessions
        .lock()
        .unwrap()
        .is_empty()
}

/// Call after persisting a config change. Restarts the daemon now if nothing
/// is in the way, or marks it pending so [`apply_if_pending`] picks it up.
pub fn schedule(app: &AppHandle) {
    emit(
        app,
        EngineRestartState {
            reload_required: true,
            ..Default::default()
        },
    );
    apply_if_pending(app);
}

/// Restart now if a reload is outstanding and nothing is generating.
///
/// Safe to call from anywhere and often — it's a no-op unless a reload is
/// actually outstanding. `bigtiny::stream` calls it as each turn completes,
/// which is what drains a queued restart, and the health loop calls it
/// periodically while another app is in the way.
pub fn apply_if_pending(app: &AppHandle) {
    #[cfg(target_os = "android")]
    {
        // Nothing to restart; the setting applies at the next launch.
        let _ = app;
    }
    #[cfg(not(target_os = "android"))]
    {
        let state = current(app);
        if !state.reload_required {
            return;
        }
        if kitty_is_generating(app) {
            emit(
                app,
                EngineRestartState {
                    restart_pending: true,
                    ..state
                },
            );
            tracing::info!("engine restart queued behind an in-flight generation");
            return;
        }
        let app2 = app.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = restart_now(&app2, false).await {
                tracing::warn!("engine restart failed: {e}");
            }
        });
    }
}

/// Restart the daemon now, or report who is in the way.
///
/// `force` overrides only the "another app is attached" check; a running turn
/// (Kitty's or anyone's) always blocks.
#[cfg(not(target_os = "android"))]
pub async fn restart_now(app: &AppHandle, force: bool) -> Result<RestartOutcome, String> {
    use std::sync::atomic::Ordering;

    if kitty_is_generating(app) {
        return Err("Wait for the reply in progress to finish, then restart.".to_string());
    }
    let flag = &app.state::<AppState>().restart_in_progress;
    if flag
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("The engine is already restarting.".to_string());
    }
    let result = restart_inner(app, force).await;
    app.state::<AppState>()
        .restart_in_progress
        .store(false, Ordering::SeqCst);

    let before = current(app);
    match &result {
        Ok(outcome) if outcome.restarted => emit(app, EngineRestartState::default()),
        Ok(outcome) => emit(
            app,
            EngineRestartState {
                reload_required: before.reload_required,
                restart_pending: before.reload_required,
                blocked_by: outcome.blocked_by.clone(),
            },
        ),
        // The running daemon still doesn't match the saved config; keep
        // saying so rather than pretending it applied.
        Err(_) => emit(
            app,
            EngineRestartState {
                reload_required: before.reload_required,
                ..Default::default()
            },
        ),
    }
    result
}

#[cfg(not(target_os = "android"))]
async fn restart_inner(app: &AppHandle, force: bool) -> Result<RestartOutcome, String> {
    let (port, instance_id) = {
        let state = app.state::<AppState>();
        let handle = state.bigtiny.lock().unwrap();
        (handle.port, handle.instance_id.clone())
    };

    if let Ok(client) = crate::bigtiny::client::ensure_client(app) {
        match ask_to_restart(&client, force).await? {
            AskOutcome::Accepted => {
                if let Some(port) = port {
                    wait_for_exit(port, instance_id.as_deref()).await;
                }
            }
            AskOutcome::Blocked(blocked_by) => {
                return Ok(RestartOutcome {
                    restarted: false,
                    blocked_by,
                    spawned_by_us: false,
                })
            }
            // Nothing is answering, so there is nothing to stop.
            AskOutcome::Unreachable => {}
        }
    }

    crate::lifecycle::attach_daemon(app).await?;
    let spawned_by_us = app
        .state::<AppState>()
        .bigtiny
        .lock()
        .unwrap()
        .spawned_by_us;
    Ok(RestartOutcome {
        restarted: true,
        blocked_by: Vec::new(),
        spawned_by_us,
    })
}

#[cfg(not(target_os = "android"))]
#[derive(Debug, PartialEq, Eq)]
enum AskOutcome {
    Accepted,
    Blocked(Vec<RestartBlocker>),
    Unreachable,
}

/// `POST /api/admin/restart`, split out from the attach half so it can be
/// tested against a mock daemon.
#[cfg(not(target_os = "android"))]
async fn ask_to_restart(
    client: &crate::bigtiny::client::BigTinyClient,
    force: bool,
) -> Result<AskOutcome, String> {
    #[derive(Deserialize)]
    struct Reply {
        accepted: bool,
        #[serde(default)]
        blocked_by: Vec<RestartBlocker>,
    }
    let resp = match client
        .request(reqwest::Method::POST, "/api/admin/restart")
        .json(&serde_json::json!({ "force": force }))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(_) => return Ok(AskOutcome::Unreachable),
    };
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("The engine refused to restart ({status}): {body}"));
    }
    let reply: Reply = resp
        .json()
        .await
        .map_err(|e| format!("The engine's restart answer was unreadable: {e}"))?;
    Ok(if reply.accepted {
        AskOutcome::Accepted
    } else {
        AskOutcome::Blocked(reply.blocked_by)
    })
}

/// Wait (bounded) until the daemon on `port` has gone, or been replaced by
/// a different instance.
#[cfg(not(target_os = "android"))]
async fn wait_for_exit(port: u16, instance_id: Option<&str>) {
    let client = crate::util::http_client();
    let url = format!("http://127.0.0.1:{port}/api/health");
    for _ in 0..100 {
        let answer = client
            .get(&url)
            .timeout(std::time::Duration::from_secs(1))
            .send()
            .await;
        let still_there = match answer {
            Ok(resp) => match resp.json::<serde_json::Value>().await {
                Ok(body) => {
                    let now = body.get("instance_id").and_then(|v| v.as_str());
                    instance_id.is_none() || now == instance_id
                }
                Err(_) => false,
            },
            Err(_) => false,
        };
        if !still_there {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    tracing::warn!("the engine accepted a restart but was still running 20s later");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon's answer decides: accepted, or blocked with who is in the
    /// way. A daemon that is not there at all is "unreachable", not an error:
    /// there is nothing to stop, and attaching will start a fresh one.
    #[cfg(not(target_os = "android"))]
    #[tokio::test]
    async fn the_daemons_restart_answer_is_read_back() {
        let mut server = mockito::Server::new_async().await;
        let client = crate::bigtiny::client::BigTinyClient::new(server.url(), Some("k".into()));

        let accepted = server
            .mock("POST", "/api/admin/restart")
            .match_body(mockito::Matcher::Json(serde_json::json!({"force": false})))
            .with_body(r#"{"accepted":true,"blocked_by":[]}"#)
            .create_async()
            .await;
        assert_eq!(
            ask_to_restart(&client, false).await,
            Ok(AskOutcome::Accepted)
        );
        accepted.remove_async().await;

        server
            .mock("POST", "/api/admin/restart")
            .with_body(
                r#"{"accepted":false,"blocked_by":[{"app_id":"research","display_name":"Research","reason":"attached"}]}"#,
            )
            .create_async()
            .await;
        match ask_to_restart(&client, false).await.unwrap() {
            AskOutcome::Blocked(by) => {
                assert_eq!(by.len(), 1);
                assert_eq!(by[0].display_name, "Research");
                assert_eq!(by[0].reason, "attached");
            }
            other => panic!("expected blocked, got {other:?}"),
        }

        let gone = crate::bigtiny::client::BigTinyClient::new("http://127.0.0.1:9", None);
        assert_eq!(
            ask_to_restart(&gone, true).await,
            Ok(AskOutcome::Unreachable)
        );
    }

    /// Only load-time settings trigger a restart. Anything else saved in the
    /// same `set_config` call — a theme, a hotkey, a folder — must not kill
    /// the daemon.
    #[test]
    fn unrelated_settings_do_not_trigger_a_restart() {
        let a = Config::default();
        let b = Config {
            theme: "dark".into(),
            remember_overlay_position: true,
            show_artifacts: false,
            ..Config::default()
        };
        assert!(!needs_restart(&a, &b));
    }

    /// Every setting the daemon reads only at spawn must schedule a restart —
    /// before `SpawnSnapshot` this list was the retired llama knobs plus the
    /// two model ids, so toggling memory or changing a specialist limit
    /// silently never reached a running daemon.
    #[test]
    fn a_changed_spawn_setting_triggers_a_restart() {
        let a = Config::default();
        for mutate in [
            (|c: &mut Config| c.summarizer.enabled = !c.summarizer.enabled) as fn(&mut Config),
            |c: &mut Config| c.token_management.max_context_tokens += 1,
            |c: &mut Config| c.memory.bm25_threshold = Some(2.5),
            |c: &mut Config| c.specialists.timeout_secs += 1,
            |c: &mut Config| c.specialists.max_concurrent += 1,
            |c: &mut Config| c.specialists.model_deny.push("x/y".into()),
        ] {
            let mut b = Config::default();
            mutate(&mut b);
            assert!(
                needs_restart(&a, &b),
                "expected a restart for {:?}",
                SpawnSnapshot::from_config(&b)
            );
        }
    }

    /// Switching either model is load-time too: the ids resolve to
    /// `BIGTINY_LITERT__*_MODEL_PATH` at spawn, so a running daemon keeps the
    /// old weights until it's replaced.
    #[test]
    fn switching_either_model_triggers_a_restart() {
        let a = Config::default();

        let b = Config {
            summarizer: crate::config::SummarizerSettings {
                model: "some-other-model".into(),
                ..Default::default()
            },
            ..Config::default()
        };
        assert!(needs_restart(&a, &b));

        let c = Config {
            adaptive_pathway_embedding_model: "bge-small-en-v1.5".into(),
            ..Config::default()
        };
        assert!(needs_restart(&a, &c));
    }

    /// The memory toggles apply live through the daemon's per-app switch
    /// (`lifecycle::memory`), so flipping one must not restart anything.
    #[test]
    fn a_memory_toggle_does_not_restart() {
        let a = Config::default();
        let b = Config {
            adaptive_pathway_enabled: !a.adaptive_pathway_enabled,
            memorabilia_enabled: !a.memorabilia_enabled,
            ..Config::default()
        };
        assert!(!needs_restart(&a, &b));
    }

    /// Saving the same config twice (the UI does this on every keystroke in
    /// some panels) must not restart the daemon repeatedly.
    #[test]
    fn an_unchanged_config_never_restarts() {
        let a = Config::default();
        assert!(!needs_restart(&a, &a.clone()));
    }
}
