//! In-memory capture of WARN/ERROR `tracing` events, so Settings → Advanced
//! can show a live error/warning log without the user needing to find and
//! read stderr. A plain module-level static (not an `AppState` field) since
//! `tracing_subscriber`'s `Layer` is installed once at process startup,
//! before `AppState`/`AppHandle` exist — a static ring buffer sidesteps that
//! ordering problem entirely, and every consumer (Tauri commands, tests) can
//! read it directly with no plumbing.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::Level;
use tracing_subscriber::layer::Context;
#[cfg(test)]
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer;

/// Caps memory use — old entries drop off the front once this is exceeded.
/// Generous enough to cover a real debugging session without needing to
/// reopen the app, small enough to never be a memory concern.
const MAX_ENTRIES: usize = 500;

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    /// RFC 3339 (matches every other timestamp already surfaced in this app).
    pub timestamp: String,
    /// `"ERROR"` or `"WARN"` — nothing lower ever reaches the buffer.
    pub level: String,
    /// The tracing target (module path) the event came from, e.g.
    /// `kitty_lib::bigtiny::stream`.
    pub target: String,
    pub message: String,
}

fn buffer() -> &'static Mutex<VecDeque<LogEntry>> {
    static BUFFER: OnceLock<Mutex<VecDeque<LogEntry>>> = OnceLock::new();
    BUFFER.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_ENTRIES)))
}

/// Oldest-first snapshot of the current buffer contents.
pub fn entries() -> Vec<LogEntry> {
    buffer().lock().unwrap().iter().cloned().collect()
}

pub fn clear() {
    buffer().lock().unwrap().clear();
}

/// Pulls just the `message` field out of a tracing event — every `warn!`/
/// `error!` call site in this codebase uses a plain format string (no
/// structured fields the UI needs to show separately), so this is all that's
/// worth extracting.
struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            // `{:?}` on a `&str`/`Arguments` formats as a quoted debug string
            // for some value kinds — strip a wrapping `"..."` if present so
            // the captured message matches what `fmt::layer()` prints to
            // stderr, not an escaped/quoted variant of it.
            let formatted = format!("{value:?}");
            self.0 = formatted
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .map(str::to_string)
                .unwrap_or(formatted);
        }
    }
}

/// A `tracing_subscriber::Layer` that captures WARN/ERROR events into the
/// module-level ring buffer. Installed alongside the existing stderr `fmt`
/// layer in `lib.rs::run` — this only ever reads events, never changes what
/// gets logged to stderr.
pub struct CaptureLayer;

impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        // `Level` orders ERROR as the most severe (`Level::ERROR < Level::WARN`
        // in the sense that `>` below means "less severe than WARN") — this
        // keeps INFO/DEBUG/TRACE out of the buffer.
        if *event.metadata().level() > Level::WARN {
            return;
        }
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        let entry = LogEntry {
            timestamp: chrono::Local::now().to_rfc3339(),
            level: event.metadata().level().to_string(),
            target: event.metadata().target().to_string(),
            message: visitor.0,
        };
        let mut buf = buffer().lock().unwrap();
        if buf.len() >= MAX_ENTRIES {
            buf.pop_front();
        }
        buf.push_back(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `buffer()` is a process-global static, and `cargo test` runs `#[test]`
    // functions concurrently on separate threads by default — without this,
    // these three tests' `warn!`/`error!` calls interleave into the *same*
    // buffer and corrupt each other's counts/ordering (confirmed: reproduced
    // exactly this failure before adding the lock). Each test acquires this
    // for its whole body so only one runs against the buffer at a time.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn captures_warn_and_error_but_not_info() {
        let _guard = TEST_LOCK.lock().unwrap();
        clear();
        let subscriber = tracing_subscriber::registry().with(CaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("should not be captured");
            tracing::warn!("a warning");
            tracing::error!("an error");
        });
        let got = entries();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].level, "WARN");
        assert_eq!(got[0].message, "a warning");
        assert_eq!(got[1].level, "ERROR");
        assert_eq!(got[1].message, "an error");
    }

    #[test]
    fn ring_buffer_drops_oldest_past_capacity() {
        let _guard = TEST_LOCK.lock().unwrap();
        clear();
        let subscriber = tracing_subscriber::registry().with(CaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            for i in 0..MAX_ENTRIES + 10 {
                tracing::warn!("entry {i}");
            }
        });
        let got = entries();
        assert_eq!(got.len(), MAX_ENTRIES);
        assert_eq!(got[0].message, "entry 10"); // the first 10 were dropped
    }

    #[test]
    fn clear_empties_the_buffer() {
        let _guard = TEST_LOCK.lock().unwrap();
        clear();
        let subscriber = tracing_subscriber::registry().with(CaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("something");
        });
        assert!(!entries().is_empty());
        clear();
        assert!(entries().is_empty());
    }
}
