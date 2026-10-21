//! Per-app decision models: configuration cache, fail-open asking, counters.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use sqlx::SqlitePool;

use super::backend::{self, DeciderBackend, ProbeResult};
use super::{Answers, DeciderConfig, Feature, Question};
use crate::storage::app_decider;

/// A state longer than this is cut before it is sent. The decision model's
/// latency grows with the state, and every caller already sends a summary
/// (a user message, a title and snippet), so hitting this means a caller
/// forgot to trim — better a truncated question than a blown budget.
const MAX_STATE_CHARS: usize = 16_000;

/// How often an unreachable endpoint is logged, per app.
const WARN_EVERY: Duration = Duration::from_secs(60);

/// One app's decision model, ready to ask.
pub struct AppDecider {
    pub config: DeciderConfig,
    backend: Arc<dyn DeciderBackend>,
}

impl AppDecider {
    pub fn is_on(&self, f: Feature) -> bool {
        self.config.features.is_on(f)
    }
}

/// Process-lifetime counters for one app's use of one feature.
#[derive(Debug, Default)]
pub struct FeatureStats {
    /// Questions sent.
    pub calls: AtomicU64,
    /// Calls that came back with answers in time.
    pub answered: AtomicU64,
    /// Calls that failed or ran out of budget (the caller fell back).
    pub failed: AtomicU64,
    /// Answers that changed what the engine did (a pass skipped, a group
    /// added, a page relabelled …). Reported by the caller.
    pub acted: AtomicU64,
}

pub struct DeciderHub {
    pool: SqlitePool,
    /// `None` = loaded, and the app has no decision model.
    apps: DashMap<String, Option<Arc<AppDecider>>>,
    stats: DashMap<(String, Feature), Arc<FeatureStats>>,
    last_warn: Mutex<HashMap<String, Instant>>,
}

