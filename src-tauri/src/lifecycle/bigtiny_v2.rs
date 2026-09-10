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

use crate::state::{DaemonHandle, ManagedProcess};

use super::bigtiny_app_key::ensure_app_key;

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
    );

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
