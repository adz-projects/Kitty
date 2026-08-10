//! Local GGUF management commands (docs/ANDROID.md §5.1).
//!
//! Thin by design: everything with logic lives in `crate::models`, which takes
//! paths and streams and no `AppHandle`, so it can be tested without a Tauri
//! runtime. This file resolves state, spawns, and emits.

use std::path::PathBuf;
use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager};

use crate::models::download::{self, DownloadSpec};
use crate::models::{gguf, InstalledModel};
use crate::state::AppState;

/// Progress for one download, emitted as `models://progress`.
///
/// `Arc<str>` for the two string fields: they're identical on every chunk, so
/// this is a refcount bump per event instead of two allocations.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DownloadProgress {
    pub download_id: Arc<str>,
    pub model: Arc<str>,
    pub received: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    pub done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A model on disk plus its card fields.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LocalModel {
    #[serde(flatten)]
    pub model: InstalledModel,
    pub info: Option<gguf::GgufInfo>,
}

/// What the daemon's engine is actually doing right now — which backend and
/// device it picked, and per slot how many layers and how much context that
/// resolved to.
///
/// Passed through as an opaque `Value` rather than re-declared as a Rust
/// struct. This is a read-only display payload with no app-side logic hanging
/// off it, and mirroring `SlotStatus`/`SelectedBackend` here would create a
/// third copy of a shape that already exists in the daemon and in
/// `src/lib/types.ts` — one that could drift without anything failing to
/// compile.
///
/// `Ok(None)` rather than an error when the daemon isn't up yet: Settings can
/// legitimately be open before the stack is ready, and "not running" is a
/// state to render, not a failure to report.
#[tauri::command]
pub async fn get_local_engine_status(app: AppHandle) -> Result<Option<serde_json::Value>, String> {
    let Ok(client) = crate::bigtiny::client::ensure_client(&app) else {
        return Ok(None);
    };
    match client.get_json("/api/local/models/status").await {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            tracing::debug!("local engine status unavailable: {e}");
            Ok(None)
        }
    }
}

#[tauri::command]
pub fn list_local_models() -> Result<Vec<LocalModel>, String> {
    Ok(crate::models::installed()
        .into_iter()
        .map(|m| {
            let info = gguf::read_info(std::path::Path::new(&m.path));
            LocalModel { model: m, info }
        })
        .collect())
}

/// Free bytes on the models volume, for the low-space warning. `None` when it
/// can't be determined — the UI then shows nothing rather than a wrong number.
#[tauri::command]
pub fn get_models_disk_free() -> Result<Option<u64>, String> {
    let dir = crate::config::models_dir().map_err(|e| e.to_string())?;
    Ok(download::free_space(&dir))
}

/// Delete an installed GGUF. Manual only (D7) — nothing deletes models
/// automatically, including on model-switch.
#[tauri::command]
pub fn delete_local_model(app: AppHandle, id: String) -> Result<(), String> {
    let path = crate::models::resolve(&id).ok_or_else(|| format!("no such model: {id}"))?;
    std::fs::remove_file(&path).map_err(|e| format!("could not delete {}: {e}", path.display()))?;
    let _ = app.emit("models://changed", ());
    refresh_embedding_status(&app);
    Ok(())
}

/// Start a download; returns its id immediately. Progress arrives as
/// `models://progress` events keyed by that id, so several can run at once.
///
/// `download_id` lets a caller pre-agree an id (the wizard and the pathway
/// embedding model both do, so they can subscribe before starting).
#[tauri::command]
pub fn download_model(
    app: AppHandle,
    repo: String,
    file: String,
    rev: Option<String>,
    download_id: Option<String>,
) -> Result<String, String> {
    let id = download_id
        .unwrap_or_else(|| format!("dl_{}", chrono::Utc::now().timestamp_millis()));
    let spec = DownloadSpec {
        repo,
        file: file.clone(),
        rev: rev.unwrap_or_else(|| "main".into()),
        sha256: None,
        expected_size: None,
    };
    let id_for_task = id.clone();
    tauri::async_runtime::spawn(async move {
        run_download(app, spec, id_for_task).await;
    });
    Ok(id)
}

