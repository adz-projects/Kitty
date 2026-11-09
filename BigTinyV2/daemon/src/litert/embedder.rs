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
    use edgefirst_tflite::{Library, Model};

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
    // GPU-first with a CPU (XNNPACK) fallback. `edgefirst-tflite` has no
    // built-in GPU backend — a GPU delegate is an external shared library — so
    // this is opportunistic: `gpu_delegate_path` finds a delegate lib only when
    // one is present (via `KITTY_LITERT_GPU_DELEGATE` or shipped next to the
    // runtime). When none is found (the default today) the delegate is `None`
    // and this behaves exactly like the previous CPU-only path. A GPU build or
    // tensor-allocation failure retries once on plain CPU so the embedder never
    // regresses to "unavailable" just because the GPU path failed.
    let gpu_delegate = gpu_delegate_path(&lib_path).and_then(|p| {
        match edgefirst_tflite::Delegate::load(&p) {
            Ok(d) => {
                tracing::info!(delegate = %p, "litert embedder GPU delegate loaded");
                Some(d)
            }
            Err(e) => {
                tracing::warn!("litert embedder GPU delegate {p} failed to load: {e}; using CPU");
                None
            }
        }
    });
    let tried_gpu = gpu_delegate.is_some();
    let mut it = match build_interpreter(&lib, &model, gpu_delegate) {
        Ok(it) => it,
        Err(e) if tried_gpu => {
            tracing::warn!("litert embedder GPU interpreter failed ({e}); falling back to CPU");
            match build_interpreter(&lib, &model, None) {
                Ok(it) => it,
                Err(e2) => {
                    tracing::warn!("litert embedder interpreter: {e2}");
                    return drain_unavailable(rx);
                }
            }
        }
        Err(e) => {
            tracing::warn!("litert embedder interpreter: {e}");
            return drain_unavailable(rx);
        }
    };

    tracing::info!(model = %model_path, gpu = tried_gpu, "litert embedder ready");
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

/// Build (and prepare) an interpreter, optionally with a GPU `delegate`
/// attached. Factored out so the actor can try GPU then retry CPU with the same
/// resize/allocate steps. Returns a plain `String` error so both legs share one
/// path. The returned interpreter borrows `lib` (and, transitively, `model`),
/// so both must outlive it — which they do, as locals in the actor scope.
fn build_interpreter<'lib>(
    lib: &'lib edgefirst_tflite::Library,
    model: &edgefirst_tflite::Model<'lib>,
    delegate: Option<edgefirst_tflite::Delegate>,
) -> Result<edgefirst_tflite::Interpreter<'lib>, String> {
    let mut builder = edgefirst_tflite::Interpreter::builder(lib).map_err(|e| e.to_string())?;
    if let Some(d) = delegate {
        builder = builder.delegate(d);
    }
    let mut it = builder.build(model).map_err(|e| e.to_string())?;
    for i in 0..it.input_count() {
        let _ = it.resize_input(i, &[1, SEQ_LEN as i32]);
    }
    it.allocate_tensors().map_err(|e| e.to_string())?;
    Ok(it)
}

