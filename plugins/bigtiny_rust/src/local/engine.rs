//! `LocalEngine` — one loaded GGUF plus the knobs needed to make contexts
//! from it (docs/ANDROID.md §3.1).
//!
//! Scope is deliberately narrow. This owns the model and knows how to build a
//! correctly-configured context; it does **not** own residency policy (that's
//! [`super::manager`]) or wire formats (that's `provider`/`embeddings`).
//! Keeping the llama.cpp surface behind this one type is what makes D1's
//! "the binding stays swappable" claim true rather than aspirational.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::LlamaModel;

use crate::config::LocalEngineConfig;

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

/// A loaded model, ready to mint contexts.
pub struct LocalEngine {
    model: LlamaModel,
    backend: Arc<LlamaBackend>,
    path: PathBuf,
    cfg: LocalEngineConfig,
}

impl LocalEngine {
    /// Load `path` under `cfg`. Blocking and slow (hundreds of MB off disk) —
    /// callers on an async runtime must `spawn_blocking` this.
    pub fn load(path: &Path, cfg: &LocalEngineConfig) -> Result<Self, LocalEngineError> {
        if !path.is_file() {
            return Err(LocalEngineError::ModelNotFound(path.to_path_buf()));
        }
        let backend = shared_backend()?;

        let mut params = LlamaModelParams::default();
        // `n_gpu_layers < 0` means "all layers" to llama.cpp; pass it through
        // rather than trying to compute a layer count we don't have yet (the
        // model isn't loaded). D20's richer backend selection layers on top.
        if cfg.n_gpu_layers >= 0 {
            params = params.with_n_gpu_layers(cfg.n_gpu_layers as u32);
        }

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
            "local model loaded"
        );

        Ok(Self {
            model,
            backend,
            path: path.to_path_buf(),
            cfg: cfg.clone(),
        })
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
            .with_n_batch(self.cfg.n_batch);
        if self.cfg.n_threads > 0 {
            p = p
                .with_n_threads(self.cfg.n_threads)
                .with_n_threads_batch(self.cfg.n_threads);
        }
        p
    }

    /// Context for text generation.
    pub fn generation_context(
        &self,
    ) -> Result<llama_cpp_2::context::LlamaContext<'_>, LocalEngineError> {
        // Clamp to what the model was actually trained on: asking for more
        // silently degrades quality on some architectures and wastes KV cache
        // on all of them.
        let n_ctx = self.cfg.n_ctx.min(self.model.n_ctx_train());
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
}
