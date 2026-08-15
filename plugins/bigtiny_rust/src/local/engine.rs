//! `LocalEngine` — one loaded GGUF plus the knobs needed to make contexts
//! from it (docs/ANDROID.md §3.1).
//!
//! Scope is deliberately narrow. This owns the model and knows how to build a
//! correctly-configured context; it does **not** own residency policy (that's
//! [`super::manager`]) or wire formats (that's `provider`/`embeddings`).
//! Keeping the llama.cpp surface behind this one type is what makes D1's
//! "the binding stays swappable" claim true rather than aspirational.

use std::ffi::CString;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use llama_cpp_2::context::params::{KvCacheType, LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::LlamaModel;

use crate::config::LocalEngineConfig;

/// Last-resort context size when `n_ctx` is `0` ("automatic") and neither
/// `fit_params` nor [`estimate_n_ctx`] could produce a number — which now
/// means only "the device registry reported no memory at all". Deliberately
/// the same as `config::default_local_n_ctx()`: "automatic" must never
/// silently mean llama.cpp's own `0`, which resolves to the model's full
/// `n_ctx_train` (128k on LFM2.5) and would allocate a KV cache far past what
/// the machine can hold.
const AUTO_N_CTX_FALLBACK: u32 = 4096;

/// The context size to advertise for the local provider *before any model is
/// loaded* — at provider-registration time and in `discover_models` while
/// the slot is still cold. The pinned `n_ctx` when one is configured, or
/// [`AUTO_N_CTX_FALLBACK`] when `n_ctx` is `0` ("automatic"), which is the
/// same value the automatic path bottoms out at when neither fitting nor
/// estimation can produce a number.
///
/// The real (fitted/estimated, `n_ctx_train`-clamped) resolution only exists
/// once a model is resident — see [`LocalEngine::effective_n_ctx`], which
/// `discover_models` prefers when the slot is loaded. Advertising nothing
/// instead made the agent budget against the daemon-wide 64k default while
/// the engine's real context was 4k, so compaction never fired before the
/// context hard-failed; advertising the literal `0` sentinel was no better.
pub fn registration_n_ctx(cfg: &LocalEngineConfig) -> u32 {
    if cfg.n_ctx > 0 {
        cfg.n_ctx
    } else {
        AUTO_N_CTX_FALLBACK
    }
}

/// Fraction of the device's free memory the KV cache may claim when sizing an
/// automatic context.
///
/// Half, and the other half is not slack for its own sake — the weights come
/// out of the same budget (subtracted separately), and what remains covers the
/// compute buffer, the logits/embeddings buffers, and allocator fragmentation.
/// Those are not small: the Phase 1 measurement put the CPU compute buffer at
/// ~142 MiB for `n_ctx` 2048 alone, and it grows with `n_batch`. A KV cache
/// sized to *all* remaining memory reliably OOMs at context-creation time,
/// which is a worse failure than a smaller context.
const AUTO_KV_MEMORY_FRACTION: u64 = 2;

/// Bytes per 32-element block for a KV cache type — the quantisation block
/// size llama.cpp uses, so these are exact rather than rounded.
///
/// Expressed per block rather than per element to keep the sizing arithmetic
/// in integers: `q4_0` is 0.5625 bytes/element, and doing that in floats would
/// introduce rounding into a number that decides an allocation.
fn kv_block_bytes(t: KvCacheType) -> u64 {
    match t {
        KvCacheType::F32 => 128,
        KvCacheType::F16 => 64,
        KvCacheType::Q8_0 => 34,
        KvCacheType::Q5_1 => 24,
        KvCacheType::Q5_0 => 22,
        KvCacheType::Q4_1 => 20,
        KvCacheType::Q4_0 => 18,
        // Any type llama.cpp adds later: assume the widest, so an unknown
        // type under-estimates the context rather than over-committing memory.
        _ => 128,
    }
}

/// The model geometry [`estimate_n_ctx`] needs. A struct so the estimator can
/// be tested against real published numbers without a GGUF on disk.
#[derive(Debug, Clone, Copy)]
pub struct KvGeometry {
    pub n_layer: u32,
    pub n_embd: u32,
    /// Attention heads, and key/value heads. These differ under GQA/MQA, which
    /// is what makes the KV cache far smaller than `n_embd` would suggest —
    /// ignoring it would under-estimate the context by the GQA ratio (8x on
    /// many current models).
    pub n_head: u32,
    pub n_head_kv: u32,
}

/// KV-cache bytes per token of context. `None` when the geometry is
/// degenerate (a zero attention-head count would divide by zero).
///
/// **`n_head_kv == 0` means "unknown", not "no attention".** Measured on
/// LFM2.5-1.2B, which reports 16 layers, 32 heads and *zero* KV heads: it is a
/// hybrid stack interleaving shortconv blocks with attention blocks, so
/// llama.cpp's single model-level query has no correct answer to give. Falling
/// back to `n_head` assumes full multi-head attention on every layer, which
/// over-charges such a model twice over (no GQA discount, and layers that
/// hold no KV cache at all counted as if they did) — the safe direction, and
/// still far better than abandoning the estimate for a flat default.
fn kv_bytes_per_token(g: KvGeometry, k: KvCacheType, v: KvCacheType) -> Option<u64> {
    if g.n_head == 0 || g.n_layer == 0 {
        return None;
    }
    let n_head_kv = if g.n_head_kv == 0 {
        g.n_head
    } else {
        g.n_head_kv
    };
    let head_dim = (g.n_embd / g.n_head) as u64;
    // One K entry and one V entry per KV head per layer.
    let elements = g.n_layer as u64 * head_dim * n_head_kv as u64;
    Some((elements * (kv_block_bytes(k) + kv_block_bytes(v))) / 32)
}

/// Estimate a context size that fits: **half the device's free memory, less
/// the weights**, divided by the per-token KV cost.
///
/// This is what `n_ctx = 0` resolves to when `fit_params` declines to pick a
/// size (which is most of the time — it only shrinks the context under
/// pressure). The alternative was a flat 4096 regardless of hardware, which
/// wastes a 24 GB card and over-commits a 4 GB one.
///
/// Deliberately conservative in three ways, because the failure modes are
/// asymmetric — too small costs recall, too large fails the load outright:
/// - Half the free memory, not all of it (see [`AUTO_KV_MEMORY_FRACTION`]).
/// - The weights are subtracted at their full size even when only some layers
///   are offloaded, so a partial offload leaves the GPU budget over-charged.
/// - Layer count is taken as uniform, so hybrid architectures that only
///   attend on *some* layers (LFM2.5 interleaves shortconv blocks) are
///   over-charged too.
///
/// Returns `None` when there is no basis to estimate — no memory figure, a
/// budget the weights already exhaust, or degenerate geometry — and the caller
/// falls back rather than inventing a number.
pub fn estimate_n_ctx(
    g: KvGeometry,
    k: KvCacheType,
    v: KvCacheType,
    usable_memory: u64,
    model_bytes: u64,
) -> Option<u32> {
    let budget = (usable_memory / AUTO_KV_MEMORY_FRACTION).checked_sub(model_bytes)?;
    let per_token = kv_bytes_per_token(g, k, v)?;
    if budget == 0 || per_token == 0 {
        return None;
    }
    let tokens = budget / per_token;
    // Round down to a 256-token boundary: the exact quotient is false
    // precision on an estimate this coarse, and a tidy number is easier to
    // recognise in a log or on the model card.
    let tokens = (tokens / 256) * 256;
    if tokens < FIT_N_CTX_MIN as u64 {
        // Below the floor, report no estimate rather than a useless context.
        // The caller's fallback then applies, and the load either fits or
        // fails loudly — both better than silently running at 512 tokens.
        return None;
    }
    Some(u32::try_from(tokens).unwrap_or(u32::MAX))
}

/// Floor handed to `fit_params` as `n_ctx_min`. Fitting is allowed to shrink
/// the context to make the model fit VRAM, but a sub-2k context makes the
/// summarizer useless for its actual job, so below this we would rather it
/// report failure and fall back to CPU.
const FIT_N_CTX_MIN: u32 = 2048;

/// The floor must leave a usable context, and must not exceed the size an
/// unfitted automatic load settles on — a floor above the fallback would mean
/// fitting could only ever *raise* the context, which is not what a
/// memory-pressure floor is for. Compile-time rather than a test: it is a
/// property of two constants, so it should fail the build, not a run.
const _: () = assert!(FIT_N_CTX_MIN >= 2048 && FIT_N_CTX_MIN <= AUTO_N_CTX_FALLBACK);

#[derive(Debug, thiserror::Error)]
pub enum LocalEngineError {
    #[error("local engine is not configured: {0}")]
    NotConfigured(String),
    #[error("model file not found: {0}")]
    ModelNotFound(PathBuf),
    #[error("llama backend init failed: {0}")]
    Backend(String),
    #[error("failed to load model {path}: {source}")]
    Load {
        path: PathBuf,
        #[source]
        source: llama_cpp_2::LlamaModelLoadError,
    },
    #[error("failed to create context: {0}")]
    Context(String),
    #[error("inference failed: {0}")]
    Inference(String),
}

/// How an embedding model pools token states into one sequence vector.
///
/// Carried per-model rather than fixed in the engine: llama.cpp's default is
/// `None`, which produces *no* sequence embedding — a causal-LM-derived
/// embedder (Qwen3-Embedding) needs `Last`, a BERT-style one (bge/gte/nomic)
/// needs `Mean` or `Cls`. Getting this wrong is not subtle: `None` fails
/// loudly with `NonePoolType`, but `Mean` on a last-token model would return
/// plausible-looking vectors that quietly degrade recall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedPooling {
    Last,
    Mean,
    Cls,
}

