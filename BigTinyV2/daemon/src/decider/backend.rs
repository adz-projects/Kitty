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
/// `api_key` is an optional bearer token and `model` an optional model id for
/// a hosted endpoint; both are ignored by the self-hosted dialect's transport
/// when empty.
pub fn for_dialect(
    dialect: &str,
    url: &str,
    api_key: Option<&str>,
    model: Option<&str>,
) -> Option<Arc<dyn DeciderBackend>> {
    let key = api_key.map(str::to_string);
    let model = model.map(str::to_string);
    match dialect {
        // Self-hosted: append `/v1/systemone` to the server root.
        super::systemone::DIALECT => Some(Arc::new(super::systemone::SystemOne::with_auth(
            url, key, model, true,
        ))),
        // Hosted gateway / Cloudflare: POST the URL as-is, with the bearer token.
        super::systemone::DIALECT_URL => Some(Arc::new(super::systemone::SystemOne::with_auth(
            url, key, model, false,
        ))),
        _ => None,
    }
}

/// A primary backend with a fallback: `ask` tries the primary and, only if it
/// returns an error (unreachable, HTTP error, or an unparseable answer), asks
/// the secondary instead. A low-confidence *answer* is a success, not a
/// failure, so it never triggers the fallback — the caller's own thresholds
/// decide what to do with it. `probe` reports the primary (the headline model);
/// the secondary endpoint is probed on its own through the config UI.
pub struct FallbackBackend {
    primary: Arc<dyn DeciderBackend>,
    secondary: Arc<dyn DeciderBackend>,
}

impl FallbackBackend {
    pub fn new(primary: Arc<dyn DeciderBackend>, secondary: Arc<dyn DeciderBackend>) -> Self {
        Self { primary, secondary }
    }
}

#[async_trait]
impl DeciderBackend for FallbackBackend {
    async fn ask(
        &self,
        state: &str,
        questions: &[(String, Question)],
    ) -> Result<Answers, DeciderError> {
        match self.primary.ask(state, questions).await {
            Ok(a) => Ok(a),
            Err(e) => {
                tracing::warn!("decision model: primary failed ({e}); trying secondary");
                self.secondary.ask(state, questions).await
            }
        }
    }

    async fn probe(&self) -> ProbeResult {
        self.primary.probe().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::decider::Answer;

    /// A backend that records how many times it was asked and returns a fixed
    /// outcome: `Ok` labelled with `tag` (so we can tell which one answered),
    /// or a transport error.
    struct Stub {
        tag: &'static str,
        fail: bool,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl DeciderBackend for Stub {
        async fn ask(
            &self,
            _state: &str,
            questions: &[(String, Question)],
        ) -> Result<Answers, DeciderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(DeciderError::Transport(format!("{} down", self.tag)));
            }
            Ok(questions
                .iter()
                .map(|(name, _)| {
                    (
                        name.clone(),
                        Answer {
                            // Encode which backend answered into the score so the
                            // test can assert on provenance, not just success.
                            yes: Some(if self.tag == "primary" { 0.11 } else { 0.22 }),
                            ..Answer::default()
                        },
                    )
                })
                .collect())
        }
        async fn probe(&self) -> ProbeResult {
            ProbeResult { ok: !self.fail, model: Some(self.tag.into()), rtt_ms: 0, error: None }
        }
    }

    fn stub(tag: &'static str, fail: bool) -> Arc<Stub> {
        Arc::new(Stub { tag, fail, calls: AtomicUsize::new(0) })
    }

    fn one_q() -> Vec<(String, Question)> {
        vec![("q".into(), Question::YesNo { instructions: "ok?".into() })]
    }

    #[tokio::test]
    async fn primary_success_never_touches_secondary() {
        let p = stub("primary", false);
        let s = stub("secondary", false);
        let fb = FallbackBackend::new(p.clone(), s.clone());
        let a = fb.ask("state", &one_q()).await.unwrap();
        assert_eq!(a.get("q").unwrap().yes, Some(0.11), "primary answered");
        assert_eq!(s.calls.load(Ordering::SeqCst), 0, "secondary not consulted on success");
    }

    #[tokio::test]
    async fn primary_failure_falls_back_to_secondary() {
        let p = stub("primary", true);
        let s = stub("secondary", false);
        let fb = FallbackBackend::new(p.clone(), s.clone());
        let a = fb.ask("state", &one_q()).await.unwrap();
        assert_eq!(p.calls.load(Ordering::SeqCst), 1, "primary was tried first");
        assert_eq!(a.get("q").unwrap().yes, Some(0.22), "secondary answered after primary failed");
    }

    #[tokio::test]
    async fn both_failing_surfaces_the_secondary_error() {
        let fb = FallbackBackend::new(stub("primary", true), stub("secondary", true));
        let err = fb.ask("state", &one_q()).await.unwrap_err();
        assert!(matches!(err, DeciderError::Transport(m) if m.contains("secondary")));
    }

    #[tokio::test]
    async fn probe_reports_the_primary() {
        let fb = FallbackBackend::new(stub("primary", false), stub("secondary", false));
        assert_eq!(fb.probe().await.model.as_deref(), Some("primary"));
    }
}
