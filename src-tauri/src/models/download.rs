//! Resumable GGUF download (docs/ANDROID.md §5.2).
//!
//! Deliberately `AppHandle`-free — every step takes a path, a byte stream and
//! a progress callback, so the whole state machine is unit testable without a
//! Tauri runtime (the convention `commands/session/crud.rs` states explicitly).
//! `commands/models.rs` is the thin shell that emits events.
//!
//! **HuggingFace only.** The Ollama registry (manifest walk, blob concat,
//! gzip-layer detection) was cut from scope when managed Ollama was retired —
//! every model in §9 resolves through an HF `resolve` URL.
//!
//! The shape, and why each piece exists:
//!
//! - `<model>.gguf.part` + a `.meta` sidecar, so a resumed run can tell an
//!   interrupted download of *this* file from a stale fragment of another.
//! - `Range: bytes=<len>-` from the existing `.part` length.
//! - sha256 verified over the finished file, then an atomic rename into place.
//!   A half-written `.gguf` would be indistinguishable from a good one to
//!   every later caller, so the file only takes its real name once verified.

use std::io::Write;
use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};

/// Refuse a download unless free space is this multiple of the expected size
/// (D7). Downloading into a nearly-full disk fails late and messily; failing
/// up front is both kinder and cheaper.
const FREE_SPACE_FACTOR: f64 = 1.5;

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("not enough disk space: {needed_gb:.1} GB required (1.5x the model), {free_gb:.1} GB free")]
    NotEnoughSpace { needed_gb: f64, free_gb: f64 },
    #[error("{0} is already installed")]
    AlreadyInstalled(String),
    #[error("download failed: {0}")]
    Transport(String),
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// What to fetch. `sha256` is optional because not every HF repo publishes
/// one; when absent the download still completes, it just isn't verified.
#[derive(Debug, Clone)]
pub struct DownloadSpec {
    pub repo: String,
    pub file: String,
    pub rev: String,
    pub sha256: Option<String>,
    pub expected_size: Option<u64>,
}

impl DownloadSpec {
    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repo.trim_matches('/'),
            if self.rev.is_empty() { "main" } else { &self.rev },
            self.file.trim_start_matches('/')
        )
    }
}

pub fn part_path(dir: &Path, file: &str) -> PathBuf {
    dir.join(format!("{file}.part"))
}

fn meta_path(dir: &Path, file: &str) -> PathBuf {
    dir.join(format!("{file}.part.meta"))
}

/// Bytes already fetched for `file`, or 0.
///
/// The `.meta` sidecar records what the `.part` was fetched *for*. If the
/// source URL or expected size has changed since, the fragment is not a
/// prefix of what we now want — resuming from it would produce a file that
/// fails the checksum after another full download. Discard and start over.
pub fn resume_offset(dir: &Path, file: &str, spec: &DownloadSpec) -> u64 {
    let part = part_path(dir, file);
    let Ok(meta) = std::fs::read_to_string(meta_path(dir, file)) else {
        // A `.part` with no sidecar predates this scheme or was interrupted
        // mid-create; treat it as untrustworthy rather than guessing.
        let _ = std::fs::remove_file(&part);
        return 0;
    };
    if meta.trim() != meta_line(spec) {
        let _ = std::fs::remove_file(&part);
        let _ = std::fs::remove_file(meta_path(dir, file));
        return 0;
    }
    std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0)
}

fn meta_line(spec: &DownloadSpec) -> String {
    format!(
        "{}|{}|{}",
        spec.url(),
        spec.expected_size.unwrap_or(0),
        spec.sha256.as_deref().unwrap_or("")
    )
}

pub fn write_meta(dir: &Path, file: &str, spec: &DownloadSpec) -> std::io::Result<()> {
    std::fs::write(meta_path(dir, file), meta_line(spec))
}

/// What to do with an existing `.part` fragment given the status the server
/// answered the GET with.
#[derive(Debug, PartialEq, Eq)]
pub enum ResumePlan {
    /// Append the body to the fragment: either a fresh download, or a
    /// properly honored `Range` request (206).
    Append,
    /// The server answered a ranged request with something other than 206 —
    /// in practice 200, meaning it ignored `Range` entirely and is about to
    /// send the *full* body. Appending that after the existing fragment
    /// would corrupt the file (a full body glued onto the first N bytes),
    /// so the fragment must be discarded and the download restarted from
    /// byte 0.
    DiscardFragment,
}

