//! Resolution, verification and (optional) installation of the CPython WASI
//! guest.
//!
//! The guest is a 26 MB `.wasm` — too large to commit, so it is fetched once
//! from a pinned, checksummed release URL and cached in the app data
//! directory. Everything about that fetch is pinned: exact release tag, exact
//! filename, exact SHA-256. A mismatch is a hard failure, never a warning —
//! this file becomes executable code inside the sandbox, and "downloaded
//! something, hope it's right" is not an acceptable posture even for
//! sandboxed code.
//!
//! Resolution order (first hit wins):
//! 1. `KITTY_WASM_PYTHON` — explicit absolute path. Lets a packager bundle
//!    the guest alongside the binary, or a developer point at a local build,
//!    with no download at all.
//! 2. `<data dir>/guests/<pinned filename>` — the installed copy.
//!
//! Nothing is downloaded implicitly: `ensure_python_guest` only fetches when
//! `allow_download` is set, so the default tool path fails with actionable
//! instructions rather than silently pulling 26 MB over a metered connection.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// Pinned CPython build. From VMware Labs' `webassembly-language-runtimes`,
/// which publishes plain `wasm32-wasi` CPython with the standard library
/// embedded in the module (verified: `statistics`, `math`, `json`, `re`,
/// `datetime`, `decimal`, `fractions`, `itertools` all import with no
/// preopened stdlib directory).
pub const PYTHON_RELEASE_TAG: &str = "python/3.12.0+20231211-040d5a6";
pub const PYTHON_GUEST_FILENAME: &str = "python-3.12.0.wasm";
pub const PYTHON_GUEST_SHA256: &str =
    "e5dc5a398b07b54ea8fdb503bf68fb583d533f10ec3f930963e02b9505f7a763";
pub const PYTHON_GUEST_URL: &str = "https://github.com/vmware-labs/webassembly-language-runtimes/releases/download/python/3.12.0%2B20231211-040d5a6/python-3.12.0.wasm";
pub const PYTHON_GUEST_BYTES: u64 = 26_267_204;

/// `~/.kitty-wasm` unless overridden. Kept independent of BigTiny's data dir
/// so this plugin works standalone (as a stdio MCP server started by hand)
/// without inheriting a host's layout.
///
/// `KITTY_WASM_DATA_DIR` is the specific override and wins outright. Failing
/// that, the base comes from `paths::home_dir`, which honours
/// `KITTY_PLUGIN_HOME` — the only thing that makes this directory writable on
/// Android, where `dirs::home_dir()` reports `/data` and the old `"."`
/// fallback resolved to `/`. That is why the guest download this crate's
/// Android path depends on could never succeed: every write went to an
/// unwritable location.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("KITTY_WASM_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    crate::paths::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kitty-wasm")
}

/// Per-run scratch space, replacing `std::env::temp_dir()`.
///
/// Android has no `/tmp` and sets no `TMPDIR` for an app process, so
/// `std::env::temp_dir()` returned a path that does not exist and cannot be
/// created — which failed every `execute_math_python` call before the sandbox
/// was even started. Keeping scratch under the (now correctly resolved) data
/// dir means there is exactly one directory to get right per platform.
pub fn run_dir() -> PathBuf {
    data_dir().join("run")
}

pub fn guests_dir() -> PathBuf {
    data_dir().join("guests")
}

/// Where compiled `.cwasm` artifacts live. See `sandbox::load_module_cached`
/// for why this matters so much (a cold compile of the CPython guest is ~20s,
/// versus ~90ms to actually run a script).
pub fn module_cache_dir() -> PathBuf {
    data_dir().join("module-cache")
}

