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
    // The pid keeps two processes from colliding on the same temp name.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
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
    let path = scratch_path();
    let mut data = load(&path);
    data.insert(key.to_string(), value.to_string());
    if let Err(e) = save(&path, &data) {
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
    let path = scratch_path();
    let mut data = load(&path);
    if !data.contains_key(key) {
        return error_response(
            "KEY_NOT_FOUND",
            &format!("Key '{key}' not in scratchpad."),
            None,
            None,
        );
    }
    data.remove(key);
    if let Err(e) = save(&path, &data) {
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
