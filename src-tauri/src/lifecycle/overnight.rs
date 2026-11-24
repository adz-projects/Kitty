//! Overnight learning, applied live (docs/MEMORY_OVERNIGHT.md).
//!
//! The engine keeps one window per app (`/api/apps/me/overnight`); this module
//! PUTs Kitty's settings whenever the engine's copy differs, and keeps the
//! Windows wake-up task (`wake_task`) in step with them.
//!
//! On Android nothing moves to the night: memorabilia does not run there and
//! pathway keeps learning at turn end, so the engine is told there is no
//! window.

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::config::OvernightSettings;
use crate::state::AppState;

/// The document the engine should hold. Pure, so the mapping is testable.
pub fn desired(s: &OvernightSettings) -> Value {
    json!({
        "enabled": s.enabled,
        "start": s.start.trim(),
        "end": s.end.trim(),
        "pathway": s.pathway,
        "idle_micro": s.idle_micro,
    })
}

/// Bring the engine's copy in line with Settings. Best-effort: failures are
/// logged and leave the engine as it was.
pub async fn apply_overnight(app: &AppHandle) {
    if let Err(e) = apply(app).await {
        tracing::warn!("could not apply the overnight-learning settings: {e}");
    }
}

async fn apply(app: &AppHandle) -> Result<(), String> {
    let client = crate::bigtiny::client::ensure_client(app)?;
    let have = client.get_json("/api/apps/me/overnight").await?;
    let have = have
        .get("status")
        .and_then(|s| s.get("config"))
        .cloned()
        .unwrap_or(Value::Null);
    if cfg!(target_os = "android") {
        if !have.is_null() {
            client.delete("/api/apps/me/overnight").await?;
        }
        return Ok(());
    }
    let want = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        desired(&cfg.overnight)
    };
    if have != want {
        client.put_json("/api/apps/me/overnight", &want).await?;
    }
    Ok(())
}

/// Bring the engine's learning-model choice in line with Settings (both
/// platforms). Best-effort, like the window.
pub async fn apply_learning_model(app: &AppHandle) {
    if let Err(e) = apply_learning(app).await {
        tracing::warn!("could not apply the memory learning model: {e}");
    }
}

async fn apply_learning(app: &AppHandle) -> Result<(), String> {
    let client = crate::bigtiny::client::ensure_client(app)?;
    let want = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.learning_model_desired()
    };
    let have = client
        .get_json("/api/apps/me/learning-model")
        .await?
        .get("learning_model")
        .cloned()
        .unwrap_or(Value::Null);
    // No row is `auto`.
    let have = if have.is_null() {
        json!({"kind": "auto"})
    } else {
        have
    };
    if have != want {
        client
            .put_json("/api/apps/me/learning-model", &want)
            .await?;
    }
    Ok(())
}

/// Make the wake-up task match Settings (desktop). Runs `schtasks`, so off the
/// async threads.
pub async fn sync_wake_task(app: &AppHandle) -> Result<(), String> {
    if cfg!(target_os = "android") {
        return Ok(());
    }
    let settings = app
        .state::<AppState>()
        .config
        .lock()
        .unwrap()
        .overnight
        .clone();
    tauri::async_runtime::spawn_blocking(move || super::wake_task::sync(&settings))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engine_gets_the_window_but_not_the_wake_switch() {
        let s = OvernightSettings {
            start: " 23:30".into(),
            wake: true,
            ..OvernightSettings::default()
        };
        let d = desired(&s);
        assert_eq!(d["start"], "23:30");
        assert_eq!(d["end"], "05:00");
        assert_eq!(d["enabled"], true);
        assert_eq!(d["pathway"], true);
        assert_eq!(d["idle_micro"], true);
        assert!(d.get("prefer_provider").is_none());
        assert!(d.get("wake").is_none());
    }
}
