//! Kitty's durable BigTiny V2 identity, shared by both hosts.
//!
//! V2 replaced V1's single shared secret with per-app registration: an app
//! presents the launch-scoped `registration_token` once to
//! `POST /api/apps/register` and gets back a key that outlives both processes
//! (it lives in the daemon's `apps.key_hash`). Every later request carries that
//! key as `X-API-Key`, and the daemon resolves it to an identity so it can
//! scope what the caller sees.
//!
//! Both hosts need exactly this, for different reasons: desktop attaches to a
//! daemon it does not own, and Android hosts the daemon in-process but still
//! talks to it across the loopback HTTP boundary, where auth is unconditional
//! (`server::middleware::auth_middleware` — every `/api/*` route but
//! `/api/health` requires a key). Registering twice from two copies of this
//! logic would be two chances to get the 409-on-lost-key path wrong, so it
//! lives here once.

/// The app id Kitty registers under. Stable: it keys every row Kitty owns in
/// the daemon's database, and the Phase 7a import stamps this exact value.
pub const APP_ID: &str = "kitty";
const DISPLAY_NAME: &str = "Kitty";

/// Where the issued app key is kept between launches.
///
/// A bearer credential for everything Kitty owns in a daemon other
/// applications can also talk to, so it goes in the same store as every other
/// secret this app holds rather than a config file.
const KEY_CREDENTIAL: &str = "bigtiny-v2-app-key";

/// Resolve Kitty's durable app key, registering once if this is a first run.
///
/// Registration is gated on the handshake's `registration_token`. A `409` means
/// some previous run already registered and we have lost the key — recoverable
/// only by revoking the app, so it is reported rather than papered over.
pub async fn ensure_app_key(base_url: &str, registration_token: &str) -> Result<String, String> {
    // A *checked* read: a transient store failure must not be mistaken for
    // "never registered". Collapsing the two would send us to register again
    // with a key already on file, turning a momentary keystore hiccup into the
    // 409 dead end below — which reads like data loss and is not.
    if let Some(existing) = bounded(read_stored_key(), "read").await? {
        return Ok(existing);
    }

    let issued = bigtiny2_client::BigTinyClient::register(
        base_url,
        registration_token,
        APP_ID,
        DISPLAY_NAME,
    )
    .await
    .map_err(|e| {
        format!(
            "could not register Kitty with BigTiny: {e}. If Kitty was registered by an \
             earlier install whose key is gone, revoke the app with \
             `DELETE /api/apps/kitty` and restart."
        )
    })?;

    bounded(store_key(&issued.api_key), "write").await?;
    Ok(issued.api_key)
}

/// How long a single credential-store round trip may take before we give up.
///
/// This runs during app startup, which on Android is exactly when a
/// hardware-backed keystore is most likely to block: the TEE can stall on first
/// use while it initializes. That is not hypothetical — it is why the V1
/// in-process host bounded its own keystore call, and the bound is kept here now
/// that this module owns the interaction. Ten seconds is far longer than a
/// working store takes and far shorter than a user's patience. Desktop's
/// Credential Manager gets the same treatment for the same reason.
const STORE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Put a wall clock on a credential-store call so a store that never answers
/// fails the stack cleanly instead of hanging startup with no upper bound.
async fn bounded<T>(
    fut: impl std::future::Future<Output = Result<T, String>>,
    what: &str,
) -> Result<T, String> {
    tokio::time::timeout(STORE_TIMEOUT, fut)
        .await
        .map_err(|_| {
            format!(
                "the credential store did not answer within {}s during the app-key {what};                  BigTiny cannot be reached without it",
                STORE_TIMEOUT.as_secs()
            )
        })?
}

// Windows and Android both go through the app's one secret store
// (`config::providers::keyring`), which already dispatches to the Credential
// Manager and the AndroidKeyStore respectively under a single service name.
// Reusing it rather than hand-rolling a third backend is also what keeps the
// key out of the `keyring` crate's Android mock (D24).
#[cfg(any(windows, target_os = "android"))]
async fn read_stored_key() -> Result<Option<String>, String> {
    crate::config::providers::get_secret_checked(KEY_CREDENTIAL).await
}

#[cfg(any(windows, target_os = "android"))]
async fn store_key(key: &str) -> Result<(), String> {
    crate::config::providers::set_secret_async(KEY_CREDENTIAL, key).await
}

/// Everything else keeps the key in the BigTiny data dir. Not as good as a
/// keychain, but the alternative is re-registering every launch, and the file
/// sits beside `encryption.key`, which is no less sensitive.
#[cfg(not(any(windows, target_os = "android")))]
fn key_file() -> Option<std::path::PathBuf> {
    crate::config::bigtiny_data_dir()
        .ok()
        .map(|d| d.join("app-key"))
}

#[cfg(not(any(windows, target_os = "android")))]
async fn read_stored_key() -> Result<Option<String>, String> {
    let Some(path) = key_file() else {
        return Ok(None);
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s.trim().to_string()).filter(|k| !k.is_empty())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(not(any(windows, target_os = "android")))]
async fn store_key(key: &str) -> Result<(), String> {
    let path = key_file().ok_or("no data directory for the app key")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&path, key).map_err(|e| format!("{}: {e}", path.display()))
}
