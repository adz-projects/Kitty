//! Find a running daemon, or start one -- without ever stepping on another
//! app's instance.
//!
//! This is the piece V1 had no need for and therefore no version of. Kitty
//! spawned unconditionally and killed whatever it found first
//! (`bigtiny_proc::kill_stale_orphan`), which is correct for a sole owner and
//! destructive for a shared daemon. The rule here is the inverse: **never kill
//! a daemon that has not been proven dead**, and prove it through the handshake
//! rather than by assuming ownership.
//!
//! The sequence, in [`attach_or_spawn`]:
//!
//! 1. Read the handshake. If it describes a live, compatible daemon, attach.
//!    No spawn, no kill.
//! 2. Otherwise take an exclusive spawn lock, so two apps launching in the same
//!    second do not both spawn. The loser waits for the winner's handshake
//!    instead of racing it.
//! 3. Under the lock, re-check (the winner may have just published one), then
//!    clear the stale handshake and spawn.
//!
//! Step 3's re-check is not redundant: between failing step 1 and taking the
//! lock, another process may have completed the whole sequence.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bigtiny2_protocol::discovery::{Handshake, HealthResponse};

use crate::error::{ClientError, Result};
use crate::paths;

/// How long to wait for a spawned daemon to answer `/api/health`.
///
/// Ported from V1's 60 x 250ms poll (`bigtiny_proc.rs:191-197`), which is
/// empirically enough for a cold start with migrations on a slow disk.
const READINESS_TIMEOUT: Duration = Duration::from_secs(15);
const READINESS_POLL: Duration = Duration::from_millis(250);

/// How long a losing spawn-lock contender waits for the winner's handshake.
/// Longer than [`READINESS_TIMEOUT`], because the winner must finish its own
/// readiness wait before it publishes.
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(25);
const LOCK_POLL: Duration = Duration::from_millis(100);

/// A lock older than this is assumed abandoned. Comfortably longer than a
/// worst-case spawn, so a slow-but-live contender is never robbed of it.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(60);

/// A validated, reachable daemon.
#[derive(Debug, Clone)]
pub struct Located {
    pub base_url: String,
    pub handshake: Handshake,
    /// True when this process started the daemon. Recorded for diagnostics
    /// only -- notably **not** as a licence to kill it on exit, which is
    /// exactly the V1 behaviour that breaks sharing. Lifetime is the daemon's
    /// own business, via its idle-exit timer.
    pub spawned_by_us: bool,
}

/// Read and parse the handshake, or `None` if it is absent or unreadable.
///
/// A corrupt file is treated as absent rather than as an error: the recovery
/// for both is identical (spawn a fresh daemon), and a hard failure here would
/// mean a truncated write during a crash permanently bricks every client.
pub async fn read_handshake(path: &Path) -> Option<Handshake> {
    let bytes = tokio::fs::read(path).await.ok()?;
    match serde_json::from_slice::<Handshake>(&bytes) {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::warn!("ignoring unreadable handshake at {path:?}: {e}");
            None
        }
    }
}

/// Whether `pid` is a live process whose name marks it as a V2 daemon.
///
/// Both halves matter. PIDs are recycled, so liveness alone would let an
/// unrelated process masquerade as the daemon; the name check is what makes a
/// stale handshake safely detectable. It is also what stops V2 from ever
/// mistaking a V1 daemon for its own.
pub fn pid_is_live_daemon(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
    let sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::new()),
    );
    sys.process(Pid::from_u32(pid))
        .map(|p| {
            p.name()
                .to_string_lossy()
                .contains(paths::DAEMON_PROCESS_NAME_FRAGMENT)
        })
        .unwrap_or(false)
}

