//! Local GGUF management commands (docs/ANDROID.md §5.1).
//!
//! Thin by design: everything with logic lives in `crate::models`, which takes
//! paths and streams and no `AppHandle`, so it can be tested without a Tauri
//! runtime. This file resolves state, spawns, and emits.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// A download in progress, tracked in `AppState::downloads`: its latest
/// progress (for a page that opens mid-download) and its cancel switch.
pub struct ActiveDownload {
    pub progress: DownloadProgress,
    pub cancel: Arc<AtomicBool>,
}

/// A model on disk plus its card fields.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LocalModel {
    #[serde(flatten)]
    pub model: InstalledModel,
    pub info: Option<gguf::GgufInfo>,
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
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        crate::lifecycle::memory::apply_memory_plugins(&app).await;
    });
    Ok(())
}

/// Start a download; returns its id immediately. Progress arrives as
/// `models://progress` events keyed by that id, so several can run at once.
///
/// One download per file (#70): asking again for a file already downloading
/// answers with that download's id instead of starting a second copy, which
/// is what used to happen when Settings was left and reopened mid-download.
///
/// `download_id` lets a caller pre-agree an id (the wizard and the pathway
/// embedding model both do, so they can subscribe before starting).
///
/// `token` is an optional HuggingFace access token for a gated repo (the
/// Gemma-licensed EmbeddingGemma). It is used only to authorize this download's
/// HTTP requests and is never persisted, logged, or written to the `.meta`
/// sidecar — see [`download::authorize`]. For EmbeddingGemma it also fetches
/// the Gemma tokenizer, which is gated under the same licence.
#[tauri::command]
pub fn download_model(
    app: AppHandle,
    repo: String,
    file: String,
    rev: Option<String>,
    download_id: Option<String>,
    token: Option<String>,
) -> Result<String, String> {
    let cancel = Arc::new(AtomicBool::new(false));
    let id = {
        let state = app.state::<AppState>();
        let mut downloads = state.downloads.lock().unwrap();
        if let Some((id, _)) = downloads
            .iter()
            .find(|(_, d)| d.progress.model.as_ref() == file)
        {
            return Ok(id.clone());
        }
        let id =
            download_id.unwrap_or_else(|| format!("dl_{}", chrono::Utc::now().timestamp_millis()));
        downloads.insert(
            id.clone(),
            ActiveDownload {
                progress: DownloadProgress {
                    download_id: Arc::from(id.as_str()),
                    model: Arc::from(file.as_str()),
                    received: 0,
                    total: None,
                    done: false,
                    error: None,
                },
                cancel: cancel.clone(),
            },
        );
        id
    };
    let spec = DownloadSpec {
        repo,
        file,
        rev: rev.unwrap_or_else(|| "main".into()),
        sha256: None,
        expected_size: None,
    };
    let id_for_task = id.clone();
    tauri::async_runtime::spawn(async move {
        run_download(app, spec, id_for_task, token, cancel).await;
    });
    Ok(id)
}

/// Every download in progress, as its latest progress - for a page that
/// opens (or reopens) while one is running.
#[tauri::command]
pub fn list_downloads(state: tauri::State<'_, AppState>) -> Result<Vec<DownloadProgress>, String> {
    let downloads = state.downloads.lock().unwrap();
    Ok(downloads.values().map(|d| d.progress.clone()).collect())
}

/// Stop a download. Its fragment stays, so starting it again resumes; it
/// can be deleted from [`list_partial_downloads`].
#[tauri::command]
pub fn cancel_download(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    let downloads = state.downloads.lock().unwrap();
    let d = downloads
        .get(&id)
        .ok_or("That download is no longer running.")?;
    d.cancel.store(true, Ordering::SeqCst);
    Ok(())
}

