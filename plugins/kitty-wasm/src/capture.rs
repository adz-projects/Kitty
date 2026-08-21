//! Memory-bounded stdout/stderr capture — the Rust equivalent of
//! `wasm_math_mcp.py`'s `SmartStdoutBuffer`.
//!
//! Head/tail ring-buffer strategy: keep the first `head_limit` bytes and the
//! last `tail_limit` bytes, dropping the middle. A script that prints a
//! gigabyte still costs ~45 KB of host RAM, and the caller still sees both
//! how the run started and how it ended — which is what actually matters for
//! debugging, and is exactly why the Python original did it this way rather
//! than plain truncation.
//!
//! **Why not `wasmtime_wasi::p2::pipe::MemoryOutputPipe`**: its `write`
//! returns `StreamError::Trap("write beyond capacity")` once full, so a
//! chatty guest would *trap the whole module* instead of being truncated.
//! That is precisely the failure mode `SmartStdoutBuffer` exists to prevent,
//! so this implements a non-trapping sink instead: writes past the limit are
//! silently folded into the tail ring and always report success.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::AsyncWrite;
// `wasmtime_wasi::cli::IsTerminal`, deliberately not `std::io::IsTerminal`:
// `StdoutStream` is bounded on wasmtime's own sealed trait, and the std one
// (which `CaptureStream` cannot implement anyway — it's sealed) would not
// satisfy that bound.
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};

/// Matches the Python defaults (20 KB head + 25 KB tail = ~45 KB).
pub const DEFAULT_HEAD_LIMIT: usize = 20_000;
pub const DEFAULT_TAIL_LIMIT: usize = 25_000;

#[derive(Debug)]
struct Inner {
    head: Vec<u8>,
    head_limit: usize,
    tail: VecDeque<u8>,
    tail_limit: usize,
    total_bytes: usize,
    truncated: bool,
}

impl Inner {
    fn push(&mut self, bytes: &[u8]) {
        self.total_bytes += bytes.len();
        let mut rest = bytes;

        if self.head.len() < self.head_limit {
            let room = self.head_limit - self.head.len();
            let take = room.min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        if rest.is_empty() {
            return;
        }
        if self.tail_limit == 0 {
            self.truncated = true;
            return;
        }

        // Work in chunks, not byte by byte. The previous version pushed every
        // byte individually and then popped every excess byte individually, so
        // a guest printing a gigabyte cost ~2e9 deque operations. Memory was
        // correctly bounded the whole time; CPU was not, and the only thing
        // stopping it was the run's wall-clock timeout — up to 300s of a
        // pinned core, which on a phone is a battery and heat problem rather
        // than a memory one.
        if rest.len() >= self.tail_limit {
            // This chunk alone fills the ring: everything currently held, and
            // everything but its last `tail_limit` bytes, goes. Only flag
            // truncation if something was genuinely dropped — a chunk landing
            // exactly on the limit against an empty tail loses nothing, and
            // must not grow a spurious "0 bytes omitted" marker.
            if rest.len() > self.tail_limit || !self.tail.is_empty() {
                self.truncated = true;
            }
            self.tail.clear();
            self.tail.extend(&rest[rest.len() - self.tail_limit..]);
            return;
        }

        self.tail.extend(rest);
        if self.tail.len() > self.tail_limit {
            self.truncated = true;
            let excess = self.tail.len() - self.tail_limit;
            self.tail.drain(..excess);
        }
    }

    fn render(&self) -> String {
        let head = String::from_utf8_lossy(&self.head).into_owned();
        let tail_bytes: Vec<u8> = self.tail.iter().copied().collect();
        let tail = String::from_utf8_lossy(&tail_bytes).into_owned();

        if !self.truncated {
            return format!("{head}{tail}");
        }
        let omitted = self
            .total_bytes
            .saturating_sub(self.head.len())
            .saturating_sub(self.tail.len());
        format!("{head}\n... [{omitted} bytes omitted] ...\n{tail}")
    }
}

/// A cloneable, memory-bounded capture sink. Clones share one buffer, which
/// is what `StdoutStream::async_stream` needs (it hands out a fresh writer
/// per acquisition, all of which must land in the same logical sink).
#[derive(Debug, Clone)]
pub struct CaptureStream {
    inner: Arc<Mutex<Inner>>,
}

impl CaptureStream {
    pub fn new(head_limit: usize, tail_limit: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                head: Vec::new(),
                head_limit,
                tail: VecDeque::new(),
                tail_limit,
                total_bytes: 0,
                truncated: false,
            })),
        }
    }

    /// The captured text, with an explicit `... [N bytes omitted] ...` marker
    /// in place of any dropped middle. Lossy UTF-8: a multi-byte character
    /// straddling the head/tail boundary becomes a replacement char rather
    /// than failing the whole capture.
    pub fn contents(&self) -> String {
        self.inner.lock().expect("capture mutex poisoned").render()
    }

    /// Total bytes the guest wrote, including any dropped middle.
    pub fn total_bytes(&self) -> usize {
        self.inner
            .lock()
            .expect("capture mutex poisoned")
            .total_bytes
    }

    pub fn truncated(&self) -> bool {
        self.inner.lock().expect("capture mutex poisoned").truncated
    }
}

impl Default for CaptureStream {
    fn default() -> Self {
        Self::new(DEFAULT_HEAD_LIMIT, DEFAULT_TAIL_LIMIT)
    }
}

impl IsTerminal for CaptureStream {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for CaptureStream {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(CaptureWriter {
            inner: self.inner.clone(),
        })
    }
}

struct CaptureWriter {
    inner: Arc<Mutex<Inner>>,
}

