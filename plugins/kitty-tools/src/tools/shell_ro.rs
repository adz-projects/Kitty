//! `lean_shell_ro` — a shell that can look but not touch.
//!
//! # Why this exists rather than a flag on `lean_shell`
//!
//! `lean_shell` is in the daemon's `WRITE_TOOL_NAMES`, so a delegate running
//! under `hitl_policy: auto_reject` has it hard-denied — correctly, since
//! nothing can establish that an arbitrary shell command is read-only. That cost
//! the `locator` specialist its ability to search: with `lean_doc_search`
//! limited to documents already extracted, a cold workspace left it opening
//! files one at a time.
//!
//! So this is a separate tool with a separate name, not a mode on the existing
//! one. A single name that behaves differently depending on its caller is hard
//! to reason about and hard to test, and the daemon's write classification is
//! by *name* — one name cannot be both.
//!
//! # The parse is an allowlist, and the metacharacter check comes first
//!
//! A denylist of dangerous commands is unclosable: `sh -c`, backticks, `$()`,
//! and `>` redirection each reintroduce arbitrary writes without naming a
//! forbidden command. So the first token must be one of a fixed set of readers,
//! and — before that check even runs — anything that could chain or redirect is
//! refused outright. Order matters: a permitted first token must not be able to
//! smuggle a second command behind a `;`.
//!
//! This refuses some legitimate commands (a quoted `&` inside a `findstr`
//! pattern, for instance). That is the intended trade: the alternative is
//! implementing shell quoting rules well enough to be *sure* a metacharacter is
//! inert, and being wrong there is a silent write.
//!
//! # `cwd` is required, and it does more work than the allowlist
//!
//! `lean_shell` takes no working directory and inherits the plugin process's,
//! which means the daemon's containment check sees no path argument at all and
//! `check_containment` fails open. Requiring `cwd` — a key
//! `sandbox::extract_candidate_paths` recognises — is what makes every call
//! containment-checkable. A `cwd` outside the session's allowed set escalates to
//! approval, which for a delegate is a clean refusal it can report.
//!
//! # ...but `cwd` alone is not containment
//!
//! Checking `cwd` proves where the command *starts*, not what it reads. The
//! daemon extracts paths from named argument keys, and the command line is one
//! opaque string to it: `cwd` inside the allowed set with a command of
//! `cat ~/.ssh/id_rsa` passes containment and reads the key. For a tool whose
//! entire purpose is to be handed to an unattended delegate under
//! `auto_reject`, that is the whole sandbox defeated by an absolute path.
//!
//! So arguments are confined here, where the command is still parsed: no
//! absolute path, no drive letter, no UNC share, no `~`, and no `..` segment.
//! Everything the command names is therefore under `cwd`, which *is* checked.
//! This refuses legitimate reads outside the working folder — deliberately. The
//! way to read elsewhere is to be granted elsewhere (drop the folder on the
//! window, or set it as the working folder), which leaves a decision the user
//! made rather than one a delegate made for them.

use crate::envelope::error_response;

/// Commands that only read. Deliberately short: every entry is one a model
/// reaches for while *finding* things, and adding to it is a security decision
/// rather than a convenience one.
pub const READ_ONLY_COMMANDS: [&str; 15] = [
    "ls", "dir", "find", "cat", "type", "head", "tail", "wc", "grep", "findstr", "rg", "stat",
    "file", "du", "tree",
];

/// Shell syntax that chains, redirects, or substitutes — i.e. every way a
/// permitted first token can be followed by something else.
const FORBIDDEN: [&str; 9] = [";", "&", "|", ">", "<", "`", "$(", "\n", "\r"];

/// `find` is the one allowlisted command that can write, through actions rather
/// than redirection. Matched as *prefixes*, which is not a stylistic choice:
/// exact matching let `-fprint0` through, since the list named `-fprint` and
/// `-fprintf` and GNU find has a third spelling. Prefixes cover the whole family
/// including any spelling nobody here thought of, at the cost of also refusing a
/// hypothetical read-only option starting with these letters — which is the
/// right way round for a list whose entries all write.
const FIND_WRITE_ACTIONS: [&str; 6] = ["-delete", "-exec", "-ok", "-fprint", "-fls", "-fprintf"];

