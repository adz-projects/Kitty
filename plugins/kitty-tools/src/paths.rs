//! Path resolution matching Python's `Path(path).resolve()` — which
//! normalizes `.`/`..` components and makes a relative path absolute against
//! the current working directory *without requiring the path to exist*.
//! `std::fs::canonicalize` isn't a drop-in replacement: it requires every
//! component to exist and resolves symlinks, which the "does this path
//! exist" check that immediately follows every call site here (matching
//! `lean_mcp.py`'s `if not resolved.exists(): return error_response(...)`)
//! needs to run on the resolved-but-possibly-missing path itself.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The user's home directory, resolved once per process.
///
/// The daemon (`bigtiny_rust`) is the primary path-containment gate and hands
/// this crate only home-bound environment; these helpers are defense-in-depth.
/// From `%USERPROFILE%` (Windows) or `$HOME`, falling back to `dirs::home_dir`,
/// then the current working directory as a last resort so the boundary stays
/// deterministic even in a stripped-down environment.
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

/// True when `path` resolves to a location inside the user's home directory.
///
/// Best-effort, two-tier checks:
///
/// 1. Canonicalized ancestor — when `path` or any existing ancestor of it can
///    be canonicalized, that canonical location is authoritative: it resolves
///    symlinked components (the symlink-escape hardening) *and* normalizes
///    Windows 8.3 short-name segments (so a `%TEMP%` pointing at
///    `C:\Users\AZOLKO~1\...` still correctly counts as inside home — the
///    lexical prefix would misjudge it).
/// 2. Lexical fallback — when nothing on the path exists yet (a brand-new
///    write target under a not-yet-created parent chain), fall back to
///    lexical containment of the normalized path. This is what keeps
///    non-existent paths workable, matching `resolve()`'s established
///    behavior; it rejects the obvious escapes (`C:\Windows\...`,
///    `/etc/passwd`, `..`-walks above home).
///
/// Windows path comparisons are case-insensitive; `..`-walks are already
/// collapsed by `resolve()` before callers invoke this.
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

pub fn resolve(path: &str) -> PathBuf {
    let candidate = Path::new(path);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(candidate)
    };
    lexically_normalize(&absolute)
}

fn lexically_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                // Mirror Python's pathlib: pop a normal segment if there is
                // one, otherwise keep the `..` (can't go above a root/prefix).
                match result.components().next_back() {
                    Some(Component::Normal(_)) => {
                        result.pop();
                    }
                    _ => result.push(".."),
                }
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_path_with_dot_segments_normalizes() {
        let resolved = resolve("C:/foo/./bar/../baz.txt");
        assert_eq!(resolved, PathBuf::from("C:/foo/baz.txt"));
    }

    #[test]
    fn relative_path_resolves_against_cwd() {
        let resolved = resolve("baz.txt");
        assert_eq!(resolved, std::env::current_dir().unwrap().join("baz.txt"));
    }

    #[test]
    fn nonexistent_path_still_resolves_without_erroring() {
        let resolved = resolve("C:/definitely/does/not/exist/file.docx");
        assert_eq!(resolved, PathBuf::from("C:/definitely/does/not/exist/file.docx"));
    }

    #[test]
    fn home_dir_is_inside_itself() {
        assert!(path_within_home(&home_dir()));
    }

    #[test]
    fn paths_inside_home_are_allowed() {
        let within = home_dir().join("some").join("deeper").join("file.txt");
        assert!(path_within_home(&within));
        // Relative-to-home lookalikes must not slip past via case differences.
        assert!(path_within_home(&home_dir().join("MixedCase").join("x")));
    }

    #[test]
    fn absolute_paths_outside_home_are_rejected() {
        #[cfg(windows)]
        let outside = PathBuf::from("C:\\Windows\\system32\\drivers\\etc\\hosts");
        #[cfg(not(windows))]
        let outside = PathBuf::from("/etc/passwd");
        assert!(!path_within_home(&outside));
    }

    #[test]
    fn sibling_directory_does_not_count_as_home() {
        // A guard against prefix-sibling aliasing: `C:\Users\alice2` must not
        // be treated as inside `C:\Users\alice`.
        let base = home_dir();
        let mut sibling = base.clone();
        if let Some(name) = base.file_name().map(|n| n.to_string_lossy().into_owned()) {
            sibling = base.parent().unwrap().join(format!("{name}2"));
        }
        // Only meaningful when the sibling is a genuinely different path.
        if sibling != base {
            assert!(!path_within_home(&sibling));
        }
    }
}
