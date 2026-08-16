//! LiteRT embedding backend: EmbeddingGemma `.tflite` → L2-normalised vector,
//! implementing [`adaptive_pathway::embed::SemanticEmbedder`] (the same seam the
//! retired llama.cpp `LocalPathwayEmbedder` filled).
//!
//! **Why an actor thread.** `edgefirst-tflite`'s `Library`/`Model`/`Interpreter`
//! wrap raw pointers (not `Send`/`Sync`) and form a borrow chain
//! (`Interpreter` borrows `Library`), which a normal struct can't store without
//! self-reference. Keeping the whole chain as locals on one dedicated OS thread
//! sidesteps both problems: nothing crosses a thread boundary, and inference is
//! serialised (TFLite interpreters are single-threaded anyway). `embed` talks to
//! that thread over a channel, so the public type is trivially `Send + Sync`.
//!
//! **Tokenisation** uses the pure-Rust `tokenizers` crate on the canonical Gemma
//! `tokenizer.json`. EmbeddingGemma wants a task prefix; we use the document
//! prompt consistently for every belief snippet so the whole vector space is
//! comparable (adaptive-pathway only ever compares vectors within this space).

use std::sync::{mpsc, Mutex};
use std::thread;

use async_trait::async_trait;
use tokenizers::Tokenizer;

use adaptive_pathway::embed::SemanticEmbedder;

/// EmbeddingGemma's fixed input sequence length for the `seq256` model variant.
const SEQ_LEN: usize = 256;

/// Document-embedding prompt EmbeddingGemma is trained with. Applied to every
/// snippet so stored beliefs and recall queries share one prefix (and thus one
/// comparable space).
fn format_input(text: &str) -> String {
    format!("title: none | text: {text}")
}

enum Cmd {
    Embed(String, mpsc::Sender<Option<Vec<f32>>>),
}

/// Handle to the embedding actor thread. Cheap to clone-free share via `Arc`.
///
/// The sender is behind a `Mutex` only to be `Sync` (`mpsc::Sender` is `Send`
/// but not `Sync`, and `SemanticEmbedder: Send + Sync`); the lock is never held
/// across the actual embedding work.
pub struct LiteRtEmbedder {
    tx: Mutex<mpsc::Sender<Cmd>>,
}

impl LiteRtEmbedder {
    /// Spawn the actor thread. `lib_path` is the LiteRT runtime
    /// (`libLiteRt.dll`/`.so`), `model_path` the EmbeddingGemma `.tflite`,
    /// `tokenizer_path` the Gemma `tokenizer.json`.
    ///
    /// Returns immediately; the (slow) model load happens on the thread. A load
    /// failure does not error here — the thread then answers every request with
    /// `None`, which `SemanticEmbedder` callers already treat as "unavailable,
    /// fall back to lexical hashing", exactly like a missing model.
    pub fn spawn(
        lib_path: impl Into<String>,
        model_path: impl Into<String>,
        tokenizer_path: impl Into<String>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let lib_path = lib_path.into();
        let model_path = model_path.into();
        let tokenizer_path = tokenizer_path.into();
        thread::Builder::new()
            .name("litert-embed".into())
            .spawn(move || actor(lib_path, model_path, tokenizer_path, rx))
            .expect("spawn litert-embed thread");
        Self { tx: Mutex::new(tx) }
    }
}

#[async_trait]
impl SemanticEmbedder for LiteRtEmbedder {
    async fn embed(&self, text: &str) -> Option<Vec<f32>> {
        let (rtx, rrx) = mpsc::channel();
        {
            let tx = self.tx.lock().ok()?;
            if tx.send(Cmd::Embed(text.to_string(), rtx)).is_err() {
                return None; // actor thread gone
            }
        }
        // The actor replies on a std channel; do the blocking wait off the
        // async runtime so a slow forward pass never stalls a worker.
        tokio::task::spawn_blocking(move || rrx.recv().ok().flatten())
            .await
            .ok()
            .flatten()
    }
}

