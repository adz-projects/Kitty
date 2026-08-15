use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

static ABSOLUTE_DRIVE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-zA-Z]:/").unwrap());
static HAS_DRIVE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-zA-Z]:").unwrap());

/// Fallback default for BigTiny's own app-data directory (always allowed
/// regardless of mode) — only used where no real data dir is available
/// (e.g. tests). The actual daemon always threads the real one through from
/// `RunOptions::data_dir` (respects `BIGTINY_DATA_DIR`, see
/// `src/bin/bigtiny_daemon.rs`) via `Agent`/`AgentLoop`, not this constant.
pub const CACHE_DIR: &str = "~/.bigtiny";

/// Directory where kitty-web's `lean_web_search` offloads full result sets
/// (`search-<id>.json`) and `lean_web_scrape` caches downloaded PDFs. Both
/// live under the user's home `.cache`, which is *outside* a session's
/// chat_dir/`cache_dir`/cwd — so without an explicit allowance the model
/// reaching for those files with a path-arg tool (`lean_file_read`) would be
/// force-escalated to a human approval every time. These are app-owned cache
/// dirs (Kitty's own bundled plugins wrote the files, on a predictable fixed
/// path), so reads there are always legitimate; treat them like the data-root
/// `cache_dir`. Both constants must stay in sync with kitty-web's
/// `search_store_dir`/`scrape::cache_dir`.
pub const SEARCH_OFFLOAD_DIR: &str = ".cache/kitty-search-offload";
pub const LEAN_CACHE_DIR: &str = ".cache/lean-goose-mcp";

fn norm(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    while p.ends_with('/') && p.len() > 1 {
        p.pop();
    }
    // Case-fold only where the filesystem itself is case-insensitive.
    // Lowercasing unconditionally was fail-OPEN on case-sensitive hosts
    // (Android/Linux): `/home/User/x` and `/home/user/x` are different
    // directories there, but compared equal after folding — a path outside
    // the allowed set could pass containment by differing only in case.
    if cfg!(windows) {
        p.to_lowercase()
    } else {
        p
    }
}

/// True if `target` lexically resolves inside at least one of `bases`.
pub fn path_within_any(bases: &[String], target: &str) -> bool {
    let t = target.replace('\\', "/");
    let is_absolute = ABSOLUTE_DRIVE_RE.is_match(&t) || t.starts_with("/");

    for base in bases {
        if base.is_empty() {
            continue;
        }
        let b = norm(base);
        let candidate = if is_absolute {
            t.clone()
        } else {
            format!("{b}/{t}")
        };

        let has_drive = HAS_DRIVE_RE.is_match(&candidate);
        let (drive, rest) = if has_drive {
            (&candidate[..2], &candidate[2..])
        } else {
            let d = &candidate[..0];
            let r = &candidate[..];
            (d, r)
        };

        let mut stack: Vec<&str> = Vec::new();
        for seg in rest.split("/") {
            match seg {
                "" | "." => continue,
                ".." => {
                    stack.pop();
                }
                s => stack.push(s),
            }
        }

        let resolved = norm(&format!("{drive}/{}", stack.join("/")));
        if resolved == b || resolved.starts_with(&format!("{b}/")) {
            return true;
        }
    }
    false
}

/// Structured-argument paths a tool call is asking to touch. Only covers
/// argument key names actually seen in tool schemas we know about (BigTiny's
/// own built-ins plus kitty-tools) — a tool whose schema uses some other key
/// name for a path (`src`, `output_path`, etc.) still isn't caught here, and
/// falls through to `check_containment`'s empty-candidates default (see its
/// doc comment) rather than being denied. Broadened defense-in-depth, not a
/// closed list: extend it whenever a new path-shaped key name shows up in a
/// registered MCP server's schema.
pub fn extract_candidate_paths(args: &Value) -> Vec<String> {
    let mut found = Vec::new();
    let obj = match args.as_object() {
        Some(o) => o,
        None => return found,
    };

    for key in &[
        "path",
        "file_path",
        "directory",
        "dir",
        "folder",
        "src",
        "source",
        "source_path",
        "target",
        "target_path",
        "dest",
        "dest_path",
        "destination",
        "from",
        "to",
        "output_path",
        "input_path",
        "filename",
        "filepath",
    ] {
        if let Some(value) = obj.get(*key).and_then(|v| v.as_str()) {
            if !value.is_empty() {
                found.push(value.to_string());
            }
        }
    }

    // Every element, not just the first — a call like
    // `{"paths": ["/allowed/x", "/etc/passwd"]}` must have both checked.
    if let Some(paths) = obj.get("paths").and_then(|v| v.as_array()) {
        for p in paths {
            if let Some(s) = p.as_str() {
                if !s.is_empty() {
                    found.push(s.to_string());
                }
            }
        }
    }

    found
}

