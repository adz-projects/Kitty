//! A short listing of the session's working directory, carried in the system
//! prompt so the model already knows where it is.
//!
//! Without this, changing Kitty's working directory reliably sent the model
//! into a run of orientation tool calls before it could start the actual task
//! — and those calls did not even land in the right place. `lean_analyze_
//! workspace` defaults its `path` to `"."`, which kitty-tools resolves against
//! *its own process* working directory (the MCP child is spawned without
//! `current_dir`, so it inherits Kitty's launch directory), not the session's.
//! The model would list Kitty's app-data folder, not recognize it, and try
//! again with different arguments.
//!
//! Telling it up front is strictly cheaper than any number of tool calls: one
//! directory scan and a few hundred tokens, against a round trip per guess.
//!
//! # Stability
//!
//! This block sits in the stable head of every request, which
//! `build_messages` keeps byte-identical turn over turn so llama-server and
//! OpenAI prefix caches keep hitting. So the rendering here must be
//! deterministic — entries are sorted, never filesystem-ordered — and callers
//! must cache the result per working directory rather than rescanning each
//! turn. Both matter: an unstable head silently costs a full prompt
//! re-evaluation on every turn.

use std::path::Path;

/// Directory names never worth showing the model. Same set
/// `lean_analyze_workspace` skips (`plugins/kitty-tools/src/tools/
/// workspace.rs`), kept in sync by hand — they are two processes with no
/// shared crate.
const SKIP_DIRS: [&str; 8] = [
    ".git",
    "node_modules",
    "__pycache__",
    "venv",
    ".venv",
    "dist",
    "build",
    ".tox",
];

/// How many entries the whole snapshot may list. Chosen to keep the block to a
/// few hundred tokens: this is orientation, not an index, and a model that
/// needs the complete tree still has `lean_analyze_workspace`.
const MAX_ENTRIES: usize = 60;

/// How many entries to show inside any one subdirectory before summarizing the
/// rest as a count.
const MAX_CHILDREN_PER_DIR: usize = 12;

/// Recursion depth. One level down is what tells the model whether a folder
/// holds the files it wants; two would blow the entry budget on anything real.
const MAX_DEPTH: usize = 2;

fn skipped(name: &str) -> bool {
    SKIP_DIRS.contains(&name) || name.starts_with('.')
}

/// One directory's entries, sorted, directories first.
///
/// Returns `None` when the directory cannot be read at all — a snapshot that
/// silently claims an unreadable folder is empty would be worse than no
/// snapshot, because the model would believe it.
fn read_sorted(dir: &Path) -> Option<(Vec<String>, Vec<String>)> {
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        if skipped(&name) {
            continue;
        }
        // `file_type` rather than `metadata`: it does not follow symlinks, so
        // a link pointing outside the tree is listed as what it is instead of
        // being silently descended into.
        match entry.file_type() {
            Ok(t) if t.is_dir() => dirs.push(name),
            Ok(_) => files.push(name),
            Err(_) => continue,
        }
    }
    dirs.sort();
    files.sort();
    Some((dirs, files))
}

/// Render a plain-text listing of `root`, or `None` if it cannot be read.
///
/// The format is deliberately boring — one entry per line, two-space indent
/// per level, a trailing `/` on directories. No box drawing, no tree glyphs:
/// this is read by a model, and the characters cost tokens without adding
/// information.
pub fn render(root: &Path) -> Option<String> {
    let (dirs, files) = read_sorted(root)?;
    let mut out = String::new();
    let mut budget = MAX_ENTRIES;
    let mut truncated = false;

    for name in dirs.iter().chain(files.iter()) {
        if budget == 0 {
            truncated = true;
            break;
        }
        let is_dir = dirs.iter().any(|d| d == name);
        if is_dir {
            out.push_str(&format!("  {name}/\n"));
        } else {
            out.push_str(&format!("  {name}\n"));
        }
        budget -= 1;

        if !is_dir || MAX_DEPTH < 2 {
            continue;
        }
        let Some((child_dirs, child_files)) = read_sorted(&root.join(name)) else {
            continue;
        };
        let total = child_dirs.len() + child_files.len();
        let shown = total.min(MAX_CHILDREN_PER_DIR).min(budget);
        for (i, child) in child_dirs.iter().chain(child_files.iter()).enumerate() {
            if i >= shown {
                break;
            }
            let suffix = if i < child_dirs.len() { "/" } else { "" };
            out.push_str(&format!("    {child}{suffix}\n"));
            budget -= 1;
        }
        if total > shown {
            out.push_str(&format!("    …and {} more\n", total - shown));
        }
    }

    if out.is_empty() {
        out.push_str("  (empty)\n");
    }
    if truncated {
        out.push_str("  …listing truncated; call lean_analyze_workspace for the full tree\n");
    }
    Some(out)
}

