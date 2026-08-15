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
        // Android only: the same numbers also drive the foreground-service
        // notification, which is what keeps the process (and its network)
        // alive once the user switches away. Free on desktop.
        foreground::progress(&model, received, total);
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

    // Held for the rest of this function; its `Drop` stops the service, so
    // every exit path below — including the early `return fail(..)`s — tears
    // it down without needing to remember to.
    let _foreground = foreground::Session::start(&model);

    let client = crate::util::http_client();
    let (size, sha) = download::head_metadata(&client, &spec).await;
    spec.expected_size = size;
    spec.sha256 = sha;

    if let Err(e) = download::check_space(&dir, spec.expected_size) {
        return fail(e.to_string());
    }

    // Two different retries, for two different failures.
    //
    // A **checksum mismatch** gets exactly one: `verify_and_finalize` has
    // deleted the `.part` by then, so the retry is a clean full download
    // rather than a resume of the same corrupt bytes. Twice in a row means
    // something is wrong that trying again will not fix.
    //
    // A **transport error** goes to `RetryBudget`, and this is the
    // Wi-Fi-to-cellular handoff story (docs/ANDROID.md Phase 7). The socket
    // dies on handoff no matter what we observe, so rather than watch for
    // network changes with a `ConnectivityManager.NetworkCallback`, we just
    // resume: the `.part` survives, `resume_offset` reads its length, and the
    // next attempt sends `Range: bytes=<len>-`. One mechanism covers the
    // handoff, a tunnel, and a flaky AP. The budget is spent on *stalls*
    // rather than failures, so a download that keeps advancing between drops
    // runs as long as it needs to — see `RetryBudget`, which is where that
    // rule is tested.
    let mut checksum_retried = false;
    let mut budget = download::RetryBudget::new(download::resume_offset(&dir, &spec.file, &spec));

    loop {
        match attempt_download(&client, &dir, &spec, &emit).await {
            Ok(path) => {
                tracing::info!(path = %path.display(), "model downloaded");
                emit(
                    spec.expected_size.unwrap_or(0),
                    spec.expected_size,
                    true,
                    None,
                );
                let _ = app.emit("models://changed", ());
                refresh_embedding_status(&app);
                return;
            }
            Err(download::DownloadError::ChecksumMismatch { expected, actual })
                if !checksum_retried =>
            {
                checksum_retried = true;
                tracing::warn!(
                    model = %spec.file,
                    "checksum mismatch (expected {expected}, got {actual}); retrying once"
                );
            }
            Err(download::DownloadError::Transport(msg)) => {
                let offset = download::resume_offset(&dir, &spec.file, &spec);
                match budget.record_failure(offset) {
                    download::RetryDecision::GiveUp => {
                        return fail(format!(
                            "download kept failing without making progress: {msg}"
                        ));
                    }
                    download::RetryDecision::RetryAfter(backoff) => {
                        tracing::warn!(
                            model = %spec.file,
                            "transport error at byte {offset} ({msg}); resuming in {}s",
                            backoff.as_secs()
                        );
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
            Err(e) => return fail(e.to_string()),
        }
    }
}

/// The Android download foreground service, and nothing at all on desktop.
///
/// Wrapped rather than called directly so `run_download` reads the same on
/// both platforms — the `cfg` lives here, once, instead of at four call sites.
mod foreground {
    #[cfg(target_os = "android")]
    use std::sync::atomic::Ordering;

    /// Whether a session is currently open. Guards `progress` so a stray
    /// progress event (an early failure that reports before the session
    /// starts) can't start a service nothing will ever stop.
    #[cfg(target_os = "android")]
    static ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// Starts the service on construction, stops it on drop — so every exit
    /// path out of `run_download`, including the early failures, tears it
    /// down without anyone having to remember to.
    pub struct Session;

    impl Session {
        #[allow(unused_variables)]
        pub fn start(model: &str) -> Self {
            #[cfg(target_os = "android")]
            {
                // Asked for here, at the first download, rather than at
                // startup: a notification prompt before the user has done
                // anything needing one is the kind everybody dismisses.
                crate::android::download_service::request_notification_permission();
                crate::android::download_service::start_or_update(
                    &format!("Downloading {model}"),
                    0,
                    0,
                );
                ACTIVE.store(true, Ordering::SeqCst);
            }
            Session
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            #[cfg(target_os = "android")]
            {
                ACTIVE.store(false, Ordering::SeqCst);
                crate::android::download_service::stop();
            }
        }
    }

    /// Milliseconds between notification updates.
    ///
    /// `run_download`'s own event throttle is one megabyte, which on a 2 GB
    /// model is ~2000 updates — far more than a notification can usefully
    /// show, and each one is a cross-language round-trip plus an intent. Two
    /// seconds is faster than anyone reads a progress bar and cheap enough to
    /// ignore.
    #[cfg(target_os = "android")]
    const NOTICE_INTERVAL_MS: u64 = 2000;
    #[cfg(target_os = "android")]
    static LAST_NOTICE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[allow(unused_variables)]
    pub fn progress(model: &str, received: u64, total: Option<u64>) {
        #[cfg(target_os = "android")]
        {
            if !ACTIVE.load(Ordering::SeqCst) {
                return;
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let last = LAST_NOTICE_MS.load(Ordering::Relaxed);
            if now.saturating_sub(last) < NOTICE_INTERVAL_MS {
                return;
            }
            LAST_NOTICE_MS.store(now, Ordering::Relaxed);

            // Off the async worker: `run_mobile_plugin` is a synchronous
            // round-trip into the JVM, and this is called from inside the
            // download's own future. Fire-and-forget is fine here — the
            // notification is a display, and at one update every two seconds
            // a reordered pair would be invisible even if it happened.
            let title = format!("Downloading {model}");
            let total = total.unwrap_or(0);
            tauri::async_runtime::spawn_blocking(move || {
                crate::android::download_service::start_or_update(&title, received, total);
            });
        }
    }
}

async fn attempt_download(
    client: &reqwest::Client,
    dir: &std::path::Path,
    spec: &DownloadSpec,
    emit: &(impl Fn(u64, Option<u64>, bool, Option<String>) + Sync),
) -> Result<PathBuf, download::DownloadError> {
    let mut resume_from = download::resume_offset(dir, &spec.file, spec);
    download::write_meta(dir, &spec.file, spec)?;

    let url = spec.url();
    let mut req = client.get(&url);
    if resume_from > 0 {
        req = req.header("Range", format!("bytes={resume_from}-"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| download::DownloadError::Transport(e.to_string()))?;
    match download::plan_resume(&url, resp.status(), resume_from)? {
        download::ResumePlan::Append => {}
        download::ResumePlan::DiscardFragment => {
            // The server ignored our `Range` header and is about to send the
            // full body — appending it after the existing fragment would
            // corrupt the file. Start over from byte 0 instead.
            tracing::warn!(
                model = %spec.file,
                "server answered a ranged request with {}; discarding the {resume_from}-byte fragment and restarting from scratch",
                resp.status()
            );
            let part = download::part_path(dir, &spec.file);
            match std::fs::remove_file(&part) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            resume_from = 0;
        }
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