/// `scheme://...` tokens in a shell command. Stripped before path
/// extraction: the drive-letter alternative in the extraction regex
/// otherwise matches the `s://` tail of `https://…` (and the `//host`
/// remnant matches the relative-path alternative), so a write-class command
/// containing an absolute URL (`curl https://…`, `git clone https://…`) was
/// hard-denied with no approval path. URLs are not filesystem paths.
static URL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"[A-Za-z][A-Za-z0-9+.-]*://[^\s"']+"#).unwrap());

/// Best-effort extraction of literal filesystem paths from a shell command string.
fn extract_shell_paths(command: &str) -> Vec<String> {
    let re = Regex::new(
        r#""([A-Za-z]:[^"]+)"|'([A-Za-z]:[^']+)'|([A-Za-z]:[\\/][^\s"\']+)|(\.{0,2}/[^\s\"']+)"#,
    )
    .unwrap();

    let scrubbed = URL_RE.replace_all(command, " ");
    re.captures_iter(&scrubbed)
        .filter_map(|caps| {
            caps.iter()
                .skip(1)
                .find(|c| c.is_some())
                .flatten()
                .map(|m| m.as_str().to_string())
        })
        .collect()
}

/// True if every path this tool call touches resolves inside at least one of
/// `allowed_dirs`. There's no tool-schema registry here to tell a
/// non-filesystem call (a calculator, a web search) apart from a filesystem
/// call whose path arg just used a key name `extract_candidate_paths`
/// doesn't recognize — widen that function's key list before relying on
/// this as a hard boundary for a new tool with unusual argument names.
///
/// `strict` governs what happens when *no* path candidates are found at
/// all: `false` fails **open** (returns `true`) — the desktop default (see
/// `AgentConfig::sandbox_strict`), appropriate when an escalation on every
/// unrecognized call would be pure friction for a single user who *is* the
/// security boundary. `true` fails **closed** (returns `false`, forcing
/// `loop_.rs::execute_one_tool_call`'s HITL escalation) — for a host where
/// the daemon's own data root is the boundary, a false-positive escalation
/// is the safer failure mode than a silent bypass.
pub fn check_containment(args: &Value, allowed_dirs: &[String], strict: bool) -> bool {
    let mut candidates = extract_candidate_paths(args);

    for key in &["command", "cmd", "script"] {
        if let Some(value) = args
            .as_object()
            .and_then(|o| o.get(*key))
            .and_then(|v| v.as_str())
        {
            if !value.is_empty() {
                candidates.extend(extract_shell_paths(value));
            }
        }
    }

    if candidates.is_empty() {
        return !strict;
    }

    candidates.iter().all(|p| path_within_any(allowed_dirs, p))
}

/// Home directory for the current user — `USERPROFILE` on Windows, `HOME`
/// elsewhere. BigTiny deliberately doesn't pull in a `dirs`-style crate just
/// for this; these two env vars are the standard, and the paths built from
/// them (the kitty-web cache dirs below) only need to *match* what kitty-web
/// itself computes, which uses `dirs::home_dir()` — the same env var.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
}

/// Add the permissions-free "always reachable" working set: the OS temp dir
/// plus kitty-web's app-owned cache dirs under the user's home (search
/// offload + downloaded-PDF cache). These are appended to the allowed set for
/// *every* session so tools (and the model reaching for their files with a
/// path-arg read) never hit an approval just for touching scratch storage the
/// daemon's own bundled plugins manage.
fn scratch_allowance() -> Vec<String> {
    let mut dirs = Vec::new();
    let temp = std::env::temp_dir();
    if !temp.as_os_str().is_empty() {
        dirs.push(temp.to_string_lossy().replace('\\', "/"));
    }
    if let Some(home) = home_dir() {
        dirs.push(home.join(SEARCH_OFFLOAD_DIR).to_string_lossy().replace('\\', "/"));
        dirs.push(home.join(LEAN_CACHE_DIR).to_string_lossy().replace('\\', "/"));
    }
    dirs
}