impl EmbedPooling {
    /// Parse the config string. Unknown values fall back to `Last` with a
    /// warning rather than erroring — a typo shouldn't take the daemon down,
    /// and `Last` matches the pinned default model (§9.2).
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "mean" => Self::Mean,
            "cls" => Self::Cls,
            "last" => Self::Last,
            other => {
                tracing::warn!("unknown embed_pooling {other:?}; defaulting to `last`");
                Self::Last
            }
        }
    }

    fn to_llama(self) -> LlamaPoolingType {
        match self {
            Self::Last => LlamaPoolingType::Last,
            Self::Mean => LlamaPoolingType::Mean,
            Self::Cls => LlamaPoolingType::Cls,
        }
    }
}

/// Parse `LocalEngineConfig::cache_type_k`/`_v` — `"f16"` (default), or a
/// quantised type (`"q8_0"`, `"q4_0"`, `"q4_1"`, `"q5_0"`, `"q5_1"`) to trade
/// KV-cache memory for a small quality/speed cost on long contexts.
///
/// Unknown values fall back to `F16` with a warning rather than erroring —
/// same reasoning as `EmbedPooling::parse`: a typo in an advanced setting
/// shouldn't take the daemon down, and `F16` is the always-safe choice.
///
/// **Deliberately does not touch flash attention.** Quantised KV cache needs
/// it on some backends, but `LlamaContextParams::default()`'s `AUTO` policy
/// (which nothing here overrides) already asks llama.cpp to decide that per
/// backend — matching D19 ("auto-detected, never a user toggle"). Setting a
/// non-`f16` type is consequently an advanced, deliberate choice whose safety
/// on a given backend is upstream's to determine, not this function's.
fn parse_kv_cache_type(s: &str) -> KvCacheType {
    match s.trim().to_ascii_lowercase().as_str() {
        "f16" | "" => KvCacheType::F16,
        "f32" => KvCacheType::F32,
        "q8_0" => KvCacheType::Q8_0,
        "q4_0" => KvCacheType::Q4_0,
        "q4_1" => KvCacheType::Q4_1,
        "q5_0" => KvCacheType::Q5_0,
        "q5_1" => KvCacheType::Q5_1,
        other => {
            tracing::warn!("unknown cache_type {other:?}; defaulting to f16");
            KvCacheType::F16
        }
    }
}