/// Resolve a LiteRT GPU delegate library path, or `None` to run on CPU.
/// `KITTY_LITERT_GPU_DELEGATE` (an explicit path) wins; otherwise look for a
/// known GPU-delegate library shipped next to the LiteRT runtime. Returns
/// `None` when nothing is found — which is the default today, since we bundle
/// no GPU delegate — so the embedder simply runs on CPU/XNNPACK as before.
fn gpu_delegate_path(lib_path: &str) -> Option<String> {
    if let Ok(p) = std::env::var("KITTY_LITERT_GPU_DELEGATE") {
        let p = p.trim();
        if !p.is_empty() {
            return Some(p.to_string());
        }
    }
    let dir = std::path::Path::new(lib_path).parent()?;
    const CANDIDATES: &[&str] = &[
        "libLiteRtGpuAccelerator.so",
        "libLiteRtGpuAccelerator.dll",
        "LiteRtGpuAccelerator.dll",
        "libtensorflowlite_gpu_delegate.so",
        "tensorflowlite_gpu_delegate.dll",
    ];
    for name in CANDIDATES {
        let cand = dir.join(name);
        if cand.exists() {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
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

    /// Diagnostic: the model's declared I/O, and how well each way of
    /// feeding it separates obviously-related from unrelated text. Run with
    /// the same env vars as `plugins::memory_eval`.
    #[test]
    #[ignore]
    fn embedder_io_diagnostic() {
        use edgefirst_tflite::{Library, Model, TensorType};
        let var = |n: &str| std::env::var(n).expect(n);
        let (lib_path, model_path, tok_path) =
            (var("KITTY_LITERT_LIB"), var("KITTY_EMBED_MODEL"), var("KITTY_EMBED_TOKENIZER"));
        let tok = Tokenizer::from_file(&tok_path).unwrap();
        let lib = Library::from_path(&lib_path).unwrap();
        let model = Model::from_file(&lib, &model_path).unwrap();
        let mut it = build_interpreter(&lib, &model, None).unwrap();

        for t in it.inputs().unwrap() {
            println!("input  {:<30} {:?} {:?}", t.name(), t.tensor_type(), t.shape());
        }
        for t in it.outputs().unwrap() {
            println!("output {:<30} {:?} {:?}", t.name(), t.tensor_type(), t.shape());
        }

        // Tokenisation sanity and padding influence.
        {
            let sample = format_input("User bakes sourdough bread on weekends.");
            let enc = tok.encode(sample.as_str(), true).unwrap();
            println!("ids {:?}", enc.get_ids());
            println!("decoded {:?}", tok.decode(enc.get_ids(), false).unwrap());
            let mut run = |text: &str, pad: i32, keep_eos: bool| -> Vec<f32> {
                let enc = tok.encode(text, true).unwrap();
                let mut ids: Vec<i32> = enc.get_ids().iter().map(|&x| x as i32).collect();
                if !keep_eos && ids.last() == Some(&1) {
                    ids.pop();
                }
                ids.resize(SEQ_LEN, pad);
                for mut t in it.inputs_mut().unwrap() {
                    t.copy_from_slice(&ids).unwrap();
                }
                it.invoke().unwrap();
                let out = it.outputs().unwrap().into_iter().next().unwrap();
                l2_normalise(out.as_slice::<f32>().unwrap())
            };
            let d = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
            let a = format_input("User bakes sourdough bread on weekends.");
            let b = format_input("The quarterly tax filing deadline is in April.");
            let (a0, a1, b0, b1) = (run(&a, 0, true), run(&a, 1, true), run(&b, 0, true), run(&b, 1, true));
            println!("same text, pad 0 vs pad 1      : {:.3}", d(&a0, &a1));
            println!("unrelated pair, pad 0         : {:.3}", d(&a0, &b0));
            println!("unrelated pair, pad 1         : {:.3}", d(&a1, &b1));
            let (an, bn) = (run(&a, 0, false), run(&b, 0, false));
            println!("unrelated pair, pad 0, no eos : {:.3}", d(&an, &bn));
            println!("same text, eos vs no eos      : {:.3}", d(&a0, &an));
        }

        // mode 0: production (ids into every Int32 input).
        // mode 1: ids into the first Int32 input, a 1/0 mask into any other.
        let mut embed = |text: &str, mode: u8| -> Vec<f32> {
            let enc = tok.encode(text, true).unwrap();
            let mut ids: Vec<i32> = enc.get_ids().iter().map(|&x| x as i32).collect();
            ids.truncate(SEQ_LEN);
            let real = ids.len();
            ids.resize(SEQ_LEN, 0);
            let mask: Vec<i32> = (0..SEQ_LEN).map(|i| i32::from(i < real)).collect();
            let mut k = 0;
            for mut t in it.inputs_mut().unwrap() {
                if t.tensor_type() == TensorType::Int32 {
                    let data = if mode == 1 && k > 0 { &mask } else { &ids };
                    t.copy_from_slice(data).unwrap();
                    k += 1;
                }
            }
            it.invoke().unwrap();
            let out = it
                .outputs()
                .unwrap()
                .into_iter()
                .find(|t| t.tensor_type() == TensorType::Float32)
                .unwrap();
            l2_normalise(out.as_slice::<f32>().unwrap())
        };
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();

        let docs = [
            "User is training for a half marathon.",
            "User believes large technology companies act mainly in their own interest rather than the public's.",
            "User bakes sourdough bread on weekends.",
            "User uses SQLite for local storage in their projects.",
        ];
        let queries = [
            ("How much should I drink during a 21 km race?", 0),
            ("My sourdough starter isn't rising anymore. What's wrong?", 2),
            ("When should I turn on WAL mode in SQLite?", 3),
            ("Why do AI labs lobby for regulation that burdens smaller competitors?", 1),
        ];
        for (label, mode, qfmt) in [
            ("production", 0u8, false),
            ("mask", 1, false),
            ("mask+query prompt", 1, true),
            ("production+query prompt", 0, true),
        ] {
            let dv: Vec<Vec<f32>> = docs.iter().map(|d| embed(&format_input(d), mode)).collect();
            let mut right = 0;
            let mut margins = Vec::new();
            for (q, want) in queries {
                let qt = if qfmt { format!("task: search result | query: {q}") } else { format_input(q) };
                let qv = embed(&qt, mode);
                let s: Vec<f32> = dv.iter().map(|d| dot(&qv, d)).collect();
                let best = (0..s.len()).max_by(|&a, &b| s[a].partial_cmp(&s[b]).unwrap()).unwrap();
                right += usize::from(best == want);
                let other = (0..s.len()).filter(|&i| i != want).map(|i| s[i]).fold(f32::MIN, f32::max);
                margins.push(format!("{:+.3}", s[want] - other));
                println!("  [{label}] {q:<70} {}", s.iter().map(|x| format!("{x:.3}")).collect::<Vec<_>>().join(" "));
            }
            println!("{label:<26} right {right}/4  margins {}", margins.join(" "));
        }
    }
}