/// One download, start to finish, reporting everything through
/// `models://progress`.
///
/// Returns nothing: a download is fire-and-forget from the caller's point of
/// view, and every outcome — including every failure — is an event, so a UI
/// that subscribed before starting can't miss one.
async fn run_download(app: AppHandle, mut spec: DownloadSpec, id: String) {
    let id: Arc<str> = Arc::from(id.as_str());
    let model: Arc<str> = Arc::from(spec.file.as_str());
    let emit = |received: u64, total: Option<u64>, done: bool, error: Option<String>| {
        let _ = app.emit(
            "models://progress",
            DownloadProgress {
                download_id: id.clone(),
                model: model.clone(),
                received,
                total,
                done,
                error,
            },
        );
    };
    let fail = |e: String| {
        tracing::warn!(model = %spec.file, "model download failed: {e}");
        emit(0, None, true, Some(e));
    };

    let dir = match crate::config::models_dir() {
        Ok(d) => d,
        Err(e) => return fail(e.to_string()),
    };
    if dir.join(&spec.file).exists() {
        return fail(download::DownloadError::AlreadyInstalled(spec.file.clone()).to_string());
    }

    let client = crate::util::http_client();
    let (size, sha) = download::head_metadata(&client, &spec).await;
    spec.expected_size = size;
    spec.sha256 = sha;

    if let Err(e) = download::check_space(&dir, spec.expected_size) {
        return fail(e.to_string());
    }

    // One retry, and only for a checksum mismatch: `verify_and_finalize` has
    // deleted the `.part` by then, so the retry is a clean full download
    // rather than a resume of the same corrupt bytes. Transport errors are
    // not retried here — the `.part` survives, so the user re-invoking picks
    // up where it stopped.
    for attempt in 0..2 {
        match attempt_download(&client, &dir, &spec, &emit).await {
            Ok(path) => {
                tracing::info!(path = %path.display(), "model downloaded");
                emit(spec.expected_size.unwrap_or(0), spec.expected_size, true, None);
                let _ = app.emit("models://changed", ());
                refresh_embedding_status(&app);
                return;
            }
            Err(download::DownloadError::ChecksumMismatch { expected, actual }) if attempt == 0 => {
                tracing::warn!(
                    model = %spec.file,
                    "checksum mismatch (expected {expected}, got {actual}); retrying once"
                );
            }
            Err(e) => return fail(e.to_string()),
        }
    }
}

async fn attempt_download(
    client: &reqwest::Client,
    dir: &std::path::Path,
    spec: &DownloadSpec,
    emit: &(impl Fn(u64, Option<u64>, bool, Option<String>) + Sync),
) -> Result<PathBuf, download::DownloadError> {
    let resume_from = download::resume_offset(dir, &spec.file, spec);
    download::write_meta(dir, &spec.file, spec)?;

    let mut req = client.get(spec.url());
    if resume_from > 0 {
        req = req.header("Range", format!("bytes={resume_from}-"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| download::DownloadError::Transport(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(download::DownloadError::Transport(format!(
            "{} returned {}",
            spec.url(),
            resp.status()
        )));
    }

    let total = spec
        .expected_size
        .or_else(|| resp.content_length().map(|n| n + resume_from));
    emit(resume_from, total, false, None);

    let part = download::part_path(dir, &spec.file);
    let stream = resp.bytes_stream();
    futures_util::pin_mut!(stream);
    // Throttle: a multi-GB download produces tens of thousands of chunks, and
    // an event per chunk would flood the webview for no visible benefit.
    let mut last_emit = 0u64;
    download::append_stream(&part, resume_from, stream, &mut |received| {
        if received - last_emit >= 1_000_000 {
            last_emit = received;
            emit(received, total, false, None);
        }
    })
    .await?;

    download::verify_and_finalize(dir, &spec.file, spec.sha256.as_deref())
}

/// Re-derive the pathway embedding model's presence after the model set
/// changes, so Settings updates immediately instead of on the next 30s tick.
fn refresh_embedding_status(app: &AppHandle) {
    let (enabled, model) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        (
            cfg.adaptive_pathway_enabled,
            cfg.adaptive_pathway_embedding_model.clone(),
        )
    };
    if enabled {
        crate::lifecycle::embedding::refresh_embedding_status(app, &model);
    }
}
