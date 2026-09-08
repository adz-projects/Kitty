//! Phase 7b: locate a BigTiny V2 daemon instead of owning one.
//!
//! # What changes, and why it is not just a rename
//!
//! V1's model was "Kitty owns the daemon": kill whatever the pidfile names,
//! bind a fresh port, mint a fresh secret, spawn, and kill it again on exit.
//! Every one of those steps is wrong once a second frontend can be attached to
//! the same daemon — the kill takes down someone else's in-flight turn, the
//! per-launch secret means identity cannot outlive a restart, and the ephemeral
//! port means there is nothing for anyone else to find.
//!
//! V2 inverts it. The daemon is a shared machine resource that Kitty *uses*:
//!
//! * **Attach first, spawn only if nobody else has.** `attach_or_spawn` reads
//!   the handshake, proves the process behind it is really a V2 daemon, and
//!   only spawns under an exclusive lock when it is genuinely absent.
//! * **A durable identity.** Kitty registers once as the app `kitty` and keeps
//!   the issued key in the Windows Credential Manager, beside the encryption
//!   key it already keeps there. Restarting Kitty does not re-register, and the
//!   daemon restarting does not invalidate the key — it lives in `apps.key_hash`.
//! * **Never kill.** Nothing here terminates a daemon. Lifetime is the
//!   daemon's own business via its idle-exit timer, which will not fire while
//!   any app is mid-turn.
//!
//! # The consequence worth stating plainly
//!
//! **The first app to spawn decides the daemon's configuration.** The summarizer,
//! token-management, memory, and local-model settings below are passed as spawn
//! environment, so they apply only when Kitty is the process that started the
//! daemon. Attaching to one someone else started inherits their settings. This
//! is inherent to sharing a daemon rather than an oversight: per-app settings
//! that genuinely need to differ belong in the `app_plugins` table, which is
//! per-app by construction.

use std::path::PathBuf;

use bigtiny2_client::discovery::{attach_or_spawn, DiscoveryConfig};
use bigtiny2_client::BigTinyClient;

use crate::state::{DaemonHandle, ManagedProcess};

/// The app id Kitty registers under. Stable: it keys every row Kitty owns in
/// the daemon's database, and the Phase 7a import stamps this exact value.
pub const APP_ID: &str = "kitty";
const DISPLAY_NAME: &str = "Kitty";

/// Where the issued app key is kept between launches.
///
/// The Credential Manager rather than a config file: it is a bearer credential
/// for everything Kitty owns in a daemon other applications can also talk to.
///
/// `KEYRING_SERVICE` matches `config::providers::keyring`'s exactly — every
/// secret this app stores lives under one service name, so they can be
/// enumerated and cleared together rather than leaving an orphan namespace
/// behind that nothing knows to look in.
const KEYRING_SERVICE: &str = "kitty";
const KEY_CREDENTIAL: &str = "bigtiny-v2-app-key";

/// Resolve Kitty's durable app key, registering once if this is a first run.
///
/// Registration is gated on the handshake's `registration_token`, which is why
/// this needs the whole `Located` rather than just a base URL. A `409` means
/// some previous run already registered and we have lost the key — recoverable
/// only by revoking the app, so it is reported rather than papered over.
async fn ensure_app_key(base_url: &str, registration_token: &str) -> Result<String, String> {
    if let Some(existing) = tokio::task::spawn_blocking(|| read_stored_key())
        .await
        .map_err(|e| format!("key lookup task panicked: {e}"))?
    {
        return Ok(existing);
    }

    let issued = BigTinyClient::register(base_url, registration_token, APP_ID, DISPLAY_NAME)
        .await
        .map_err(|e| {
            format!(
                "could not register Kitty with BigTiny: {e}. If Kitty was registered by an \
                 earlier install whose key is gone, revoke the app with \
                 `DELETE /api/apps/kitty` and restart."
            )
        })?;

    let key = issued.api_key.clone();
    let to_store = key.clone();
    tokio::task::spawn_blocking(move || store_key(&to_store))
        .await
        .map_err(|e| format!("key store task panicked: {e}"))??;
    Ok(key)
}

#[cfg(windows)]
fn read_stored_key() -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, KEY_CREDENTIAL)
        .ok()
        .and_then(|e| e.get_password().ok())
        .filter(|k| !k.is_empty())
}

#[cfg(windows)]
fn store_key(key: &str) -> Result<(), String> {
    keyring::Entry::new(KEYRING_SERVICE, KEY_CREDENTIAL)
        .map_err(|e| format!("credential manager unavailable: {e}"))?
        .set_password(key)
        .map_err(|e| format!("could not store the BigTiny app key: {e}"))
}

