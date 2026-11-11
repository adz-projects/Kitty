//! The seam between "a question the engine wants answered" and one model's
//! wire format. Callers never see this; [`super::DeciderHub`] picks the
//! backend from the app's `dialect`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    #[error("decision model gave no answer within {0} ms")]
    Timeout(u64),
    #[error("decision model skipped: it has been failing")]
    Skipped,
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

    /// [`Self::ask`] bounded by `budget`. A backend that can split the budget
    /// (a primary with a fallback) overrides this; the default simply times
    /// the call out.
    async fn ask_within(
        &self,
        state: &str,
        questions: &[(String, Question)],
        budget: Duration,
    ) -> Result<Answers, DeciderError> {
        match tokio::time::timeout(budget, self.ask(state, questions)).await {
            Ok(r) => r,
            Err(_) => Err(DeciderError::Timeout(budget.as_millis() as u64)),
        }
    }

    /// A trivial question plus whatever the endpoint says about its model.
    async fn probe(&self) -> ProbeResult;
}

/// Consecutive failures after which an endpoint is skipped for a while.
const BREAKER_TRIP_AFTER: u32 = 3;
/// How long a tripped endpoint is skipped before one call is let through to
/// see whether it is back.
const BREAKER_OPEN_FOR: Duration = Duration::from_secs(45);

/// Remembers that an endpoint keeps failing, so callers stop waiting out
/// their whole budget on one that is down (a Tailscale box asleep is normal).
/// After [`BREAKER_TRIP_AFTER`] consecutive failures it opens for
/// [`BREAKER_OPEN_FOR`]; then one call is let through, and a success closes
/// it again.
#[derive(Default)]
pub struct Breaker {
    state: Mutex<(u32, Option<Instant>)>,
}

impl Breaker {
    /// Whether a call may go out now.
    pub fn allows(&self) -> bool {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        match st.1 {
            Some(until) if Instant::now() < until => false,
            Some(_) => {
                // Half-open: let this one through; a failure re-opens it.
                st.1 = None;
                st.0 = BREAKER_TRIP_AFTER - 1;
                true
            }
            None => true,
        }
    }

    pub fn success(&self) {
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = (0, None);
    }

    pub fn failure(&self) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        st.0 = st.0.saturating_add(1);
        if st.0 >= BREAKER_TRIP_AFTER {
            st.1 = Some(Instant::now() + BREAKER_OPEN_FOR);
        }
    }
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
///
/// Within a budget, the primary gets half of it, so an endpoint that is simply
/// offline (a connect that would take seconds to fail) still leaves time to
/// ask the fallback; and a primary that keeps failing is skipped outright for
/// a while (see [`Breaker`]).
pub struct FallbackBackend {
    primary: Arc<dyn DeciderBackend>,
    secondary: Arc<dyn DeciderBackend>,
    primary_breaker: Breaker,
}

impl FallbackBackend {
    pub fn new(primary: Arc<dyn DeciderBackend>, secondary: Arc<dyn DeciderBackend>) -> Self {
        Self {
            primary,
            secondary,
            primary_breaker: Breaker::default(),
        }
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

    async fn ask_within(
        &self,
        state: &str,
        questions: &[(String, Question)],
        budget: Duration,
    ) -> Result<Answers, DeciderError> {
        let started = Instant::now();
        if self.primary_breaker.allows() {
            match self.primary.ask_within(state, questions, budget / 2).await {
                Ok(a) => {
                    self.primary_breaker.success();
                    return Ok(a);
                }
                Err(e) => {
                    self.primary_breaker.failure();
                    tracing::debug!("decision model: primary failed ({e}); trying secondary");
                }
            }
        }
        let left = budget.saturating_sub(started.elapsed());
        self.secondary.ask_within(state, questions, left).await
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

    /// A primary that hangs (offline, connect pending) still leaves time for
    /// the secondary within the caller's budget.
    #[tokio::test]
    async fn a_hanging_primary_leaves_time_for_the_secondary() {
        struct Hang;
        #[async_trait]
        impl DeciderBackend for Hang {
            async fn ask(
                &self,
                _: &str,
                _: &[(String, Question)],
            ) -> Result<Answers, DeciderError> {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Err(DeciderError::Transport("never".into()))
            }
            async fn probe(&self) -> ProbeResult {
                ProbeResult { ok: false, model: None, rtt_ms: 0, error: None }
            }
        }
        let s = stub("secondary", false);
        let fb = FallbackBackend::new(Arc::new(Hang), s.clone());
        let a = fb
            .ask_within("state", &one_q(), Duration::from_millis(200))
            .await
            .unwrap();
        assert_eq!(a.get("q").unwrap().yes, Some(0.22));
    }

    #[tokio::test]
    async fn a_failing_primary_is_skipped_after_a_few_failures() {
        let p = stub("primary", true);
        let s = stub("secondary", false);
        let fb = FallbackBackend::new(p.clone(), s.clone());
        for _ in 0..6 {
            fb.ask_within("state", &one_q(), Duration::from_secs(1)).await.unwrap();
        }
        assert_eq!(p.calls.load(Ordering::SeqCst), 3, "skipped once the breaker opened");
        assert_eq!(s.calls.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn the_breaker_opens_after_repeated_failures_and_closes_on_success() {
        let b = Breaker::default();
        assert!(b.allows());
        b.failure();
        b.failure();
        assert!(b.allows());
        b.failure();
        assert!(!b.allows(), "open after three in a row");
        let b = Breaker::default();
        b.failure();
        b.failure();
        b.success();
        b.failure();
        assert!(b.allows(), "a success resets the count");
    }

    #[tokio::test]
    async fn probe_reports_the_primary() {
        let fb = FallbackBackend::new(stub("primary", false), stub("secondary", false));
        assert_eq!(fb.probe().await.model.as_deref(), Some("primary"));
    }
}
