//! Tool approvals: what can be answered automatically, what needs a person,
//! and the list of those still waiting.
//!
//! The daemon pauses for approval on nearly every tool call (its default
//! `always_ask` policy), and most of those are safe enough to answer at once:
//! a file operation inside the chat's own granted folders, a shell command
//! that is not security-sensitive. [`decide`] makes that call. What it cannot
//! clear waits for a person, in `AppState::pending_approvals`, and is shown
//! wherever that person is - the chat itself if it is on screen, otherwise an
//! interrupting dialog over whatever Kitty shows, summoning the overlay if no
//! Kitty window is visible (decision #7).
//!
//! This used to live in the frontend, per chat window, which is why an
//! approval for any chat not currently on screen was never answered at all:
//! switching chats mid-turn, or any scheduled run. It now runs once, in this
//! process, for every session Kitty owns (`lifecycle::app_events`).
//!
//! The daemon is the authoritative sandbox either way (it re-checks path
//! containment itself); this is the round-trip-avoidance layer on top, so a
//! wrong guess costs one extra prompt, never a security gap.

use std::path::{Component, Path};
use std::sync::OnceLock;

use serde::Serialize;
use serde_json::Value;

/// What to do with one approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Safe: allow it once, now.
    Allow,
    /// Needs a person; `warning` says why.
    Prompt { warning: String },
}

/// The paths a tool call names, from the argument shapes Kitty's tools use.
fn target_path(args: &Value) -> Option<&str> {
    args.get("path")
        .or_else(|| args.get("file_path"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            args.get("paths")
                .and_then(|p| p.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
        })
        .filter(|p| !p.is_empty())
}

/// Commands whose blast radius (remote access, destructive filesystem or
/// network changes, privilege escalation) is too high to run unattended just
/// because shell is not sandboxed anyway. Matched against the whole command,
/// so a wrapper (`cmd /c rm -rf ...`) still matches.
fn security_sensitive(command: &str) -> bool {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(ssh|scp|sftp|sudo|su|rm\s+-rf|chmod|chown|curl\s+-o|wget\s+-O|netsh|iptables|shutdown|format|diskpart|nc|ncat|telnet|taskkill)\b",
        )
        .expect("the pattern is valid")
    })
    .is_match(command)
}

/// A tool's own cache (`.../Block/goose/cache/...`, a legacy path some
/// scrapers still use): its working storage, not a file saved for the user.
fn is_internal_tool_cache_path(target: &str) -> bool {
    target
        .replace('\\', "/")
        .to_ascii_lowercase()
        .split('/')
        .collect::<Vec<_>>()
        .windows(3)
        .any(|w| w == ["block", "goose", "cache"])
}

/// Lexical, case-insensitive containment: whether `target` (resolved against
/// `base` when relative, `.`/`..` collapsed) lies inside `base`. No
/// filesystem access, so it works for paths that do not exist yet.
pub(crate) fn path_within_dir(base: &str, target: &str) -> bool {
    let norm = |p: &str| {
        let lexical = lexical_normalize(Path::new(&p.replace('\\', "/")));
        lexical.trim_end_matches('/').to_ascii_lowercase()
    };
    let base = norm(base);
    if base.is_empty() {
        return false;
    }
    let target = target.replace('\\', "/");
    let is_absolute = target.starts_with('/')
        || (target.len() >= 3 && target.as_bytes()[1] == b':' && target.as_bytes()[2] == b'/');
    let joined = if is_absolute {
        target
    } else {
        format!("{base}/{target}")
    };
    let resolved = norm(&joined);
    resolved == base || resolved.starts_with(&format!("{base}/"))
}

