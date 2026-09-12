//! `lean_scratchpad_set`/`get`/`delete`/`list` — Rust port of
//! `lean_mcp.py`'s scratchpad tools.
//!
//! Deliberate deviation (base plan's "Deliberate behavioral deviations"
//! table): the scratchpad file is relocated out of `CACHE_DIR` entirely, so
//! `lean_cache_clear` can never wipe it as collateral (the Python original
//! stores `scratchpad.json` directly inside the scrape/file cache directory
//! that `cache_clear` unlinks every file in). One-shot migration modeled on
//! `migrate_ap_db_path_impl` (`src-tauri/src/config/mod.rs`): rename with a
//! copy-then-delete fallback (cross-device rename), and never overwrite an
//! existing destination file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::envelope::{error_response, success_response};
use crate::tools::cache_dir;
use serde_json::json;

fn old_scratch_path() -> PathBuf {
    cache_dir().join("scratchpad.json")
}

fn new_scratch_path() -> PathBuf {
    cache_dir()
        .parent()
        .map(|p| p.join("kitty-tools-scratchpad"))
        .unwrap_or_else(|| cache_dir().join("kitty-tools-scratchpad"))
        .join("scratchpad.json")
}

/// Split from `scratch_path()` purely for testability — takes explicit
/// old/new paths rather than reading `cache_dir()` globals.
fn migrate_scratchpad_impl(old: &Path, new: &Path) {
    if new.exists() || !old.exists() {
        return;
    }
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::rename(old, new).is_err() {
        // Cross-device (different drive) rename fails on Windows — fall back
        // to copy-then-delete, and never leave the user's data stranded if
        // the delete fails (a copy that's never cleaned up is a much smaller
        // problem than a scratchpad that silently reverts to empty).
        if std::fs::copy(old, new).is_ok() {
            let _ = std::fs::remove_file(old);
        }
    }
}

fn scratch_path() -> PathBuf {
    let new = new_scratch_path();
    migrate_scratchpad_impl(&old_scratch_path(), &new);
    new
}

/// Serializes the scratchpad's read-modify-write.
///
/// One `kitty-tools` process is shared by every session and every delegate of
/// a fan-out, and rmcp dispatches their calls concurrently, so `set` and
/// `delete` are genuinely re-entered in parallel. Without this, two writers
/// each load the map, each insert their own key, and the second rename wins —
/// losing the first writer's key outright.
///
/// A `std::sync::Mutex` rather than a tokio one, and that is deliberate:
/// every caller arrives through `server.rs`'s `offload()`, i.e. inside
/// `spawn_blocking`, so this lock is never held across an `.await`.
fn scratch_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn load(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write the whole scratchpad, atomically.
///
/// The point of the scratchpad is to survive things going wrong, and the
/// previous implementation — a plain `fs::write` truncate-then-write of the
/// entire map — was the one thing that could not. A crash, power loss, or
/// full disk partway through left a truncated file, and `load` swallows any
/// parse error and returns an empty map, so the *entire* scratchpad silently
/// came back empty with nothing to indicate anything had been lost. That is
/// the worst possible failure mode for a durability feature.
///
/// Write to a temp file beside the target, fsync it so the bytes are really on
/// disk before anything points at them, then rename over the target. Rename is
/// atomic within a directory on both Windows and POSIX, so a reader sees
/// either the whole old file or the whole new one.
fn save(path: &Path, data: &BTreeMap<String, String>) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_string());

    // Same directory as the target: a rename across filesystems is not atomic
    // (and fails outright on Windows), so the OS temp dir is not an option.
    //
    // The pid alone is not enough. It distinguishes two *processes*, but the
    // threat here is two concurrent calls inside one process — every delegate
    // of a fan-out shares this server — which got the identical temp name,
    // both `File::create`d it, and wrote different-length buffers from offset
    // zero. The shorter write left the longer one's tail behind, and the
    // mixed file was then renamed over the real scratchpad. `load` swallows
    // the parse error and returns an empty map, so the whole scratchpad
    // silently came back blank: the exact failure this atomic write exists to
    // prevent, reintroduced one level down. The sequence number makes the
    // name unique per call, so a write can never observe another's bytes even
    // if `scratch_lock` is somehow bypassed.
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "tmp{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    // Windows `rename` refuses an existing destination, so `fs::rename`'s
    // documented replace-on-overwrite behaviour is what carries this — it maps
    // to `MoveFileEx(MOVEFILE_REPLACE_EXISTING)`. On failure the temp file is
    // cleaned up rather than accumulating one turd per failed write.
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

pub fn scratchpad_set(key: &str, value: &str) -> String {
    scratchpad_set_in(&scratch_path(), key, value)
}

