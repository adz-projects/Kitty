//! Error/warning log commands — thin wrappers over `log_capture`'s
//! module-level ring buffer (see that module's doc comment for why it isn't
//! an `AppState` field). Powers Settings → Advanced's error log viewer.

use crate::log_capture::{self, LogEntry};

#[tauri::command]
pub fn list_log_entries() -> Vec<LogEntry> {
    log_capture::entries()
}

#[tauri::command]
pub fn clear_log_entries() {
    log_capture::clear();
}

/// The captured entries as plain text, one per line, oldest first — what
/// "Copy" and "Save…" in the error log produce (#74).
pub fn render(entries: &[LogEntry]) -> String {
    entries
        .iter()
        .map(|e| format!("{} {} {}: {}", e.timestamp, e.level, e.target, e.message))
        .collect::<Vec<_>>()
        .join(
            "
",
        )
}

#[tauri::command]
pub fn log_text() -> String {
    render(&log_capture::entries())
}

/// Write the error log to `path` (chosen in a save dialog).
#[tauri::command]
pub async fn save_log_file(path: String) -> Result<(), String> {
    let text = render(&log_capture::entries());
    tokio::task::spawn_blocking(move || std::fs::write(&path, text))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("Could not save the log: {e}"))
}