/// Options that hand an allowlisted reader something else to run, or a file to
/// write. None of these needs a shell metacharacter, so nothing else here sees
/// them: `rg --pre` runs an arbitrary preprocessor on every file it visits, and
/// `tree -o` is a redirection with a different spelling.
///
/// Keyed by the command, because the same letter means different things — `-o`
/// is an output file to `tree` and a pattern list to `grep`.
const DANGEROUS_FLAGS: [(&str, &[&str]); 3] = [
    ("rg", &["--pre", "--pre-glob", "--hostname-bin"]),
    ("tree", &["-o"]),
    ("find", &["-files0-from"]),
];

/// Why a command was refused, or `Ok` with the command name that was matched.
///
/// Pure and separated from execution so the whole security argument is
/// unit-testable without spawning anything — the same reason
/// `agent::sandbox`'s checks are pure.
pub fn validate_read_only(command: &str) -> Result<&'static str, String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err("command must not be empty".to_string());
    }

    // First, and before the allowlist: `ls; rm -rf /` starts with a permitted
    // token.
    for bad in FORBIDDEN {
        if trimmed.contains(bad) {
            let shown = match bad {
                "\n" => "a newline",
                "\r" => "a carriage return",
                other => other,
            };
            return Err(format!(
                "`{shown}` is not allowed here — lean_shell_ro runs exactly one read-only \
                 command, with no chaining, redirection or substitution. Use lean_shell if you \
                 genuinely need those."
            ));
        }
    }

    let first = trimmed.split_whitespace().next().unwrap_or_default();
    // A bare name, not a path: allowing `/usr/bin/ls` would also allow
    // `/tmp/anything-i-just-wrote ls`.
    if first.contains('/') || first.contains('\\') {
        return Err(format!(
            "`{first}` is a path; lean_shell_ro takes a bare command name from: {}",
            READ_ONLY_COMMANDS.join(", ")
        ));
    }

    let lowered = first.to_ascii_lowercase();
    let Some(matched) = READ_ONLY_COMMANDS.iter().find(|c| **c == lowered) else {
        return Err(format!(
            "`{first}` is not a read-only command. Available: {}.",
            READ_ONLY_COMMANDS.join(", ")
        ));
    };

    // `find` can write without any shell metacharacter at all.
    if *matched == "find" {
        for arg in trimmed.split_whitespace().skip(1) {
            let arg_lower = arg.to_ascii_lowercase();
            if FIND_WRITE_ACTIONS
                .iter()
                .any(|bad| arg_lower.starts_with(bad))
            {
                return Err(format!(
                    "`find {arg}` can modify or execute; lean_shell_ro allows only find's \
                     matching and printing actions."
                ));
            }
        }
    }

    // Options that run something else, or write a file, without any shell
    // syntax to notice.
    if let Some((_, flags)) = DANGEROUS_FLAGS.iter().find(|(c, _)| *c == *matched) {
        for arg in trimmed.split_whitespace().skip(1) {
            let arg_lower = arg.to_ascii_lowercase();
            // `--pre=x` as well as `--pre x`.
            let head = arg_lower.split('=').next().unwrap_or_default();
            if flags.contains(&head) {
                return Err(format!(
                    "`{matched} {arg}` can run or write something; lean_shell_ro allows only \
                     the reading options."
                ));
            }
        }
    }

    // Everything the command names must be under `cwd`, which is the only part
    // of this call the daemon can check. See the module doc.
    for arg in trimmed.split_whitespace().skip(1) {
        if let Some(why) = escapes_cwd(arg, *matched) {
            return Err(format!(
                "`{arg}` {why}. lean_shell_ro only reads inside the working directory it was \
                 given, because that is the part of the call the daemon can check against this \
                 session's allowed folders. Use a path relative to `cwd`, or set the working \
                 folder to where you need to look."
            ));
        }
    }

    Ok(matched)
}

