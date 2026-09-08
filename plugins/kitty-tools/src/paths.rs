//! Path resolution matching Python's `Path(path).resolve()` — which
//! normalizes `.`/`..` components and makes a relative path absolute against
//! the current working directory *without requiring the path to exist*.
//! `std::fs::canonicalize` isn't a drop-in replacement: it requires every
//! component to exist and resolves symlinks, which the "does this path
//! exist" check that immediately follows every call site here (matching
//! `lean_mcp.py`'s `if not resolved.exists(): return error_response(...)`)
//! needs to run on the resolved-but-possibly-missing path itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The environment variable a host sets to tell this crate where **its own
/// storage** goes — the scratchpad, the extract-once document cache.
///
/// It exists for Android, where none of the usual answers work: the app
/// process has no useful `$HOME` (bionic's `getpwuid` reports `/data`), and
/// the working directory is `/`. The BigTiny V2 daemon also sets it per-app
/// (`mcp::manager::scoped_env`) so two apps sharing this server do not share
/// one scratchpad.
///
/// **This is a storage root, not an authorization boundary.** It used to be
/// both, and that conflation was a bug with teeth: once the daemon began
/// handing each app its own `apps/<id>/plugin-home`, that narrow per-app
/// directory silently became the only tree the file tools would read. The
/// session's own chat folder — which the daemon explicitly allows, and which
/// the system prompt tells the model its attached files live in — was then
/// rejected as outside "home", so a model asked to read an attached document
/// could not open it by any route. Authorization now lives in
/// [`allowed_roots`]; this variable no longer decides what may be touched.
pub const PLUGIN_HOME_ENV: &str = "KITTY_PLUGIN_HOME";

/// Directories this process may read and write, beyond [`home_dir`].
///
/// Set by the daemon at spawn (`mcp::manager::scoped_env`) and holding the
/// parts of the grant set that never change for the life of the process: the
/// user's home directory, the OS temp directory, and the daemon's own data
/// root. Parsed with `std::env::split_paths`, so it uses the platform's own
/// `PATH` convention (`;` on Windows, `:` elsewhere) and drive letters survive.
pub const ALLOWED_DIRS_ENV: &str = "KITTY_ALLOWED_DIRS";

/// Path to a JSON array of additional allowed directories, re-read as it
/// changes.
///
/// This carries the *volatile* half of the grant set — the files the user
/// attached to a turn and the working folders they chose, which are per
/// session and accumulate mid-run. They cannot travel in the environment:
/// a stdio MCP server is one long-lived process shared by every session, and
/// its environment is fixed at spawn. A file the daemon rewrites and this
/// process re-reads is the cheapest channel that stays correct.
///
/// Read through an mtime guard, so the steady-state cost is one `stat` per
/// containment check rather than a parse.
pub const ALLOWED_DIRS_FILE_ENV: &str = "KITTY_ALLOWED_DIRS_FILE";

/// The user's home directory, resolved once per process, or `None` when it
/// genuinely cannot be determined.
///
/// This is the crate's **storage** root (scratchpad, document cache) and the
/// fallback authorization root when the host supplies no explicit grant set.
/// `KITTY_PLUGIN_HOME` wins, then `%USERPROFILE%` (Windows) or `$HOME`, then
/// `dirs::home_dir`. The daemon is the primary path-containment gate; these
/// helpers are defense-in-depth. See [`allowed_roots`] for what may be
/// *touched*, which is a wider and more volatile set.
///
/// **There is deliberately no working-directory fallback.** There used to be,
/// and it silently inverted the boundary this module exists to enforce: on a
/// host where none of the above resolve, the working directory can be `/`, so
/// `path_within_allowed` compared every path against the filesystem root and
/// answered `true` for all of them. A boundary that cannot be located must
/// reject, not wave everything through — see `path_within_allowed`.
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
/// An undeterminable set of allowed roots rejects everything - see
/// [`allowed_roots`].
/// Canonicalized form of one allowed root, memoized for the process lifetime.
///
/// `canonicalize` is a filesystem round-trip, and this runs once per root on
/// every tool call's boundary check - so with home, temp, the daemon data root
/// and a handful of attachments granted, an unmemoized version would stat the
/// disk a dozen times per call.
///
/// Only a *successful* canonicalization is cached, and the fallback
/// deliberately is not. `canonicalize` fails when the directory does not exist
/// yet, and on Windows it is also what rewrites `C:\...` into the verbatim
/// `\?\C:\...` form. `within_home_of_canon` always canonicalizes the
/// candidate, and `path_is_within` is a plain string prefix test with no
/// verbatim-prefix handling - so a cached un-canonicalized base would compare
/// `\?\c:\users\me\f.txt` against `c:\users\me`, fail, and reject every path
/// for the remaining life of the process. Not caching the fallback means the
/// next call re-canonicalizes and recovers on its own once the directory
/// appears.
fn canon_of(root: &Path) -> PathBuf {
    static CANON: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();
    let cache = CANON.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(hit) = guard.get(root) {
        return hit.clone();
    }
    match std::fs::canonicalize(root) {
        Ok(canon) => {
            guard.insert(root.to_path_buf(), canon.clone());
            canon
        }
        Err(_) => root.to_path_buf(),
    }
}