/// The system-prompt block, ready to push as `role: "system"`.
///
/// Names the directory explicitly and says it is the default for relative
/// paths, because the two failures this fixes are the model not knowing where
/// it is and not knowing what a bare `.` resolves to.
pub fn block(root: &str) -> Option<String> {
    let listing = render(Path::new(root))?;
    Some(format!(
        "Your working directory is {root}\n\
         It currently contains:\n{listing}\
         Relative paths in file tools resolve against this directory, so you do \
         not need to list it before starting. Re-list only if you need detail \
         this summary omits."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bigtiny-snapshot-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lists_files_and_one_level_of_subdirectories() {
        let root = tmp();
        std::fs::write(root.join("readme.md"), "x").unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("main.rs"), "x").unwrap();

        let out = render(&root).unwrap();
        assert!(out.contains("readme.md"), "{out}");
        assert!(out.contains("src/"), "{out}");
        assert!(out.contains("main.rs"), "expected one level down: {out}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// The block lands in the prompt head, which `build_messages` keeps
    /// byte-identical across turns so prefix caches keep hitting. Filesystem
    /// iteration order is not stable, so the sort is what makes that true.
    #[test]
    fn rendering_is_deterministic() {
        let root = tmp();
        for name in ["c.txt", "a.txt", "b.txt"] {
            std::fs::write(root.join(name), "x").unwrap();
        }
        std::fs::create_dir(root.join("zdir")).unwrap();
        std::fs::create_dir(root.join("adir")).unwrap();

        let first = render(&root).unwrap();
        for _ in 0..5 {
            assert_eq!(render(&root).unwrap(), first);
        }
        // Directories before files, each alphabetical.
        let order: Vec<&str> = first.lines().map(str::trim).collect();
        let pos = |needle: &str| order.iter().position(|l| l.starts_with(needle)).unwrap();
        assert!(pos("adir/") < pos("zdir/"));
        assert!(pos("zdir/") < pos("a.txt"));
        assert!(pos("a.txt") < pos("b.txt"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// An unreadable root must produce no block at all. Rendering it as empty
    /// would have the model confidently report that the user's folder has
    /// nothing in it.
    #[test]
    fn an_unreadable_root_yields_nothing_rather_than_an_empty_listing() {
        assert!(render(Path::new("Z:/definitely/not/here")).is_none());
        assert!(block("Z:/definitely/not/here").is_none());
    }

    #[test]
    fn a_large_directory_is_capped_and_says_so() {
        let root = tmp();
        for i in 0..(MAX_ENTRIES + 40) {
            std::fs::write(root.join(format!("f{i:04}.txt")), "x").unwrap();
        }
        let out = render(&root).unwrap();
        assert!(out.contains("truncated"), "{out}");
        assert!(
            out.lines().count() <= MAX_ENTRIES + 2,
            "cap not honored: {} lines",
            out.lines().count()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_empty_directory_is_stated_not_omitted() {
        let root = tmp();
        assert!(render(&root).unwrap().contains("(empty)"));
        std::fs::remove_dir_all(&root).ok();
    }
}