/// Commands whose options are spelled `/x` rather than `-x`.
///
/// The carve-out is per command rather than global because it is genuinely
/// ambiguous: `/s` is a switch to `findstr` and an absolute path to `grep`. Only
/// the cmd.exe-flavoured readers get to spend it.
const SLASH_SWITCH_COMMANDS: [&str; 4] = ["dir", "type", "findstr", "tree"];

/// Why `arg` would read outside `cwd`, or `None` if it stays inside.
///
/// Pure, and separate from the allowlist, because it answers a different
/// question: the allowlist decides whether the command can write, this decides
/// whether it can *reach*. A tool that satisfies only the first is a read-only
/// shell over the entire filesystem.
fn escapes_cwd(arg: &str, command: &str) -> Option<&'static str> {
    // Quotes are the shell's, not the path's. Stripping them matters: without
    // it, `cat "/etc/passwd"` is a token beginning with a quote and every test
    // below misses it.
    let arg = arg.trim_matches(|c| c == '"' || c == '\'');
    // A flag, not a path — but `--file=/etc/passwd` carries one.
    let arg = if arg.starts_with('-') {
        match arg.split_once('=') {
            Some((_, value)) if !value.is_empty() => value,
            _ => return None,
        }
    } else if SLASH_SWITCH_COMMANDS.contains(&command)
        && arg.starts_with('/')
        && !arg[1..].contains('/')
        && !arg[1..].contains('\\')
    {
        // `dir /b`, `findstr /i`. A second separator means it is a path after
        // all, so `/etc/passwd` is not mistaken for a switch.
        return None;
    } else {
        arg
    };

    if arg.starts_with('~') {
        return Some("is a home-relative path");
    }
    if arg.starts_with('/') || arg.starts_with('\\') {
        return Some("is an absolute path");
    }
    let mut chars = arg.chars();
    if let (Some(c), Some(':')) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return Some("names a drive");
        }
    }
    if arg
        .split(|c| c == '/' || c == '\\')
        .any(|segment| segment == "..")
    {
        return Some("climbs out of the working directory");
    }
    None
}

