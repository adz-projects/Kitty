//! Secret storage for provider profiles — Windows Credential Manager via
//! `keyring`, never plaintext on disk (CLAUDE.md rule 4).

const KEYRING_SERVICE: &str = "kitty";

/// Pre-rename service name (the app used to be "goose-overlay") — read-only,
/// only ever consulted by `migrate_secrets`.
const OLD_KEYRING_SERVICE: &str = "goose-overlay";

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

/// Same underlying read as [`get_secret_async`], but distinguishes "no entry
/// stored" (`Ok(None)`) from "the read itself failed" (`Err`) — a transiently
/// unavailable Windows Credential Manager looks identical to "never
/// configured" if collapsed through `.ok()`, which is exactly what let a
/// flaky read silently disable `brave-mcp-search` (see
/// `bigtiny::mcp::ensure_builtin_servers`). Callers that must not treat a
/// read failure as "delete the secret" should use this instead of
/// [`get_secret_async`].
pub async fn get_secret_checked(id: &str) -> Result<Option<String>, String> {
    let owned_id = id.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let entry = entry(&owned_id).map_err(|e| format!("keyring entry error: {e}"))?;
        classify_read_result(entry.get_password())
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(join_err) => Err(format!("keyring read task panicked: {join_err}")),
    }
}

/// The distinction [`get_secret_checked`] exists for, pulled out as a pure
/// function so it's unit-testable without a real OS credential store: a
/// confirmed-absent entry is `Ok(None)` (safe to treat as "not configured"),
/// while every other error is `Err` (must NOT be treated as "not
/// configured" — see the doc comment on [`get_secret_checked`]).
fn classify_read_result(result: keyring::Result<String>) -> Result<Option<String>, String> {
    match result {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("keyring read error: {e}")),
    }
}

pub fn delete_secret(id: &str) {
    if let Ok(e) = entry(id) {
        let _ = e.delete_credential();
    }
}

pub fn has_secret(id: &str) -> bool {
    get_secret(id).is_some()
}

/// One-time migration off the pre-rename `goose-overlay` keyring service: for
/// each given provider id, if no secret exists yet under the current `kitty`
/// service but one exists under the old service, copy it over and remove the
/// old entry. Idempotent (a no-op once a profile has migrated) and safe to
/// call on every launch — best-effort throughout, since a failure here should
/// never block startup.
pub fn migrate_secrets(provider_ids: &[String]) {
    for id in provider_ids {
        if has_secret(id) {
            continue;
        }
        let Ok(old_entry) = keyring::Entry::new(OLD_KEYRING_SERVICE, id) else {
            continue;
        };
        let Ok(secret) = old_entry.get_password() else {
            continue;
        };
        if set_secret(id, &secret).is_ok() {
            let _ = old_entry.delete_credential();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_confirmed_absent_entry_classifies_as_ok_none() {
        assert_eq!(classify_read_result(Err(keyring::Error::NoEntry)), Ok(None));
    }

    #[test]
    fn a_successful_read_classifies_as_ok_some() {
        assert_eq!(
            classify_read_result(Ok("s3cr3t".to_string())),
            Ok(Some("s3cr3t".to_string()))
        );
    }

    /// The regression this addendum fixes: a transient platform failure
    /// (locked/contended Credential Manager) must classify as `Err`, never
    /// as `Ok(None)` — collapsing it to "no secret" is what let a flaky read
    /// silently disable `brave-mcp-search`.
    #[test]
    fn a_platform_failure_classifies_as_err_not_ok_none() {
        let err = keyring::Error::PlatformFailure(Box::new(std::io::Error::other(
            "credential manager busy",
        )));
        let result = classify_read_result(Err(err));
        assert!(result.is_err(), "expected Err, got {result:?}");
    }

    /// Same for the storage-locked case specifically named in the platform
    /// docs (e.g. the store is locked) — also not "no secret configured".
    #[test]
    fn no_storage_access_classifies_as_err_not_ok_none() {
        let err = keyring::Error::NoStorageAccess(Box::new(std::io::Error::other("locked")));
        let result = classify_read_result(Err(err));
        assert!(result.is_err(), "expected Err, got {result:?}");
    }
}