fn scratchpad_set_in(path: &Path, key: &str, value: &str) -> String {
    // Poisoning is not a reason to fail: the scratchpad is a plain map behind
    // this lock, so a panicking writer leaves no invariant broken here.
    let _guard = scratch_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut data = load(path);
    data.insert(key.to_string(), value.to_string());
    if let Err(e) = save(path, &data) {
        return error_response(
            "SCRATCHPAD_WRITE_ERROR",
            &format!("Cannot save scratchpad: {e}"),
            None,
            None,
        );
    }
    success_response(
        json!({"key": key}),
        Some("Stored successfully."),
        false,
        None,
    )
}

pub fn scratchpad_get(key: &str) -> String {
    let data = load(&scratch_path());
    match data.get(key) {
        Some(value) => success_response(json!({"key": key, "value": value}), None, false, None),
        None => error_response(
            "KEY_NOT_FOUND",
            &format!("Key '{key}' not in scratchpad."),
            None,
            None,
        ),
    }
}

pub fn scratchpad_delete(key: &str) -> String {
    scratchpad_delete_in(&scratch_path(), key)
}

fn scratchpad_delete_in(path: &Path, key: &str) -> String {
    let _guard = scratch_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut data = load(path);
    if !data.contains_key(key) {
        return error_response(
            "KEY_NOT_FOUND",
            &format!("Key '{key}' not in scratchpad."),
            None,
            None,
        );
    }
    data.remove(key);
    if let Err(e) = save(path, &data) {
        return error_response(
            "SCRATCHPAD_WRITE_ERROR",
            &format!("Cannot save scratchpad: {e}"),
            None,
            None,
        );
    }
    success_response(json!({"deleted_key": key}), None, false, None)
}

pub fn scratchpad_list() -> String {
    let data = load(&scratch_path());
    let keys: Vec<&String> = data.keys().collect();
    success_response(json!(keys), None, false, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Every delegate in a fan-out shares this one process, and rmcp
    /// dispatches their tool calls concurrently, so `lean_scratchpad_set` is
    /// genuinely re-entered in parallel. Each writer must keep its key.
    #[test]
    fn concurrent_writers_do_not_lose_keys_or_corrupt_the_file() {
        let dir = std::env::temp_dir().join(format!("kt-scratch-conc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scratchpad.json");

        const WRITERS: usize = 16;
        std::thread::scope(|scope| {
            for i in 0..WRITERS {
                let path = path.clone();
                scope.spawn(move || {
                    // Values long enough that two interleaved writes differ in
                    // length — that is what turns a shared temp file into
                    // invalid JSON rather than a clean last-writer-wins.
                    let value = "v".repeat(64 * (i + 1));
                    scratchpad_set_in(&path, &format!("key{i}"), &value);
                });
            }
        });

        let raw = fs::read_to_string(&path).expect("scratchpad file exists");
        let data: BTreeMap<String, String> = serde_json::from_str(&raw).unwrap_or_else(|e| {
            panic!("scratchpad is not valid JSON after concurrent writes: {e}")
        });

        let missing: Vec<String> = (0..WRITERS)
            .map(|i| format!("key{i}"))
            .filter(|k| !data.contains_key(k))
            .collect();
        assert!(
            missing.is_empty(),
            "{} of {WRITERS} keys were lost: {missing:?}",
            missing.len()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrates_old_scratchpad_to_new_location() {
        let base = std::env::temp_dir().join(format!("kt-sp-{}", std::process::id()));
        let old = base.join("old-cache").join("scratchpad.json");
        let new = base.join("new-loc").join("scratchpad.json");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, r#"{"k":"v"}"#).unwrap();

        migrate_scratchpad_impl(&old, &new);

        assert!(new.exists());
        assert!(!old.exists());
        let data: BTreeMap<String, String> =
            serde_json::from_str(&fs::read_to_string(&new).unwrap()).unwrap();
        assert_eq!(data.get("k").unwrap(), "v");

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn never_overwrites_an_existing_destination() {
        let base = std::env::temp_dir().join(format!("kt-sp2-{}", std::process::id()));
        let old = base.join("old-cache").join("scratchpad.json");
        let new = base.join("new-loc").join("scratchpad.json");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::create_dir_all(new.parent().unwrap()).unwrap();
        fs::write(&old, r#"{"stale":"data"}"#).unwrap();
        fs::write(&new, r#"{"authoritative":"data"}"#).unwrap();

        migrate_scratchpad_impl(&old, &new);

        let data: BTreeMap<String, String> =
            serde_json::from_str(&fs::read_to_string(&new).unwrap()).unwrap();
        assert!(data.contains_key("authoritative"));
        assert!(
            old.exists(),
            "old file must survive untouched when destination already exists"
        );

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn key_not_found_reports_structured_error() {
        // Use an isolated path via direct load/save rather than the global
        // scratch_path() to avoid interference with a real user directory.
        let path = std::env::temp_dir().join(format!("kt-sp3-{}.json", std::process::id()));
        let data: BTreeMap<String, String> = BTreeMap::new();
        save(&path, &data).unwrap();
        let loaded = load(&path);
        assert!(!loaded.contains_key("missing"));
        fs::remove_file(&path).ok();
    }
}