/// Optional directory of pure-Python packages made importable inside the
/// sandbox.
///
/// The pinned guest embeds only the standard library. `wasm_math_mcp.py`'s
/// sandbox also exposed `networkx`, which is pure Python and therefore works
/// here if dropped into this directory — but it is *not* shipped, because
/// vendoring a third-party package tree into this repo is a maintenance and
/// provenance burden that only some callers need. When the directory exists
/// it is mounted read-only and added to `PYTHONPATH`.
pub fn site_packages_dir() -> PathBuf {
    data_dir().join("site-packages")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

/// Where the Python guest is, if it's available right now.
pub fn find_python_guest() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("KITTY_WASM_PYTHON") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
        // An explicitly configured path that doesn't exist is a
        // misconfiguration worth surfacing, not something to paper over by
        // silently falling back to a downloaded copy.
        return None;
    }
    let installed = guests_dir().join(PYTHON_GUEST_FILENAME);
    installed.is_file().then_some(installed)
}

/// Human-readable status, used by the `wasm_guest_status` tool so a missing
/// guest is an actionable message rather than a mysterious failure.
pub fn python_guest_status() -> serde_json::Value {
    let explicit = std::env::var("KITTY_WASM_PYTHON").ok();
    let installed = guests_dir().join(PYTHON_GUEST_FILENAME);
    let resolved = find_python_guest();

    serde_json::json!({
        "available": resolved.is_some(),
        "resolved_path": resolved.as_ref().map(|p| p.to_string_lossy()),
        "env_override": explicit,
        "env_override_valid": explicit.as_ref().map(|p| Path::new(p).is_file()),
        "install_path": installed.to_string_lossy(),
        "site_packages_dir": site_packages_dir().to_string_lossy(),
        "site_packages_present": site_packages_dir().is_dir(),
        "pinned": {
            "release_tag": PYTHON_RELEASE_TAG,
            "filename": PYTHON_GUEST_FILENAME,
            "sha256": PYTHON_GUEST_SHA256,
            "size_bytes": PYTHON_GUEST_BYTES,
            "url": PYTHON_GUEST_URL,
        },
    })
}

