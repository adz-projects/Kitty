//! Secret storage for provider profiles — never plaintext on disk
//! (CLAUDE.md rule 4).
//!
//! Two backends behind one API:
//!
//! - **Windows**: `keyring`, i.e. Credential Manager.
//! - **Android**: `crate::android::secrets`, i.e. AES-256-GCM under an
//!   AndroidKeyStore key. **Not** `keyring` — that crate has no Android
//!   backend at all (apple-native / linux-native / windows-native and nothing
//!   else), so on this target it falls through to a catch-all in-memory mock.
//!   It compiles, it appears to work, and every secret is gone on the next
//!   launch. That was D24, and it is why every entry point in this file
//!   dispatches instead of calling `keyring` directly.
//!
//! The dispatch is per-function rather than per-module so the two platforms
//! cannot drift in *behaviour*: the absent-vs-failed distinction that
//! `classify_read_result` protects has to hold on both, and keeping the
//! shared logic here is what makes that checkable.

#[cfg(target_os = "android")]
use crate::android::secrets as android_secrets;

#[cfg(not(target_os = "android"))]
const KEYRING_SERVICE: &str = "kitty";

/// Pre-rename service name (the app used to be "goose-overlay") — read-only,
/// only ever consulted by `migrate_secrets`.
#[cfg(not(target_os = "android"))]
const OLD_KEYRING_SERVICE: &str = "goose-overlay";

#[cfg(not(target_os = "android"))]
fn entry(id: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, id)
}

pub fn set_secret(id: &str, secret: &str) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        android_secrets::set(id, secret)
    }
    #[cfg(not(target_os = "android"))]
    {
        entry(id)
            .and_then(|e| e.set_password(secret))
            .map_err(|e| format!("could not store secret: {e}"))
    }
}

/// Same as [`set_secret`], but off the async runtime's worker thread — the
/// write half of [`get_secret_async`]'s rationale: Windows Credential
/// Manager access is synchronous OS IPC, and calling [`set_secret`] directly
/// from an async context blocks that tokio worker for however long the OS
/// call takes.
pub async fn set_secret_async(id: &str, secret: &str) -> Result<(), String> {
    let id = id.to_string();
    let secret = secret.to_string();
    tokio::task::spawn_blocking(move || set_secret(&id, &secret))
        .await
        .map_err(|e| format!("keyring write task panicked: {e}"))?
}

pub fn get_secret(id: &str) -> Option<String> {
    #[cfg(target_os = "android")]
    {
        android_secrets::get(id).ok().flatten()
    }
    #[cfg(not(target_os = "android"))]
    {
        entry(id).ok().and_then(|e| e.get_password().ok())
    }
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
        #[cfg(target_os = "android")]
        {
            // Already `Result<Option<_>, String>` with the same contract —
            // the Kotlin side resolves `{found: false}` for absent and
            // rejects for unreadable, so there is nothing to classify.
            android_secrets::get(&owned_id)
        }
        #[cfg(not(target_os = "android"))]
        {
            let entry = entry(&owned_id).map_err(|e| format!("keyring entry error: {e}"))?;
            classify_read_result(entry.get_password())
        }
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
#[cfg(not(target_os = "android"))]
fn classify_read_result(result: keyring::Result<String>) -> Result<Option<String>, String> {
    match result {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("keyring read error: {e}")),
    }
}

pub fn delete_secret(id: &str) {
    #[cfg(target_os = "android")]
    {
        android_secrets::delete(id);
    }
    #[cfg(not(target_os = "android"))]
    {
        if let Ok(e) = entry(id) {
            let _ = e.delete_credential();
        }
    }
}

pub fn has_secret(id: &str) -> bool {
    get_secret(id).is_some()
}

