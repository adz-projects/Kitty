//! Shares and notification taps from outside the app (Android, A2–A4). See
//! `android::intents`.

use serde::Serialize;

/// Mirrors `android::intents::IncomingBatch`, on every platform so the
/// webview calls one command; desktop never has anything queued.
#[derive(Debug, Default, Serialize)]
pub struct IncomingView {
    pub intents: Vec<serde_json::Value>,
    pub copying: bool,
}

/// Collect what Android has handed the app since last asked: shared text and
/// files, and tapped notifications. Async: the Android side blocks on the
/// main looper, which a synchronous command would run on.
#[tauri::command]
pub async fn take_incoming() -> Result<IncomingView, String> {
    #[cfg(target_os = "android")]
    {
        let batch = tokio::task::spawn_blocking(crate::android::intents::take)
            .await
            .map_err(|e| e.to_string())??;
        Ok(IncomingView {
            intents: batch
                .intents
                .into_iter()
                .filter_map(|i| serde_json::to_value(i).ok())
                .collect(),
            copying: batch.copying,
        })
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(IncomingView::default())
    }
}