/// Collapse `.` and `..` without touching the filesystem, keeping a drive
/// prefix or root.
fn lexical_normalize(path: &Path) -> String {
    let text = path.to_string_lossy();
    let (prefix, rest) = if text.len() >= 2 && text.as_bytes()[1] == b':' {
        (&text[..2], &text[2..])
    } else {
        ("", &text[..])
    };
    let mut stack: Vec<String> = Vec::new();
    for component in Path::new(rest).components() {
        match component {
            Component::Normal(seg) => stack.push(seg.to_string_lossy().into_owned()),
            Component::ParentDir => {
                stack.pop();
            }
            _ => {}
        }
    }
    format!("{prefix}/{}", stack.join("/"))
}

/// Decide an approval request. `dirs` is everything the session may touch
/// (chat folder, working folders, attached files), from the daemon's own
/// view of the session.
///
/// A path-based file operation must stay inside one of `dirs`; a
/// security-sensitive shell command always needs a person; everything else
/// is allowed.
pub fn decide(args: &Value, dirs: &[String]) -> Decision {
    if let Some(path) = target_path(args) {
        let bases: Vec<&String> = dirs.iter().filter(|d| !d.is_empty()).collect();
        if !bases.is_empty()
            && !bases.iter().any(|base| path_within_dir(base, path))
            && !is_internal_tool_cache_path(path)
        {
            return Decision::Prompt {
                warning: format!(
                    "A file operation wants to reach outside this chat's folders ({path})."
                ),
            };
        }
    }
    if let Some(command) = args.get("command").and_then(|c| c.as_str()) {
        if security_sensitive(command) {
            return Decision::Prompt {
                warning: format!(
                    "\"{command}\" looks security-sensitive and needs your say-so before it runs."
                ),
            };
        }
    }
    Decision::Allow
}

/// What "Always allow" would cover for this call (decision #8): the tool,
/// narrowed to the command's first two words for a shell command, so allowing
/// `git status` does not also allow `git push`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AlwaysScope {
    /// Stored as the rule's `args_pattern`; `None` means the whole tool.
    pub args_pattern: Option<String>,
    /// How the scope reads to a person: `git status …` or the tool name.
    pub label: String,
}

pub fn always_scope(tool_name: &str, args: &Value) -> AlwaysScope {
    let Some(command) = args.get("command").and_then(|c| c.as_str()) else {
        return AlwaysScope {
            args_pattern: None,
            label: format!("every use of {tool_name}"),
        };
    };
    let prefix = command
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    if prefix.is_empty() {
        return AlwaysScope {
            args_pattern: None,
            label: format!("every use of {tool_name}"),
        };
    }
    // The daemon matches `args_pattern` against the call's arguments as
    // compact JSON, so the prefix is matched in its JSON-escaped form, and
    // must be followed by whitespace or the end of the string - so
    // `git status` does not also match `git statusx`.
    let json_prefix = serde_json::to_string(&prefix).unwrap_or_default();
    let json_prefix = json_prefix.strip_suffix('"').unwrap_or(&json_prefix);
    AlwaysScope {
        args_pattern: Some(format!(
            r#""command":{}(?:"|\s|\\[nt])"#,
            regex::escape(json_prefix)
        )),
        label: format!("{prefix} …"),
    }
}

