//! Size-based rotation of `daemon.log` while the daemon runs.
//!
//! The spawning client rotates the log once per spawn
//! (`bigtiny2_client::discovery`), but a shared daemon can live for days, and
//! its one file only ever grew. The daemon cannot rename the file — the
//! handle is inherited as stdout/stderr — so this is copy-and-truncate: when
//! the file passes [`MAX_LOG_BYTES`], its contents are copied to
//! `daemon.log.1` (shifting the older ones up) and the live file is truncated
//! in place. A line written during the copy can be lost; that is the accepted
//! cost of the method.
//!
//! Only runs when stderr really is `<data_dir>/daemon.log`. A daemon started
//! from a terminal, or redirected anywhere else, is left alone.

use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;
/// Same retention as the spawn-time rotation.
const KEPT_LOGS: usize = 2;
const CHECK_EVERY: Duration = Duration::from_secs(60);

/// Start the rotation task. Call once from the daemon binary.
pub fn spawn(data_dir: &Path) {
    let log = data_dir.join("daemon.log");
    match stderr_path() {
        Some(p) if same_path(&p, &log) => {}
        _ => return,
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(CHECK_EVERY);
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = rotate_if_large(&log, MAX_LOG_BYTES) {
                tracing::warn!("daemon.log rotation failed: {e}");
            }
        }
    });
}

fn numbered(log: &Path, n: usize) -> PathBuf {
    let mut name = log.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// Copy `log` aside and truncate the live stderr file, when it is too big.
fn rotate_if_large(log: &Path, max: u64) -> std::io::Result<()> {
    let stderr = live_stderr();
    let Some(file) = stderr.as_ref() else {
        return Ok(());
    };
    if file.metadata()?.len() < max {
        return Ok(());
    }
    shift_and_copy(log, KEPT_LOGS)?;
    truncate(file)?;
    tracing::info!("daemon.log passed {} MiB; rotated", max >> 20);
    Ok(())
}

fn shift_and_copy(log: &Path, keep: usize) -> std::io::Result<()> {
    let _ = std::fs::remove_file(numbered(log, keep));
    for n in (1..keep).rev() {
        let _ = std::fs::rename(numbered(log, n), numbered(log, n + 1));
    }
    std::fs::copy(log, numbered(log, 1)).map(|_| ())
}

/// Truncate to zero and move the shared file position back to the start.
/// stdout and stderr are duplicates of one file object, so they share the
/// position and both continue at offset 0 (or at the end, if opened for
/// append).
fn truncate(file: &std::fs::File) -> std::io::Result<()> {
    use std::io::Seek;
    file.set_len(0)?;
    let mut f = file;
    f.seek(std::io::SeekFrom::Start(0))?;
    Ok(())
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// stderr as a `File` that is never closed (it is borrowed, not owned).
#[cfg(any(windows, unix))]
fn live_stderr() -> Option<std::mem::ManuallyDrop<std::fs::File>> {
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        let h = std::io::stderr().as_raw_handle();
        if h.is_null() {
            return None;
        }
        // SAFETY: a valid handle for the life of the process; ManuallyDrop
        // keeps us from closing it.
        Some(std::mem::ManuallyDrop::new(unsafe {
            std::fs::File::from_raw_handle(h)
        }))
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::FromRawFd;
        // SAFETY: fd 2 lives for the process; ManuallyDrop keeps it open.
        Some(std::mem::ManuallyDrop::new(unsafe {
            std::fs::File::from_raw_fd(2)
        }))
    }
}

#[cfg(not(any(windows, unix)))]
fn live_stderr() -> Option<std::mem::ManuallyDrop<std::fs::File>> {
    None
}

/// The path of the file stderr writes to, or `None` for a console or pipe.
#[cfg(windows)]
fn stderr_path() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED};
    let h = std::io::stderr().as_raw_handle();
    if h.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    // SAFETY: `buf` is writable for its length; the call fails cleanly on a
    // handle that is not a file.
    let n = unsafe {
        GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, FILE_NAME_NORMALIZED)
    } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    let s = std::ffi::OsString::from_wide(&buf[..n]);
    // `\\?\C:\...` canonicalizes the same as the plain path on both sides.
    Some(PathBuf::from(s))
}

#[cfg(unix)]
fn stderr_path() -> Option<PathBuf> {
    std::fs::read_link("/proc/self/fd/2").ok().filter(|p| p.is_file())
}

#[cfg(not(any(windows, unix)))]
fn stderr_path() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_and_truncate_keeps_the_old_contents_and_empties_the_live_file() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("daemon.log");
        std::fs::write(numbered(&log, 1), "older").unwrap();
        let mut live = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log)
            .unwrap();
        live.write_all(b"current run").unwrap();

        shift_and_copy(&log, 2).unwrap();
        truncate(&live).unwrap();
        live.write_all(b"after").unwrap();
        drop(live);

        let read = |p: PathBuf| std::fs::read_to_string(p).unwrap();
        assert_eq!(read(numbered(&log, 1)), "current run");
        assert_eq!(read(numbered(&log, 2)), "older");
        // Written at offset 0, not after a gap of zeros where the old
        // contents were.
        assert_eq!(read(log), "after");
    }
}