impl AsyncWrite for CaptureWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        // Always accepts everything and never errors — the whole point.
        // Over-limit bytes are folded into the tail ring by `push`.
        self.inner.lock().expect("capture mutex poisoned").push(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(stream: &CaptureStream, s: &str) {
        stream.inner.lock().unwrap().push(s.as_bytes());
    }

    #[test]
    fn short_output_passes_through_unchanged() {
        let c = CaptureStream::new(100, 100);
        push(&c, "hello world");
        assert_eq!(c.contents(), "hello world");
        assert!(!c.truncated());
        assert_eq!(c.total_bytes(), 11);
    }

    #[test]
    fn output_exactly_at_the_head_limit_is_not_truncated() {
        let c = CaptureStream::new(10, 10);
        push(&c, "0123456789");
        assert_eq!(c.contents(), "0123456789");
        assert!(!c.truncated());
    }

    #[test]
    fn overflow_keeps_head_and_tail_and_marks_the_gap() {
        let c = CaptureStream::new(5, 5);
        push(&c, "AAAAA");
        push(&c, &"x".repeat(50));
        push(&c, "ZZZZZ");
        let out = c.contents();
        assert!(out.starts_with("AAAAA"), "head lost: {out}");
        assert!(out.ends_with("ZZZZZ"), "tail lost: {out}");
        assert!(out.contains("bytes omitted"), "no truncation marker: {out}");
        assert!(c.truncated());
        assert_eq!(c.total_bytes(), 60);
    }

    #[test]
    fn memory_stays_bounded_regardless_of_volume() {
        let c = CaptureStream::new(1_000, 1_000);
        for _ in 0..1_000 {
            push(&c, &"y".repeat(1_000));
        }
        assert_eq!(c.total_bytes(), 1_000_000);
        // Bounded by head + tail + the marker text, nowhere near 1 MB.
        assert!(c.contents().len() < 3_000, "buffer grew unbounded");
    }

    #[test]
    fn a_gigantic_single_write_is_still_bounded() {
        // The case that traps `MemoryOutputPipe`: one write far larger than
        // capacity. Must be absorbed, not rejected.
        let c = CaptureStream::new(10, 10);
        push(&c, &"q".repeat(500_000));
        assert!(c.truncated());
        assert!(c.contents().len() < 200);
        assert_eq!(c.total_bytes(), 500_000);
    }

    #[test]
    fn invalid_utf8_does_not_panic() {
        let c = CaptureStream::new(10, 10);
        c.inner.lock().unwrap().push(&[0xff, 0xfe, 0x80]);
        let _ = c.contents();
    }

    /// A gigabyte of output used to cost ~2e9 individual deque operations
    /// (every byte pushed, every excess byte popped). Memory was bounded the
    /// whole time; CPU was not, and only the run's wall-clock timeout stopped
    /// it — up to 300s of a pinned core. Bounded time here, not just bounded
    /// bytes.
    #[test]
    fn a_huge_write_is_bounded_in_time_not_only_in_memory() {
        let c = CaptureStream::new(1_000, 1_000);
        let big = vec![b'x'; 64 * 1024 * 1024];

        let start = std::time::Instant::now();
        c.inner.lock().unwrap().push(&big);
        let elapsed = start.elapsed();

        assert_eq!(c.total_bytes(), big.len());
        assert!(c.truncated());
        // Generous by two orders of magnitude against the chunk-wise path and
        // still far under what the byte-at-a-time version took, so this fails
        // on a regression without being flaky on a loaded machine.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "64 MiB took {elapsed:?}; the ring is back to per-byte work"
        );
    }

    /// The chunk-wise rewrite must not change *what* is kept: head first, then
    /// the last `tail_limit` bytes, whether they arrive in one write or many.
    #[test]
    fn chunked_and_byte_sized_writes_keep_the_same_window() {
        let one_shot = CaptureStream::new(4, 4);
        one_shot.inner.lock().unwrap().push(b"abcdefghij");

        let drip = CaptureStream::new(4, 4);
        for b in b"abcdefghij" {
            drip.inner.lock().unwrap().push(&[*b]);
        }

        assert_eq!(one_shot.contents(), drip.contents());
        assert!(one_shot.contents().starts_with("abcd"));
        assert!(one_shot.contents().ends_with("ghij"));
        assert_eq!(one_shot.total_bytes(), drip.total_bytes());
    }

    /// A write landing exactly on the tail limit takes the whole-chunk path;
    /// it must still be kept in full, not treated as an overrun of itself.
    #[test]
    fn a_write_exactly_the_size_of_the_tail_is_kept_whole() {
        let c = CaptureStream::new(0, 4);
        c.inner.lock().unwrap().push(b"wxyz");
        assert_eq!(c.contents(), "wxyz");
        assert!(!c.truncated());
    }

    #[tokio::test]
    async fn async_writer_writes_land_in_the_shared_sink() {
        use tokio::io::AsyncWriteExt;
        let c = CaptureStream::new(100, 100);
        // `async_stream` returns a boxed trait object, which isn't `Unpin`,
        // so the `AsyncWriteExt` combinators need it pinned first.
        let mut w = Box::into_pin(c.async_stream());
        w.write_all(b"from the writer").await.unwrap();
        w.flush().await.unwrap();
        assert_eq!(c.contents(), "from the writer");
    }

    #[tokio::test]
    async fn separate_writers_share_one_logical_sink() {
        use tokio::io::AsyncWriteExt;
        // `StdoutStream` explicitly permits handing out multiple independent
        // writers; all of them must land in the same buffer, in order.
        let c = CaptureStream::new(100, 100);
        let mut first = Box::into_pin(c.async_stream());
        first.write_all(b"one ").await.unwrap();
        let mut second = Box::into_pin(c.async_stream());
        second.write_all(b"two").await.unwrap();
        assert_eq!(c.contents(), "one two");
    }
}