/// Decide the fate of the `.part` fragment. `resume_from` is the fragment's
/// current length — `0` means no `Range` header was sent, so any 2xx is a
/// plain full download. Anything non-2xx is a transport failure.
pub fn plan_resume(
    url: &str,
    status: reqwest::StatusCode,
    resume_from: u64,
) -> Result<ResumePlan, DownloadError> {
    if !status.is_success() {
        return Err(DownloadError::Transport(format!("{url} returned {status}")));
    }
    if resume_from > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Ok(ResumePlan::DiscardFragment);
    }
    Ok(ResumePlan::Append)
}

/// Free bytes on the volume holding `dir`.
///
/// `None` when the volume can't be identified — treated as "don't block",
/// since refusing a download because we couldn't measure the disk would be
/// worse than letting the write fail with a real ENOSPC.
pub fn free_space(dir: &Path) -> Option<u64> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .iter()
        .filter(|d| dir.starts_with(d.mount_point()))
        // Longest mount point wins: on a machine with both `C:\` and a volume
        // mounted at `C:\models`, the latter is the one that matters.
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

pub fn check_space(dir: &Path, expected_size: Option<u64>) -> Result<(), DownloadError> {
    let (Some(expected), Some(free)) = (expected_size, free_space(dir)) else {
        return Ok(());
    };
    let needed = (expected as f64 * FREE_SPACE_FACTOR) as u64;
    if free < needed {
        return Err(DownloadError::NotEnoughSpace {
            needed_gb: needed as f64 / 1e9,
            free_gb: free as f64 / 1e9,
        });
    }
    Ok(())
}

/// Append `stream` to the `.part`, reporting cumulative bytes.
///
/// Returns the total size of the `.part` afterwards. The caller is
/// responsible for having written the `.meta` sidecar first — otherwise a
/// crash mid-write leaves a fragment that `resume_offset` will (correctly)
/// throw away.
pub async fn append_stream<S, E>(
    part: &Path,
    resume_from: u64,
    mut stream: S,
    on_progress: &mut (dyn FnMut(u64) + Send),
) -> Result<u64, DownloadError>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(part)?;
    let mut received = resume_from;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| DownloadError::Transport(e.to_string()))?;
        f.write_all(&chunk)?;
        received += chunk.len() as u64;
        on_progress(received);
    }
    f.flush()?;
    Ok(received)
}

/// sha256 of a file, streamed so a multi-GB GGUF never lands in memory.
///
/// Re-read from disk rather than hashed incrementally during the write: with
/// resume, an incremental hash would have to be persisted across runs, and a
/// wrong resumed hash fails *after* a full download with no way to tell which
/// half was bad. One extra sequential read is a cheap price for that.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Verify (when a checksum is known) and atomically rename into place.
///
/// On mismatch the `.part` is deleted, so the caller's single retry starts
/// clean instead of resuming a corrupt fragment forever.
pub fn verify_and_finalize(
    dir: &Path,
    file: &str,
    expected_sha256: Option<&str>,
) -> Result<PathBuf, DownloadError> {
    let part = part_path(dir, file);
    if let Some(expected) = expected_sha256.filter(|s| !s.trim().is_empty()) {
        let actual = sha256_file(&part)?;
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            let _ = std::fs::remove_file(&part);
            let _ = std::fs::remove_file(meta_path(dir, file));
            return Err(DownloadError::ChecksumMismatch {
                expected: expected.to_string(),
                actual,
            });
        }
    }
    let dest = dir.join(file);
    std::fs::rename(&part, &dest)?;
    let _ = std::fs::remove_file(meta_path(dir, file));
    Ok(dest)
}

