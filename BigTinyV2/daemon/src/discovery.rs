//! The daemon half of discovery: publishing where we are.
//!
//! The client half lives in `bigtiny2-client`; the shared types live in
//! `bigtiny2-protocol`. This module only writes and removes the file.
//!
//! # Ordering matters
//!
//! The handshake is published **after** the listener is bound and migrations
//! have run, and removed on graceful shutdown. A client that finds and
//! validates this file is entitled to assume the daemon can serve a request, so
//! publishing it early would convert a startup race into a confusing connection
//! error — the client would attach successfully and then fail its first call.
//!
//! Removal on shutdown is a courtesy, not a guarantee: a killed daemon leaves a
//! stale file behind, which is exactly why the client validates PID liveness,
//! process name, and instance id rather than trusting the file's existence.

use std::path::{Path, PathBuf};

use bigtiny2_protocol::discovery::Handshake;
use bigtiny2_protocol::API_VERSION;
use rand::Rng;

/// Environment override for the data directory. Must match
/// `bigtiny2_client::paths::DATA_DIR_ENV` — the client sets it when spawning,
/// and the daemon reads it here.
pub const DATA_DIR_ENV: &str = "BIGTINYV2_DATA_DIR";

/// Generate a random hex token: registration tokens and app API keys.
///
/// 32 bytes from the thread RNG, hex-encoded to 64 characters. High enough
/// entropy that `storage::apps::hash_key` can be a plain SHA-256 rather than a
/// password KDF — there is no dictionary to defend against.
pub fn generate_token() -> String {
    let bytes: [u8; 32] = rand::thread_rng().gen();
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Where this daemon's handshake lives, given its data dir.
pub fn handshake_path(data_dir: &Path) -> PathBuf {
    data_dir.join("daemon.json")
}

/// Build the handshake for this launch.
///
/// `instance_id` is regenerated per launch on purpose: it is what lets a client
/// distinguish "the daemon this file describes" from "a different daemon that
/// happens to have been handed the same port after a restart". Without it, a
/// client could attach to a daemon whose registration token and database
/// generation no longer match the file it read.
pub fn build(
    instance_id: String,
    host: &str,
    port: u16,
    data_dir: &Path,
    registration_token: String,
) -> Handshake {
    Handshake {
        api_version: API_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        instance_id,
        pid: std::process::id(),
        host: host.to_string(),
        port,
        data_dir: data_dir.to_string_lossy().into_owned(),
        registration_token,
        started_at: now_rfc3339(),
    }
}

/// Write the handshake atomically.
///
/// Write-then-rename rather than write-in-place: a client can read this file at
/// any moment, and a torn read would look like corruption. The client tolerates
/// corruption (it treats it as absent), but making that path unreachable is
/// cheaper than relying on it.
pub async fn publish(data_dir: &Path, handshake: &Handshake) -> std::io::Result<()> {
    tokio::fs::create_dir_all(data_dir).await?;
    let final_path = handshake_path(data_dir);
    let tmp_path = final_path.with_extension("json.tmp");

    let json = serde_json::to_vec_pretty(handshake)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    tokio::fs::write(&tmp_path, &json).await?;
    tokio::fs::rename(&tmp_path, &final_path).await?;
    tracing::info!("published handshake at {final_path:?}");
    Ok(())
}

/// Remove the handshake on graceful shutdown.
///
/// Failure is logged, not propagated: we are already shutting down, and a
/// leftover file is a recoverable condition the client handles by validating
/// rather than trusting.
pub async fn withdraw(data_dir: &Path) {
    let path = handshake_path(data_dir);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => tracing::info!("withdrew handshake at {path:?}"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("failed to remove handshake at {path:?}: {e}"),
    }
}

/// Seconds-precision RFC 3339, for the handshake's `started_at`.
///
/// Hand-rolled rather than pulling in `chrono`: this is diagnostic metadata
/// nothing parses or branches on, and the crate already avoids a date
/// dependency elsewhere.
fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let days = secs / 86_400;
    let tod = secs % 86_400;
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    // Civil-from-days (Howard Hinnant's algorithm), era-based.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };

    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_random_hex() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b, "two tokens must not collide");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn publish_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let handshake = build(
            "inst-1".into(),
            "127.0.0.1",
            51431,
            dir.path(),
            "tok".into(),
        );
        publish(dir.path(), &handshake).await.unwrap();

        let raw = tokio::fs::read(handshake_path(dir.path())).await.unwrap();
        let back: Handshake = serde_json::from_slice(&raw).unwrap();
        assert_eq!(back.port, 51431);
        assert_eq!(back.instance_id, "inst-1");
        assert_eq!(back.pid, std::process::id());
        assert_eq!(back.api_version, API_VERSION);
    }

    #[tokio::test]
    async fn publish_leaves_no_temp_file_behind() {
        // The rename must complete; a stray .tmp would accumulate every launch.
        let dir = tempfile::tempdir().unwrap();
        let h = build("i".into(), "127.0.0.1", 1, dir.path(), "t".into());
        publish(dir.path(), &h).await.unwrap();

        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        let mut names = Vec::new();
        while let Some(e) = entries.next_entry().await.unwrap() {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(names, vec!["daemon.json".to_string()]);
    }

    #[tokio::test]
    async fn withdraw_is_idempotent() {
        // Shutdown can race, and a missing file is the desired end state
        // anyway — calling twice must not error.
        let dir = tempfile::tempdir().unwrap();
        let h = build("i".into(), "127.0.0.1", 1, dir.path(), "t".into());
        publish(dir.path(), &h).await.unwrap();

        withdraw(dir.path()).await;
        assert!(!handshake_path(dir.path()).exists());
        withdraw(dir.path()).await;
    }

    #[test]
    fn rfc3339_formatting_is_well_formed_and_current() {
        let s = now_rfc3339();
        assert_eq!(s.len(), 20, "YYYY-MM-DDTHH:MM:SSZ");
        assert!(s.ends_with('Z'));
        let year: i32 = s[..4].parse().unwrap();
        assert!((2024..2100).contains(&year), "implausible year in {s}");
    }
}
