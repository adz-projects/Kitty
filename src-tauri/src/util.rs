//! Small cross-cutting helpers.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::OnceLock;

/// Read one line (up to and including `\n`, stripped) from `reader` as
/// lossily-decoded UTF-8 — `None` at EOF. Reads raw bytes rather than using
/// [`BufRead::lines`], whose strict UTF-8 decoding turns a single
/// non-UTF8-encoded line (e.g. a child process whose stdio fell back to a
/// legacy Windows codepage) into a permanent `Err` that silently ends the
/// whole relay loop — see `capture_output`'s doc comment.
fn read_lossy_line(reader: &mut impl BufRead) -> Option<String> {
    let mut buf = Vec::new();
    match reader.read_until(b'\n', &mut buf) {
        Ok(0) => None,
        Ok(_) => {
            while matches!(buf.last(), Some(b'\n' | b'\r')) {
                buf.pop();
            }
            Some(String::from_utf8_lossy(&buf).into_owned())
        }
        Err(_) => None,
    }
}

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// One process-wide `reqwest::Client`, built on first use. Every call site
/// that used to build its own client (`Client::builder()...build()`,
/// `Client::new()`, or the bare `reqwest::get`/`reqwest::Client::new()`
/// one-offs scattered across `ollama/`, `openrouter/`, `adaptive_pathway/`,
/// `lifecycle/`, `config/providers.rs`, and `wizard.rs`) now clones this
/// instead — a clone is a cheap `Arc` bump, while building a fresh client
/// re-initializes TLS/connection-pool state and throws away keep-alive.
pub fn http_client() -> reqwest::Client {
    HTTP_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent("kitty-app")
                .build()
                .expect("reqwest client")
        })
        .clone()
}

/// Build a [`Command`] that does not flash a console window on Windows.
pub fn hidden_command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Forward a managed child's stdout/stderr into our own tracing log, each
/// line prefixed with `tag` (e.g. `"bigtiny"`). Without this, a crash/panic
/// inside a child process we spawned (BigTiny, Ollama, the Adaptive Pathway
/// sidecar) is completely invisible — all Kitty itself ever sees is a
/// downstream symptom, with no indication of the actual cause, since none of
/// these commands piped their output anywhere before (confirmed real report:
/// a backend crash left only one generic line in the log). Call this right
/// after `spawn()`,
/// with the command having been built with
/// `.stdout(Stdio::piped()).stderr(Stdio::piped())` — takes the pipes as soon
/// as the child exists so nothing is missed, and reads them on plain OS
/// threads (not tokio tasks) since this is blocking, line-buffered I/O with
/// no async runtime dependency of its own.
///
/// Uses [`read_lossy_line`] rather than [`BufRead::lines`]: a child whose
/// stdio encoding doesn't line up with UTF-8 (observed with a frozen Python
/// child on Windows — its stdio silently falls back to the ambient legacy
/// codepage once redirected to a pipe instead of a real console, and this
/// codebase's own log text is full of em-dashes/curly quotes) would otherwise
/// produce one `io::Result::Err` that permanently ends `.lines()`'s iterator
/// — Kitty stops draining that pipe forever, and every write the child makes
/// after that backs up and eventually fails on its end too (confirmed real
/// report: continuous `OSError: [Errno 22] Invalid argument` "Logging error"
/// spam from BigTiny once this happened). Lossy decoding never errors, so one
/// bad line just renders with replacement characters instead of ending relay.
pub fn capture_output(child: &mut Child, tag: &'static str) {
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(line) = read_lossy_line(&mut reader) {
                tracing::info!("{tag}: {line}");
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            while let Some(line) = read_lossy_line(&mut reader) {
                tracing::warn!("{tag}: {line}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::read_lossy_line;
    use std::io::Cursor;

    #[test]
    fn reads_plain_lines_stripping_newline() {
        let mut r = Cursor::new(b"hello\nworld\n".to_vec());
        assert_eq!(read_lossy_line(&mut r).as_deref(), Some("hello"));
        assert_eq!(read_lossy_line(&mut r).as_deref(), Some("world"));
        assert_eq!(read_lossy_line(&mut r), None);
    }

    #[test]
    fn strips_crlf_line_endings() {
        let mut r = Cursor::new(b"hello\r\nworld\r\n".to_vec());
        assert_eq!(read_lossy_line(&mut r).as_deref(), Some("hello"));
        assert_eq!(read_lossy_line(&mut r).as_deref(), Some("world"));
    }

    #[test]
    fn returns_final_line_without_trailing_newline() {
        let mut r = Cursor::new(b"no newline at end".to_vec());
        assert_eq!(
            read_lossy_line(&mut r).as_deref(),
            Some("no newline at end")
        );
        assert_eq!(read_lossy_line(&mut r), None);
    }

    #[test]
    fn lossily_decodes_invalid_utf8_instead_of_ending_the_stream() {
        // A line encoded in a legacy Windows codepage (e.g. cp1252's em-dash
        // is the single byte 0xE2 alone is actually valid as a UTF-8 lead
        // byte prefix, so use a byte that is never valid UTF-8 in any
        // position: 0xFF) sandwiched between two clean lines — the bad line
        // must not prevent the good lines around it from being read.
        let mut bytes = b"before\n".to_vec();
        bytes.extend_from_slice(&[0xFF, 0xFE, b'\n']);
        bytes.extend_from_slice(b"after\n");
        let mut r = Cursor::new(bytes);
        assert_eq!(read_lossy_line(&mut r).as_deref(), Some("before"));
        let bad = read_lossy_line(&mut r).expect("bad line still yielded, just lossily decoded");
        assert!(bad.contains('\u{FFFD}'));
        assert_eq!(read_lossy_line(&mut r).as_deref(), Some("after"));
        assert_eq!(read_lossy_line(&mut r), None);
    }
}