/// Ask HuggingFace for a file's size and sha256 without downloading it.
///
/// Best-effort: any failure yields `(None, None)` and the download proceeds
/// unverified with no space gate, because a metadata endpoint being down is
/// not a reason to refuse a download the user asked for.
pub async fn head_metadata(client: &reqwest::Client, spec: &DownloadSpec) -> (Option<u64>, Option<String>) {
    let Ok(resp) = client.get(spec.url()).header("Range", "bytes=0-0").send().await else {
        return (None, None);
    };
    // HF returns the true length in `x-linked-size` and the LFS sha256 in
    // `x-linked-etag` (`"sha256:<hex>"`) for LFS-backed files, which every
    // GGUF of interest is. `content-length` on a ranged request is 1 byte, so
    // it is useless here.
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let size = header("x-linked-size").and_then(|v| v.parse().ok());
    let sha = header("x-linked-etag")
        .or_else(|| header("etag"))
        .and_then(|v| {
            let v = v.trim_matches('"');
            v.strip_prefix("sha256:").map(str::to_string)
        });
    (size, sha)
}

/// How many consecutive *unproductive* transport failures to absorb before
/// giving up. Productive ones (bytes advanced since the last failure) don't
/// count — see [`RetryBudget`].
const MAX_STALLED_RETRIES: u32 = 8;

/// The retry policy for a long download that has to survive a network it does
/// not control: a phone moving between Wi-Fi and cellular, through a tunnel,
/// off a flaky AP.
///
/// The rule that matters is that the budget is spent on *stalls*, not on
/// failures. A multi-gigabyte GGUF over cellular can legitimately drop a
/// dozen times and still finish, so counting raw failures would abandon a
/// download that was working. Counting only failures that made no progress
/// distinguishes "the connection is bad" (keep going, the `.part` file grows
/// each time) from "this is not going to work" (same byte offset, over and
/// over).
///
/// Pulled out of `commands::models::run_download` as a value type purely so it
/// can be tested: everything in `commands/` needs an `AppHandle` and a live
/// network, and this is the part that would break silently.
#[derive(Debug)]
pub struct RetryBudget {
    stalled: u32,
    last_offset: u64,
    max_stalled: u32,
}

/// What [`RetryBudget::record_failure`] decided.
#[derive(Debug, PartialEq, Eq)]
pub enum RetryDecision {
    /// Try again after this long.
    RetryAfter(std::time::Duration),
    /// Out of budget — the download made no progress across
    /// `max_stalled` consecutive attempts.
    GiveUp,
}

impl RetryBudget {
    /// `start_offset` is how many bytes were already on disk when the download
    /// began, so a resumed download doesn't count its inherited progress as
    /// forward motion on the first failure.
    pub fn new(start_offset: u64) -> Self {
        Self {
            stalled: 0,
            last_offset: start_offset,
            max_stalled: MAX_STALLED_RETRIES,
        }
    }

