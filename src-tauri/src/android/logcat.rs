//! Send `tracing` output to logcat.
//!
//! Without this the app is undebuggable on a real device. `tracing_subscriber`'s
//! `fmt` layer writes to stdout, and Android discards a process's stdout unless
//! something explicitly redirects it — so on a release build every `info!` and
//! `error!` Kitty emits, including the ones explaining why the daemon refused
//! to start, goes nowhere. The in-app viewer (`log_capture`) still has them,
//! but reading a startup failure through a UI that needs the app to have
//! started is not a debugging strategy.
//!
//! `__android_log_write` rather than a crate: `liblog` is already linked (the
//! Android build passes `-llog`), the whole binding is three lines, and this
//! avoids taking a dependency for something the platform hands us directly.
//!
//! Read it with `adb logcat -s Kitty:V`.

use std::ffi::CString;
use std::io::{self, Write};

use tracing::Level;
use tracing_subscriber::fmt::MakeWriter;

const TAG: &[u8] = b"Kitty\0";

// Android log priorities from `<android/log.h>`.
const PRIO_DEBUG: i32 = 3;
const PRIO_INFO: i32 = 4;
const PRIO_WARN: i32 = 5;
const PRIO_ERROR: i32 = 6;

extern "C" {
    fn __android_log_write(prio: i32, tag: *const std::os::raw::c_char, text: *const std::os::raw::c_char) -> i32;
}

/// Buffers one formatted event, then emits it as a single logcat line on drop.
///
/// Buffered rather than written through, because the `fmt` layer calls
/// `write` several times per event (timestamp, level, target, message) and
/// each `__android_log_write` is its own logcat entry — write-through turns
/// every line into five fragments.
pub struct LogcatWriter {
    priority: i32,
    buffer: Vec<u8>,
}

impl Write for LogcatWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&self.buffer);
        // Interior NULs would truncate the line, and a trailing newline just
        // renders as a blank second entry.
        let cleaned = text.trim_end().replace('\0', "");
        self.buffer.clear();
        if cleaned.is_empty() {
            return Ok(());
        }
        if let Ok(msg) = CString::new(cleaned) {
            // SAFETY: both pointers are NUL-terminated and live for the call.
            unsafe {
                __android_log_write(self.priority, TAG.as_ptr().cast(), msg.as_ptr());
            }
        }
        Ok(())
    }
}

impl Drop for LogcatWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// Maps tracing levels onto logcat priorities so `adb logcat *:W` filters the
/// way anyone would expect.
pub struct MakeLogcatWriter;

impl<'a> MakeWriter<'a> for MakeLogcatWriter {
    type Writer = LogcatWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogcatWriter {
            priority: PRIO_INFO,
            buffer: Vec::new(),
        }
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        let priority = match *meta.level() {
            Level::ERROR => PRIO_ERROR,
            Level::WARN => PRIO_WARN,
            Level::INFO => PRIO_INFO,
            _ => PRIO_DEBUG,
        };
        LogcatWriter {
            priority,
            buffer: Vec::new(),
        }
    }
}