/// One approval waiting on a person.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PendingApproval {
    /// The daemon's action id; what `/approve` answers.
    pub action_id: String,
    pub session_id: String,
    pub tool_name: String,
    pub tool_args: Value,
    /// Why it was not answered automatically.
    pub warning: Option<String>,
    /// What "Always allow" would cover.
    pub always_scope: AlwaysScope,
    /// A scheduled run's approval, which is denied on its own after a while
    /// if nobody answers (decision #7).
    pub scheduled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dirs(d: &[&str]) -> Vec<String> {
        d.iter().map(|s| s.to_string()).collect()
    }

    // Parity with the TypeScript policy this replaced.

    #[test]
    fn a_file_op_inside_the_chat_folder_is_allowed() {
        let d = dirs(&["C:/Users/me/Kitty/chats/c1"]);
        assert_eq!(
            decide(
                &json!({"path": "C:\\Users\\me\\Kitty\\chats\\c1\\out.docx"}),
                &d
            ),
            Decision::Allow
        );
        assert_eq!(
            decide(&json!({"path": "notes/a.md"}), &d),
            Decision::Allow,
            "relative resolves inside"
        );
        assert_eq!(
            decide(&json!({"file_path": "c:/USERS/me/kitty/CHATS/c1/x"}), &d),
            Decision::Allow,
            "case-insensitive"
        );
    }

    #[test]
    fn a_file_op_outside_every_granted_folder_prompts() {
        let d = dirs(&["C:/Users/me/Kitty/chats/c1", "D:/project"]);
        assert!(matches!(
            decide(&json!({"path": "C:/Windows/system.ini"}), &d),
            Decision::Prompt { .. }
        ));
        assert!(
            matches!(
                decide(&json!({"path": "../c2/secret"}), &d),
                Decision::Prompt { .. }
            ),
            "no ..-walk out"
        );
        assert_eq!(
            decide(&json!({"paths": ["D:/project/src/a.rs"]}), &d),
            Decision::Allow,
            "any granted folder"
        );
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_inside() {
        assert!(!path_within_dir("C:/chats/c1", "C:/chats/c10/x"));
        assert!(path_within_dir("C:/chats/c1", "C:/chats/c1"));
    }

    #[test]
    fn a_tool_cache_path_is_never_prompted_for() {
        let d = dirs(&["C:/chats/c1"]);
        assert_eq!(
            decide(
                &json!({"path": "C:/Users/me/AppData/Block/goose/cache/page.html"}),
                &d
            ),
            Decision::Allow
        );
    }

    #[test]
    fn security_sensitive_commands_prompt_and_others_run() {
        let d = dirs(&["C:/chats/c1"]);
        assert!(matches!(
            decide(&json!({"command": "ssh box"}), &d),
            Decision::Prompt { .. }
        ));
        assert!(matches!(
            decide(&json!({"command": "cmd /c rm -rf C:/x"}), &d),
            Decision::Prompt { .. }
        ));
        assert_eq!(
            decide(&json!({"command": "python make_docx.py"}), &d),
            Decision::Allow
        );
        assert_eq!(
            decide(&json!({"command": "git status"}), &d),
            Decision::Allow
        );
    }

    #[test]
    fn with_no_folders_known_a_path_is_not_judged() {
        assert_eq!(
            decide(&json!({"path": "C:/anything"}), &[]),
            Decision::Allow
        );
    }

    // Always-allow scope.

    #[test]
    fn a_shell_rule_covers_its_first_two_words_only() {
        let scope = always_scope("lean_shell", &json!({"command": "git status --short"}));
        assert_eq!(scope.label, "git status …");
        let re = regex::Regex::new(scope.args_pattern.as_deref().unwrap()).unwrap();
        let args = |cmd: &str| serde_json::to_string(&json!({"command": cmd})).unwrap();
        assert!(re.is_match(&args("git status")));
        assert!(re.is_match(&args("git status -sb")));
        assert!(!re.is_match(&args("git push")));
        assert!(!re.is_match(&args("git statusx")));
    }

    #[test]
    fn a_non_shell_rule_covers_the_tool() {
        let scope = always_scope("lean_file_read", &json!({"path": "a.txt"}));
        assert_eq!(scope.args_pattern, None);
        assert_eq!(scope.label, "every use of lean_file_read");
    }

    #[test]
    fn a_command_with_quotes_is_escaped_for_the_json_match() {
        let scope = always_scope("lean_shell", &json!({"command": "echo \"hi\" there"}));
        let re = regex::Regex::new(scope.args_pattern.as_deref().unwrap()).unwrap();
        let args = serde_json::to_string(&json!({"command": "echo \"hi\" there"})).unwrap();
        assert!(
            re.is_match(&args),
            "{} vs {args}",
            scope.args_pattern.unwrap()
        );
    }
}