    /// Record a transport failure at `offset` (the current `.part` length).
    pub fn record_failure(&mut self, offset: u64) -> RetryDecision {
        if offset > self.last_offset {
            self.stalled = 0;
            self.last_offset = offset;
        } else {
            self.stalled += 1;
            if self.stalled > self.max_stalled {
                return RetryDecision::GiveUp;
            }
        }
        // Exponential, capped at 32s: long enough not to hammer a network
        // that is genuinely down, short enough to stay well inside the
        // foreground service's lifetime.
        RetryDecision::RetryAfter(std::time::Duration::from_secs(
            2u64.pow(self.stalled.min(5)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> DownloadSpec {
        DownloadSpec {
            repo: "acme/models".into(),
            file: "m.gguf".into(),
            rev: "main".into(),
            sha256: None,
            expected_size: Some(100),
        }
    }

    fn stream_of(chunks: Vec<&'static [u8]>) -> impl futures_util::Stream<Item = Result<bytes::Bytes, String>> + Unpin
    {
        Box::pin(futures_util::stream::iter(
            chunks.into_iter().map(|c| Ok(bytes::Bytes::from_static(c))),
        ))
    }

    #[test]
    fn the_url_is_a_huggingface_resolve_url() {
        let s = spec();
        assert_eq!(s.url(), "https://huggingface.co/acme/models/resolve/main/m.gguf");
        let s = DownloadSpec {
            rev: String::new(),
            ..spec()
        };
        assert!(s.url().contains("/resolve/main/"), "an empty rev means main");
    }

    /// Regression (815bugs #5): a resumed download that gets a 200 (server
    /// ignored `Range`) must discard the fragment and restart — appending the
    /// full body after the existing bytes used to corrupt the GGUF.
    #[test]
    fn plan_resume_requires_206_for_a_ranged_request() {
        use reqwest::StatusCode;
        let url = "https://example.invalid/m.gguf";
        // Fresh download: any 2xx appends (there is no fragment).
        assert_eq!(plan_resume(url, StatusCode::OK, 0).ok(), Some(ResumePlan::Append));
        // Honored Range: 206 appends after the fragment.
        assert_eq!(
            plan_resume(url, StatusCode::PARTIAL_CONTENT, 42).ok(),
            Some(ResumePlan::Append)
        );
        // Ignored Range: 200 means a full body is coming — restart, never append.
        assert_eq!(
            plan_resume(url, StatusCode::OK, 42).ok(),
            Some(ResumePlan::DiscardFragment)
        );
        // Non-2xx is a transport failure either way.
        assert!(plan_resume(url, StatusCode::RANGE_NOT_SATISFIABLE, 42).is_err());
        assert!(plan_resume(url, StatusCode::NOT_FOUND, 0).is_err());
    }

    #[tokio::test]
    async fn a_download_resumes_from_the_existing_part() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write_meta(d, "m.gguf", &spec()).unwrap();

        // First attempt is interrupted after one chunk.
        let mut seen = Vec::new();
        append_stream(&part_path(d, "m.gguf"), 0, stream_of(vec![b"hello "]), &mut |n| {
            seen.push(n)
        })
        .await
        .unwrap();
        assert_eq!(seen, vec![6]);

        // Second attempt resumes rather than restarting.
        let offset = resume_offset(d, "m.gguf", &spec());
        assert_eq!(offset, 6, "the part's length is the resume point");
        let total = append_stream(
            &part_path(d, "m.gguf"),
            offset,
            stream_of(vec![b"world"]),
            &mut |_| {},
        )
        .await
        .unwrap();
        assert_eq!(total, 11);

        let out = verify_and_finalize(d, "m.gguf", None).unwrap();
        assert_eq!(std::fs::read_to_string(out).unwrap(), "hello world");
    }

    /// A fragment left over from a *different* source is not a prefix of what
    /// we want. Resuming from it would burn a full download and then fail the
    /// checksum, with nothing pointing at the real cause.
    #[test]
    fn a_part_from_a_different_source_is_discarded_not_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(part_path(d, "m.gguf"), b"stale").unwrap();
        write_meta(d, "m.gguf", &spec()).unwrap();
        assert_eq!(resume_offset(d, "m.gguf", &spec()), 5);

        let moved = DownloadSpec {
            rev: "v2".into(),
            ..spec()
        };
        assert_eq!(resume_offset(d, "m.gguf", &moved), 0);
        assert!(!part_path(d, "m.gguf").exists(), "the stale part is removed");
    }

    #[test]
    fn a_part_with_no_sidecar_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(part_path(dir.path(), "m.gguf"), b"orphan").unwrap();
        assert_eq!(resume_offset(dir.path(), "m.gguf", &spec()), 0);
        assert!(!part_path(dir.path(), "m.gguf").exists());
    }

    #[tokio::test]
    async fn a_checksum_mismatch_deletes_the_part_so_a_retry_starts_clean() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write_meta(d, "m.gguf", &spec()).unwrap();
        append_stream(&part_path(d, "m.gguf"), 0, stream_of(vec![b"data"]), &mut |_| {})
            .await
            .unwrap();

        let err = verify_and_finalize(d, "m.gguf", Some("00deadbeef")).unwrap_err();
        assert!(matches!(err, DownloadError::ChecksumMismatch { .. }), "got {err}");
        assert!(!part_path(d, "m.gguf").exists());
        assert!(!d.join("m.gguf").exists(), "a corrupt file must never take the real name");
    }

    #[tokio::test]
    async fn a_matching_checksum_finalizes_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write_meta(d, "m.gguf", &spec()).unwrap();
        append_stream(&part_path(d, "m.gguf"), 0, stream_of(vec![b"data"]), &mut |_| {})
            .await
            .unwrap();
        // sha256("data")
        let sha = "3a6eb0790f39ac87c94f3856b2dd2c5d110e6811602261a9a923d3bb23adc8b7";
        let out = verify_and_finalize(d, "m.gguf", Some(sha)).unwrap();
        assert_eq!(out, d.join("m.gguf"));
        assert!(!part_path(d, "m.gguf").exists());
    }