/// The actor thread body. Owns the whole non-`Send` LiteRT chain for its
/// lifetime; loops answering embed requests. On any load failure it still drains
/// the channel, replying `None` so callers never hang.
///
/// `lib`, `model` and `it` are all kept as locals in this one scope: `Model` and
/// `Interpreter` both borrow `lib`, and the TFLite interpreter references the
/// model's buffer for its whole life (the C API requires the model to outlive
/// it), so the model must not be dropped early. Locals drop in reverse order
/// (`it`, then `model`, then `lib`), which is exactly the required teardown
/// order.
fn actor(lib_path: String, model_path: String, tokenizer_path: String, rx: mpsc::Receiver<Cmd>) {
    use edgefirst_tflite::{Interpreter, Library, Model};

    /// Reply `None` to every request, forever. Used when the engine can't load.
    fn drain_unavailable(rx: mpsc::Receiver<Cmd>) {
        for Cmd::Embed(_, reply) in rx.iter() {
            let _ = reply.send(None);
        }
    }

    let tok = match Tokenizer::from_file(&tokenizer_path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("litert embedder tokenizer {tokenizer_path}: {e}");
            return drain_unavailable(rx);
        }
    };
    let lib = match Library::from_path(&lib_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("litert embedder runtime {lib_path}: {e}");
            return drain_unavailable(rx);
        }
    };
    let model = match Model::from_file(&lib, &model_path) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("litert embedder model {model_path}: {e}");
            return drain_unavailable(rx);
        }
    };
    let mut it = match Interpreter::builder(&lib).and_then(|b| b.build(&model)) {
        Ok(it) => it,
        Err(e) => {
            tracing::warn!("litert embedder interpreter: {e}");
            return drain_unavailable(rx);
        }
    };
    for i in 0..it.input_count() {
        let _ = it.resize_input(i, &[1, SEQ_LEN as i32]);
    }
    if let Err(e) = it.allocate_tensors() {
        tracing::warn!("litert embedder allocate_tensors: {e}");
        return drain_unavailable(rx);
    }

    tracing::info!(model = %model_path, "litert embedder ready");
    for Cmd::Embed(text, reply) in rx.iter() {
        let t0 = std::time::Instant::now();
        let v = embed_once(&mut it, &tok, &text);
        tracing::debug!(
            ok = v.is_some(),
            dims = v.as_ref().map(|x| x.len()).unwrap_or(0),
            ms = t0.elapsed().as_millis() as u64,
            "litert embed"
        );
        let _ = reply.send(v);
    }
}

fn embed_once(
    it: &mut edgefirst_tflite::Interpreter<'_>,
    tok: &Tokenizer,
    text: &str,
) -> Option<Vec<f32>> {
    use edgefirst_tflite::TensorType;

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    let enc = tok.encode(format_input(trimmed), true).ok()?;
    let mut ids: Vec<i32> = enc.get_ids().iter().map(|&x| x as i32).collect();
    ids.truncate(SEQ_LEN);
    ids.resize(SEQ_LEN, 0); // pad token

    for mut t in it.inputs_mut().ok()? {
        if t.tensor_type() == TensorType::Int32 {
            t.copy_from_slice(&ids).ok()?;
        }
    }
    it.invoke().ok()?;
    let out = it
        .outputs()
        .ok()?
        .into_iter()
        .find(|t| t.tensor_type() == TensorType::Float32)?;
    let v = out.as_slice::<f32>().ok()?.to_vec();
    Some(l2_normalise(&v))
}

/// EmbeddingGemma already emits unit vectors, but re-normalise defensively so
/// cosine reduces to a dot product exactly (adaptive-pathway assumes this).
fn l2_normalise(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-12 {
        v.iter().map(|x| x / norm).collect()
    } else {
        v.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation_yields_unit_length() {
        let out = l2_normalise(&[3.0, 4.0]);
        let len: f32 = out.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((len - 1.0).abs() < 1e-6, "got {len}");
    }

    #[test]
    fn the_document_prompt_wraps_the_text() {
        assert_eq!(format_input("hi"), "title: none | text: hi");
    }
}