/// The llama.cpp backend is a process-wide singleton — initialising it twice
/// is an error, and it must outlive every model. Held in an `Arc` so the
/// manager can hand clones to each slot.
pub fn shared_backend() -> Result<Arc<LlamaBackend>, LocalEngineError> {
    use std::sync::OnceLock;
    static BACKEND: OnceLock<Result<Arc<LlamaBackend>, String>> = OnceLock::new();
    BACKEND
        .get_or_init(|| {
            LlamaBackend::init()
                .map(Arc::new)
                .map_err(|e| e.to_string())
        })
        .clone()
        .map_err(LocalEngineError::Backend)
}

/// Ask llama.cpp's own solver to choose `n_gpu_layers` (and, when `n_ctx` is
/// `0`, the context size) for the device it just selected — D20's fit half,
/// superseding §3.3's hand-rolled `(file_size x resident_fraction) + KV +
/// scratch, x1.18` estimate.
///
/// Returns the context size fitting settled on, or `None` if fitting failed —
/// in which case the caller loads with llama.cpp's own `-1` ("all layers")
/// default, exactly as it did before this existed. **A fit failure is not a
/// load failure**: the honest reading of "could not find allocations that fit"
/// is "this may not fit", not "this cannot work", and the load that follows
/// either succeeds or reports its own, more specific error.
///
/// # Preconditions
///
/// `params` must not have any field `fit_params` decides already set —
/// `fit_params` only writes fields still holding their default, so a prior
/// `with_n_gpu_layers` would silently make the call a no-op for the one field
/// it exists to decide. A device pin (`with_devices`) is fine and expected:
/// it constrains *which* device fit sizes for, not the `n_gpu_layers`/`n_ctx`
/// it chooses. [`LocalEngine::load`] enforces this by only reaching here on
/// the branch that has set nothing else.
fn fit_to_device(
    path: &Path,
    params: Pin<&mut LlamaModelParams>,
    requested_n_ctx: u32,
) -> Option<u32> {
    use std::sync::Mutex;

    // `common_fit_params` mutates llama.cpp's global logger state, so it is
    // explicitly not thread-safe. Slot loads are already serialised by the
    // manager today, but that is its scheduling decision, not a guarantee this
    // function may rely on — two slots warming concurrently is a change away.
    static FIT_LOCK: Mutex<()> = Mutex::new(());

    let c_path = CString::new(path.to_string_lossy().as_bytes()).ok()?;
    // `n_ctx = 0` is fit's "you choose" signal; any other value is left alone,
    // so a user who pinned a context still gets layer fitting against it.
    let mut cparams = LlamaContextParams::default().with_n_ctx(NonZeroU32::new(requested_n_ctx));
    // One margin per device, all zero: leave no headroom beyond what fit
    // already reserves. `max_devices` is the documented minimum length.
    let mut margins = vec![0usize; llama_cpp_2::max_devices()];

    let _guard = FIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 3 = GGML_LOG_LEVEL_WARN. Named constants for these live in
    // `llama-cpp-sys-2`, which this crate deliberately does not depend on
    // directly — `llama-cpp-2` is the whole binding surface (D1).
    match params.fit_params(&c_path, &mut cparams, &mut margins, FIT_N_CTX_MIN, 3) {
        // A returned `0` is "no opinion", not a size: fitting only rewrites
        // `n_ctx` when it has to shrink the context to make the model fit, and
        // otherwise hands back the `0` it was given. Measured on a GTX 1650 Ti
        // where all 17 layers fit — it returned 0, and passing that through
        // would reach llama.cpp as "use the full `n_ctx_train`" (128k on
        // LFM2.5), the exact opposite of a memory-aware choice.
        Ok(fit) if fit.n_ctx > 0 => Some(fit.n_ctx),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                "fit_params could not size this model to the device ({e}); \
                 falling back to offloading all layers"
            );
            None
        }
    }
}

