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

fn norm(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    while p.ends_with('/') && p.len() > 1 {
        p.pop();
    }
    p.to_lowercase()
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

/// Best-effort extraction of literal filesystem paths from a shell command string.
fn extract_shell_paths(command: &str) -> Vec<String> {
    let re = Regex::new(
        r#""([A-Za-z]:[^"]+)"|'([A-Za-z]:[^']+)'|([A-Za-z]:[\\/][^\s"\']+)|(\.{0,2}/[^\s\"']+)"#,
    )
    .unwrap();

    re.captures_iter(command)
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
/// `allowed_dirs`. Deliberately fails **open** (returns `true`) when no path
/// candidates are found at all — this is called for every tool call, not
/// just filesystem ones, and there's no tool-schema registry here to tell a
/// non-filesystem call (a calculator, a web search) apart from a filesystem
/// call whose path arg just used a key name `extract_candidate_paths`
/// doesn't recognize. Widen that function's key list before relying on this
/// as a hard boundary for a new tool with unusual argument names.
pub fn check_containment(args: &Value, allowed_dirs: &[String]) -> bool {
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

    candidates.iter().all(|p| path_within_any(allowed_dirs, p))
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
        assert!(check_containment(&args, &dirs));
    }

    #[test]
    fn test_check_containment_denied() {
        let args = json!({
            "path": "/etc/passwd"
        });
        let dirs = vec!["/home/user/project".to_string()];
        assert!(!check_containment(&args, &dirs));
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
    fn test_check_containment_checks_every_paths_array_element() {
        let args = json!({
            "paths": ["/home/user/project/ok.rs", "/etc/passwd"]
        });
        let dirs = vec!["/home/user/project".to_string()];
        assert!(!check_containment(&args, &dirs));
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
    fn test_check_containment_no_paths() {
        let args = json!({
            "operation": "calculate",
            "x": 1,
            "y": 2
        });
        let dirs = vec!["/home/user/project".to_string()];
        assert!(check_containment(&args, &dirs));
    }
}
