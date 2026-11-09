//! LiteRT-LM embedding backend: a `.litertlm` embedding bundle (EmbeddingGemma
//! 2 today) run through LiteRT-LM's C `EmbeddingEngine`, implementing
//! [`adaptive_pathway::embed::SemanticEmbedder`].
//!
//! **Why not the `.tflite` path in [`super::embedder`].** EmbeddingGemma 2
//! ships only as a LiteRT-LM bundle (model, tokenizer and per-length
//! signatures in one file), and the bundle's runtime does the tokenizing,
//! special tokens, length bucketing, pooling and normalisation itself. None of
//! the host-side tokenizer/padding code is needed, and none of it can drift.
//! It also replaced EmbeddingGemma 1, whose mixed-precision `.tflite` gave
//! vectors that barely separated topics (`docs/memory-eval/README.md`).
//!
//! **Why the library is loaded at runtime, under its own name.** The embedding
//! API arrived in LiteRT-LM's 1.0 C API, which also changed every older entry
//! point to return a status code. The Windows summarizer still links the older
//! `litert-lm.dll` through `litert-lm-rust`, whose bindings are written against
//! the old signatures, so the two cannot be the same file. The embedding
//! runtime ships as `litert-lm-embed.dll` (Windows) / `liblitert-lm.so`
//! (Android, where no other LiteRT-LM copy exists) and is opened with
//! `libloading`: nothing links against it, and a missing library degrades to
//! "embedder unavailable" exactly like a missing model.
//!
//! **Why an actor thread.** As in [`super::embedder`]: the engine is a raw C
//! handle with no documented thread-safety, so it lives on one dedicated OS
//! thread and `embed` talks to it over a channel.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::{mpsc, Mutex};
use std::thread;

use async_trait::async_trait;

use adaptive_pathway::embed::SemanticEmbedder;

/// Document prompt EmbeddingGemma 2 is trained with (no title). Applied to
/// every snippet, queries included, so the whole store shares one prompt and
/// stays comparable -- the same choice [`super::embedder`] made for
/// EmbeddingGemma 1. On the recall eval the asymmetric query prompt ranked no
/// better (22/27 either way).
fn format_input(text: &str) -> String {
    format!("title: none | text: {text}")
}

/// `LiteRtLmStatusCode` success value (`c/error_reporter.h`).
const STATUS_OK: c_int = 0;
/// `LiteRtLmInputDataType::kText`.
const INPUT_TEXT: c_int = 0;
/// `InputOverflowStrategy::kChunkAndAverage`: a text longer than the longest
/// signature (8 192 tokens for EmbeddingGemma 2) is embedded in pieces and
/// averaged rather than refused -- memorabilia's overnight chunks are long.
const OVERFLOW_CHUNK_AND_AVERAGE: c_int = 0;
/// `LogSeverity::kError`. The runtime otherwise writes dozens of loader
/// warnings and INFO lines to stderr on every start (missing vision/audio
/// sections in a text-only bundle, accelerator registration).
const LOG_ERROR: c_int = 4;

type Handle = *mut c_void;
type FnStatus0 = unsafe extern "C" fn(*mut Handle) -> c_int;
type FnSettingsCreate =
    unsafe extern "C" fn(*const c_char, *const c_char, *const c_char, *const c_char, *mut Handle) -> c_int;
type FnHandleOut = unsafe extern "C" fn(Handle, *mut Handle) -> c_int;
type FnDelete = unsafe extern "C" fn(Handle);
type FnSetBool = unsafe extern "C" fn(Handle, bool) -> c_int;
type FnSetInt = unsafe extern "C" fn(Handle, c_int) -> c_int;
type FnInputCreate = unsafe extern "C" fn(c_int, *const c_void, usize, *mut Handle) -> c_int;
type FnCompute = unsafe extern "C" fn(Handle, *const Handle, usize, Handle, *mut Handle) -> c_int;
type FnGetSize = unsafe extern "C" fn(Handle, *mut usize) -> c_int;
type FnGetValues = unsafe extern "C" fn(Handle, *mut *const f32) -> c_int;
type FnLastError = unsafe extern "C" fn() -> *const c_char;
type FnSetLogLevel = unsafe extern "C" fn(c_int) -> c_int;

/// The C entry points, resolved once. Function pointers copied out of the
/// `libloading::Symbol`s stay valid for as long as `_lib` is alive, which is
/// the whole life of the actor thread that owns this struct.
struct Api {
    _lib: libloading::Library,
    settings_create: FnSettingsCreate,
    settings_delete: FnDelete,
    engine_create: FnHandleOut,
    engine_delete: FnDelete,
    options_create: FnStatus0,
    options_delete: FnDelete,
    options_set_normalize: FnSetBool,
    options_set_overflow: FnSetInt,
    input_create: FnInputCreate,
    input_delete: FnDelete,
    compute: FnCompute,
    response_delete: FnDelete,
    response_size: FnGetSize,
    response_values: FnGetValues,
    last_error: FnLastError,
    set_min_log_level: FnSetLogLevel,
}

