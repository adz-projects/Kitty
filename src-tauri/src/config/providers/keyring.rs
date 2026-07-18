//! Secret storage for provider profiles — Windows Credential Manager via
//! `keyring`, never plaintext on disk (CLAUDE.md rule 4).

const KEYRING_SERVICE: &str = "goose-overlay";

fn entry(id: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, id)
}

pub fn set_secret(id: &str, secret: &str) -> Result<(), String> {
    entry(id)
        .and_then(|e| e.set_password(secret))
        .map_err(|e| format!("could not store secret: {e}"))
}

pub fn get_secret(id: &str) -> Option<String> {
    entry(id).ok().and_then(|e| e.get_password().ok())
}

/// Same as [`get_secret`], but off the async runtime's worker thread — use
/// this from `async fn`/`#[tauri::command] async fn` bodies. Windows
/// Credential Manager access is synchronous OS IPC; calling `get_secret`
/// directly from an async context blocks that tokio worker (and everything
/// else scheduled on it) for however long the OS call takes.
pub async fn get_secret_async(id: &str) -> Option<String> {
    let id = id.to_string();
    tokio::task::spawn_blocking(move || get_secret(&id))
        .await
        .ok()
        .flatten()
}

pub fn delete_secret(id: &str) {
    if let Ok(e) = entry(id) {
        let _ = e.delete_credential();
    }
}

pub fn has_secret(id: &str) -> bool {
    get_secret(id).is_some()
}