/// The effective allowed-directory set for a session.
pub fn allowed_dirs_for_session(metadata: &Value, cache_dir: &str) -> Vec<String> {
    let mut dirs = Vec::new();

    if let Some(chat_dir) = metadata.get("chat_dir").and_then(|v| v.as_str()) {
        dirs.push(chat_dir.to_string());
    }
    dirs.push(cache_dir.to_string());

    if let Some(mode) = metadata.get("mode").and_then(|v| v.as_str()) {
        if mode == "agentic" {
            if let Some(cwd) = metadata.get("cwd").and_then(|v| v.as_str()) {
                dirs.push(cwd.to_string());
            }
        }
    }

    dirs.extend(scratch_allowance());

    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_path_within_any_simple() {
        let bases = vec!["/home/user/project".to_string()];
        assert!(path_within_any(&bases, "/home/user/project/src/main.rs"));
        assert!(!path_within_any(&bases, "/home/user/other/main.rs"));
    }

    #[test]
    fn test_path_within_any_with_traversal() {
        let bases = vec!["/home/user/project".to_string()];
        assert!(!path_within_any(
            &bases,
            "/home/user/project/../other/main.rs"
        ));
        assert!(path_within_any(
            &bases,
            "/home/user/project/src/../../project/file.rs"
        ));
    }

    #[test]
    fn test_path_within_any_relative() {
        let bases = vec!["/home/user/project".to_string()];
        assert!(path_within_any(&bases, "src/main.rs"));
    }

    #[test]
    fn test_extract_candidate_paths() {
        let args = json!({
            "path": "/home/user/test.txt",
            "file_path": "/home/user/other.txt"
        });
        let paths = extract_candidate_paths(&args);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], "/home/user/test.txt");
    }

    #[test]
    fn test_check_containment_allowed() {
        let args = json!({
            "path": "/home/user/project/src/main.rs"
        });
        let dirs = vec!["/home/user/project".to_string()];
        assert!(check_containment(&args, &dirs, false));
        assert!(check_containment(&args, &dirs, true));
    }

    #[test]
    fn test_check_containment_denied() {
        let args = json!({
            "path": "/etc/passwd"
        });
        let dirs = vec!["/home/user/project".to_string()];
        assert!(!check_containment(&args, &dirs, false));
        assert!(!check_containment(&args, &dirs, true));
    }

    #[test]
    fn test_allowed_dirs_for_session() {
        let metadata = json!({
            "chat_dir": "/home/user/chat",
            "mode": "agentic",
            "cwd": "/home/user/work"
        });
        let dirs = allowed_dirs_for_session(&metadata, "~/.bigtiny");
        assert!(dirs.contains(&"/home/user/chat".to_string()));
        assert!(dirs.contains(&"~/.bigtiny".to_string()));
        assert!(dirs.contains(&"/home/user/work".to_string()));
    }

    #[test]
    fn test_allowed_dirs_include_os_temp_and_app_cache_dirs() {
        let metadata = json!({"chat_dir": "/home/user/chat"});
        let dirs = allowed_dirs_for_session(&metadata, "~/.bigtiny");

        let temp = std::env::temp_dir().to_string_lossy().replace('\\', "/");
        assert!(dirs.contains(&temp), "OS temp dir must be allowed, got {dirs:?}");

        if let Some(home) = home_dir() {
            let offload = home.join(SEARCH_OFFLOAD_DIR).to_string_lossy().replace('\\', "/");
            assert!(dirs.contains(&offload), "search offload dir must be allowed");
            let lean = home.join(LEAN_CACHE_DIR).to_string_lossy().replace('\\', "/");
            assert!(dirs.contains(&lean), "lean cache dir must be allowed");
        }
    }

    #[test]
    fn test_search_offload_files_resolve_inside_allowed_dirs() {
        // Regression: the model reaching for a search result file (written by
        // kitty-web's lean_web_search to ~/.cache/kitty-search-offload) with a
        // path-arg read tool must NOT trip containment -> approval.
        let Some(home) = home_dir() else {
            eprintln!("no home dir in this env; skipping");
            return;
        };
        let offload_file = home
            .join(SEARCH_OFFLOAD_DIR)
            .join("search-abc123.json")
            .to_string_lossy()
            .replace('\\', "/");

        let metadata = json!({"chat_dir": "/home/user/chat"});
        let dirs = allowed_dirs_for_session(&metadata, "~/.bigtiny");
        assert!(path_within_any(&dirs, &offload_file), "{offload_file} not allowed");
    }

    #[test]
    fn test_check_containment_checks_every_paths_array_element() {
        let args = json!({
            "paths": ["/home/user/project/ok.rs", "/etc/passwd"]
        });
        let dirs = vec!["/home/user/project".to_string()];
        assert!(!check_containment(&args, &dirs, false));
    }

    #[test]
    fn test_extract_candidate_paths_recognizes_common_alt_key_names() {
        let args = json!({"src": "/a", "dest": "/b", "output_path": "/c"});
        let paths = extract_candidate_paths(&args);
        assert_eq!(
            paths,
            vec!["/a".to_string(), "/b".to_string(), "/c".to_string()]
        );
    }

    #[test]
    fn test_check_containment_no_paths_fails_open_by_default() {
        let args = json!({
            "operation": "calculate",
            "x": 1,
            "y": 2
        });
        let dirs = vec!["/home/user/project".to_string()];
        assert!(check_containment(&args, &dirs, false));
    }

    #[test]
    fn test_check_containment_no_paths_fails_closed_when_strict() {
        let args = json!({
            "operation": "calculate",
            "x": 1,
            "y": 2
        });
        let dirs = vec!["/home/user/project".to_string()];
        assert!(!check_containment(&args, &dirs, true));
    }

    /// Regression: the shell-path regex matched the `s://` tail of
    /// `https://…` as a drive-letter path (and `//host` as a relative one),
    /// so a write-class shell command containing an absolute URL was
    /// hard-denied with no approval path. URLs must be stripped before path
    /// extraction.
    #[test]
    fn test_extract_shell_paths_ignores_absolute_urls() {
        assert!(extract_shell_paths("curl https://example.com").is_empty());
        assert!(
            extract_shell_paths("git clone https://github.com/org/repo.git").is_empty()
        );
        assert!(extract_shell_paths("curl -X POST 'https://api.example.com/v1' -d '{}'").is_empty());
    }

    /// Real Windows paths in a command must still be extracted — including
    /// alongside a URL in the same command.
    #[test]
    fn test_extract_shell_paths_still_finds_real_paths() {
        let paths = extract_shell_paths(r"curl -o C:\Users\x\out.txt https://example.com");
        assert_eq!(paths, vec!["C:\\Users\\x\\out.txt".to_string()]);

        let paths = extract_shell_paths(r#"type "C:\Users\x\file.txt""#);
        assert_eq!(paths, vec!["C:\\Users\\x\\file.txt".to_string()]);
    }

    /// The containment-level view of the URL fix: `curl https://…` produces
    /// no path candidates at all, so the desktop (non-strict) default fails
    /// open instead of hard-denying.
    #[test]
    fn test_check_containment_url_only_shell_command_is_not_denied() {
        let args = json!({"command": "curl https://example.com"});
        let dirs = vec!["C:\\chat".to_string()];
        assert!(check_containment(&args, &dirs, false));
    }

    /// `norm` case-folds only on Windows, where the filesystem itself is
    /// case-insensitive.
    #[cfg(windows)]
    #[test]
    fn test_norm_lowercases_on_windows() {
        assert_eq!(norm("C:\\Users\\Foo\\"), "c:/users/foo");
    }

    /// On case-sensitive hosts (Android/Linux) folding case made two
    /// different directories compare equal — fail-open containment.
    #[cfg(not(windows))]
    #[test]
    fn test_norm_preserves_case_on_case_sensitive_hosts() {
        assert_eq!(norm("/home/User/Project/"), "/home/User/Project");
        let bases = vec!["/home/User/project".to_string()];
        assert!(!path_within_any(&bases, "/home/user/project/evil"));
    }
}