/// Whether [`fit_to_device`] should run for this load.
///
/// Its own precondition ("params must be untouched") is not something the
/// compiler checks, so the rule lives here, next to the branch in
/// [`LocalEngine::load`] that has to agree with it, rather than being
/// re-derived at the call site.
fn should_fit(on_cpu: bool, configured_n_gpu_layers: i32) -> bool {
    // On CPU there is nowhere to offload to, so fitting could only ever
    // answer "0 layers" — and it would pay a globally-mutating, non-thread-safe
    // call to say so. A pinned layer count is the user overriding the solver,
    // which is the whole point of the setting existing.
    !on_cpu && configured_n_gpu_layers < 0
}

/// Resolve the context size a generation context is built with.
///
/// Split out from [`LocalEngine::effective_n_ctx`] because the method needs a
/// loaded model for `n_ctx_train`, and the interesting behaviour — what
/// "automatic" means, and that the clamp applies to *every* path, not just the
/// configured one — is worth pinning without a GGUF on disk.
///
/// The automatic path has three sources in descending order of authority:
/// `fitted` (llama.cpp measured it), `estimated` (we computed it from real
/// geometry and a real memory figure), then [`AUTO_N_CTX_FALLBACK`].
fn resolve_n_ctx(
    configured: u32,
    fitted: Option<u32>,
    estimated: Option<u32>,
    n_ctx_train: u32,
) -> u32 {
    let requested = match configured {
        // `filter` rather than a bare `unwrap_or`: a fitted `0` means "fitting
        // had no opinion" and must not be mistaken for a chosen size.
        // `fit_to_device` already normalises that away, and this is the second
        // line of defence on the value that decides KV-cache allocation.
        0 => fitted
            .filter(|n| *n > 0)
            .or(estimated)
            .unwrap_or(AUTO_N_CTX_FALLBACK),
        n => n,
    };
    requested.min(n_ctx_train)
}

/// A loaded model, ready to mint contexts.
pub struct LocalEngine {
    model: LlamaModel,
    backend: Arc<LlamaBackend>,
    path: PathBuf,
    cfg: LocalEngineConfig,
    /// Which compute backend this model was loaded onto (D20).
    selected_backend: super::backend::SelectedBackend,
    /// Layers actually offloaded, read back off the params after any fitting.
    /// Reported on the model card so "Automatic" is inspectable rather than
    /// opaque.
    n_gpu_layers: i32,
    /// Context size `fit_params` chose, when it ran and `n_ctx` was automatic.
    /// `None` means the configured value (or [`AUTO_N_CTX_FALLBACK`]) applies.
    fitted_n_ctx: Option<u32>,
}

impl LocalEngine {
    /// Load `path` under `cfg`. Blocking and slow (hundreds of MB off disk) —
    /// callers on an async runtime must `spawn_blocking` this.
    pub fn load(path: &Path, cfg: &LocalEngineConfig) -> Result<Self, LocalEngineError> {
        if !path.is_file() {
            return Err(LocalEngineError::ModelNotFound(path.to_path_buf()));
        }
        let backend = shared_backend()?;

        // D20: pick the compute backend from llama.cpp's own device registry
        // before deciding how many layers to offload. `select_backend`
        // resolves `"auto"`/`"cuda"`/`"vulkan"`/`"cpu"` against what's
        // actually enumerated, falling back to CPU rather than failing.
        let selected = super::backend::select_backend(&cfg.backend);

        let on_cpu = selected.kind() == super::backend::BackendKind::Cpu;
        // NOTE (815bugs #94, reverted): we deliberately do NOT pin the load to
        // `selected.device_index` via `LlamaModelParams::with_devices`. The
        // index is still recorded on `SelectedBackend` and used for the model
        // card / VRAM sizing, but applying it to the load params hangs the
        // tensor load indefinitely on at least one real multi-GPU Vulkan setup
        // (Intel UHD + discrete NVIDIA): the load never reaches `load_tensors`,
        // the daemon never becomes healthy, and Kitty reports `backend_down`.
        // Letting llama.cpp keep its own default device selection is the
        // known-good behavior. The only cost is a cosmetic mismatch when the UI
        // labels the load device — far better than a daemon that won't start.
        // Do not re-add `with_devices` here without validating a discrete-GPU
        // Vulkan load on real hardware.
        //
        // Three-way, and the ordering matters: `fit_params` may only run on
        // params where nothing has been set (see `fit_to_device`), so the
        // fitting branch must be the one that touches nothing.
        let mut params = Box::pin(if on_cpu {
            // Force 0 rather than leaving `-1` ("all layers"): with no GPU
            // selected there is nowhere to offload to, and being explicit
            // keeps the intent legible in a crash log.
            LlamaModelParams::default().with_n_gpu_layers(0)
        } else if cfg.n_gpu_layers >= 0 {
            LlamaModelParams::default().with_n_gpu_layers(cfg.n_gpu_layers as u32)
        } else {
            LlamaModelParams::default()
        });

        // Automatic offload: a GPU is selected and the user hasn't pinned a
        // layer count. Skipped on CPU, where it could only ever answer "0
        // layers" while still paying for a globally-mutating call.
        let fitted_n_ctx = should_fit(on_cpu, cfg.n_gpu_layers)
            .then(|| fit_to_device(path, params.as_mut(), cfg.n_ctx))
            .flatten();
        let n_gpu_layers = params.n_gpu_layers();

        let model = LlamaModel::load_from_file(&backend, path, &params).map_err(|source| {
            LocalEngineError::Load {
                path: path.to_path_buf(),
                source,
            }
        })?;

        tracing::info!(
            path = %path.display(),
            layers = model.n_layer(),
            n_embd = model.n_embd(),
            n_ctx_train = model.n_ctx_train(),
            compute_backend = %selected.backend,
            device = selected.device.as_deref().unwrap_or("cpu"),
            n_gpu_layers,
            fitted_n_ctx,
            "local model loaded"
        );

        Ok(Self {
            model,
            backend,
            path: path.to_path_buf(),
            cfg: cfg.clone(),
            selected_backend: selected,
            n_gpu_layers,
            fitted_n_ctx,
        })
    }

