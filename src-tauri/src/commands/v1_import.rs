//! One-click import of a Kitty V1 install's data (#11).
//!
//! Kitty V1 (0.8 and earlier) ran its own daemon with its data under
//! `%APPDATA%/Kitty/bigtiny/` (the app-private dir on Android). An upgrade
//! used to start empty and leave that behind for a manual CLI step. Now Kitty
//! notices it, offers to bring it across, and supplies V1's encryption key
//! itself from the credential store, so saved provider keys come too.
//!
//! The daemon does the merge (`POST /api/apps/me/import-v1`): it reads a
//! copy, skips anything already present, and re-seals secrets under its own
//! key. Afterwards V1's directory is renamed to `bigtiny.v1-imported`, kept as
//! a rollback path, and never offered again.

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::bigtiny::client::ensure_client;
use crate::state::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct V1Data {
    /// Size of V1's database, for "about N MB of chats" copy.
    pub db_bytes: u64,
    /// V1 also kept a belief graph.
    pub has_pathway: bool,
}

/// V1 data waiting to be imported, or `None` when there is none or the user
/// already imported or dismissed it.
#[tauri::command]
pub fn detect_v1_data(state: tauri::State<'_, AppState>) -> Result<Option<V1Data>, String> {
    if state.config.lock().unwrap().v1_import_state.is_some() {
        return Ok(None);
    }
    let Ok(dir) = crate::config::v1_data_dir() else {
        return Ok(None);
    };
    Ok(detect_in(&dir))
}

fn detect_in(dir: &std::path::Path) -> Option<V1Data> {
    let meta = std::fs::metadata(dir.join("bigtiny.db")).ok()?;
    Some(V1Data {
        db_bytes: meta.len(),
        has_pathway: dir.join("pathway.db").is_file(),
    })
}

/// Import V1's chats, providers, tool servers, approval rules and beliefs.
/// Returns the daemon's summary (counts, plus `undecryptable_secrets` when
/// some saved keys could not be read and need entering again).
#[tauri::command]
pub async fn import_v1_data(app: AppHandle) -> Result<Value, String> {
    let dir = crate::config::v1_data_dir().map_err(|e| e.to_string())?;
    let db = dir.join("bigtiny.db");
    if !db.is_file() {
        return Err("There is no earlier Kitty data on this device to import.".into());
    }
    // Without V1's key the chats still come across; only the saved provider
    // keys stay unreadable, which the summary reports so the user can enter
    // them again. A random key makes the daemon count them instead of failing.
    let key = match crate::config::providers::v1_encryption_key().await {
        Ok(Some(key)) => key,
        Ok(None) => {
            tracing::warn!("no V1 encryption key in the credential store; importing without it");
            random_key()
        }
        Err(e) => {
            tracing::warn!("could not read the V1 encryption key: {e}");
            return Err(
                "Could not read your earlier saved keys from this device's secure \
                 credential store. Try again in a moment."
                    .into(),
            );
        }
    };
    let pathway = dir.join("pathway.db");
    let body = json!({
        "v1_db_path": db.to_string_lossy(),
        "v1_encryption_key_hex": key,
        "pathway_db_path": pathway.is_file().then(|| pathway.to_string_lossy().into_owned()),
    });
    let client = ensure_client(&app)?;
    let summary = client
        .post_json_long("/api/apps/me/import-v1", &body)
        .await
        .map_err(|e| {
            tracing::warn!("V1 import failed: {e}");
            "The import did not finish. Nothing was changed; you can try again.".to_string()
        })?;

    record(&app, "imported");
    let kept = dir.with_file_name("bigtiny.v1-imported");
    if let Err(e) = std::fs::rename(&dir, &kept) {
        // The recorded state already stops it being offered again.
        tracing::warn!("could not rename {}: {e}", dir.display());
    }
    // Kitty's own profiles are the truth for providers: re-send them, keys
    // included, over whatever the import brought.
    if let Err(e) = crate::bigtiny::providers::sync_all_providers(&app).await {
        tracing::warn!("provider sync after the V1 import failed: {e}");
    }
    Ok(summary)
}

/// "Not now, and don't ask again."
#[tauri::command]
pub fn dismiss_v1_import(app: AppHandle) -> Result<(), String> {
    record(&app, "dismissed");
    Ok(())
}

fn record(app: &AppHandle, outcome: &str) {
    let state = app.state::<AppState>();
    let mut cfg = state.config.lock().unwrap();
    cfg.v1_import_state = Some(outcome.to_string());
    if let Err(e) = crate::config::save(&cfg) {
        tracing::warn!("could not record the V1 import state: {e}");
    }
}

fn random_key() -> String {
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_data_is_found_by_its_database() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            detect_in(dir.path()).is_none(),
            "an empty directory is not V1 data"
        );
        std::fs::write(dir.path().join("bigtiny.db"), [0u8; 7]).unwrap();
        let found = detect_in(dir.path()).unwrap();
        assert_eq!(found.db_bytes, 7);
        assert!(!found.has_pathway);
        std::fs::write(dir.path().join("pathway.db"), []).unwrap();
        assert!(detect_in(dir.path()).unwrap().has_pathway);
    }

    #[test]
    fn a_stand_in_key_is_one_the_daemon_accepts() {
        let key = random_key();
        assert_eq!(key.len(), 64);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
