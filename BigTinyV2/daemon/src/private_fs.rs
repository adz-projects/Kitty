//! Owner-only files and directories for what the daemon keeps on disk.
//!
//! The data directory holds the SQLite databases (every app's conversations),
//! the handshake (whose registration token reclaims any app's key) and, off
//! Windows, the at-rest encryption key in plain hex. On Windows the directory
//! sits under the user's `%APPDATA%`, whose ACL already excludes other users,
//! and the key is DPAPI-sealed. On Unix nothing does that for us: a default
//! umask leaves a fresh directory `0755` and a fresh file `0644`, i.e.
//! readable by every account on the machine -- harmless in an Android app's
//! private storage, but not on a shared Linux server.
//!
//! So on Unix the data directory is `0700` and the secret-bearing files are
//! `0600`. Everything here is a no-op on Windows.

use std::io;
use std::path::Path;

/// Create `dir` if needed and restrict it to its owner.
///
/// An existing directory is tightened too, which is what fixes a data
/// directory an earlier version created world-readable. Only `dir` itself is
/// changed, never its parents.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
        if mode != 0o700 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// Write `contents` to `path` atomically (beside it, then renamed over it),
/// readable and writable by the owner only.
///
/// A crash never leaves a truncated file, and on Unix the file is never
/// visible with wider permissions, not even as the temporary: it is created
/// `0600`, and re-chmodded in case a stale temporary from an earlier run
/// already existed with a different mode.
pub fn write_private_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp_name);

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write_private_atomic(&path, b"one").unwrap();
        write_private_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert!(!dir.path().join("secret.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn files_and_dirs_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("data");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        ensure_private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);

        // A stale world-readable temporary must not lend its mode to the file.
        let path = dir.join("key");
        std::fs::write(dir.join("key.tmp"), b"stale").unwrap();
        std::fs::set_permissions(dir.join("key.tmp"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        write_private_atomic(&path, b"k").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