/// Unfinished downloads on disk (#70).
#[tauri::command]
pub fn list_partial_downloads(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<download::PartialDownload>, String> {
    let dir = crate::config::models_dir().map_err(|e| e.to_string())?;
    let running: Vec<String> = state
        .downloads
        .lock()
        .unwrap()
        .values()
        .map(|d| d.progress.model.to_string())
        .collect();
    Ok(download::partial_downloads(&dir)
        .into_iter()
        .filter(|p| !running.contains(&p.file))
        .collect())
}

#[tauri::command]
pub fn delete_partial_download(file: String) -> Result<(), String> {
    let dir = crate::config::models_dir().map_err(|e| e.to_string())?;
    download::delete_partial(&dir, &file).map_err(|e| format!("could not delete it: {e}"))
}

/// One download, start to finish, reporting everything through
/// `models://progress`. For EmbeddingGemma the Gemma tokenizer follows, under
/// the same id, and the download reports done only when both are in place.
///
/// Returns nothing: a download is fire-and-forget from the caller's point of
/// view, and every outcome — including every failure — is an event, so a UI
/// that subscribed before starting can't miss one.
async fn run_download(
    app: AppHandle,
    spec: DownloadSpec,
    id: String,
    token: Option<String>,
    cancel: Arc<AtomicBool>,
) {
    let token = token.as_deref();
    let id_arc: Arc<str> = Arc::from(id.as_str());
    let emit =
        |model: &Arc<str>, received: u64, total: Option<u64>, done: bool, error: Option<String>| {
            // Android only: the same numbers also drive the foreground-service
            // notification, which is what keeps the process (and its network)
            // alive once the user switches away. Free on desktop.
            foreground::progress(model, received, total);
            let progress = DownloadProgress {
                download_id: id_arc.clone(),
                model: model.clone(),
                received,
                total,
                done,
                error,
            };
            if let Some(d) = app
                .state::<AppState>()
                .downloads
                .lock()
                .unwrap()
                .get_mut(&id)
            {
                d.progress = progress.clone();
            }
            let _ = app.emit("models://progress", progress);
        };
    let finish = |model: &Arc<str>, error: Option<String>, total: Option<u64>| {
        if let Some(e) = &error {
            tracing::warn!(model = %model, "model download failed: {e}");
        }
        app.state::<AppState>()
            .downloads
            .lock()
            .unwrap()
            .remove(&id);
        emit(model, total.unwrap_or(0), total, true, error);
    };
    let model: Arc<str> = Arc::from(spec.file.as_str());

    let dir = match crate::config::models_dir() {
        Ok(d) => d,
        Err(e) => return finish(&model, Some(e.to_string()), None),
    };
    let wants_tokenizer = crate::models::needs_tokenizer(&spec.file);
    let model_present = dir.join(&spec.file).exists();
    if model_present && !(wants_tokenizer && crate::models::tokenizer_path().is_none()) {
        return finish(
            &model,
            Some(download::DownloadError::AlreadyInstalled(spec.file.clone()).to_string()),
            None,
        );
    }

    // Held for the rest of this function; its `Drop` stops the service, so
    // every exit path below — including the early returns — tears it down
    // without needing to remember to.
    let _foreground = foreground::Session::start(&model);
    let client = crate::util::http_client();

    let mut last_total = None;
    if !model_present {
        match fetch(&client, &dir, spec.clone(), token, &cancel, &|r, t| {
            emit(&model, r, t, false, None)
        })
        .await
        {
            Ok(total) => last_total = total,
            Err(e) => return finish(&model, Some(e), None),
        }
        let _ = app.emit("models://changed", ());
    }
    if wants_tokenizer && crate::models::tokenizer_path().is_none() {
        let tokenizer: Arc<str> = Arc::from(crate::models::TOKENIZER_FILE);
        let spec = DownloadSpec {
            repo: crate::models::TOKENIZER_REPO.to_string(),
            file: crate::models::TOKENIZER_FILE.to_string(),
            rev: "main".to_string(),
            sha256: None,
            expected_size: None,
        };
        if let Err(e) = fetch(&client, &dir, spec, token, &cancel, &|r, t| {
            emit(&tokenizer, r, t, false, None)
        })
        .await
        {
            return finish(
                &model,
                Some(format!(
                    "The model downloaded, but its tokenizer did not ({e}). Memory needs both; \\
                     accept the licence at huggingface.co/{} too, then download again.",
                    crate::models::TOKENIZER_REPO
                )),
                None,
            );
        }
    }

    finish(&model, None, last_total);
    let _ = app.emit("models://changed", ());
    refresh_embedding_status(&app);
    crate::lifecycle::memory::apply_memory_plugins(&app).await;
    summarizer_model_arrived(&app, &spec.file);
}

/// A newly downloaded summarizer model only takes effect when the engine
/// starts, so schedule that - if the local summarizer is what the user chose.
fn summarizer_model_arrived(app: &AppHandle, file: &str) {
    let wanted = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.summarizer.enabled
            && crate::models::resolve(&cfg.summarizer.model)
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .is_some_and(|n| n.eq_ignore_ascii_case(file))
    };
    if wanted {
        crate::lifecycle::engine_restart::schedule(app);
    }
}

