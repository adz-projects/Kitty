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

/// Where `lean_scratchpad_set`/`lean_scratchpad_delete` actually write.
///
/// Deliberately a sibling of `LEAN_CACHE_DIR` rather than a child, because
/// `lean_cache_clear` must not be able to wipe the scratchpad — see
/// `kitty-tools`' `scratchpad::new_scratch_path`, which this must stay in sync
/// with.
///
/// That sibling placement is also why this was missing here: it sits outside
/// `LEAN_CACHE_DIR`'s subtree, so the scratchpad allowance came from nowhere.
/// Both scratchpad tools are in `WRITE_TOOL_NAMES`, so a write there should
/// have been hard-denied on every call; it only worked because their arguments
/// are `key`/`value` with no path-shaped key, which makes
/// `extract_candidate_paths` return nothing and `check_containment` fail open.
/// Under `sandbox_strict = true` the same code denies every scratchpad write.
pub const SCRATCHPAD_DIR: &str = ".cache/kitty-tools-scratchpad";

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
        // A working directory is a path like any other, and `lean_shell_ro`
        // requires one precisely so its calls are checkable here — `lean_shell`
        // takes no directory at all, which is why containment finds nothing to
        // check on it and falls open.
        "cwd",
        "working_dir",
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

/// Quoted Windows paths, bare drive-letter paths, and relative paths.
///
/// `Lazy` like its three neighbours in this file. This was recompiled once
/// per tool call, and a turn runs its tool calls concurrently, so every one
/// of them paid to build the same regex from scratch.
static SHELL_PATH_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#""([A-Za-z]:[^"]+)"|'([A-Za-z]:[^']+)'|([A-Za-z]:[\\/][^\s"\']+)|(\.{0,2}/[^\s\"']+)"#,
    )
    .unwrap()
});

