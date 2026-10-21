//! The seam between "a question the engine wants answered" and one model's
//! wire format. Callers never see this; [`super::DeciderHub`] picks the
//! backend from the app's `dialect`.

use std::sync::Arc;

use async_trait::async_trait;

use super::{Answers, Question};

#[derive(Debug, thiserror::Error)]
pub enum DeciderError {
    #[error("decision model unreachable: {0}")]
    Transport(String),
    #[error("decision model returned HTTP {0}")]
    Status(u16),
    #[error("decision model answered in an unexpected shape: {0}")]
    Shape(String),
}

/// What [`DeciderBackend::probe`] learned about an endpoint.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProbeResult {
    pub ok: bool,
    pub model: Option<String>,
    pub rtt_ms: u64,
    pub error: Option<String>,
}

#[async_trait]
pub trait DeciderBackend: Send + Sync {
    /// Answer every question about `state` in one call. Question names are
    /// the keys of `questions` and come back as the keys of the answers; a
    /// backend may omit an answer it could not produce.
    async fn ask(
        &self,
        state: &str,
        questions: &[(String, Question)],
    ) -> Result<Answers, DeciderError>;

    /// A trivial question plus whatever the endpoint says about its model.
    async fn probe(&self) -> ProbeResult;
}

/// Build the backend for a dialect, or `None` for one this build can't speak
/// (rejected at configuration time, so this only guards a stale row).
pub fn for_dialect(dialect: &str, url: &str) -> Option<Arc<dyn DeciderBackend>> {
    match dialect {
        super::systemone::DIALECT => Some(Arc::new(super::systemone::SystemOne::new(url))),
        _ => None,
    }
}
