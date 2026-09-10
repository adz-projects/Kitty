//! Health probing, and the secret the Android in-process host still needs.
//!
//! # What used to be here
//!
//! This module owned the BigTiny daemon: it killed whatever a pidfile named,
//! reserved a port, minted a per-launch secret, spawned the executable, wrote
//! a pidfile, and killed the child again on exit. All of that is gone
//! (Phase 7c), because every step of it is actively wrong once a second
//! frontend can attach to the same daemon:
//!
//! * `kill_stale_orphan` killed any live process whose name matched, named by
//!   *Kitty's* pidfile. Under a shared daemon that is another application's
//!   in-flight turn being terminated at Kitty's launch.
//! * `demote_others` (in `bigtiny::providers`) PATCHed `fallback_priority: 100`
//!   onto every provider row Kitty did not own, because "the active provider"
//!   was daemon-global. That is now a scoped `PATCH /api/apps/me`.
//! * The per-launch `BIGTINY_SECRET` made identity ephemeral, so nothing could
//!   outlive a restart and no other app could hold a distinct one.
//! * `bind_reserved_port` pre-reserved a port in the client and passed it in,
//!   with a careful dance to narrow a TOCTOU window. The daemon now binds an
//!   ephemeral port itself and publishes it in the handshake, which removes
//!   the race rather than narrowing it.
//!
//! Desktop discovery lives in `bigtiny_v2`, on top of `bigtiny2_client`.
//!
//! # What is left
//!
//! `probe_health` — used by the health monitor and by the Android host — and
//! `generate_secret`, which the Android host uses to mint the per-launch
//! registration token it hands the in-process daemon. Both platforms are on
//! V2 and authenticate as registered apps; nothing here mints a daemon-wide
//! shared secret any more.

use std::time::Duration;

#[cfg(target_os = "android")]
use rand::Rng;

/// 32 hex chars of randomness for the daemon's per-launch
/// `registration_token` (`BIGTINY_SECRET`, `RunOptions::secret`).
///
/// Android only: the desktop daemon generates its own and publishes it in the
/// handshake file. On both platforms the *durable* credential is the app key
/// this token is exchanged for (`bigtiny_app_key::ensure_app_key`); the token
/// authorizes `POST /api/apps/register` and nothing else.
#[cfg(target_os = "android")]
pub fn generate_secret() -> String {
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| format!("{:x}", rng.gen_range(0u8..16)))
        .collect()
}

/// Protocol-level liveness: does `GET /api/health` answer 200? (Open without
/// the API key by design, exactly for this probe.)
pub async fn probe_health(client: &reqwest::Client, port: u16) -> bool {
    client
        .get(format!("http://127.0.0.1:{port}/api/health"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}
