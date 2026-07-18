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