/// Best-effort extraction of literal filesystem paths from a shell command string.
fn extract_shell_paths(command: &str) -> Vec<String> {
    let scrubbed = URL_RE.replace_all(command, " ");
    SHELL_PATH_RE.captures_iter(&scrubbed)
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
/// elsewhere, with `KITTY_PLUGIN_HOME` taking priority over both. BigTiny
/// deliberately doesn't pull in a `dirs`-style crate just for this; these env
/// vars are the standard, and the paths built from them (the kitty-web/
/// kitty-tools cache dirs below) only need to *match* what those crates
/// themselves compute.
///
/// `KITTY_PLUGIN_HOME` has to come first because it's the one override that
/// actually changes the answer on Android: bionic's `getpwuid` reports `/data`
/// for `$HOME` there, which is unwritable and shared by every app on the
/// device, so `bigtiny_env.rs`'s `daemon_env` sets `KITTY_PLUGIN_HOME` to
/// Kitty's real app data dir before the daemon starts (see that env var's own
/// doc comment in `kitty-tools`' `paths.rs`, which resolves `home_dir()` in
/// exactly this order). Without checking it here too, this function and
/// `kitty-tools::paths::home_dir` disagreed about where "home" was on
/// Android — the cache dir `scratch_allowance` builds below never matched the
/// directory `kitty-tools` actually writes its cache under
/// (`~/.cache/lean-goose-mcp`), so every read of the daemon's own cached
/// scrape/search files failed containment and was force-escalated to a human
/// approval, on every single call.
fn home_dir() -> Option<std::path::PathBuf> {
    // A closure, not the bare `std::env::var_os` function item: that's
    // generic over `K: AsRef<OsStr>`, and rustc can't unify it with the
    // `impl Fn(&str) -> _` this function wants for every lifetime (a known
    // higher-ranked-trait-bound inference gap around generic function items).
    resolve_home(|key: &str| std::env::var_os(key))
}

/// The resolution order itself, taking its environment lookup as a parameter
/// so the priority order is testable without mutating real process
/// environment variables — `cargo test` runs this crate's tests in parallel
/// within one binary, and `std::env::set_var` isn't safe against a
/// concurrent reader (see `bigtiny_embedded.rs`'s own comment on that).
/// Mirrors `kitty-tools`' `paths::resolve_home`.
fn resolve_home(
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    // Filtered on the *string* form, not just non-empty `OsString`: Android
    // sets some of these to whitespace/empty rather than leaving them unset
    // (same reasoning as `kitty-tools::paths::resolve_home`, which this
    // mirrors).
    ["KITTY_PLUGIN_HOME", "USERPROFILE", "HOME"]
        .into_iter()
        .find_map(|key| {
            env(key).filter(|v| v.to_str().is_none_or(|s| !s.trim().is_empty()))
        })
        .map(std::path::PathBuf::from)
}

/// Add the permissions-free "always reachable" working set: the app-owned
/// cache dirs, under the user's home, that the daemon's own bundled plugins
/// write to. These are appended to the allowed set for *every* session so tools
/// (and the model reaching for their files with a path-arg read) never hit an
/// approval just for touching scratch storage those plugins manage.
///
/// The OS temp directory is deliberately **not** here. It used to be, on the
/// theory that tools scratch there — but no production kitty-tools path does
/// (`std::env::temp_dir()` appears only in its `#[cfg(test)]` blocks, and
/// `doc_store::write_atomic` writes its temp file same-dir precisely to avoid a
/// cross-filesystem rename). So the grant covered nothing we do while handing
/// the model read/write over every other process's temp files, which on a
/// shared machine is other people's data.
fn scratch_allowance() -> Vec<String> {
    let mut dirs = Vec::new();
    if let Some(home) = home_dir() {
        dirs.push(
            home.join(SEARCH_OFFLOAD_DIR)
                .to_string_lossy()
                .replace('\\', "/"),
        );
        dirs.push(
            home.join(LEAN_CACHE_DIR)
                .to_string_lossy()
                .replace('\\', "/"),
        );
        dirs.push(
            home.join(SCRATCHPAD_DIR)
                .to_string_lossy()
                .replace('\\', "/"),
        );
    }
    dirs
}

/// The effective allowed-directory set for a session.
/// Argument keys that name a filesystem path in kitty-tools' surface.
///
/// `cwd` is here as well as in `extract_candidate_paths` because a relative
/// working directory has to be resolved against the session's own before it can
/// be judged: unqualified, `lean_shell_ro` with `cwd: "src"` would be checked as
/// the literal string `src` and denied, while the model reasonably meant the
/// `src` inside the folder it is working in.
const PATH_ARG_KEYS: [&str; 2] = ["path", "cwd"];

/// Rewrite a relative `path` argument to be relative to the *session's*
/// working directory, returning whether anything changed.
///
/// kitty-tools resolves a relative path — including the `"."` that
/// `lean_analyze_workspace` uses as its default — against its own process
/// working directory. That process is an MCP child spawned without
/// `current_dir`, so it inherits Kitty's launch directory: listing `"."`
/// returned Kitty's app-data folder rather than the folder the user chose.
/// The session's `cwd` lives in its metadata and never reached the tool
/// process in any form, so there was no way for the model to get this right
/// except by guessing absolute paths.
///
/// Rewriting here rather than in kitty-tools is deliberate. The tool process
/// is shared by every session, so "the working directory" cannot be a
/// property of it; only the daemon knows which session a call belongs to. It
/// also keeps kitty-tools' own home-relative resolution intact, which Android
/// depends on.
///
/// Scoped to `lean_*` tools and the `path` key: that is kitty-tools'
/// naming convention, and a third-party MCP server is free to use "path" for
/// something that is not a filesystem path at all (a JSON pointer, an API
/// route). Absolute and home-relative (`~`) paths are left exactly as written
/// — the model meant those literally.
pub fn qualify_relative_path_args(tool_name: &str, args: &mut Value, cwd: &str) -> bool {
    if !tool_name.starts_with("lean_") || cwd.is_empty() {
        return false;
    }
    let Some(obj) = args.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for key in PATH_ARG_KEYS {
        let Some(raw) = obj.get(key).and_then(|v| v.as_str()) else {
            continue;
        };
        let trimmed = raw.trim();
        if trimmed.starts_with('~') {
            continue;
        }
        // `has_root` as well as `is_absolute`: on Windows a POSIX-style
        // "/foo" is rooted but not absolute (no drive prefix), and joining it
        // onto the cwd would silently invent a path the model never asked for.
        let p = std::path::Path::new(trimmed);
        if p.is_absolute() || p.has_root() {
            continue;
        }
        let rel = trimmed
            .trim_start_matches("./")
            .trim_start_matches(".\\")
            .trim_end_matches('/')
            .trim_end_matches('\\');
        // Deliberately NOT `norm`: that case-folds on Windows for
        // *comparison* purposes, and this string is handed straight to the
        // tool as a real path — lowercasing the user's folder name in every
        // result and error message would be gratuitous noise. Only separators
        // and a trailing slash need normalizing here.
        let base = cwd.replace('\\', "/");
        let base = base.trim_end_matches('/');
        let qualified = if rel.is_empty() || rel == "." {
            base.to_string()
        } else {
            format!("{base}/{}", rel.replace('\\', "/"))
        };
        obj.insert(key.to_string(), Value::String(qualified));
        changed = true;
    }
    changed
}

pub fn allowed_dirs_for_session(metadata: &Value, cache_dir: &str) -> Vec<String> {
    let mut dirs = Vec::new();

    if let Some(chat_dir) = metadata.get("chat_dir").and_then(|v| v.as_str()) {
        dirs.push(chat_dir.to_string());
    }
    dirs.push(cache_dir.to_string());

    // The session's working directory is always writable. This used to be
    // gated on `metadata.mode == "agentic"`, but the chat/agent mode was
    // removed (H1) — with no mode stamped, that gate silently stopped allowing
    // the cwd at all, so a model's file tools could no longer create artifacts
    // in the chat folder. Allowing cwd unconditionally is the widening H1
    // intended ("cwd is always allowed"); the folder is the session's own
    // private per-chat directory (or a folder the user explicitly chose), so
    // writing there is exactly what tool calls are for.
    if let Some(cwd) = metadata.get("cwd").and_then(|v| v.as_str()) {
        dirs.push(cwd.to_string());
    }

    // Every working folder set during this session, not just the current one.
    //
    // `cwd` holds only the folder in force *now*, so switching the pill used to
    // silently revoke the previous one — mid-task, with no notice, and with the
    // model's next read of a file it had been working on turning into an
    // approval prompt. A user who points Kitty at a second folder is adding a
    // place to work, not withdrawing the first. Accumulated (union, never
    // pruned) by `routes::chat::update_config`, and revocable one entry at a
    // time from the working-directory pill.
    if let Some(paths) = metadata.get("working_dirs").and_then(|v| v.as_array()) {
        for p in paths {
            if let Some(s) = p.as_str() {
                if !s.is_empty() {
                    dirs.push(s.to_string());
                }
            }
        }
    }

    // Files the user explicitly attached to a turn (drag-and-drop / paste). Each
    // is an absolute path that is, by construction, outside the session's
    // chat_dir/cwd — so without this it would force a HITL approval every time
    // the model reached for it, which is exactly the friction users hit ("models
    // can't find attached files without approval"). The user handing us the file
    // *is* the authorization; an exact file path added here allows reading that
    // one file (`path_within_any`'s `resolved == base` case) without widening to
    // its directory. Accumulated in metadata by `run_inner`, so an attachment
    // stays reachable on later turns too.
    if let Some(paths) = metadata.get("attached_paths").and_then(|v| v.as_array()) {
        for p in paths {
            if let Some(s) = p.as_str() {
                if !s.is_empty() {
                    dirs.push(s.to_string());
                }
            }
        }
    }

    dirs.extend(scratch_allowance());

    // Normalised and de-duplicated. `cwd` is almost always also `chat_dir`, and
    // a folder can be both the working directory and an attachment, so the raw
    // list repeats itself — which makes `path_within_any` do the same
    // comparison several times per tool call, and makes the "you may write to
    // {first two}" denial message list one directory twice. `norm` is the same
    // function containment compares with, so de-duplicating on it cannot change
    // which paths are allowed.
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| !d.is_empty() && seen.insert(norm(d)));

    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The regression this module exists to prevent: on Android,
    /// `KITTY_PLUGIN_HOME` must win over `USERPROFILE`/`HOME` (bionic's
    /// `$HOME` there is the unhelpful, unwritable `/data`), or this crate's
    /// notion of "home" disagrees with `kitty-tools`'/`kitty-web`'s — and every
    /// read of their own cache files (`~/.cache/lean-goose-mcp/...`) fails
    /// containment and is force-escalated to a human approval, every time.
    #[test]
    fn kitty_plugin_home_wins_over_userprofile_and_home() {
        let env = |key: &str| match key {
            "KITTY_PLUGIN_HOME" => {
                Some(std::ffi::OsString::from("/data/user/0/com.kitty.app/Kitty"))
            }
            "HOME" => Some(std::ffi::OsString::from("/data")),
            "USERPROFILE" => Some(std::ffi::OsString::from(r"C:\Users\someone")),
            _ => None,
        };
        assert_eq!(
            resolve_home(env),
            Some(std::path::PathBuf::from(
                "/data/user/0/com.kitty.app/Kitty"
            ))
        );
    }

    /// Android sets some of these to the empty string rather than leaving
    /// them unset — a blank override must not shadow a real fallback.
    #[test]
    fn a_blank_kitty_plugin_home_is_skipped_not_used() {
        let env = |key: &str| match key {
            "KITTY_PLUGIN_HOME" => Some(std::ffi::OsString::from("   ")),
            "HOME" => Some(std::ffi::OsString::from("/home/real")),
            _ => None,
        };
        assert_eq!(
            resolve_home(env),
            Some(std::path::PathBuf::from("/home/real"))
        );
    }

    /// With no `KITTY_PLUGIN_HOME` set at all (every non-Android host),
    /// resolution falls through to `USERPROFILE`/`HOME` exactly as before —
    /// the desktop path this change must not disturb.
    #[test]
    fn resolution_falls_through_to_userprofile_and_home_when_unset() {
        let env = |key: &str| match key {
            "USERPROFILE" => Some(std::ffi::OsString::from(r"C:\Users\someone")),
            _ => None,
        };
        assert_eq!(
            resolve_home(env),
            Some(std::path::PathBuf::from(r"C:\Users\someone"))
        );
    }

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
        // No `mode` in metadata (the chat/agent mode was removed) — the cwd
        // must still be allowed so tool calls can write artifacts into the
        // chat folder.
        let metadata = json!({
            "chat_dir": "/home/user/chat",
            "cwd": "/home/user/work"
        });
        let dirs = allowed_dirs_for_session(&metadata, "~/.bigtiny");
        assert!(dirs.contains(&"/home/user/chat".to_string()));
        assert!(dirs.contains(&"~/.bigtiny".to_string()));
        assert!(
            dirs.contains(&"/home/user/work".to_string()),
            "cwd must be writable regardless of mode, got {dirs:?}"
        );
    }

    #[test]
    fn test_allowed_dirs_include_the_app_cache_dirs_but_not_os_temp() {
        let metadata = json!({"chat_dir": "/home/user/chat"});
        let dirs = allowed_dirs_for_session(&metadata, "~/.bigtiny");

        // The OS temp dir is NOT granted. Nothing in kitty-tools writes there
        // outside its own tests, so the grant covered none of our own work
        // while handing the model every other process's temp files.
        let temp = std::env::temp_dir().to_string_lossy().replace('\\', "/");
        assert!(
            !dirs.contains(&temp),
            "the OS temp dir must not be granted wholesale, got {dirs:?}"
        );

        if let Some(home) = home_dir() {
            let offload = home
                .join(SEARCH_OFFLOAD_DIR)
                .to_string_lossy()
                .replace('\\', "/");
            assert!(
                dirs.contains(&offload),
                "search offload dir must be allowed"
            );
            let lean = home
                .join(LEAN_CACHE_DIR)
                .to_string_lossy()
                .replace('\\', "/");
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
        assert!(
            path_within_any(&dirs, &offload_file),
            "{offload_file} not allowed"
        );
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
        assert!(extract_shell_paths("git clone https://github.com/org/repo.git").is_empty());
        assert!(
            extract_shell_paths("curl -X POST 'https://api.example.com/v1' -d '{}'").is_empty()
        );
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

    /// The bug this exists for: `lean_analyze_workspace` defaults `path` to
    /// "." and kitty-tools resolves that against its own process working
    /// directory — Kitty's launch folder — so the model was shown app-data
    /// instead of the folder the user chose, did not recognize it, and kept
    /// guessing.
    #[test]
    fn a_bare_dot_becomes_the_session_working_directory() {
        let mut args = json!({"path": "."});
        assert!(qualify_relative_path_args(
            "lean_analyze_workspace",
            &mut args,
            "C:/Users/me/Labs"
        ));
        assert_eq!(args["path"], json!("C:/Users/me/Labs"));
    }

    #[test]
    fn relative_paths_are_qualified_and_separators_normalized() {
        for (input, want) in [
            ("src", "C:/proj/src"),
            ("./src", "C:/proj/src"),
            ("src/main.rs", "C:/proj/src/main.rs"),
            (r"src\main.rs", "C:/proj/src/main.rs"),
            ("", "C:/proj"),
        ] {
            let mut args = json!({ "path": input });
            qualify_relative_path_args("lean_file_read", &mut args, "C:/proj");
            assert_eq!(args["path"], json!(want), "input {input:?}");
        }
    }

    /// An absolute or home-relative path is what the model literally meant;
    /// silently reparenting it under the cwd would invent a path nobody asked
    /// for and turn a clear "not found" into a confusing one.
    #[test]
    fn absolute_and_home_relative_paths_are_left_alone() {
        for input in [
            "C:/elsewhere/file.txt",
            "/etc/hosts",
            "~/notes.md",
            r"\\server\share\f.txt",
        ] {
            let mut args = json!({ "path": input });
            assert!(
                !qualify_relative_path_args("lean_file_read", &mut args, "C:/proj"),
                "input {input:?} must not be rewritten"
            );
            assert_eq!(args["path"], json!(input));
        }
    }

    /// Scoped to kitty-tools' `lean_` namespace: a third-party MCP server is
    /// free to use "path" for a JSON pointer or an API route, and rewriting
    /// that into a filesystem path would corrupt the call.
    #[test]
    fn other_servers_arguments_are_untouched() {
        let mut args = json!({"path": "/users/me"});
        assert!(!qualify_relative_path_args("github_get", &mut args, "C:/proj"));
        assert_eq!(args["path"], json!("/users/me"));

        // No cwd to qualify against is also a no-op, not a panic.
        let mut args = json!({"path": "."});
        assert!(!qualify_relative_path_args("lean_file_read", &mut args, ""));
    }

    /// `lean_shell_ro` requires a `cwd` for exactly one reason: so that this
    /// function sees it. If `cwd` were not a recognised key the call would carry
    /// no path candidates at all, `check_containment` would fall open, and the
    /// read-only shell would be running wherever it liked — which is the hole
    /// `lean_shell` has and this tool exists to avoid.
    #[test]
    fn test_a_cwd_argument_is_containment_checked() {
        let allowed = vec!["/home/user/project".to_string()];
        assert!(check_containment(
            &json!({"command": "ls", "cwd": "/home/user/project/src"}),
            &allowed,
            false
        ));
        assert!(
            !check_containment(
                &json!({"command": "ls", "cwd": "/etc"}),
                &allowed,
                false
            ),
            "a cwd outside the allowed set must not pass containment"
        );
    }

    /// The scratchpad lives *beside* `LEAN_CACHE_DIR`, not inside it (so
    /// `lean_cache_clear` cannot wipe it), which is exactly why it was missing
    /// from the allowed set.
    ///
    /// This is not a theoretical gap: `lean_scratchpad_set` and
    /// `lean_scratchpad_delete` are both in `WRITE_TOOL_NAMES`, so a write
    /// there is supposed to be hard-denied unless contained. It only ever
    /// worked because those tools take `key`/`value` and no path-shaped
    /// argument, so `extract_candidate_paths` finds nothing and containment
    /// fails open — and under `sandbox_strict` the same code denies every
    /// scratchpad write instead.
    #[test]
    fn test_scratchpad_writes_are_inside_the_allowed_set() {
        let Some(home) = home_dir() else {
            eprintln!("no home dir in this env; skipping");
            return;
        };
        let scratch_file = home
            .join(SCRATCHPAD_DIR)
            .join("scratchpad.json")
            .to_string_lossy()
            .replace('\\', "/");

        let metadata = json!({"chat_dir": "/home/user/chat"});
        let dirs = allowed_dirs_for_session(&metadata, "~/.bigtiny");
        assert!(
            path_within_any(&dirs, &scratch_file),
            "{scratch_file} not allowed; scratchpad writes would rely on \
             containment failing open"
        );
    }

    /// Switching the working folder adds, never revokes. A model mid-task in
    /// the first folder must not silently lose it when the user points the pill
    /// somewhere else.
    #[test]
    fn test_working_dirs_accumulate_across_a_folder_change() {
        let metadata = json!({
            "chat_dir": "/home/user/chat",
            "cwd": "/home/user/second",
            "working_dirs": ["/home/user/first", "/home/user/second"],
        });
        let dirs = allowed_dirs_for_session(&metadata, "~/.bigtiny");
        assert!(
            path_within_any(&dirs, "/home/user/first/notes.md"),
            "the previous working folder must stay reachable, got {dirs:?}"
        );
        assert!(path_within_any(&dirs, "/home/user/second/notes.md"));
    }

    /// An attached *file* allows that file alone. Widening to its directory
    /// would turn "the user handed me this document" into "the user handed me
    /// their whole Downloads folder".
    #[test]
    fn test_an_attached_file_does_not_widen_to_its_directory() {
        let metadata = json!({
            "chat_dir": "/home/user/chat",
            "attached_paths": ["/home/user/docs/report.pdf"],
        });
        let dirs = allowed_dirs_for_session(&metadata, "~/.bigtiny");
        assert!(path_within_any(&dirs, "/home/user/docs/report.pdf"));
        assert!(
            !path_within_any(&dirs, "/home/user/docs/private.pdf"),
            "a sibling of an attached file must not be readable, got {dirs:?}"
        );
    }

    /// An attached *directory* does widen to its subtree — that is what
    /// dropping a folder means, and the companion to the test above.
    #[test]
    fn test_an_attached_directory_widens_to_its_subtree() {
        let metadata = json!({
            "chat_dir": "/home/user/chat",
            "attached_paths": ["/home/user/project"],
        });
        let dirs = allowed_dirs_for_session(&metadata, "~/.bigtiny");
        assert!(path_within_any(&dirs, "/home/user/project/src/main.rs"));
    }

    /// `cwd` is almost always also `chat_dir`, and a folder is often both the
    /// working directory and an attachment. Repeats cost a redundant comparison
    /// per tool call and make the "you may write to {first two}" denial name one
    /// directory twice.
    #[test]
    fn test_allowed_dirs_are_deduplicated() {
        let metadata = json!({
            "chat_dir": "/home/user/chat",
            "cwd": "/home/user/chat",
            "working_dirs": ["/home/user/chat"],
            "attached_paths": ["/home/user/chat"],
        });
        let dirs = allowed_dirs_for_session(&metadata, "~/.bigtiny");
        let chat_entries = dirs.iter().filter(|d| d.contains("/home/user/chat")).count();
        assert_eq!(chat_entries, 1, "expected one entry, got {dirs:?}");
    }

}