/// Account name for BigTiny's at-rest encryption key — a single, fixed
/// entry distinct from any per-provider secret (those use the profile id as
/// the account name) and distinct from `BIGTINY_SECRET` (that one isn't in
/// keyring at all — it's regenerated fresh in memory every launch, see
/// `lifecycle/bigtiny_proc.rs::generate_secret`). This key must be stable
/// across restarts (unlike `BIGTINY_SECRET`) or previously-encrypted rows
/// in BigTiny's DB would become undecryptable.
const BIGTINY_ENCRYPTION_KEY_ACCOUNT: &str = "bigtiny-encryption-key";

/// Serializes first-time key generation within this process — see
/// [`get_or_create_bigtiny_encryption_key`]. A `std::sync::Mutex` (not
/// tokio's): the guarded section is deliberately synchronous (the whole
/// function is the blocking Credential Manager call), so it must be usable
/// from plain blocking contexts too.
static ENCRYPTION_KEYGEN: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

/// Return the existing at-rest encryption key for BigTiny's SQLite DB
/// (provider API keys, MCP server auth headers), generating and storing a
/// fresh random one on first call. Blocking (real Windows Credential
/// Manager I/O) — call via `spawn_blocking` from async contexts, same
/// rationale as `get_secret_async`.
pub fn get_or_create_bigtiny_encryption_key() -> Result<String, String> {
    // First-time generation is check-then-act: without a process-wide guard,
    // two concurrent first runs (e.g. the daemon boot racing a UI call) both
    // read "no key", generate *different* keys, and the last writer wins —
    // rows the loser encrypted are undecryptable on the next launch.
    let _guard = ENCRYPTION_KEYGEN
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap();
    // Re-read under the lock: a concurrent creator may have won while we
    // were waiting for it.
    if let Some(existing) = get_secret(BIGTINY_ENCRYPTION_KEY_ACCOUNT) {
        return Ok(existing);
    }
    let mut key_bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key_bytes);
    let key_hex: String = key_bytes.iter().map(|b| format!("{b:02x}")).collect();
    set_secret(BIGTINY_ENCRYPTION_KEY_ACCOUNT, &key_hex)?;
    Ok(key_hex)
}

/// One-time migration off the pre-rename `goose-overlay` keyring service: for
/// each given provider id, if no secret exists yet under the current `kitty`
/// service but one exists under the old service, copy it over and remove the
/// old entry. Idempotent (a no-op once a profile has migrated) and safe to
/// call on every launch — best-effort throughout, since a failure here should
/// never block startup.
#[cfg(target_os = "android")]
pub fn migrate_secrets(_provider_ids: &[String]) {
    // Nothing to migrate: the `goose-overlay` service name predates Android
    // support entirely, so no Android install has ever written under it.
}

#[cfg(not(target_os = "android"))]
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

// Desktop-only: every test below either round-trips Credential Manager or
// exercises `classify_read_result`, which is itself the non-Android arm.
// Android's equivalent contract is enforced on the Kotlin side of the
// boundary (`SecretStore.get` returns null vs. throws) and verified on device.
#[cfg(all(test, not(target_os = "android")))]
mod tests {
    use super::*;

    /// The one real (not pure-function) Credential Manager round-trip test
    /// in this module — deliberately exercises the actual OS store rather
    /// than a pure function, since this is new functionality whose real
    /// integration with Credential Manager hasn't been verified any other
    /// way. Cleans up after itself either way.
    #[test]
    fn get_or_create_bigtiny_encryption_key_is_idempotent_against_real_credential_manager() {
        delete_secret(BIGTINY_ENCRYPTION_KEY_ACCOUNT); // clean slate
        let first = get_or_create_bigtiny_encryption_key();
        let second = get_or_create_bigtiny_encryption_key();
        delete_secret(BIGTINY_ENCRYPTION_KEY_ACCOUNT); // clean up regardless of outcome

        let first = first.expect("first call should succeed");
        let second = second.expect("second call should succeed");
        assert_eq!(first, second, "the key must be stable across calls");
        assert_eq!(first.len(), 64, "32 bytes hex-encoded");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

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