/// Probe `/api/health`, which is unauthenticated precisely so this works
/// before registration.
async fn probe_health(client: &reqwest::Client, base_url: &str) -> Option<HealthResponse> {
    let resp = client
        .get(format!("{base_url}/api/health"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<HealthResponse>().await.ok()
}

/// Confirm the process answering on that port is the one the handshake
/// describes.
///
/// The `instance_id` comparison is the load-bearing part. A live PID with a
/// matching name on the recorded port is *still* not proof: the daemon could
/// have restarted and been handed the same port, in which case its database
/// generation and registration token differ from the file's. Comparing the
/// per-launch instance id closes that.
async fn validate(client: &reqwest::Client, handshake: &Handshake) -> Option<String> {
    if !pid_is_live_daemon(handshake.pid) {
        return None;
    }
    let base_url = format!("http://{}:{}", handshake.host, handshake.port);
    let health = probe_health(client, &base_url).await?;
    match health.instance_id.as_deref() {
        Some(id) if id == handshake.instance_id => Some(base_url),
        Some(other) => {
            tracing::warn!(
                "handshake names instance {} but {base_url} reports {other} - treating as stale",
                handshake.instance_id
            );
            None
        }
        // A daemon reporting no instance id is not a V2 daemon (V1's health
        // route has no such field). Refuse rather than guess.
        None => None,
    }
}

/// Configuration for locating a daemon.
pub struct DiscoveryConfig {
    /// Path to the daemon binary, used only if we have to spawn one.
    pub daemon_binary: PathBuf,
    /// Minimum wire-contract version this client can work with.
    pub min_api_version: u32,
    /// Extra environment for a spawned daemon (data dir overrides, model
    /// paths). Ignored when attaching to an existing one -- a real consequence
    /// worth knowing: **the first app to spawn decides the daemon's
    /// configuration**, and later attachers inherit it.
    pub env: Vec<(String, String)>,
}

/// Attach to a running daemon, or start one. See the module docs.
pub async fn attach_or_spawn(config: &DiscoveryConfig) -> Result<Located> {
    let client = reqwest::Client::new();
    let handshake_path = paths::handshake_path();

    // 1. Fast path: someone else already has a daemon up.
    if let Some(found) = try_attach(&client, &handshake_path, config.min_api_version).await? {
        return Ok(found);
    }

    // 2. Serialize the decision to spawn.
    let _lock = SpawnLock::acquire(&paths::spawn_lock_path()).await?;

    // 3. Re-check under the lock: a contender may have finished while we
    //    waited for it.
    if let Some(found) = try_attach(&client, &handshake_path, config.min_api_version).await? {
        return Ok(found);
    }

    // The handshake is now known-stale (or absent). Removing it is safe
    // *because* validation failed -- this is the only place a client deletes a
    // handshake, and only after proving nothing is behind it.
    let _ = tokio::fs::remove_file(&handshake_path).await;

    spawn_and_wait(&client, config, &handshake_path).await
}

/// The attach half, factored out because [`attach_or_spawn`] runs it twice.
///
/// Returns `Err(DaemonTooOld)` rather than `Ok(None)` for a live-but-outdated
/// daemon: spawning a second one alongside it would not help (they would fight
/// over the same data dir), so the user genuinely has to act.
async fn try_attach(
    client: &reqwest::Client,
    handshake_path: &Path,
    min_api_version: u32,
) -> Result<Option<Located>> {
    let Some(handshake) = read_handshake(handshake_path).await else {
        return Ok(None);
    };
    let Some(base_url) = validate(client, &handshake).await else {
        return Ok(None);
    };
    if !handshake.is_compatible_with(min_api_version) {
        return Err(ClientError::DaemonTooOld {
            found: handshake.api_version,
            needed: min_api_version,
        });
    }
    Ok(Some(Located {
        base_url,
        handshake,
        spawned_by_us: false,
    }))
}

async fn spawn_and_wait(
    client: &reqwest::Client,
    config: &DiscoveryConfig,
    handshake_path: &Path,
) -> Result<Located> {
    let data_dir = paths::data_dir();
    tokio::fs::create_dir_all(&data_dir).await?;

    let mut cmd = tokio::process::Command::new(&config.daemon_binary);
    // Port 0: the daemon binds an ephemeral port and publishes the real one in
    // its handshake. V1 pre-reserved a port in the *client* and passed it in
    // (`bind_reserved_port`), which needed a careful dance around holding the
    // listener open through setup to narrow a TOCTOU window. Letting the
    // daemon bind and then report removes that race rather than narrowing it.
    cmd.arg("--host").arg("127.0.0.1").arg("--port").arg("0");
    for (k, v) in &config.env {
        cmd.env(k, v);
    }
    cmd.env(paths::DATA_DIR_ENV, data_dir.as_os_str());
    cmd.spawn()
        .map_err(|e| ClientError::NotFound(format!("{:?}: {e}", config.daemon_binary)))?;

    // Poll for a handshake we can validate, rather than watching the child's
    // stdout, so this behaves identically whether we spawned the daemon or
    // (in a future supervised mode) something else did.
    let deadline = tokio::time::Instant::now() + READINESS_TIMEOUT;
    loop {
        if let Some(found) = try_attach(client, handshake_path, config.min_api_version).await? {
            return Ok(Located {
                spawned_by_us: true,
                ..found
            });
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ClientError::ReadinessTimeout(READINESS_TIMEOUT));
        }
        tokio::time::sleep(READINESS_POLL).await;
    }
}

/// Exclusive spawn lock, released on drop.
///
/// `create_new` is the whole mechanism: it is atomic at the filesystem level,
/// so exactly one contender creates the file and the rest see `AlreadyExists`.
/// A lock left behind by a crashed process is reclaimed by age rather than by
/// trusting a recorded PID -- the process that would have to be checked is, by
/// definition, the one that failed to clean up.
struct SpawnLock {
    path: PathBuf,
}

impl SpawnLock {
    async fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let deadline = tokio::time::Instant::now() + LOCK_WAIT_TIMEOUT;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => {
                    return Ok(Self {
                        path: path.to_path_buf(),
                    })
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Self::is_stale(path) {
                        tracing::warn!("reclaiming stale spawn lock at {path:?}");
                        let _ = std::fs::remove_file(path);
                        continue;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(ClientError::SpawnLock {
                            path: path.to_path_buf(),
                            reason: "another process held it too long".into(),
                        });
                    }
                    tokio::time::sleep(LOCK_POLL).await;
                }
                Err(e) => {
                    return Err(ClientError::SpawnLock {
                        path: path.to_path_buf(),
                        reason: e.to_string(),
                    })
                }
            }
        }
    }

    fn is_stale(path: &Path) -> bool {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|e| e > LOCK_STALE_AFTER).unwrap_or(false))
            .unwrap_or(false)
    }
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_missing_handshake_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_handshake(&dir.path().join("nope.json")).await.is_none());
    }

    #[tokio::test]
    async fn a_corrupt_handshake_reads_as_none_rather_than_erroring() {
        // A truncated write during a crash must not permanently brick every
        // client: absent and corrupt both mean "spawn a fresh one".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        tokio::fs::write(&path, b"{ not json").await.unwrap();
        assert!(read_handshake(&path).await.is_none());
    }

    #[test]
    fn pid_zero_is_not_a_live_daemon() {
        assert!(!pid_is_live_daemon(0));
    }

    #[test]
    fn our_own_pid_is_not_mistaken_for_a_daemon() {
        // Liveness alone is not enough - this process is very much alive, and
        // must still fail the name check.
        assert!(!pid_is_live_daemon(std::process::id()));
    }

    #[tokio::test]
    async fn the_spawn_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spawn.lock");

        let lock = SpawnLock::acquire(&path).await.unwrap();
        assert!(path.exists());

        // A second acquire must not succeed while the first is held.
        let contended =
            tokio::time::timeout(Duration::from_millis(300), SpawnLock::acquire(&path)).await;
        assert!(contended.is_err(), "second acquire should still be waiting");

        drop(lock);
        assert!(!path.exists(), "drop must release the lock");
        SpawnLock::acquire(&path)
            .await
            .expect("a released lock is re-acquirable");
    }

    #[tokio::test]
    async fn a_stale_lock_is_reclaimed_rather_than_waited_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spawn.lock");
        std::fs::write(&path, b"").unwrap();

        // Backdate past the staleness threshold to stand in for a crashed
        // holder. Without reclamation this would block for LOCK_WAIT_TIMEOUT
        // and then fail - i.e. one crash would lock every app out for 25s.
        let old = std::time::SystemTime::now() - LOCK_STALE_AFTER - Duration::from_secs(10);
        let times = std::fs::File::options().write(true).open(&path).unwrap();
        times.set_modified(old).unwrap();
        drop(times);

        let lock = tokio::time::timeout(Duration::from_secs(2), SpawnLock::acquire(&path))
            .await
            .expect("a stale lock must be reclaimed promptly")
            .unwrap();
        drop(lock);
    }
}