pub fn allowed_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = home_dir() {
        roots.push(home);
    }
    if let Some(raw) = std::env::var_os(ALLOWED_DIRS_ENV) {
        // `split_paths` rather than a hand-rolled split on ';' or ':' —
        // on Windows the latter would cut every path in half at its drive
        // letter.
        roots.extend(std::env::split_paths(&raw).filter(|p| !p.as_os_str().is_empty()));
    }
    roots.extend(grant_file_roots());
    roots.sort();
    roots.dedup();
    roots
}

/// The volatile grants, re-read when the file changes. See
/// [`ALLOWED_DIRS_FILE_ENV`].
///
/// Every failure — unset, missing, unreadable, malformed — yields no extra
/// roots rather than an error. The daemon has already run the authoritative
/// per-session containment check by the time a call reaches this process, so
/// a stale or absent grants file costs a false rejection, never a false
/// admission.
fn grant_file_roots() -> Vec<PathBuf> {
    /// The grants file's identity as last read, with what it contained. Length
    /// as well as mtime: a rewrite inside the filesystem's mtime granularity
    /// (1-2s on some Windows volumes) is exactly the case a mtime-only guard
    /// would serve stale, and this file is rewritten mid-turn when the user
    /// attaches something.
    struct Cached {
        mtime: std::time::SystemTime,
        len: u64,
        roots: Vec<PathBuf>,
    }
    static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();

    let Some(path) = std::env::var_os(ALLOWED_DIRS_FILE_ENV) else {
        return Vec::new();
    };
    let path = PathBuf::from(path);
    let Ok(meta) = std::fs::metadata(&path) else {
        return Vec::new();
    };
    let stamp = (meta.modified().unwrap_or(std::time::UNIX_EPOCH), meta.len());

    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = match cache.lock() {
        Ok(g) => g,
        // A panic in another thread poisoned it; re-read rather than give up
        // the grants entirely.
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(hit) = guard.as_ref() {
        if (hit.mtime, hit.len) == stamp {
            return hit.roots.clone();
        }
    }

    let roots = std::fs::read_to_string(&path)
        .ok()
        .map(|text| parse_grants(&text))
        .unwrap_or_default();
    *guard = Some(Cached {
        mtime: stamp.0,
        len: stamp.1,
        roots: roots.clone(),
    });
    roots
}

/// The grants file's payload: a flat JSON array of directory (or exact file)
/// paths. Split out from the I/O so the parse is testable without a fixture
/// on disk and without touching this process's environment.
///
/// Malformed input yields no roots rather than an error, for the reason given
/// on [`ALLOWED_DIRS_FILE_ENV`]: this is the second gate, so losing grants
/// costs a spurious rejection, never an unauthorized read.
fn parse_grants(text: &str) -> Vec<PathBuf> {
    serde_json::from_str::<Vec<String>>(text)
        .map(|entries| {
            entries
                .into_iter()
                .filter(|e| !e.trim().is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// A hint naming the directories a rejected call *could* have used.
///
/// The old message said only that a path was "outside the HOME directory",
/// which was both untrue (the boundary is a set of roots, not one home) and
/// useless: a model told where it may not go, and not where it may, retries.
/// A captured session shows exactly that — a dozen tool calls, each guessing a
/// different path, none of them the allowed one, and the user's attached
/// documents never read.
pub fn allowed_roots_hint() -> String {
    let roots = allowed_roots();
    if roots.is_empty() {
        return "No directory is currently readable; the host granted none.".to_string();
    }
    // A handful, not all of them: the tail is cache and scratch machinery that
    // is true but useless to suggest.
    let listed: Vec<String> = roots
        .iter()
        .take(3)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .collect();
    format!("Readable directories include: {}.", listed.join(", "))
}

/// True when `path` resolves inside **any** allowed root.
pub fn path_within_allowed(path: &Path) -> bool {
    let roots = allowed_roots();
    // Nothing to be inside of. Fail closed.
    if roots.is_empty() {
        return false;
    }
    // Canonicalized because symlinked components and Windows 8.3 short names
    // would otherwise make a contained path look outside its own root.
    roots
        .iter()
        .any(|root| within_home_of_canon(&canon_of(root), path))
}

/// The containment test against an explicit home, split out so the
/// fail-closed behaviour can be tested directly rather than by trying to
/// convince the process it has no home directory.
#[cfg(test)]
fn within_home_of(home: Option<&Path>, path: &Path) -> bool {
    // No home means no boundary to be inside of. Fail closed.
    let Some(home) = home else {
        return false;
    };
    let home_canon = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    within_home_of_canon(&home_canon, path)
}

fn within_home_of_canon(home_canon: &Path, path: &Path) -> bool {
    if let Some(anchor) = nearest_existing_ancestor(path) {
        if let Ok(canon) = std::fs::canonicalize(&anchor) {
            return path_is_within(home_canon, &canon);
        }
    }

    path_is_within(home_canon, path)
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
        assert!(path_within_allowed(&home()));
    }

    #[test]
    fn paths_inside_home_are_allowed() {
        let within = home().join("some").join("deeper").join("file.txt");
        assert!(path_within_allowed(&within));
        // Relative-to-home lookalikes must not slip past via case differences.
        assert!(path_within_allowed(&home().join("MixedCase").join("x")));
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
        assert!(!path_within_allowed(&outside));
    }

    /// The grants file is the channel for the volatile half of the grant set
    /// (attached files, chosen working folders) — the half that cannot ride in
    /// the environment, because one stdio server process is shared by every
    /// session and its environment is fixed at spawn.
    #[test]
    fn grants_parse_into_roots() {
        let roots = parse_grants(r#"["C:/Users/me/Documents/Kitty/chats/abc","D:/work"]"#);
        assert_eq!(
            roots,
            vec![
                PathBuf::from("C:/Users/me/Documents/Kitty/chats/abc"),
                PathBuf::from("D:/work"),
            ]
        );
    }

    /// Blank entries are dropped rather than becoming an empty `PathBuf`,
    /// which would canonicalize to the process working directory and quietly
    /// grant it — the exact inversion `home_dir`'s missing cwd fallback exists
    /// to prevent.
    #[test]
    fn blank_grant_entries_are_dropped() {
        assert!(parse_grants(r#"["", "   "]"#).is_empty());
    }

    /// Every malformed shape must lose grants, never invent them.
    #[test]
    fn malformed_grants_yield_nothing() {
        for text in ["", "not json", "{}", "[1,2,3]", "null"] {
            assert!(
                parse_grants(text).is_empty(),
                "{text:?} must not produce roots"
            );
        }
    }

    /// With no host-supplied grants, the allowed set is exactly the home
    /// directory — the behaviour before authorization was split out of
    /// `KITTY_PLUGIN_HOME`, which is what keeps a standalone run of this
    /// server (and Android, where the process is the app) working unchanged.
    #[test]
    fn the_default_allowed_set_is_just_home() {
        if std::env::var_os(ALLOWED_DIRS_ENV).is_some()
            || std::env::var_os(ALLOWED_DIRS_FILE_ENV).is_some()
        {
            return; // A host-configured environment; nothing to assert here.
        }
        assert_eq!(allowed_roots(), vec![home()]);
    }

    /// The regression that motivated the split: a session's chat directory
    /// lives under the user's home, and the system prompt tells the model its
    /// attached files are there. It must be readable.
    #[test]
    fn a_session_chat_directory_is_inside_the_allowed_set() {
        let chat = home()
            .join("Documents")
            .join("Kitty")
            .join("chats")
            .join("20260908_104607-686d64");
        assert!(path_within_allowed(&chat));
        assert!(path_within_allowed(&chat.join("critique.docx")));
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
            assert!(!path_within_allowed(&sibling));
        }
    }
}
