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
/// Registration is gated on the handshake's `registration_token`, which only
/// a process that can read the daemon's data directory has. The same token
/// authorizes the two recoveries:
///
/// * **The key is gone** (the credential store was reset, or Kitty was
///   reinstalled): registering answers `409` because `kitty` exists, so the
///   identity is *reclaimed* instead - a fresh key for the same app id, with
///   every chat and setting it owns kept.
/// * **The key is no longer accepted** (the daemon's data was reset, or the
///   key was revoked): the stored key is checked before use, and a rejected
///   one goes through the same register-or-reclaim path.
pub async fn ensure_app_key(base_url: &str, registration_token: &str) -> Result<String, String> {
    // A *checked* read: a transient store failure must not be mistaken for
    // "never registered". Collapsing the two would send us to register again
    // with a key already on file, turning a momentary keystore hiccup into a
    // pointless reclaim.
    let stored = bounded(read_stored_key(), "read").await?;
    let key = obtain_key(base_url, registration_token, stored.clone()).await?;
    if stored.as_deref() != Some(key.as_str()) {
        bounded(store_key(&key), "write").await?;
    }
    Ok(key)
}

/// The key-resolution policy, without the credential store: `stored` if the
/// daemon still accepts it, otherwise a newly registered or reclaimed one.
async fn obtain_key(
    base_url: &str,
    registration_token: &str,
    stored: Option<String>,
) -> Result<String, String> {
    if let Some(existing) = stored {
        match key_is_accepted(base_url, &existing).await {
            // Could not tell (a slow daemon, a network blip): keep the key
            // rather than churn the identity over a transient failure.
            Some(true) | None => return Ok(existing),
            Some(false) => {
                tracing::warn!("the engine no longer accepts Kitty's stored key; recovering it")
            }
        }
    }

    use bigtiny2_client::{BigTinyClient, ClientError};
    let issued = match BigTinyClient::register(base_url, registration_token, APP_ID, DISPLAY_NAME)
        .await
    {
        Ok(issued) => issued,
        Err(ClientError::AlreadyRegistered(_)) => {
            tracing::info!("Kitty is registered but its key is lost; reclaiming it");
            BigTinyClient::reclaim(base_url, registration_token, APP_ID)
                    .await
                    .map_err(|e| match e {
                        ClientError::AppInUse(_) => "Another copy of Kitty is using this                              engine right now. Close it, or wait two minutes and restart Kitty."
                            .to_string(),
                        other => format!("could not recover Kitty's engine identity: {other}"),
                    })?
        }
        Err(e) => return Err(format!("could not register Kitty with the engine: {e}")),
    };
    Ok(issued.api_key)
}

/// Whether the daemon accepts `key`: `Some(false)` only on an explicit
/// rejection, `None` when the answer was anything else.
async fn key_is_accepted(base_url: &str, key: &str) -> Option<bool> {
    let resp = crate::util::http_client()
        .get(format!("{base_url}/api/apps/me"))
        .header("X-API-Key", key)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    match resp.status() {
        s if s.is_success() => Some(true),
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => Some(false),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_accepted_stored_key_is_kept() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/apps/me")
            .match_header("X-API-Key", "old")
            .with_body(r#"{"id":"kitty"}"#)
            .create_async()
            .await;
        let key = obtain_key(&server.url(), "tok", Some("old".into())).await;
        assert_eq!(key.as_deref(), Ok("old"));
    }

    /// The lost-key dead end this used to be: registration says "already
    /// registered", and the identity is reclaimed instead of failing forever.
    #[tokio::test]
    async fn a_lost_key_is_reclaimed() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/apps/register")
            .with_status(409)
            .create_async()
            .await;
        server
            .mock("POST", "/api/apps/reclaim")
            .match_header("X-Registration-Token", "tok")
            .with_body(r#"{"app_id":"kitty","api_key":"fresh"}"#)
            .create_async()
            .await;
        let key = obtain_key(&server.url(), "tok", None).await;
        assert_eq!(key.as_deref(), Ok("fresh"));
    }

    /// A stored key the daemon rejects goes through the same recovery.
    #[tokio::test]
    async fn a_rejected_stored_key_is_replaced() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/apps/me")
            .with_status(401)
            .create_async()
            .await;
        server
            .mock("POST", "/api/apps/register")
            .with_body(r#"{"app_id":"kitty","api_key":"new"}"#)
            .create_async()
            .await;
        let key = obtain_key(&server.url(), "tok", Some("stale".into())).await;
        assert_eq!(key.as_deref(), Ok("new"));
    }

    #[tokio::test]
    async fn a_reclaim_refused_while_in_use_says_what_to_do() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/apps/register")
            .with_status(409)
            .create_async()
            .await;
        server
            .mock("POST", "/api/apps/reclaim")
            .with_status(409)
            .create_async()
            .await;
        let err = obtain_key(&server.url(), "tok", None).await.unwrap_err();
        assert!(err.contains("Another copy of Kitty"), "{err}");
    }
}