/// Verifies an on-disk guest against the pinned digest.
pub fn verify_guest(path: &Path) -> Result<()> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read guest at {}", path.display()))?;
    let actual = sha256_hex(&bytes);
    if actual != PYTHON_GUEST_SHA256 {
        bail!(
            "guest checksum mismatch for {}: expected {PYTHON_GUEST_SHA256}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

/// Per-process sequence for download temp names — see `download_tmp_name`.
static TMP_NAME_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A temp filename for the in-flight download, unique per call.
///
/// Process id *and* a per-process counter — the same fix `sandbox.rs`'s
/// compile cache took for audit #127. The pid alone is not unique when
/// several servers share one process, which is exactly how this crate is
/// hosted on Android, so two concurrent `install=true` calls would interleave
/// their writes into a single temp file and then race each other's rename.
fn download_tmp_name() -> String {
    format!(
        "{PYTHON_GUEST_FILENAME}.tmp{}-{}",
        std::process::id(),
        TMP_NAME_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// Hard ceiling on the download, a little above the pinned size so a
/// legitimate transfer always fits.
const DOWNLOAD_MAX_BYTES: u64 = PYTHON_GUEST_BYTES + 1024 * 1024;

/// Reads the response body with a byte ceiling, checking `Content-Length`
/// first and capping the streaming accumulation regardless.
///
/// The download used to be a bare `response.bytes()`. That has no bound at
/// all: the checksum would eventually reject a wrong file, but only *after*
/// buffering however much the server chose to send — and the whole point of
/// this path is that it runs on a phone, where an endpoint that streams
/// indefinitely takes the app down long before any verification happens. Same
/// header-plus-streaming pattern `kitty-web`'s `read_body_capped` uses (audit
/// #112); duplicated rather than shared for the same reason the rest of these
/// crates duplicate.
async fn read_body_capped(mut response: reqwest::Response, cap: u64) -> Result<Vec<u8>> {
    if let Some(len) = response.content_length() {
        if len > cap {
            bail!("guest download is {len} bytes, over the {cap}-byte ceiling; refusing it");
        }
    }
    let mut buf: Vec<u8> = Vec::with_capacity(PYTHON_GUEST_BYTES as usize);
    // `chunk()` rather than `bytes_stream()`: one fewer feature to enable, and
    // the cap check has to happen per-chunk either way.
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read the guest download body")?
    {
        if buf.len() as u64 + chunk.len() as u64 > cap {
            bail!("guest download exceeded the {cap}-byte ceiling; refusing it");
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Returns the guest path, downloading it first if necessary and permitted.
///
/// The download is verified before it is moved into place, so a truncated or
/// tampered transfer can never leave a usable-looking file behind: it is
/// written to a temporary name, checksummed, and only then renamed.
pub async fn ensure_python_guest(allow_download: bool) -> Result<PathBuf> {
    if let Some(found) = find_python_guest() {
        return Ok(found);
    }
    if std::env::var("KITTY_WASM_PYTHON").is_ok() {
        bail!(
            "KITTY_WASM_PYTHON is set but does not point at a readable file; \
             unset it to use the managed guest at {}",
            guests_dir().join(PYTHON_GUEST_FILENAME).display()
        );
    }
    if !allow_download {
        bail!(
            "the CPython WASM guest is not installed. Expected it at {}. \
             Re-run this tool with install=true to download it ({} MB, pinned to \
             {PYTHON_RELEASE_TAG}, SHA-256 {PYTHON_GUEST_SHA256}), or set \
             KITTY_WASM_PYTHON to an existing copy.",
            guests_dir().join(PYTHON_GUEST_FILENAME).display(),
            PYTHON_GUEST_BYTES / 1_000_000,
        );
    }

    let dir = guests_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create guest directory {}", dir.display()))?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()?;
    let response = client
        .get(PYTHON_GUEST_URL)
        .send()
        .await
        .context("failed to download the CPython WASM guest")?;
    if !response.status().is_success() {
        bail!("guest download failed: HTTP {}", response.status());
    }
    let bytes = read_body_capped(response, DOWNLOAD_MAX_BYTES).await?;

    let actual = sha256_hex(&bytes);
    if actual != PYTHON_GUEST_SHA256 {
        bail!(
            "downloaded guest failed checksum verification: expected \
             {PYTHON_GUEST_SHA256}, got {actual}. Refusing to install it."
        );
    }

    let final_path = dir.join(PYTHON_GUEST_FILENAME);
    // Process id *and* a per-process counter (the same fix `sandbox.rs`'s
    // compile cache took for audit #127): the pid alone is not unique when
    // several servers share one process, which is how this crate is hosted on
    // Android. Two concurrent `install=true` calls would otherwise interleave
    // writes into a single temp file and race each other's rename.
    let tmp = dir.join(download_tmp_name());
    std::fs::write(&tmp, &bytes).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, &final_path).with_context(|| {
        format!(
            "failed to move the verified guest into place at {}",
            final_path.display()
        )
    })?;

    Ok(final_path)
}

/// Test-only helpers for the env vars this module reads.
///
/// Environment variables are process-global, so any test that mutates one
/// races every other test in the binary — including tests in *other* modules.
/// `env_lock()` serializes them; `EnvGuard` restores the previous value on
/// drop so a failure can't leak state into whatever runs next.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        // A poisoned lock just means some other test panicked while holding
        // it; the env vars are still restored by that test's guards, so
        // recovering is correct here rather than cascading the failure.
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Drives a future to completion on a throwaway runtime.
    ///
    /// Lets an env-mutating test stay a plain `#[test]`: the env lock is then
    /// held across a *blocking* call rather than across an `.await` inside an
    /// async fn, which is both clearer and avoids the deadlock hazard clippy
    /// (correctly) warns about for `MutexGuard` held across await points.
    pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime should build")
            .block_on(future)
    }

    pub(crate) struct EnvGuard {
        key: String,
        previous: Option<String>,
    }

    impl EnvGuard {
        pub(crate) fn set(key: &str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self {
                key: key.to_string(),
                previous,
            }
        }

        pub(crate) fn unset(key: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::remove_var(key);
            Self {
                key: key.to_string(),
                previous,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{block_on, env_lock, EnvGuard};
    use super::*;

    /// Two concurrent installs in one process must not collide on one temp
    /// file. The name used to be the pid alone, which is identical for every
    /// in-process server — the hosting model Android uses.
    #[test]
    fn concurrent_downloads_do_not_share_a_temp_filename() {
        let names: std::collections::HashSet<String> =
            (0..64).map(|_| download_tmp_name()).collect();
        assert_eq!(
            names.len(),
            64,
            "every in-flight download needs its own file"
        );

        // Still recognisably the guest's temp file, and still not mistakable
        // for the finished artifact `find_python_guest` looks for.
        let one = download_tmp_name();
        assert!(one.starts_with(PYTHON_GUEST_FILENAME));
        assert_ne!(one, PYTHON_GUEST_FILENAME);
    }

    /// The ceiling has to leave room for the real file, or a correct download
    /// is rejected before it can even be checksummed.
    #[test]
    fn the_download_ceiling_admits_the_pinned_guest() {
        const { assert!(DOWNLOAD_MAX_BYTES > PYTHON_GUEST_BYTES) };
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_hex_is_always_64_lowercase_hex_chars() {
        let digest = sha256_hex(&[0u8, 1, 2, 255]);
        assert_eq!(digest.len(), 64);
        assert!(digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn verify_guest_rejects_a_file_that_is_not_the_pinned_build() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("python.wasm");
        std::fs::write(&fake, b"not really cpython").unwrap();
        let err = verify_guest(&fake).unwrap_err().to_string();
        assert!(err.contains("checksum mismatch"), "got: {err}");
    }

    #[test]
    fn verify_guest_reports_a_missing_file_clearly() {
        let err = verify_guest(Path::new("/nope/does/not/exist.wasm"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("failed to read guest"), "got: {err}");
    }

    #[test]
    fn status_reports_unavailable_without_panicking_when_nothing_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = env_lock();
        // Both vars must be neutralized. Pointing `KITTY_WASM_DATA_DIR` at an
        // empty dir is not enough on its own: `KITTY_WASM_PYTHON` takes
        // priority in `find_python_guest`, so a developer who has it set —
        // which is the documented way to use a local guest — would otherwise
        // see this test fail for a reason that has nothing to do with it.
        let _data = EnvGuard::set("KITTY_WASM_DATA_DIR", dir.path().to_str().unwrap());
        let _no_override = EnvGuard::unset("KITTY_WASM_PYTHON");

        let status = python_guest_status();
        assert_eq!(status["available"], serde_json::json!(false));
        assert_eq!(
            status["pinned"]["sha256"],
            serde_json::json!(PYTHON_GUEST_SHA256)
        );
        assert!(status["install_path"].as_str().unwrap().contains("guests"));
    }

    #[test]
    fn status_reports_an_env_override_that_points_at_nothing() {
        let _lock = env_lock();
        let _bad = EnvGuard::set("KITTY_WASM_PYTHON", "/definitely/not/here.wasm");
        let status = python_guest_status();
        assert_eq!(status["available"], serde_json::json!(false));
        assert_eq!(status["env_override_valid"], serde_json::json!(false));
    }

    #[test]
    fn ensure_guest_without_download_permission_explains_how_to_fix_it() {
        let dir = tempfile::tempdir().unwrap();
        let err = {
            let _lock = env_lock();
            let _data = EnvGuard::set("KITTY_WASM_DATA_DIR", dir.path().to_str().unwrap());
            let _no_override = EnvGuard::unset("KITTY_WASM_PYTHON");
            block_on(ensure_python_guest(false))
                .unwrap_err()
                .to_string()
        };
        assert!(err.contains("not installed"), "got: {err}");
        assert!(
            err.contains("install=true"),
            "must say how to fix it: {err}"
        );
        assert!(
            err.contains(PYTHON_GUEST_SHA256),
            "must state the pin: {err}"
        );
    }

    #[test]
    fn ensure_guest_reports_a_broken_env_override_rather_than_falling_back() {
        let err = {
            let _lock = env_lock();
            let _bad = EnvGuard::set("KITTY_WASM_PYTHON", "/definitely/not/here.wasm");
            block_on(ensure_python_guest(false))
                .unwrap_err()
                .to_string()
        };
        // A configured-but-wrong path is a misconfiguration to surface, not
        // something to silently paper over with the managed guest.
        assert!(err.contains("KITTY_WASM_PYTHON"), "got: {err}");
    }
}