/// Download one file into `dir`, resuming and retrying as needed. Returns
/// its size when known.
async fn fetch(
    client: &reqwest::Client,
    dir: &std::path::Path,
    mut spec: DownloadSpec,
    token: Option<&str>,
    cancel: &AtomicBool,
    emit: &(dyn Fn(u64, Option<u64>) + Sync),
) -> Result<Option<u64>, String> {
    let (size, sha) = download::head_metadata(client, &spec, token).await;
    spec.expected_size = size;
    spec.sha256 = sha;
    download::check_space(dir, spec.expected_size).map_err(|e| e.to_string())?;

    // Two different retries, for two different failures.
    //
    // A **checksum mismatch** gets exactly one: `verify_and_finalize` has
    // deleted the `.part` by then, so the retry is a clean full download
    // rather than a resume of the same corrupt bytes. Twice in a row means
    // something is wrong that trying again will not fix.
    //
    // A **transport error** goes to `RetryBudget`, and this is the
    // Wi-Fi-to-cellular handoff story (docs/ANDROID.md Phase 7): the `.part`
    // survives, `resume_offset` reads its length, and the next attempt sends
    // `Range: bytes=<len>-`. The budget is spent on *stalls* rather than
    // failures, so a download that keeps advancing between drops runs as long
    // as it needs to — see `RetryBudget`.
    //
    // Anything else - a licence refusal, a cancel, a full disk - is final.
    let mut checksum_retried = false;
    let mut budget = download::RetryBudget::new(download::resume_offset(dir, &spec.file, &spec));
    loop {
        match attempt_download(client, dir, &spec, token, emit, cancel).await {
            Ok(path) => {
                tracing::info!(path = %path.display(), "model downloaded");
                return Ok(spec.expected_size);
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
                let offset = download::resume_offset(dir, &spec.file, &spec);
                match budget.record_failure(offset) {
                    download::RetryDecision::GiveUp => {
                        return Err(format!(
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
            Err(e) => return Err(e.to_string()),
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
    token: Option<&str>,
    emit: &(dyn Fn(u64, Option<u64>) + Sync),
    cancel: &AtomicBool,
) -> Result<PathBuf, download::DownloadError> {
    let mut resume_from = download::resume_offset(dir, &spec.file, spec);
    download::write_meta(dir, &spec.file, spec)?;

    let url = spec.url();
    let mut req = download::authorize(client.get(&url), token);
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
    emit(resume_from, total);

    let part = download::part_path(dir, &spec.file);
    let stream = resp.bytes_stream();
    futures_util::pin_mut!(stream);
    // Throttle: a multi-GB download produces tens of thousands of chunks, and
    // an event per chunk would flood the webview for no visible benefit.
    let mut last_emit = 0u64;
    download::append_stream(
        &part,
        resume_from,
        stream,
        &mut |received| {
            if received - last_emit >= 1_000_000 {
                last_emit = received;
                emit(received, total);
            }
        },
        cancel,
    )
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

/// Which summarizer compaction uses, for Settings: the local model when that
/// was chosen and is on disk, otherwise the chat's own provider.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SummarizerStatus {
    /// `local` or `provider`: what the user chose.
    pub source: &'static str,
    pub model: String,
    pub model_installed: bool,
    /// What compaction actually uses: `local` only when chosen and present.
    pub effective: &'static str,
}

#[tauri::command]
pub fn get_summarizer_status(
    state: tauri::State<'_, AppState>,
) -> Result<SummarizerStatus, String> {
    let cfg = state.config.lock().unwrap();
    let installed = crate::models::resolve(&cfg.summarizer.model).is_some();
    let source = if cfg.summarizer.enabled {
        "local"
    } else {
        "provider"
    };
    Ok(SummarizerStatus {
        source,
        model: cfg.summarizer.model.clone(),
        model_installed: installed,
        effective: if cfg.summarizer.enabled && installed {
            "local"
        } else {
            "provider"
        },
    })
}
