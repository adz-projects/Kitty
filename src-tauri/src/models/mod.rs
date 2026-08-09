//! Local GGUF models on disk (docs/ANDROID.md §5).
//!
//! Kitty no longer manages an inference process, so "do we have a model?" is
//! now a filesystem question rather than an HTTP one. This module owns that
//! question and, from Phase 3, the downloader that answers it.
//!
//! Everything here is deliberately free of `AppHandle` so it stays unit
//! testable — the `#[tauri::command]` wrappers in `commands/models.rs` do the
//! event emitting.

use std::path::{Path, PathBuf};

/// A GGUF present in the models directory.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InstalledModel {
    /// File stem — what the UI shows and what config fields refer to.
    pub id: String,
    /// Filename including the `.gguf` extension.
    pub file: String,
    pub path: String,
    pub size_bytes: u64,
}

/// Every `.gguf` in `dir`, sorted by name.
///
/// A missing or unreadable directory yields an empty list rather than an
/// error: "no models yet" is the normal first-run state, not a failure, and
/// the callers (status computation, Settings) all want to render it as such.
/// Partial downloads (`.part`) are skipped — they aren't loadable yet.
pub fn installed_in(dir: &Path) -> Vec<InstalledModel> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<InstalledModel> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if !path.is_file() {
                return None;
            }
            if !path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("gguf"))
            {
                return None;
            }
            Some(InstalledModel {
                id: path.file_stem()?.to_string_lossy().into_owned(),
                file: path.file_name()?.to_string_lossy().into_owned(),
                path: path.to_string_lossy().into_owned(),
                size_bytes: e.metadata().map(|m| m.len()).unwrap_or(0),
            })
        })
        .collect();
    out.sort_by_key(|m| m.id.to_lowercase());
    out
}

/// Every GGUF in the app's models directory.
pub fn installed() -> Vec<InstalledModel> {
    match crate::config::models_dir() {
        Ok(dir) => installed_in(&dir),
        Err(e) => {
            tracing::warn!("could not resolve the models directory: {e}");
            Vec::new()
        }
    }
}

/// Resolve `name` — a bare id (`Qwen3-Embedding-0.6B-q4_k_m`), a filename, or
/// an absolute path — to a GGUF that actually exists.
///
/// Accepting all three is not sloppiness: config carries ids, the UI carries
/// filenames, and a hand-edited config or a dev override carries a full path.
/// Rejecting two of the three would fail in a way that reads as "the model is
/// missing" when it's really "the string was spelled differently".
pub fn resolve_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let as_path = Path::new(name);
    if as_path.is_absolute() {
        return as_path.is_file().then(|| as_path.to_path_buf());
    }
    installed_in(dir)
        .into_iter()
        .find(|m| m.id.eq_ignore_ascii_case(name) || m.file.eq_ignore_ascii_case(name))
        .map(|m| PathBuf::from(m.path))
}

/// [`resolve_in`] against the app's models directory.
pub fn resolve(name: &str) -> Option<PathBuf> {
    match crate::config::models_dir() {
        Ok(dir) => resolve_in(&dir, name),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str, bytes: usize) {
        std::fs::write(dir.join(name), vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn a_missing_directory_is_empty_not_an_error() {
        assert!(installed_in(Path::new("no-such-dir-anywhere")).is_empty());
    }

    #[test]
    fn only_gguf_files_count_and_they_come_back_sorted() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "zeta.gguf", 3);
        touch(dir.path(), "Alpha.gguf", 5);
        touch(dir.path(), "notes.txt", 1);
        std::fs::create_dir(dir.path().join("subdir.gguf")).unwrap();

        let found = installed_in(dir.path());
        let ids: Vec<&str> = found.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["Alpha", "zeta"]);
        assert_eq!(found[0].size_bytes, 5);
    }

    /// A half-finished download must not read as an installed model — the
    /// engine would try to load it and fail in a much more confusing way.
    #[test]
    fn partial_downloads_are_not_installed_models() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "model.gguf.part", 9);
        assert!(installed_in(dir.path()).is_empty());
    }

    #[test]
    fn resolve_accepts_an_id_a_filename_or_an_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "Qwen3-Embedding-0.6B-q4_k_m.gguf", 1);

        assert!(resolve_in(dir.path(), "Qwen3-Embedding-0.6B-q4_k_m").is_some());
        assert!(resolve_in(dir.path(), "Qwen3-Embedding-0.6B-q4_k_m.gguf").is_some());
        let abs = dir.path().join("Qwen3-Embedding-0.6B-q4_k_m.gguf");
        assert!(resolve_in(dir.path(), &abs.to_string_lossy()).is_some());

        assert!(resolve_in(dir.path(), "something-else").is_none());
        assert!(resolve_in(dir.path(), "  ").is_none());
    }

    /// Windows paths are case-insensitive and users retype these by hand.
    #[test]
    fn resolution_is_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "LFM2.5-1.2B-Instruct-Q4_K_M.gguf", 1);
        assert!(resolve_in(dir.path(), "lfm2.5-1.2b-instruct-q4_k_m").is_some());
    }
}