    /// Case is not meaningful in a hex digest, and registries disagree about
    /// it — a case-sensitive compare would reject good downloads.
    #[tokio::test]
    async fn checksum_comparison_ignores_case() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write_meta(d, "m.gguf", &spec()).unwrap();
        append_stream(&part_path(d, "m.gguf"), 0, stream_of(vec![b"data"]), &mut |_| {})
            .await
            .unwrap();
        let sha = "3A6EB0790F39AC87C94F3856B2DD2C5D110E6811602261A9A923D3BB23ADC8B7";
        assert!(verify_and_finalize(d, "m.gguf", Some(sha)).is_ok());
    }

    #[test]
    fn the_space_gate_demands_one_and_a_half_times_the_model() {
        let dir = tempfile::tempdir().unwrap();
        // An implausible size no test machine can satisfy.
        let err = check_space(dir.path(), Some(u64::MAX / 2)).unwrap_err();
        assert!(matches!(err, DownloadError::NotEnoughSpace { .. }), "got {err}");
        // A tiny one always passes, and an unknown size never blocks.
        assert!(check_space(dir.path(), Some(1024)).is_ok());
        assert!(check_space(dir.path(), None).is_ok());
    }

    /// The whole point of the budget: a download that keeps advancing can
    /// drop as many times as the network wants it to. This is the Wi-Fi-to-
    /// cellular case, and a naive failure counter would abandon it.
    #[test]
    fn progress_between_failures_refills_the_budget() {
        let mut budget = RetryBudget::new(0);
        let mut offset = 0u64;
        for _ in 0..50 {
            offset += 10_000_000;
            assert!(
                matches!(
                    budget.record_failure(offset),
                    RetryDecision::RetryAfter(_)
                ),
                "a failure that followed real progress must never give up"
            );
        }
    }

    #[test]
    fn repeated_failures_at_the_same_offset_eventually_give_up() {
        let mut budget = RetryBudget::new(0);
        // First failure came after 500 bytes of real progress, so it is not a
        // stall — the download then wedges at that offset.
        assert!(matches!(
            budget.record_failure(500),
            RetryDecision::RetryAfter(_)
        ));
        for i in 0..MAX_STALLED_RETRIES {
            assert!(
                matches!(budget.record_failure(500), RetryDecision::RetryAfter(_)),
                "attempt {i} should still be within budget"
            );
        }
        assert_eq!(budget.record_failure(500), RetryDecision::GiveUp);
    }

    /// A resumed download starts with bytes already on disk. Those are not
    /// progress *this* run made, so the first failure at that same offset has
    /// to count as a stall — otherwise every resume silently gets one free
    /// retry it did not earn.
    #[test]
    fn inherited_bytes_do_not_count_as_progress() {
        let mut budget = RetryBudget::new(1_000);
        for _ in 0..MAX_STALLED_RETRIES {
            assert!(matches!(
                budget.record_failure(1_000),
                RetryDecision::RetryAfter(_)
            ));
        }
        assert_eq!(budget.record_failure(1_000), RetryDecision::GiveUp);
    }

    /// Backoff grows with consecutive stalls and then stops, so a long-dead
    /// network doesn't push the retry interval past the foreground service's
    /// lifetime.
    #[test]
    fn backoff_grows_then_caps() {
        let mut budget = RetryBudget::new(0);
        let mut seen = Vec::new();
        for _ in 0..MAX_STALLED_RETRIES {
            match budget.record_failure(0) {
                RetryDecision::RetryAfter(d) => seen.push(d.as_secs()),
                RetryDecision::GiveUp => panic!("gave up inside the budget"),
            }
        }
        assert_eq!(seen, vec![2, 4, 8, 16, 32, 32, 32, 32]);
    }

    /// Progress resets the backoff too, not just the counter — the next drop
    /// after a good stretch should retry promptly rather than inherit a
    /// 32-second wait from earlier trouble.
    #[test]
    fn progress_resets_the_backoff_as_well_as_the_counter() {
        let mut budget = RetryBudget::new(0);
        for _ in 0..4 {
            let _ = budget.record_failure(0);
        }
        assert_eq!(
            budget.record_failure(9_999),
            RetryDecision::RetryAfter(std::time::Duration::from_secs(1))
        );
    }
}
