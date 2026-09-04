//! Where V2 keeps its handshake, lock and data.
//!
//! Every path here is **version-scoped**, and that is the point rather than
//! tidiness: V1 and V2 will be running on the same machine for the whole
//! migration, and the two must not be able to touch each other's state.
//!
//! Concretely, the collisions being avoided:
//!
//!   * **Data dir.** V1 uses `%APPDATA%/Kitty/bigtiny` (or `~/.bigtiny`). If
//!     V2 shared it, the two daemons would open the same SQLite file with
//!     different migration chains -- V2 would migrate the database forward and
//!     V1 would then be running against a schema it does not know.
//!   * **Pidfile.** V1's Kitty writes `bigtiny-daemon.pid` into its data dir
//!     and kills the PID it names at boot, if that process is called
//!     `bigtiny-daemon` (`src-tauri/src/lifecycle/bigtiny_proc.rs:23,64`). A
//!     separate data dir means it can never read a V2 pid; the distinct binary
//!     name (`bigtiny2-daemon`) means it would refuse to kill it even if it
//!     did.
//!   * **Handshake.** V1 has none, so there is nothing to collide with *today*
//!     -- but a V3 will exist eventually, and an unversioned path would make
//!     that migration strictly harder than this one.

use std::path::PathBuf;

/// Environment override for the data directory, mirroring V1's
/// `BIGTINY_DATA_DIR` but under a distinct name so setting one cannot
/// accidentally repoint the other.
pub const DATA_DIR_ENV: &str = "BIGTINYV2_DATA_DIR";

/// Process-name fragment a discovered daemon must match before we will treat a
/// PID as live, or ever consider killing it.
///
/// Deliberately not `"bigtiny-daemon"`: PIDs get recycled, and on a machine
/// running both daemons the wrong match would mean one version reaping the
/// other. Note this is a *substring* check, so it must not be a prefix of V1's
/// name -- `"bigtiny2-daemon"` is safe in both directions.
pub const DAEMON_PROCESS_NAME_FRAGMENT: &str = "bigtiny2-daemon";

/// The daemon binary's file name, used when spawning.
pub const DAEMON_BINARY: &str = if cfg!(windows) {
    "bigtiny2-daemon.exe"
} else {
    "bigtiny2-daemon"
};

/// `%APPDATA%/BigTinyV2` on Windows, `~/.bigtiny-v2` elsewhere, overridable by
/// [`DATA_DIR_ENV`].
///
/// Falls back to the current directory only if there is genuinely no home to
/// resolve, which in practice means a misconfigured container. That is a bad
/// answer, but a loud one -- the daemon will fail to write its database
/// somewhere visible rather than silently choosing a surprising location.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV) {
        let dir = PathBuf::from(dir);
        if !dir.as_os_str().is_empty() {
            return dir;
        }
    }
    if cfg!(windows) {
        if let Some(appdata) = dirs::config_dir() {
            return appdata.join("BigTinyV2");
        }
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".bigtiny-v2");
    }
    PathBuf::from(".bigtiny-v2")
}

/// The handshake file a running daemon publishes. See
/// [`bigtiny2_protocol::discovery`].
pub fn handshake_path() -> PathBuf {
    data_dir().join("daemon.json")
}

/// Exclusive lock held across "decide to spawn -> daemon is ready".
///
/// Without it, two apps launching in the same second both find no handshake
/// and both spawn, and the second one's daemon overwrites the first's
/// handshake -- leaving one orphaned daemon holding a database lock that the
/// surviving one cannot take.
pub fn spawn_lock_path() -> PathBuf {
    data_dir().join("spawn.lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_process_name_fragment_cannot_match_a_v1_daemon() {
        // The check that keeps the two versions from reaping each other. V1's
        // fragment is "bigtiny-daemon"; ours must not be a substring of a V1
        // process name, nor V1's a substring of ours in a way that matches.
        assert!(!"bigtiny-daemon".contains(DAEMON_PROCESS_NAME_FRAGMENT));
        assert!(!"bigtiny-daemon.exe".contains(DAEMON_PROCESS_NAME_FRAGMENT));
        assert!(DAEMON_BINARY.contains(DAEMON_PROCESS_NAME_FRAGMENT));
    }

    #[test]
    fn the_env_override_wins_and_an_empty_value_is_ignored() {
        // An empty env var is a common shell accident ("export X=$UNSET"); it
        // must not resolve the data dir to "".
        temp_env("/custom/dir", |dir| assert_eq!(dir, PathBuf::from("/custom/dir")));
        temp_env("", |dir| assert_ne!(dir, PathBuf::from("")));
    }

    fn temp_env(value: &str, check: impl FnOnce(PathBuf)) {
        let prev = std::env::var_os(DATA_DIR_ENV);
        std::env::set_var(DATA_DIR_ENV, value);
        let dir = data_dir();
        match prev {
            Some(p) => std::env::set_var(DATA_DIR_ENV, p),
            None => std::env::remove_var(DATA_DIR_ENV),
        }
        check(dir);
    }

    #[test]
    fn handshake_and_lock_live_beside_each_other_in_the_data_dir() {
        assert_eq!(handshake_path().parent(), spawn_lock_path().parent());
    }
}