impl Api {
    fn load(path: &str) -> Result<Self, String> {
        // SAFETY: loading a library runs its initialisers. This is the
        // LiteRT-LM runtime Kitty ships beside the engine, on a path the host
        // resolved -- the same trust as linking it.
        let lib = unsafe { libloading::Library::new(path) }.map_err(|e| format!("load {path}: {e}"))?;
        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: the type matches the C prototype in
                // LiteRT-LM's `c/embedding_engine.h` / `c/engine.h`.
                *unsafe { lib.get($name) }.map_err(|e| {
                    format!("{}: {e}", String::from_utf8_lossy(&$name[..$name.len() - 1]))
                })?
            };
        }
        Ok(Self {
            settings_create: sym!(b"litert_lm_embedding_engine_settings_create\0"),
            settings_delete: sym!(b"litert_lm_embedding_engine_settings_delete\0"),
            engine_create: sym!(b"litert_lm_embedding_engine_create\0"),
            engine_delete: sym!(b"litert_lm_embedding_engine_delete\0"),
            options_create: sym!(b"litert_lm_embedding_options_create\0"),
            options_delete: sym!(b"litert_lm_embedding_options_delete\0"),
            options_set_normalize: sym!(b"litert_lm_embedding_options_set_normalize\0"),
            options_set_overflow: sym!(b"litert_lm_embedding_options_set_input_overflow_strategy\0"),
            input_create: sym!(b"litert_lm_input_data_create\0"),
            input_delete: sym!(b"litert_lm_input_data_delete\0"),
            compute: sym!(b"litert_lm_embedding_engine_compute_embedding\0"),
            response_delete: sym!(b"litert_lm_embedding_response_delete\0"),
            response_size: sym!(b"litert_lm_embedding_response_get_size\0"),
            response_values: sym!(b"litert_lm_embedding_response_get_values\0"),
            last_error: sym!(b"litert_lm_get_last_error_message\0"),
            set_min_log_level: sym!(b"litert_lm_set_min_log_level\0"),
            _lib: lib,
        })
    }

    /// The runtime's last error message, for logs.
    fn error(&self, what: &str, status: c_int) -> String {
        // SAFETY: returns a NUL-terminated string owned by the runtime (or
        // null); copied out immediately.
        let msg = unsafe {
            let p = (self.last_error)();
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        format!("{what} failed (status {status}): {msg}")
    }
}

/// A loaded engine plus the reusable options handle. Dropped (and both C
/// handles freed) when the actor thread ends.
struct Engine {
    api: Api,
    engine: Handle,
    options: Handle,
}

impl Engine {
    fn open(lib_path: &str, model_path: &str) -> Result<Self, String> {
        let api = Api::load(lib_path)?;
        // SAFETY: plain setter, no pointers. Best-effort: a failure only
        // means a noisier stderr.
        unsafe { (api.set_min_log_level)(LOG_ERROR) };
        let model = CString::new(model_path).map_err(|e| e.to_string())?;
        let backend = CString::new("cpu").expect("static");
        let mut settings: Handle = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated strings; null vision/audio backends
        // mean "text only"; `settings` is an out-parameter.
        let s = unsafe {
            (api.settings_create)(model.as_ptr(), backend.as_ptr(), std::ptr::null(), std::ptr::null(), &mut settings)
        };
        if s != STATUS_OK || settings.is_null() {
            return Err(api.error("embedding settings", s));
        }
        let mut engine: Handle = std::ptr::null_mut();
        // SAFETY: `settings` is live; the engine copies what it needs, so the
        // settings are freed right after either way.
        let s = unsafe { (api.engine_create)(settings, &mut engine) };
        unsafe { (api.settings_delete)(settings) };
        if s != STATUS_OK || engine.is_null() {
            return Err(api.error("embedding engine", s));
        }
        let mut options: Handle = std::ptr::null_mut();
        // SAFETY: out-parameter; then two setters on the live handle.
        let s = unsafe { (api.options_create)(&mut options) };
        if s != STATUS_OK || options.is_null() {
            unsafe { (api.engine_delete)(engine) };
            return Err(api.error("embedding options", s));
        }
        unsafe {
            (api.options_set_normalize)(options, true);
            (api.options_set_overflow)(options, OVERFLOW_CHUNK_AND_AVERAGE);
        }
        Ok(Self { api, engine, options })
    }

    fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
        let api = &self.api;
        let bytes = format_input(text).into_bytes();
        let mut input: Handle = std::ptr::null_mut();
        // SAFETY: the runtime copies `bytes` into the input object.
        let s = unsafe { (api.input_create)(INPUT_TEXT, bytes.as_ptr().cast(), bytes.len(), &mut input) };
        if s != STATUS_OK || input.is_null() {
            return Err(api.error("input data", s));
        }
        let mut response: Handle = std::ptr::null_mut();
        let inputs = [input];
        // SAFETY: one live input; `options` and `engine` live for `self`.
        let s = unsafe { (api.compute)(self.engine, inputs.as_ptr(), 1, self.options, &mut response) };
        unsafe { (api.input_delete)(input) };
        if s != STATUS_OK || response.is_null() {
            return Err(api.error("compute embedding", s));
        }
        let mut len: usize = 0;
        let mut values: *const f32 = std::ptr::null();
        // SAFETY: `values` points into `response`, copied out before it is
        // freed below.
        let out = unsafe {
            let s1 = (api.response_size)(response, &mut len);
            let s2 = (api.response_values)(response, &mut values);
            let v = if s1 == STATUS_OK && s2 == STATUS_OK && !values.is_null() {
                Ok(std::slice::from_raw_parts(values, len).to_vec())
            } else {
                Err(api.error("read embedding", if s1 != STATUS_OK { s1 } else { s2 }))
            };
            (api.response_delete)(response);
            v
        };
        out
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: both handles were created by this runtime and are freed
        // exactly once, before `api` (and with it the library) drops.
        unsafe {
            (self.api.options_delete)(self.options);
            (self.api.engine_delete)(self.engine);
        }
    }
}

struct Cmd(String, mpsc::Sender<Option<Vec<f32>>>);

/// Handle to the LiteRT-LM embedding actor thread.
pub struct LiteRtLmEmbedder {
    tx: Mutex<mpsc::Sender<Cmd>>,
}

impl LiteRtLmEmbedder {
    /// Spawn the actor thread. `lib_path` is the LiteRT-LM runtime with the
    /// embedding API (`litert-lm-embed.dll` / `liblitert-lm.so`), `model_path`
    /// the `.litertlm` embedding bundle. Returns immediately; a load failure
    /// on the thread answers every request with `None`, which callers treat
    /// as "embedder unavailable" (lexical fallback), never as an error.
    pub fn spawn(lib_path: impl Into<String>, model_path: impl Into<String>) -> Self {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let (lib_path, model_path) = (lib_path.into(), model_path.into());
        thread::Builder::new()
            .name("litert-lm-embed".into())
            .spawn(move || actor(&lib_path, &model_path, rx))
            .expect("spawn litert-lm-embed thread");
        Self { tx: Mutex::new(tx) }
    }
}

#[async_trait]
impl SemanticEmbedder for LiteRtLmEmbedder {
    async fn embed(&self, text: &str) -> Option<Vec<f32>> {
        let (rtx, rrx) = mpsc::channel();
        {
            let tx = self.tx.lock().ok()?;
            if tx.send(Cmd(text.to_string(), rtx)).is_err() {
                return None; // actor thread gone
            }
        }
        tokio::task::spawn_blocking(move || rrx.recv().ok().flatten())
            .await
            .ok()
            .flatten()
    }
}

fn actor(lib_path: &str, model_path: &str, rx: mpsc::Receiver<Cmd>) {
    let mut engine = match Engine::open(lib_path, model_path) {
        Ok(e) => {
            tracing::info!(model = %model_path, "litert-lm embedder ready");
            e
        }
        Err(e) => {
            tracing::warn!("litert-lm embedder unavailable: {e}");
            for Cmd(_, reply) in rx.iter() {
                let _ = reply.send(None);
            }
            return;
        }
    };
    for Cmd(text, reply) in rx.iter() {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            let _ = reply.send(Some(Vec::new()));
            continue;
        }
        let t0 = std::time::Instant::now();
        let v = match engine.embed(trimmed) {
            Ok(v) if !v.is_empty() => Some(v),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!("litert-lm embed: {e}");
                None
            }
        };
        tracing::debug!(
            ok = v.is_some(),
            dims = v.as_ref().map(Vec::len).unwrap_or(0),
            ms = t0.elapsed().as_millis() as u64,
            "litert-lm embed"
        );
        let _ = reply.send(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_document_prompt_wraps_the_text() {
        assert_eq!(format_input("hi"), "title: none | text: hi");
    }

    /// A missing runtime library is "unavailable", not a panic or a hang.
    #[tokio::test]
    async fn a_missing_library_answers_none() {
        let e = LiteRtLmEmbedder::spawn("no-such-litert-lm-library", "no-such-model.litertlm");
        assert_eq!(e.embed("hello").await, None);
    }

    /// Live check against the shipped runtime and model. Same env-var shape as
    /// `plugins::memory_eval`, plus `KITTY_LITERT_LM_EMBED_LIB`.
    #[tokio::test]
    #[ignore]
    async fn live_litert_lm_embedder_separates_topics() {
        let var = |n: &str| std::env::var(n).expect(n);
        let e = LiteRtLmEmbedder::spawn(var("KITTY_LITERT_LM_EMBED_LIB"), var("KITTY_EMBED_MODEL"));
        let v = |t: &'static str| async { e.embed(t).await.expect("embedding") };
        let (q, run, tax) = (
            v("How much should I drink during a 21 km race?").await,
            v("User is training for a half marathon.").await,
            v("The quarterly tax filing deadline is in April.").await,
        );
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        println!("dim {} race~marathon {:.3} race~tax {:.3}", q.len(), dot(&q, &run), dot(&q, &tax));
        assert_eq!(q.len(), 768);
        assert!(dot(&q, &run) > dot(&q, &tax) + 0.05);
    }
}
