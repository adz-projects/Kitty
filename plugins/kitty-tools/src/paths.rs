//! Path resolution matching Python's `Path(path).resolve()` — which
//! normalizes `.`/`..` components and makes a relative path absolute against
//! the current working directory *without requiring the path to exist*.
//! `std::fs::canonicalize` isn't a drop-in replacement: it requires every
//! component to exist and resolves symlinks, which the "does this path
//! exist" check that immediately follows every call site here (matching
//! `lean_mcp.py`'s `if not resolved.exists(): return error_response(...)`)
//! needs to run on the resolved-but-possibly-missing path itself.

use std::path::{Path, PathBuf};

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
}
