//! Scheduled-task execution loop, extracted from `lifecycle/mod.rs`. The
//! `commands` module already depends on `lifecycle` (orchestration calls like
//! `start_stack`), so `lifecycle/mod.rs` itself reaching back into `commands`
//! (to fire `new_session`/`send_prompt`) is the reverse half of a cycle.
//! Isolating that one call to this leaf file confines the cyclic edge to a
//! single narrow file instead of `mod.rs` — the hub every other module also
//! depends on.

use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::state::AppState;

/// Fires due scheduled tasks headlessly — no window needs to be open. Checks
/// every 30s (sub-minute precision isn't needed for a user-authored
/// schedule). Reuses `commands::new_session`/`send_prompt` verbatim: they're
/// plain `AppHandle`-only Tauri commands with no frontend dependency, and
/// `send_prompt`'s own fire-and-forget completion handler already calls
/// `notifications::notify_if_hidden` — no separate notification plumbing
/// needed here.
///
/// Missed-fire policy: a task's `next_fire` is only ever advanced *forward
/// from now* after it fires (`now + interval_secs` for `Recurring`; disabled
/// for `OneShot`), never backfilled per missed interval — so a task that was
/// due while the app wasn't running fires exactly once on the next tick after
/// launch, not once per missed occurrence.
pub fn spawn_scheduler_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        loop {
            ticker.tick().await;
            let due: Vec<crate::config::scheduled_tasks::ScheduledTask> = {
                let state = app.state::<AppState>();
                let cfg = state.config.lock().unwrap();
                let now = chrono::Local::now();
                cfg.scheduled_tasks
                    .iter()
                    .filter(|t| t.enabled && t.next_fire <= now)
                    .cloned()
                    .collect()
            };
            for task in due {
                fire_scheduled_task(&app, task).await;
            }
        }
    });
}

async fn fire_scheduled_task(app: &AppHandle, task: crate::config::scheduled_tasks::ScheduledTask) {
    tracing::info!("scheduled task '{}' ({}) firing", task.name, task.id);
    let sent = match crate::commands::new_session(app.clone(), task.cwd.clone()).await {
        Ok(info) => {
            // Stamp the task's model override (D3) onto the session *before*
            // sending, so the very first turn runs on it.
            //
            // Applied as a separate `PATCH` rather than passed to
            // `new_session`: `POST /api/chat/` only honours `model` when
            // `provider` is sent with it (`routes/chat.rs`), and a task
            // override names a model, not a provider — the provider is
            // whichever one is active at fire time. Going through
            // `set_session_provider` pairs the two correctly and reuses a
            // command that already exists.
            if let Some(model) = task.model_id.as_deref().filter(|m| !m.is_empty()) {
                let active_provider = {
                    let state = app.state::<AppState>();
                    let cfg = state.config.lock().unwrap();
                    cfg.active_provider_id.clone()
                };
                match active_provider {
                    Some(provider_id) => {
                        crate::bigtiny::providers::set_session_provider(
                            app,
                            &info.session_id,
                            &provider_id,
                            model,
                        )
                        .await;
                    }
                    None => tracing::warn!(
                        "scheduled task '{}' wants model '{model}' but no provider is active; \
                         running on the session default",
                        task.id
                    ),
                }
            }
            match crate::commands::send_prompt(
                app.clone(),
                info.session_id,
                task.prompt.clone(),
                None,
            )
            .await
            {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!("scheduled task '{}' failed to send: {e}", task.id);
                    false
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                "scheduled task '{}' failed to start a session: {e}",
                task.id
            );
            false
        }
    };
    if sent {
        advance_scheduled_task(app, &task.id);
    } else {
        // Deliberately *not* advanced: leaving `next_fire` past-due makes the
        // next 30s tick retry the task — a send failure here is transient by
        // assumption (the daemon being down/restarting), and the old
        // unconditional advance silently disabled a one-shot that never
        // actually ran. The miss policy above ("advance forward from now, no
        // backfill") still applies to *successful* fires, so a retried task
        // runs once, not once per missed interval.
        tracing::warn!(
            "scheduled task '{}' was not delivered; it stays past-due and the next tick will retry it",
            task.id
        );
    }
}

fn advance_scheduled_task(app: &AppHandle, task_id: &str) {
    use crate::config::scheduled_tasks::Schedule;
    let state = app.state::<AppState>();
    let mut cfg = state.config.lock().unwrap();
    if let Some(t) = cfg.scheduled_tasks.iter_mut().find(|t| t.id == task_id) {
        match &t.schedule {
            Schedule::OneShot => t.enabled = false,
            Schedule::Recurring { interval_secs } => {
                t.next_fire =
                    chrono::Local::now() + chrono::Duration::seconds(*interval_secs as i64);
            }
        }
    }
    if let Err(e) = crate::config::save(&cfg) {
        tracing::warn!("failed to persist scheduled task advance: {e}");
    }
}