/// Run one read-only command in `cwd`.
///
/// Execution, timeout, tree-kill, output caps and ANSI stripping are
/// `lean_shell`'s verbatim — this differs only in what it will agree to run and
/// in where it runs it.
pub async fn shell_ro(command: &str, cwd: &str) -> String {
    if let Err(why) = validate_read_only(command) {
        return error_response("SHELL_RO_REFUSED", &why, None, None);
    }
    if cwd.trim().is_empty() {
        return error_response(
            "SHELL_RO_REFUSED",
            "cwd is required — it is what lets the daemon check this call against the \
             session's allowed directories.",
            None,
            None,
        );
    }
    super::shell::shell_in(command, Some(cwd)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_the_readers_it_advertises() {
        for cmd in READ_ONLY_COMMANDS {
            assert!(
                validate_read_only(&format!("{cmd} something")).is_ok(),
                "{cmd} should be permitted"
            );
        }
    }

    #[test]
    fn refuses_a_command_that_is_not_on_the_list() {
        let err = validate_read_only("rm -rf /tmp/x").unwrap_err();
        assert!(err.contains("not a read-only command"), "got: {err}");
        // The refusal names what IS available, so the model can retry usefully
        // rather than guessing.
        assert!(err.contains("grep"), "got: {err}");
    }

    /// The ordering property this whole module rests on: a permitted first
    /// token must not be able to carry a second command.
    #[test]
    fn metacharacters_are_refused_before_the_allowlist_is_consulted() {
        for sneaky in [
            "ls; rm -rf /",
            "ls && rm -rf /",
            "ls | tee /etc/passwd",
            "cat x > /etc/passwd",
            "cat < /etc/shadow",
            "ls `rm -rf /`",
            "ls $(rm -rf /)",
            "ls\nrm -rf /",
        ] {
            let err = validate_read_only(sneaky).unwrap_err();
            assert!(
                err.contains("not allowed here"),
                "{sneaky:?} must be refused as chaining, got: {err}"
            );
        }
    }

    /// `find` writes through actions, not redirection, so the metacharacter
    /// check does not see them.
    #[test]
    fn find_write_actions_are_refused() {
        for bad in [
            "find . -delete",
            "find . -name x -exec rm {} ;",
            "find . -fprint out.txt",
        ] {
            // `-exec rm {} ;` also trips the metacharacter check, which is
            // fine — either refusal is correct. What matters is that none of
            // these is permitted.
            assert!(validate_read_only(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(validate_read_only("find . -name '*.rs' -print").is_ok());
    }

    #[test]
    fn a_path_is_not_a_command_name() {
        let err = validate_read_only("/usr/bin/ls -la").unwrap_err();
        assert!(err.contains("is a path"), "got: {err}");
        // The point: otherwise this would also permit a binary the model just
        // wrote somewhere and named `ls`.
        assert!(validate_read_only("./ls").is_err());
    }

    #[test]
    fn an_empty_command_is_refused() {
        assert!(validate_read_only("   ").is_err());
    }

    /// The hole `cwd` alone left open: the allowlist is satisfied, the command
    /// writes nothing, and it still reads a file the session was never granted.
    #[test]
    fn an_argument_may_not_reach_outside_the_working_directory() {
        for sneaky in [
            "cat /etc/shadow",
            "cat ~/.ssh/id_rsa",
            "grep -r secret /home/other",
            "ls ../../..",
            "cat C:/Users/someone/private.txt",
            "type D:\\secrets\\keys.txt",
            "cat \"/etc/passwd\"",
            "grep --file=/etc/passwd .",
        ] {
            let err = validate_read_only(sneaky).unwrap_err();
            assert!(
                err.contains("only reads inside the working directory"),
                "{sneaky:?} must be confined, got: {err}"
            );
        }
    }

    /// ...without making the tool useless for the searching it exists to do.
    #[test]
    fn relative_work_inside_the_working_directory_is_still_allowed() {
        for fine in [
            "ls -la",
            "ls src",
            "find . -name '*.rs' -print",
            "grep -rn TODO src",
            "rg --hidden pattern .",
            "cat src/main.rs",
            "head -n 50 docs/README.md",
            "wc -l src/lib.rs",
        ] {
            assert!(validate_read_only(fine).is_ok(), "{fine:?} should be permitted");
        }
    }

    /// `/b` is a switch to `dir` and a path to `grep`, so the carve-out cannot
    /// be global.
    #[test]
    fn slash_switches_are_read_as_switches_only_for_the_commands_that_have_them() {
        assert!(validate_read_only("dir /b /s").is_ok());
        assert!(validate_read_only("findstr /i /s pattern").is_ok());
        assert!(validate_read_only("grep -r x /etc").is_err());
        // A second separator is a path even on those commands.
        assert!(validate_read_only("dir /etc/passwd").is_err());
    }

    /// Exact matching let `-fprint0` through: the list named two of the three
    /// spellings and GNU find has more.
    #[test]
    fn finds_writing_actions_are_matched_as_a_family() {
        for bad in [
            "find . -fprint0 out.bin",
            "find . -fls listing.txt",
            "find . -execdir cat {} +",
            "find . -okdir cat {} +",
        ] {
            assert!(validate_read_only(bad).is_err(), "{bad:?} must be refused");
        }
    }

    /// No shell metacharacter, no forbidden command name, arbitrary execution.
    #[test]
    fn options_that_run_something_else_are_refused() {
        for bad in ["rg --pre evil pattern", "rg --pre=evil pattern", "tree -o out.txt"] {
            let err = validate_read_only(bad).unwrap_err();
            assert!(
                err.contains("can run or write something"),
                "{bad:?} must be refused, got: {err}"
            );
        }
    }

    #[test]
    fn matching_is_case_insensitive_for_windows_shells() {
        assert!(validate_read_only("DIR /b").is_ok());
        assert!(validate_read_only("FindStr foo x.txt").is_ok());
    }
}
