//! The daemon handshake: how a client finds a running BigTiny V2 without
//! having spawned it.
//!
//! V1 had no equivalent. Kitty spawned a daemon on a fresh ephemeral port with
//! a fresh secret every launch, held both in its own process memory, and killed
//! whatever it found in its pidfile at boot
//! (`src-tauri/src/lifecycle/bigtiny_proc.rs:64`). That is a perfectly good
//! design for exactly one owner and an actively hostile one for several: a
//! second app has no way to learn the port, no way to authenticate, and gets
//! its daemon killed by the first app's next launch.
//!
//! The handshake file replaces all three: it publishes where the daemon is,
//! proves whether it is still alive, and carries a bootstrap token an app can
//! trade for a durable key of its own.
//!
//! # What the registration token does and does not protect
//!
//! [`Handshake::registration_token`] authorizes `POST /api/apps/register` and
//! nothing else. The file sits in the user's own app-data directory and is
//! readable by any process running as that user, so this is **not** a boundary
//! against local malware -- anything that can read the file could also read the
//! issued key afterwards. It is a boundary against *accidental* cross-app
//! interference: it makes an app declare an identity before it can touch
//! anything, so one app's sessions, providers and MCP servers cannot be
//! silently mutated by another's self-healing. Say that plainly in the docs
//! rather than implying more.

use serde::{Deserialize, Serialize};

/// Contents of `daemon.json`, written by the daemon once its listener is bound
/// and its migrations have run, and removed on graceful shutdown.
///
/// Written *after* readiness, never before: a client that finds this file and
/// validates it is entitled to assume the daemon can serve a request, so
/// publishing it early would turn a race into a confusing connection error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handshake {
    /// Wire-contract version this daemon serves. See [`crate::API_VERSION`].
    pub api_version: u32,
    /// Human-facing build version, for diagnostics and error messages only.
    /// Never branch on this -- branch on `api_version`.
    pub daemon_version: String,
    /// Regenerated every launch. This is what distinguishes "the daemon I
    /// found is the one this file describes" from "the PID was recycled onto
    /// an unrelated process": the client compares it against the same field on
    /// `GET /api/health`.
    pub instance_id: String,
    pub pid: u32,
    pub host: String,
    pub port: u16,
    /// Where this daemon keeps its database and keys. Surfaced so a client can
    /// tell V1 and V2 instances apart in diagnostics, and so a misconfigured
    /// second data dir is visible rather than mysterious.
    pub data_dir: String,
    /// Bootstrap-only credential -- see the module docs. Per-launch, and
    /// useless once an app has registered and stored its own key.
    pub registration_token: String,
    pub started_at: String,
}

impl Handshake {
    /// Whether a client requiring `min_api_version` can safely talk to this
    /// daemon.
    ///
    /// Deliberately one-directional. A *newer* daemon serving an older client
    /// is fine, because additive fields deserialize cleanly and
    /// [`crate::API_VERSION`] is only bumped for changes that break that. An
    /// *older* daemon is not fine, and must produce a clear error telling the
    /// user another app is running an older BigTiny -- never a silently
    /// wrong-shaped call, which is the failure mode this check exists to
    /// prevent.
    pub fn is_compatible_with(&self, min_api_version: u32) -> bool {
        self.api_version >= min_api_version
    }
}

/// `POST /api/apps/register`, authorized by the handshake's registration token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// Stable, caller-chosen identity (`"kitty"`, `"research-pipeline"`).
    /// Everything an app owns is keyed by this, so it must not change between
    /// launches -- an app that generates a fresh id each start orphans all of
    /// its own sessions.
    pub app_id: String,
    pub display_name: String,
}

/// The issued key. Long-lived and hashed at rest in the `apps` table, so it
/// **survives daemon restarts**: an app registers once, persists this in its
/// own secret store, and skips registration on every later launch.
///
/// This is the reason keys are stored rather than generated per launch the way
/// V1's single `BIGTINY_SECRET` was. With one owner, a per-launch secret was
/// free; with several, it would force every app to re-register (and to still be
/// running) every time the daemon bounced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub app_id: String,
    pub api_key: String,
}

/// `GET /api/health`. Unauthenticated by design, so a client can poll for
/// readiness before it has a key -- and so the handshake can be validated
/// against the live process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    /// Present from V2 onward. Compared against [`Handshake::instance_id`] to
    /// prove the process answering on that port is the one the file describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake(api_version: u32) -> Handshake {
        Handshake {
            api_version,
            daemon_version: "2.0.0".into(),
            instance_id: "abc".into(),
            pid: 1234,
            host: "127.0.0.1".into(),
            port: 51431,
            data_dir: "/tmp/bigtiny2".into(),
            registration_token: "tok".into(),
            started_at: "2026-08-30T12:00:00Z".into(),
        }
    }

    #[test]
    fn a_newer_daemon_serves_an_older_client() {
        assert!(handshake(3).is_compatible_with(1));
    }

    #[test]
    fn an_older_daemon_is_refused() {
        // The case that must never silently proceed: the client would send
        // request shapes this daemon does not understand.
        assert!(!handshake(1).is_compatible_with(2));
    }

    #[test]
    fn an_exact_match_is_compatible() {
        assert!(handshake(2).is_compatible_with(2));
    }

    #[test]
    fn handshake_round_trips_through_json() {
        // This file is written by one process and read by another, possibly of
        // a different build -- a serde round-trip is the actual contract.
        let json = serde_json::to_string(&handshake(1)).unwrap();
        let back: Handshake = serde_json::from_str(&json).unwrap();
        assert_eq!(back.port, 51431);
        assert_eq!(back.instance_id, "abc");
    }

    #[test]
    fn health_tolerates_a_v1_style_response() {
        // V1's /api/health has neither field. A client probing an instance it
        // has not identified yet must not fail to parse it.
        let back: HealthResponse = serde_json::from_str(r#"{"status":"ok"}"#).unwrap();
        assert_eq!(back.status, "ok");
        assert!(back.instance_id.is_none());
    }
}
