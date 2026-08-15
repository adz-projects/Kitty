//! Path containment for the `workspace` mount (audit #111).
//!
//! The `workspace` tool argument mounts a host directory **read-write** into
//! the guest at `/work`. Before this check it accepted *any* existing
//! directory — `C:\`, `C:\Windows\System32`, another user's profile — giving
//! model-written guest code read-write run of the host. The policy here is
//! the same one `kitty-tools` applies to every file tool (`paths.rs` there,
//! ported rather than shared: these ship as separate frozen binaries, and a
//! cross-crate dependency for one function would drag the wrong way — see
//! `envelope.rs`'s duplication note in kitty-web for the same reasoning):
//! the workspace must resolve to a location inside the user's home
//! directory. The daemon is the primary path-containment gate; this is
//! defense-in-depth for the crate's own tool surface.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The user's home directory, resolved once per process. From
/// `%USERPROFILE%` (Windows) or `$HOME`, falling back to `dirs::home_dir`,
/// then the current working directory as a last resort so the boundary
/// stays deterministic even in a stripped-down environment.
pub fn home_dir() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        std::env::var("USERPROFILE")
            .ok()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var("HOME").ok().filter(|p| !p.is_empty()).map(PathBuf::from))
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    })
    .clone()
}

/// True when `path` resolves to a location inside the user's home
/// directory. Mirrors kitty-tools' `path_within_home`: canonicalize the
/// nearest existing ancestor (the workspace itself always exists — callers
/// check `is_dir` first) so symlinked components and Windows 8.3 short-name
/// segments can't alias their way across the boundary, with a lexical
/// fallback for paths that don't exist yet.
pub fn path_within_home(path: &Path) -> bool {
    let home = home_dir();
    let home_canon = std::fs::canonicalize(&home).unwrap_or_else(|_| home.clone());

    if let Some(anchor) = nearest_existing_ancestor(path) {
        if let Ok(canon) = std::fs::canonicalize(&anchor) {
            return path_is_within(&home_canon, &canon);
        }
    }

    path_is_within(&home_canon, path)
}

/// The deepest ancestor of `path` (including `path` itself) that exists on
/// disk, or `None` if even the root-most segment is unavailable.
fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Case-insensitive (on Windows) "is `candidate` equal to `base` or strictly
/// inside it" check; on Windows the boundary is a trailing `\` so a sibling
/// like `C:\Users\alice2` can't alias `C:\Users\alice`.
#[cfg(windows)]
fn path_is_within(base: &Path, candidate: &Path) -> bool {
    fn norm(p: &Path) -> String {
        p.to_string_lossy().replace('/', "\\").to_lowercase()
    }
    let base_s = norm(base);
    let candidate_s = norm(candidate);
    candidate_s == base_s || candidate_s.starts_with(&format!("{base_s}\\"))
}

#[cfg(not(windows))]
fn path_is_within(base: &Path, candidate: &Path) -> bool {
    candidate == base || candidate.starts_with(base.as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_dir_is_inside_itself() {
        assert!(path_within_home(&home_dir()));
    }

    #[test]
    fn paths_inside_home_are_allowed() {
        let within = home_dir().join("some").join("deeper").join("workspace");
        assert!(path_within_home(&within));
        // Case differences must not slip past the boundary on Windows.
        assert!(path_within_home(&home_dir().join("MixedCase").join("x")));
    }

    #[test]
    fn absolute_paths_outside_home_are_rejected() {
        #[cfg(windows)]
        let outside = PathBuf::from("C:\\Windows\\system32");
        #[cfg(not(windows))]
        let outside = PathBuf::from("/etc");
        assert!(!path_within_home(&outside));
    }

    #[test]
    fn sibling_directory_does_not_count_as_home() {
        // `C:\Users\alice2` must not be treated as inside `C:\Users\alice`.
        let base = home_dir();
        let mut sibling = base.clone();
        if let Some(name) = base.file_name().map(|n| n.to_string_lossy().into_owned()) {
            sibling = base.parent().unwrap().join(format!("{name}2"));
        }
        if sibling != base {
            assert!(!path_within_home(&sibling));
        }
    }
}
