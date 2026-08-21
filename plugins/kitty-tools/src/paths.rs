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

/// The environment variable a host sets to tell this crate where the user's
/// files actually live.
///
/// It exists for Android, where none of the usual answers work: the app
/// process has no useful `$HOME` (bionic's `getpwuid` reports `/data`), and
/// the working directory is `/`. Kitty sets this to its resolved app data
/// directory before the daemon starts — see
/// `src-tauri/src/lifecycle/bigtiny_env.rs`. Unset everywhere else, so
/// desktop resolution is unchanged.
pub const PLUGIN_HOME_ENV: &str = "KITTY_PLUGIN_HOME";

/// The user's home directory, resolved once per process, or `None` when it
/// genuinely cannot be determined.
///
/// The daemon (`bigtiny_rust`) is the primary path-containment gate and hands
/// this crate only home-bound environment; these helpers are defense-in-depth.
/// `KITTY_PLUGIN_HOME` wins, then `%USERPROFILE%` (Windows) or `$HOME`, then
/// `dirs::home_dir`.
///
/// **There is deliberately no working-directory fallback.** There used to be,
/// and it silently inverted the boundary this module exists to enforce: on a
/// host where none of the above resolve, the working directory can be `/`, so
/// `path_within_home` compared every path against the filesystem root and
/// answered `true` for all of them. A boundary that cannot be located must
/// reject, not wave everything through — see `path_within_home`.
pub fn home_dir() -> Option<PathBuf> {
    static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOME.get_or_init(|| resolve_home(|key| std::env::var(key).ok()))
        .clone()
}

/// The resolution order itself, taking its environment as a parameter so it
/// is testable without mutating the process (and without fighting
/// `home_dir`'s process-lifetime cache).
fn resolve_home(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    [PLUGIN_HOME_ENV, "USERPROFILE", "HOME"]
        .into_iter()
        .find_map(|key| env(key).filter(|p| !p.trim().is_empty()).map(PathBuf::from))
        .or_else(dirs::home_dir)
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
///
/// An undeterminable home directory rejects everything — see `home_dir`.
pub fn path_within_home(path: &Path) -> bool {
    within_home_of(home_dir().as_deref(), path)
}

/// The containment test against an explicit home, split out so the
/// fail-closed behaviour can be tested directly rather than by trying to
/// convince the process it has no home directory.
fn within_home_of(home: Option<&Path>, path: &Path) -> bool {
    // No home means no boundary to be inside of. Fail closed.
    let Some(home) = home else {
        return false;
    };
    let home_canon = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());

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
        assert_eq!(
            resolved,
            PathBuf::from("C:/definitely/does/not/exist/file.docx")
        );
    }

    fn home() -> PathBuf {
        home_dir().expect("the test host has a home directory")
    }

    #[test]
    fn home_dir_is_inside_itself() {
        assert!(path_within_home(&home()));
    }

    #[test]
    fn paths_inside_home_are_allowed() {
        let within = home().join("some").join("deeper").join("file.txt");
        assert!(path_within_home(&within));
        // Relative-to-home lookalikes must not slip past via case differences.
        assert!(path_within_home(&home().join("MixedCase").join("x")));
    }

    /// The regression that matters most in this module: an undeterminable
    /// home used to fall back to the working directory, which on an Android
    /// app process is `/` — so the boundary compared every path against the
    /// filesystem root and admitted all of them. A boundary that cannot be
    /// located must reject.
    #[test]
    fn an_undeterminable_home_rejects_every_path() {
        for candidate in [
            "/etc/passwd",
            "/",
            "/data/user/0/com.example/files/x.txt",
            r"C:\Users\someone\file.txt",
        ] {
            assert!(
                !within_home_of(None, Path::new(candidate)),
                "{candidate} must be rejected when there is no home to be inside of"
            );
        }
    }

    /// `KITTY_PLUGIN_HOME` is the Android channel; it has to win over the
    /// values that resolve to the wrong place there.
    #[test]
    fn the_plugin_home_override_wins_over_the_usual_sources() {
        let env = |key: &str| match key {
            PLUGIN_HOME_ENV => Some("/data/user/0/com.kitty/files".to_string()),
            "HOME" => Some("/".to_string()),
            "USERPROFILE" => Some(r"C:\Users\someone".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_home(env),
            Some(PathBuf::from("/data/user/0/com.kitty/files"))
        );
    }

    /// An empty or whitespace-only value is not a home directory. Android
    /// sets some of these to the empty string rather than leaving them unset.
    #[test]
    fn blank_environment_values_are_skipped_not_used() {
        let env = |key: &str| match key {
            PLUGIN_HOME_ENV => Some("".to_string()),
            "USERPROFILE" => Some("   ".to_string()),
            "HOME" => Some("/home/real".to_string()),
            _ => None,
        };
        assert_eq!(resolve_home(env), Some(PathBuf::from("/home/real")));
    }

    /// With nothing in the environment, resolution falls through to `dirs`
    /// (present on any real host) — and if even that fails, to `None`, never
    /// to a working-directory guess.
    #[test]
    fn resolution_never_falls_back_to_the_working_directory() {
        let resolved = resolve_home(|_| None);
        assert_eq!(resolved, dirs::home_dir());
        if let Ok(cwd) = std::env::current_dir() {
            assert_ne!(
                resolved,
                Some(cwd),
                "the working directory must never stand in for a home directory"
            );
        }
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
        let base = home();
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