    /// The backend this model was actually loaded onto — for the Settings
    /// model card's "Backend now" row and VRAM figure. Recorded at load time
    /// rather than re-queried, so the card reports what the resident model is
    /// really using, not what a fresh selection would pick now.
    pub fn selected_backend(&self) -> &super::backend::SelectedBackend {
        &self.selected_backend
    }

    /// Layers actually offloaded to the selected backend. `0` on CPU;
    /// otherwise either the pinned `n_gpu_layers` or whatever fitting chose.
    pub fn n_gpu_layers(&self) -> i32 {
        self.n_gpu_layers
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn model(&self) -> &LlamaModel {
        &self.model
    }

    pub fn backend(&self) -> &LlamaBackend {
        &self.backend
    }

    /// Native embedding width. Callers persisting vectors should record this
    /// alongside them — adaptive-pathway keys its re-embed migration on the
    /// model identity, and a silent width change is the failure it exists to
    /// prevent.
    pub fn n_embd(&self) -> i32 {
        self.model.n_embd()
    }

    fn base_params(&self, n_ctx: u32) -> LlamaContextParams {
        let mut p = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(n_ctx))
            .with_n_batch(self.cfg.n_batch)
            .with_type_k(parse_kv_cache_type(&self.cfg.cache_type_k))
            .with_type_v(parse_kv_cache_type(&self.cfg.cache_type_v));
        if let Some(t) = self.resolve_n_threads() {
            p = p.with_n_threads(t).with_n_threads_batch(t);
        }
        p
    }

    /// Threads for compute contexts. An explicit `n_threads` (> 0) is honored
    /// verbatim. Auto (`0`) leaves llama.cpp to pick — all cores — which is fine
    /// on a desktop but on Android pegs every core and thermally throttles the
    /// SoC, which is *slower* than using fewer plus it cooks the phone
    /// (ADDENDUM 3). So auto is capped on Android; elsewhere it stays `None` and
    /// llama.cpp keeps its own default.
    fn resolve_n_threads(&self) -> Option<i32> {
        if self.cfg.n_threads > 0 {
            return Some(self.cfg.n_threads);
        }
        #[cfg(target_os = "android")]
        {
            let avail = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            Some(avail.min(4) as i32)
        }
        #[cfg(not(target_os = "android"))]
        {
            None
        }
    }

    /// The model geometry that determines KV-cache cost per token.
    fn kv_geometry(&self) -> KvGeometry {
        KvGeometry {
            n_layer: self.model.n_layer(),
            n_embd: self.model.n_embd().max(0) as u32,
            n_head: self.model.n_head(),
            n_head_kv: self.model.n_head_kv(),
        }
    }

    /// Resolve `n_ctx` for a generation context.
    ///
    /// `cfg.n_ctx == 0` means "automatic": prefer what `fit_params` measured,
    /// else size the KV cache against this device's actual free memory and
    /// this model's actual geometry ([`estimate_n_ctx`]), else
    /// [`AUTO_N_CTX_FALLBACK`]. Always clamped to what the model was trained
    /// on — asking for more silently degrades quality on some architectures
    /// and wastes KV cache on all of them.
    pub fn effective_n_ctx(&self) -> u32 {
        let estimated = (self.cfg.n_ctx == 0)
            .then(|| {
                estimate_n_ctx(
                    self.kv_geometry(),
                    parse_kv_cache_type(&self.cfg.cache_type_k),
                    parse_kv_cache_type(&self.cfg.cache_type_v),
                    self.selected_backend.usable_memory,
                    self.model.size(),
                )
            })
            .flatten();
        resolve_n_ctx(
            self.cfg.n_ctx,
            self.fitted_n_ctx,
            estimated,
            self.model.n_ctx_train(),
        )
    }