impl DeciderHub {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            apps: DashMap::new(),
            stats: DashMap::new(),
            last_warn: Mutex::new(HashMap::new()),
        }
    }

    /// The app's decision model, loading its configuration on first use.
    pub async fn for_app(&self, app_id: &str) -> Option<Arc<AppDecider>> {
        if let Some(cached) = self.apps.get(app_id) {
            return cached.clone();
        }
        let loaded = match app_decider::get(&self.pool, app_id).await {
            Ok(Some(raw)) => match serde_json::from_str::<DeciderConfig>(&raw) {
                Ok(config) => build(config),
                Err(e) => {
                    tracing::warn!(app = app_id, "stored decision-model config unreadable: {e}");
                    None
                }
            },
            Ok(None) => None,
            // Not cached: a transient read error should be retried next time,
            // not pinned as "no model" for the life of the process.
            Err(e) => {
                tracing::warn!(app = app_id, "could not read decision-model config: {e}");
                return None;
            }
        };
        self.apps.insert(app_id.to_string(), loaded.clone());
        loaded
    }

    /// The app's decision model when it has one **and** `feature` is on.
    pub async fn enabled(&self, app_id: &str, feature: Feature) -> Option<Arc<AppDecider>> {
        self.for_app(app_id).await.filter(|d| d.is_on(feature))
    }

    /// Ask `questions` about `state` for `feature`, within `budget`.
    ///
    /// `None` whenever the caller should behave as if there were no decision
    /// model: none configured, feature off, endpoint down, too slow, or an
    /// answer in a shape the backend didn't recognise. Never an error.
    pub async fn ask(
        &self,
        app_id: &str,
        feature: Feature,
        state: &str,
        questions: &[(String, Question)],
        budget: Duration,
    ) -> Option<Answers> {
        if questions.is_empty() {
            return None;
        }
        let decider = self.enabled(app_id, feature).await?;
        let stats = self.stats(app_id, feature);
        stats.calls.fetch_add(1, Ordering::Relaxed);
        let state = truncate_chars(state, MAX_STATE_CHARS);
        let result = tokio::time::timeout(budget, decider.backend.ask(state, questions)).await;
        match result {
            Ok(Ok(answers)) if !answers.is_empty() => {
                stats.answered.fetch_add(1, Ordering::Relaxed);
                Some(answers)
            }
            Ok(Ok(_)) => {
                stats.failed.fetch_add(1, Ordering::Relaxed);
                self.warn(app_id, feature, "the decision model returned no usable answers");
                None
            }
            Ok(Err(e)) => {
                stats.failed.fetch_add(1, Ordering::Relaxed);
                self.warn(app_id, feature, &e.to_string());
                None
            }
            Err(_) => {
                stats.failed.fetch_add(1, Ordering::Relaxed);
                self.warn(
                    app_id,
                    feature,
                    &format!("no answer within {} ms", budget.as_millis()),
                );
                None
            }
        }
    }

    /// Record that an answer changed what the engine did.
    pub fn note_acted(&self, app_id: &str, feature: Feature) {
        self.stats(app_id, feature)
            .acted
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Replace (or with `None`, remove) an app's configuration.
    pub async fn set(&self, app_id: &str, config: Option<DeciderConfig>) -> Result<(), String> {
        match &config {
            Some(c) => {
                c.validate()?;
                let raw = serde_json::to_string(c).map_err(|e| e.to_string())?;
                app_decider::set(&self.pool, app_id, &raw)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            None => {
                app_decider::clear(&self.pool, app_id)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        self.apps
            .insert(app_id.to_string(), config.and_then(build));
        Ok(())
    }

    /// Forget an app entirely (it was deleted).
    pub fn evict(&self, app_id: &str) {
        self.apps.remove(app_id);
        self.stats.retain(|(a, _), _| a != app_id);
    }

    /// Probe an endpoint without saving anything — for a settings "Test".
    pub async fn probe(dialect: &str, url: &str) -> ProbeResult {
        match backend::for_dialect(dialect, url) {
            Some(b) => b.probe().await,
            None => ProbeResult {
                ok: false,
                model: None,
                rtt_ms: 0,
                error: Some(format!("unknown dialect '{dialect}'")),
            },
        }
    }

    /// `(calls, answered, failed, acted)` per feature for one app.
    pub fn snapshot(&self, app_id: &str) -> Vec<(Feature, [u64; 4])> {
        Feature::ALL
            .iter()
            .map(|&f| {
                let s = self.stats(app_id, f);
                (
                    f,
                    [
                        s.calls.load(Ordering::Relaxed),
                        s.answered.load(Ordering::Relaxed),
                        s.failed.load(Ordering::Relaxed),
                        s.acted.load(Ordering::Relaxed),
                    ],
                )
            })
            .collect()
    }

    fn stats(&self, app_id: &str, feature: Feature) -> Arc<FeatureStats> {
        self.stats
            .entry((app_id.to_string(), feature))
            .or_default()
            .clone()
    }

    fn warn(&self, app_id: &str, feature: Feature, detail: &str) {
        let mut last = self.last_warn.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let due = last
            .get(app_id)
            .is_none_or(|t| now.duration_since(*t) >= WARN_EVERY);
        if due {
            last.insert(app_id.to_string(), now);
            tracing::warn!(
                app = app_id,
                feature = feature.as_str(),
                "decision model skipped, falling back: {detail}"
            );
        }
    }

    /// Test seam: install a backend directly.
    #[cfg(test)]
    pub fn install_for_test(
        &self,
        app_id: &str,
        config: DeciderConfig,
        backend: Arc<dyn DeciderBackend>,
    ) {
        self.apps.insert(
            app_id.to_string(),
            Some(Arc::new(AppDecider { config, backend })),
        );
    }
}

fn build(config: DeciderConfig) -> Option<Arc<AppDecider>> {
    let backend = backend::for_dialect(&config.dialect, &config.url)?;
    Some(Arc::new(AppDecider { config, backend }))
}

fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
pub mod testing {
    //! A scripted backend for callers' tests.

    use async_trait::async_trait;

    use super::super::backend::{DeciderBackend, DeciderError, ProbeResult};
    use super::super::{Answer, Answers, Question};

    /// `(question name, question + state) -> P(yes)`.
    pub type ScoreFn = dyn Fn(&str, &str) -> f32 + Send + Sync;

    /// Answers every yes/no with `p_yes(name)`, every choice with the first
    /// option, every score with `level`. `fail` makes every call error.
    pub struct Scripted {
        pub p_yes: Box<ScoreFn>,
        pub level: usize,
        pub fail: bool,
        pub delay: std::time::Duration,
    }

    impl Scripted {
        pub fn yes(f: impl Fn(&str, &str) -> f32 + Send + Sync + 'static) -> Self {
            Self {
                p_yes: Box::new(f),
                level: 0,
                fail: false,
                delay: std::time::Duration::ZERO,
            }
        }
    }

    #[async_trait]
    impl DeciderBackend for Scripted {
        async fn ask(
            &self,
            state: &str,
            questions: &[(String, Question)],
        ) -> Result<Answers, DeciderError> {
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            if self.fail {
                return Err(DeciderError::Transport("scripted failure".into()));
            }
            Ok(questions
                .iter()
                .map(|(name, q)| {
                    let a = match q {
                        Question::YesNo { instructions } => Answer {
                            yes: Some((self.p_yes)(name, &format!("{instructions}\n{state}"))),
                            ..Answer::default()
                        },
                        Question::Choice { options, .. } => Answer {
                            choice: options.first().map(|o| o.0.clone()),
                            confidence: Some(0.9),
                            ..Answer::default()
                        },
                        Question::Score { levels, .. } => Answer {
                            level: Some(self.level.min(levels.len().saturating_sub(1))),
                            confidence: Some(0.9),
                            ..Answer::default()
                        },
                    };
                    (name.clone(), a)
                })
                .collect())
        }

        async fn probe(&self) -> ProbeResult {
            ProbeResult {
                ok: !self.fail,
                model: Some("scripted".into()),
                rtt_ms: 0,
                error: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Scripted;
    use super::*;

    async fn hub() -> DeciderHub {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        DeciderHub::new(pool)
    }

    fn config(on: &[&str]) -> DeciderConfig {
        let features: serde_json::Map<String, serde_json::Value> = on
            .iter()
            .map(|f| (f.to_string(), serde_json::Value::Bool(true)))
            .collect();
        serde_json::from_value(serde_json::json!({
            "url": "http://127.0.0.1:1", "features": features,
        }))
        .unwrap()
    }

    fn q() -> Vec<(String, Question)> {
        vec![("a".into(), Question::yes_no("?"))]
    }

    #[tokio::test]
    async fn no_config_means_no_answer() {
        let h = hub().await;
        assert!(h
            .ask("kitty", Feature::LearnGate, "x", &q(), Duration::from_secs(1))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn a_feature_that_is_off_is_never_asked() {
        let h = hub().await;
        h.install_for_test("kitty", config(&["tool_gating"]), Arc::new(Scripted::yes(|_, _| 0.9)));
        assert!(h
            .ask("kitty", Feature::LearnGate, "x", &q(), Duration::from_secs(1))
            .await
            .is_none());
        let a = h
            .ask("kitty", Feature::ToolGating, "x", &q(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(a["a"].yes, Some(0.9));
        // Another app sees none of it.
        assert!(h.for_app("notebook").await.is_none());
    }

    #[tokio::test]
    async fn failures_and_timeouts_fall_back_and_are_counted() {
        let h = hub().await;
        let mut slow = Scripted::yes(|_, _| 0.9);
        slow.delay = Duration::from_millis(200);
        h.install_for_test("kitty", config(&["learn_gate"]), Arc::new(slow));
        assert!(h
            .ask("kitty", Feature::LearnGate, "x", &q(), Duration::from_millis(20))
            .await
            .is_none());
        let mut failing = Scripted::yes(|_, _| 0.9);
        failing.fail = true;
        h.install_for_test("kitty", config(&["learn_gate"]), Arc::new(failing));
        assert!(h
            .ask("kitty", Feature::LearnGate, "x", &q(), Duration::from_secs(1))
            .await
            .is_none());
        let snap = h.snapshot("kitty");
        let learn = snap.iter().find(|(f, _)| *f == Feature::LearnGate).unwrap().1;
        assert_eq!(learn, [2, 0, 2, 0]);
    }

    #[tokio::test]
    async fn set_persists_validates_and_clears() {
        let h = hub().await;
        assert!(h.set("kitty", Some(config(&["compaction"]))).await.is_ok());
        h.apps.clear(); // force a reload from the database
        assert!(h.enabled("kitty", Feature::Compaction).await.is_some());
        let mut bad = config(&[]);
        bad.dialect = "nope".into();
        assert!(h.set("kitty", Some(bad)).await.is_err());
        h.set("kitty", None).await.unwrap();
        h.apps.clear();
        assert!(h.for_app("kitty").await.is_none());
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_chars("hi", 10), "hi");
    }
}