/// Non-Windows keeps the key in the BigTiny data dir. Not as good as a
/// keychain, but the alternative is re-registering every launch, and the file
/// sits beside `encryption.key`, which is no less sensitive.
#[cfg(not(windows))]
fn key_file() -> Option<PathBuf> {
    crate::config::bigtiny_data_dir().ok().map(|d| d.join("app-key"))
}

#[cfg(not(windows))]
fn read_stored_key() -> Option<String> {
    let path = key_file()?;
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|k| !k.is_empty())
}

#[cfg(not(windows))]
fn store_key(key: &str) -> Result<(), String> {
    let path = key_file().ok_or("no data directory for the app key")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&path, key).map_err(|e| format!("{}: {e}", path.display()))
}

/// Find (or start) a V2 daemon and return a handle Kitty can use.
///
/// Mirrors `bigtiny_proc::spawn`'s signature so the call site changes in one
/// place, but note what is absent: no port to pick, no secret to mint, no
/// orphan to kill. The port comes from whichever daemon we found.
#[allow(clippy::too_many_arguments)]
pub async fn locate(
    command: &str,
    args: &[String],
    summarizer: &crate::config::SummarizerSettings,
    token_management: &crate::config::TokenManagementSettings,
    memory: &crate::config::MemorySettings,
    local: &crate::config::LocalModelSettings,
    specialists: &crate::config::SpecialistSettings,
    pathway_enabled: bool,
    pathway_embedding_model: &str,
    tokenizer_path: &str,
    litert_lib_dir: Option<&str>,
) -> Result<DaemonHandle, String> {
    // **Kitty no longer supplies the at-rest encryption key.** V2 owns it, in
    // `{data_dir}/encryption.key`.
    //
    // Under V1, injecting Kitty's Credential Manager key was right because
    // Kitty owned the daemon. With a shared daemon it becomes a hazard: the
    // key would depend on *which app happened to spawn it*, so rows written
    // while Kitty started the daemon would be unreadable when the pipeline
    // started it instead, and vice versa. Silent, and indistinguishable from a
    // provider rejecting its credentials.
    //
    // Continuity with an existing Kitty install is handled once, at migration:
    // `bigtiny2-daemon import --encryption-key <V1 key>` adopts V1's key as the
    // daemon's own, so previously-encrypted provider rows keep decrypting for
    // every app that starts it.
    let encryption_key = String::new();

    // Spawn-time only — see the module doc. `daemon_env` is shared with the
    // Android in-process host, so the secret argument stays in its signature;
    // V2 authenticates per app instead, and passing an empty secret leaves
    // `BIGTINY_SECRET` unset rather than pinning a daemon-wide one.
    let mut env: Vec<(String, String)> = crate::lifecycle::bigtiny_env::daemon_env(
        "",
        &encryption_key,
        summarizer,
        token_management,
        memory,
        local,
        specialists,
        pathway_enabled,
        pathway_embedding_model,
        tokenizer_path,
    )
    .into_iter()
    // Drop the two credentials this path no longer supplies rather than
    // sending them empty: the daemon treats a present-but-empty
    // `BIGTINY_ENCRYPTION_KEY` as a malformed key and refuses to start, and an
    // empty `BIGTINY_SECRET` would pin a daemon-wide shared secret in place of
    // per-app keys. `daemon_env` keeps both in its signature because the
    // Android in-process host still uses them.
    .filter(|(k, v)| !((k == "BIGTINY_SECRET" || k == "BIGTINY_ENCRYPTION_KEY") && v.is_empty()))
    .collect();

    if let Some(lib_dir) = litert_lib_dir.filter(|d| !d.is_empty()) {
        let existing = std::env::var("PATH").unwrap_or_default();
        let sep = if cfg!(windows) { ";" } else { ":" };
        env.push(("PATH".to_string(), format!("{lib_dir}{sep}{existing}")));
    }

    let located = attach_or_spawn(&DiscoveryConfig {
        daemon_binary: PathBuf::from(command),
        daemon_args: args.to_vec(),
        min_api_version: bigtiny2_client::MIN_API_VERSION,
        env,
    })
    .await
    .map_err(|e| format!("could not reach BigTiny: {e}"))?;

    let key = ensure_app_key(&located.base_url, &located.handshake.registration_token).await?;

    tracing::info!(
        port = located.handshake.port,
        spawned = located.spawned_by_us,
        "attached to BigTiny V2"
    );

    Ok(DaemonHandle {
        // No child to hold and nothing to kill. `owned: false` is the load-
        // bearing part: `shutdown` calls `kill_if_owned`, and a shared daemon
        // must survive Kitty exiting — another app may be mid-turn, and even
        // if not, the daemon's idle timer is what decides.
        process: ManagedProcess {
            child: None,
            owned: false,
        },
        port: Some(located.handshake.port),
        secret_key: Some(key),
        // `attach_or_spawn` only returns once `/api/health` has answered.
        healthy: true,
    })
}