    /// Context for text generation.
    pub fn generation_context(
        &self,
    ) -> Result<llama_cpp_2::context::LlamaContext<'_>, LocalEngineError> {
        let n_ctx = self.effective_n_ctx();
        self.model
            .new_context(&self.backend, self.base_params(n_ctx))
            .map_err(|e| LocalEngineError::Context(e.to_string()))
    }

    /// Context for embeddings. Separate from [`Self::generation_context`]
    /// because `with_embeddings` and the pooling type are *construction*
    /// flags — the same context cannot do both jobs.
    pub fn embedding_context(
        &self,
        pooling: EmbedPooling,
    ) -> Result<llama_cpp_2::context::LlamaContext<'_>, LocalEngineError> {
        let n_ctx = self.cfg.embed_n_ctx.min(self.model.n_ctx_train());
        let params = self
            .base_params(n_ctx)
            .with_embeddings(true)
            .with_pooling_type(pooling.to_llama());
        self.model
            .new_context(&self.backend, params)
            .map_err(|e| LocalEngineError::Context(e.to_string()))
    }
}

impl std::fmt::Debug for LocalEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalEngine")
            .field("path", &self.path)
            .field("n_embd", &self.model.n_embd())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pooling_parses_known_values_case_insensitively() {
        assert_eq!(EmbedPooling::parse("last"), EmbedPooling::Last);
        assert_eq!(EmbedPooling::parse("MEAN"), EmbedPooling::Mean);
        assert_eq!(EmbedPooling::parse(" Cls "), EmbedPooling::Cls);
    }

    /// An unknown pooling string must not be fatal, and must land on the
    /// pinned default model's correct mode rather than llama.cpp's `None`
    /// (which would yield no embedding at all).
    #[test]
    fn unknown_pooling_falls_back_to_last() {
        assert_eq!(EmbedPooling::parse("bogus"), EmbedPooling::Last);
        assert_eq!(EmbedPooling::parse(""), EmbedPooling::Last);
    }

    #[test]
    fn loading_a_missing_model_reports_the_path_not_a_backend_error() {
        let cfg = LocalEngineConfig::default();
        let err = LocalEngine::load(Path::new("does-not-exist.gguf"), &cfg).unwrap_err();
        assert!(
            matches!(err, LocalEngineError::ModelNotFound(_)),
            "expected ModelNotFound, got {err:?}"
        );
    }

    /// Regression: `cache_type_k`/`_v` used to be accepted config fields that
    /// `base_params` never applied — the setting had no effect at all. This
    /// pins the parse side; `base_params` applying it is covered by the fact
    /// that a wrong parse here would make every context construction request
    /// the wrong type.
    #[test]
    fn cache_type_parses_the_documented_values_case_insensitively() {
        assert_eq!(parse_kv_cache_type("f16"), KvCacheType::F16);
        assert_eq!(parse_kv_cache_type("Q8_0"), KvCacheType::Q8_0);
        assert_eq!(parse_kv_cache_type(" q4_0 "), KvCacheType::Q4_0);
        assert_eq!(parse_kv_cache_type("Q4_1"), KvCacheType::Q4_1);
        assert_eq!(parse_kv_cache_type("q5_0"), KvCacheType::Q5_0);
        assert_eq!(parse_kv_cache_type("q5_1"), KvCacheType::Q5_1);
    }

    /// An unknown/empty cache type must not be fatal — same reasoning as
    /// pooling above — and must land on `F16`, the type every backend
    /// supports unconditionally.
    #[test]
    fn unknown_cache_type_falls_back_to_f16() {
        assert_eq!(parse_kv_cache_type("bogus"), KvCacheType::F16);
        assert_eq!(parse_kv_cache_type(""), KvCacheType::F16);
    }

    /// `LocalEngineConfig::default()`'s `cache_type_k`/`_v` ("f16") must
    /// round-trip through the parser to `F16` — the one combination every
    /// existing install and every test GGUF actually exercises.
    #[test]
    fn the_default_config_parses_to_f16_for_both_slots() {
        let cfg = LocalEngineConfig::default();
        assert_eq!(parse_kv_cache_type(&cfg.cache_type_k), KvCacheType::F16);
        assert_eq!(parse_kv_cache_type(&cfg.cache_type_v), KvCacheType::F16);
    }

    /// `fit_params` only writes params still holding their default, so calling
    /// it after `with_n_gpu_layers` would make it a silent no-op for the one
    /// field it exists to decide. These cases are the contract between
    /// `should_fit` and the branch in `load` that builds the params.
    #[test]
    fn fitting_runs_only_for_an_unpinned_layer_count_on_a_gpu() {
        assert!(should_fit(false, -1), "GPU + automatic is the fitting case");
        assert!(
            !should_fit(false, 12),
            "a pinned layer count is the user overriding the solver"
        );
        assert!(
            !should_fit(false, 0),
            "0 is a pin (CPU-only offload), not 'unset' — only negative means auto"
        );
        assert!(!should_fit(true, -1), "nothing to offload to on CPU");
        assert!(!should_fit(true, 12));
    }

    /// `LocalEngineConfig`'s default must actually reach the fitting branch —
    /// if the default ever changed to a non-negative value, automatic offload
    /// would quietly stop happening for everyone who never touched the
    /// setting, and no other test would notice.
    #[test]
    fn the_default_config_asks_for_automatic_offload() {
        assert!(should_fit(false, LocalEngineConfig::default().n_gpu_layers));
    }

    #[test]
    fn a_configured_context_size_is_used_as_is() {
        assert_eq!(resolve_n_ctx(8192, None, None, 128_000), 8192);
        // ...and neither a fitted nor an estimated value overrides an explicit
        // choice. Fitting leaves a non-zero `n_ctx` alone, and estimating is
        // only ever consulted for the automatic sentinel.
        assert_eq!(resolve_n_ctx(8192, Some(4096), Some(2048), 128_000), 8192);
    }

    /// 815bugs #83: the value advertised at provider registration (before the
    /// lazy first load) must never be `None` (budgets against the 64k
    /// daemon-wide default) nor the literal `0` "automatic" sentinel — a
    /// pinned size advertises itself, and automatic advertises the same
    /// fallback the engine bottoms out at.
    #[test]
    fn registration_n_ctx_never_advertises_zero_or_nothing() {
        let pinned = LocalEngineConfig {
            n_ctx: 8192,
            ..Default::default()
        };
        assert_eq!(registration_n_ctx(&pinned), 8192);

        let automatic = LocalEngineConfig {
            n_ctx: 0,
            ..Default::default()
        };
        assert_eq!(registration_n_ctx(&automatic), AUTO_N_CTX_FALLBACK);
        assert_ne!(registration_n_ctx(&automatic), 0);

        // The crate default (4096, pinned) advertises exactly what the
        // engine will use — the case the bug report was about.
        assert_eq!(registration_n_ctx(&LocalEngineConfig::default()), 4096);
    }

    /// `n_ctx = 0` is the "automatic" sentinel, resolved from three sources in
    /// descending authority. It must *never* reach llama.cpp as a literal 0,
    /// which it reads as "use the full `n_ctx_train`" (128k on LFM2.5) and
    /// would allocate a KV cache far past anything the machine can hold.
    #[test]
    fn automatic_prefers_measured_then_estimated_then_the_fallback() {
        assert_eq!(resolve_n_ctx(0, Some(16_384), Some(8192), 128_000), 16_384);
        assert_eq!(resolve_n_ctx(0, None, Some(8192), 128_000), 8192);
        assert_eq!(resolve_n_ctx(0, None, None, 128_000), AUTO_N_CTX_FALLBACK);
        assert_ne!(resolve_n_ctx(0, None, None, 128_000), 0);
    }

    /// Regression, measured on a GTX 1650 Ti with all 17 layers fitting:
    /// `fit_params` hands back the `0` it was given when it sees no reason to
    /// shrink the context. That is "no opinion", not a size — treating it as
    /// one produced `n_ctx = 0`, which llama.cpp reads as "use the full
    /// `n_ctx_train`" and would allocate a 128k KV cache on a 4 GB card. It
    /// must fall through to the estimate, not short-circuit it.
    #[test]
    fn a_fitted_zero_means_no_opinion_not_a_context_of_zero() {
        assert_eq!(resolve_n_ctx(0, Some(0), None, 128_000), AUTO_N_CTX_FALLBACK);
        assert_eq!(resolve_n_ctx(0, Some(0), Some(8192), 128_000), 8192);
    }

    /// The `n_ctx_train` clamp applies to every path, fitted and estimated
    /// alike. Both size against device memory and know nothing about what the
    /// architecture was trained on, so a large-VRAM card could otherwise be
    /// handed a context the model cannot use.
    #[test]
    fn every_path_is_clamped_to_what_the_model_was_trained_on() {
        assert_eq!(resolve_n_ctx(32_768, None, None, 4096), 4096);
        assert_eq!(resolve_n_ctx(0, Some(32_768), None, 4096), 4096);
        assert_eq!(resolve_n_ctx(0, None, Some(32_768), 4096), 4096);
        assert_eq!(resolve_n_ctx(0, None, None, 2048), 2048);
    }

    /// LFM2.5-1.2B as actually measured in Phase 1: 16 layers, `n_embd` 2048,
    /// 730 MB of weights, 32 attention heads over 8 KV heads (GQA 4x).
    const LFM2: KvGeometry = KvGeometry {
        n_layer: 16,
        n_embd: 2048,
        n_head: 32,
        n_head_kv: 8,
    };
    const LFM2_BYTES: u64 = 730_895_168;
    const GIB: u64 = 1024 * 1024 * 1024;

    /// Worked by hand: head_dim 64 x 8 KV heads x 16 layers = 8,192 elements
    /// per token, x (64 + 64) bytes per 32-element f16 block / 32 = 32,768
    /// bytes/token.
    #[test]
    fn per_token_kv_cost_matches_the_hand_calculation() {
        let b = kv_bytes_per_token(LFM2, KvCacheType::F16, KvCacheType::F16).unwrap();
        assert_eq!(b, 32_768);
    }

    /// GQA is the whole reason a long context is affordable. Charging
    /// `n_head` instead of `n_head_kv` would over-count by the GQA ratio and
    /// quarter the estimate on this model.
    #[test]
    fn grouped_query_attention_lowers_the_per_token_cost() {
        let mha = KvGeometry {
            n_head_kv: LFM2.n_head,
            ..LFM2
        };
        let gqa = kv_bytes_per_token(LFM2, KvCacheType::F16, KvCacheType::F16).unwrap();
        let full = kv_bytes_per_token(mha, KvCacheType::F16, KvCacheType::F16).unwrap();
        assert_eq!(full / gqa, 4, "LFM2.5 is 32/8, so 4x cheaper than MHA");
    }

    /// Quantising the cache buys context roughly in proportion to the bytes
    /// saved — the reason the setting exists at all.
    #[test]
    fn a_quantised_kv_cache_buys_more_context_than_f16() {
        let f16 = estimate_n_ctx(LFM2, KvCacheType::F16, KvCacheType::F16, 4 * GIB, LFM2_BYTES);
        let q8 = estimate_n_ctx(LFM2, KvCacheType::Q8_0, KvCacheType::Q8_0, 4 * GIB, LFM2_BYTES);
        assert!(q8 > f16, "q8_0 ({q8:?}) should beat f16 ({f16:?})");
    }

    /// The GTX 1650 Ti this was validated on: a 4 GB card holding LFM2.5.
    /// Half of 4 GiB is 2 GiB; less 730 MB of weights leaves ~1.4 GiB; at
    /// 32 KiB/token that is ~44k tokens. The point is that it comfortably
    /// beats the flat 4096 this replaced.
    #[test]
    fn a_four_gigabyte_card_gets_a_real_context_not_a_flat_default() {
        let n = estimate_n_ctx(LFM2, KvCacheType::F16, KvCacheType::F16, 4 * GIB, LFM2_BYTES)
            .expect("a 4 GB card should support some context");
        assert!((40_000..48_000).contains(&n), "expected ~44k tokens, got {n}");
        assert_eq!(n % 256, 0, "should land on a 256-token boundary");
        assert!(
            n > AUTO_N_CTX_FALLBACK,
            "the point of estimating is to beat the flat default on real hardware"
        );
    }

    /// More memory must mean more context — the property that makes this worth
    /// doing rather than hardcoding a constant.
    #[test]
    fn the_estimate_scales_with_available_memory() {
        let at = |gb: u64| {
            estimate_n_ctx(LFM2, KvCacheType::F16, KvCacheType::F16, gb * GIB, LFM2_BYTES)
        };
        assert!(at(24) > at(8));
        assert!(at(8) > at(4));
    }

    /// No estimate rather than a bad one. Each of these is a real state — a
    /// registry reporting no memory, a device too small for the weights, a
    /// budget with no room left for a usable context, broken geometry — and in
    /// every one the caller's fallback is the honest answer.
    #[test]
    fn there_is_no_estimate_without_a_basis_for_one() {
        let est = |mem, bytes, g| estimate_n_ctx(g, KvCacheType::F16, KvCacheType::F16, mem, bytes);
        assert_eq!(est(0, LFM2_BYTES, LFM2), None, "no memory figure at all");
        assert_eq!(est(GIB, LFM2_BYTES, LFM2), None, "weights exceed the budget");
        // Room for the weights with ~49 MB to spare — about 1,500 tokens,
        // under the floor.
        assert_eq!(
            est(1_560_000_000, LFM2_BYTES, LFM2),
            None,
            "room for the weights, but not for a context worth having"
        );
        // Just above it, the estimate appears rather than the floor being a
        // cliff that swallows workable configurations.
        assert!(est(1_700_000_000, LFM2_BYTES, LFM2).is_some());
        let broken = KvGeometry { n_head: 0, ..LFM2 };
        assert_eq!(est(24 * GIB, LFM2_BYTES, broken), None, "no divide by zero");
    }

    /// Regression, measured: LFM2.5 reports `n_head_kv = 0` because it is a
    /// hybrid stack and llama.cpp's model-level query has no single correct
    /// answer. That is unknown geometry, not an absent KV cache — treating it
    /// as zero made `estimate_n_ctx` return `None` on the exact model this
    /// feature ships for, silently reverting every automatic context to the
    /// flat default on real hardware.
    #[test]
    fn an_unknown_kv_head_count_assumes_full_attention_rather_than_none() {
        let hybrid = KvGeometry {
            n_head_kv: 0,
            ..LFM2
        };
        let mha = KvGeometry {
            n_head_kv: LFM2.n_head,
            ..LFM2
        };
        let f16 = (KvCacheType::F16, KvCacheType::F16);
        assert_eq!(
            kv_bytes_per_token(hybrid, f16.0, f16.1),
            kv_bytes_per_token(mha, f16.0, f16.1),
            "unknown must cost the same as the safe worst case"
        );
        let n = estimate_n_ctx(hybrid, f16.0, f16.1, 4 * GIB, LFM2_BYTES)
            .expect("unknown geometry must still yield an estimate");
        assert!(
            n > AUTO_N_CTX_FALLBACK,
            "even the conservative assumption should beat the flat default, got {n}"
        );
    }

    /// Every estimate must clear the same floor `fit_params` is given, so the
    /// two automatic paths cannot disagree about what "too small to bother"
    /// means.
    #[test]
    fn an_estimate_never_lands_below_the_fit_floor() {
        for gb in 1..=32u64 {
            if let Some(n) =
                estimate_n_ctx(LFM2, KvCacheType::F16, KvCacheType::F16, gb * GIB, LFM2_BYTES)
            {
                assert!(n >= FIT_N_CTX_MIN, "{gb} GB produced {n}");
            }
        }
    }
}
