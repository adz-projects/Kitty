//! The decision model as the memory engines' optional judge.
//!
//! One gate object per app engine, attached when the host opens it. Each
//! question checks its own feature switch at call time (through
//! [`DeciderHub::ask`]), so turning a feature off or changing a threshold takes
//! effect on the next call with no engine re-open.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::{DeciderHub, Feature, Question};

/// A learn pass runs in the background, after the turn: there is time.
const LEARN_BUDGET: Duration = Duration::from_secs(2);
/// Recall runs on the turn path, inside the engine's 1500 ms recall budget.
const RECALL_BUDGET: Duration = Duration::from_millis(600);
/// Ingestion runs at turn end, in the background.
const INGEST_BUDGET: Duration = Duration::from_secs(2);

/// How much of a learn chunk is shown. The newest part, where the chunk ends:
/// that is where the exchanges not yet learned from are.
const LEARN_STATE_CHARS: usize = 6_000;
/// How much of a document is shown: its opening says what kind of thing it is.
const INGEST_STATE_CHARS: usize = 4_000;

pub struct PathwayGate {
    hub: Arc<DeciderHub>,
    app_id: String,
}

impl PathwayGate {
    pub fn new(hub: Arc<DeciderHub>, app_id: &str) -> Self {
        Self {
            hub,
            app_id: app_id.to_string(),
        }
    }
}

#[async_trait]
impl adaptive_pathway::traits::MemoryGate for PathwayGate {
    async fn skip_learning(&self, chunk: &str) -> Option<f32> {
        let decider = self.hub.enabled(&self.app_id, Feature::LearnGate).await?;
        let state = tail_chars(chunk, LEARN_STATE_CHARS);
        let q = vec![(
            "durable".to_string(),
            Question::yes_no(
                "Does this conversation reveal something lasting about the user: their \
                 preferences, goals, background, habits, how they like to work, or a correction \
                 to something assumed about them?",
            ),
        )];
        let a = self
            .hub
            .ask(&self.app_id, Feature::LearnGate, state, &q, LEARN_BUDGET)
            .await?;
        let p = a.get("durable")?.yes?;
        if p < decider.config.thresholds.learn_gate_skip_below {
            self.hub.note_acted(&self.app_id, Feature::LearnGate);
            Some(p)
        } else {
            None
        }
    }

    async fn belief_relevance(
        &self,
        message: &str,
        beliefs: &[&str],
    ) -> Option<(Vec<Option<f32>>, f32)> {
        let decider = self.hub.enabled(&self.app_id, Feature::RecallRerank).await?;
        let questions: Vec<(String, Question)> = beliefs
            .iter()
            .enumerate()
            .map(|(i, b)| {
                (
                    format!("b{i}"),
                    Question::yes_no(format!(
                        "Would knowing this about the user change how to answer their message? \
                         \"{b}\""
                    )),
                )
            })
            .collect();
        let a = self
            .hub
            .ask(&self.app_id, Feature::RecallRerank, message, &questions, RECALL_BUDGET)
            .await?;
        let drop_below = decider.config.thresholds.recall_drop_below;
        let ps: Vec<Option<f32>> = (0..beliefs.len())
            .map(|i| a.get(&format!("b{i}")).and_then(|x| x.yes))
            .collect();
        if ps.iter().flatten().any(|p| *p < drop_below) {
            self.hub.note_acted(&self.app_id, Feature::RecallRerank);
        }
        Some((ps, drop_below))
    }
}

pub struct MemorabiliaGate {
    hub: Arc<DeciderHub>,
    app_id: String,
}

impl MemorabiliaGate {
    pub fn new(hub: Arc<DeciderHub>, app_id: &str) -> Self {
        Self {
            hub,
            app_id: app_id.to_string(),
        }
    }
}

#[async_trait]
impl memorabilia::traits::WorthinessGate for MemorabiliaGate {
    async fn reject(&self, text: &str) -> Option<f32> {
        let decider = self.hub.enabled(&self.app_id, Feature::IngestGate).await?;
        let state: String = text.chars().take(INGEST_STATE_CHARS).collect();
        let q = vec![(
            "worth".to_string(),
            // Names essays and arguments on purpose: asked only about
            // "reference material", d1 rejected the user's own op-ed drafts
            // and a published opinion piece (3 of 12 rejections over 119 real
            // documents); this wording kept all three and caught more junk.
            Question::yes_no(
                "Is this substantive content worth remembering (an article, essay, argument, \
                 documentation, data or explanation) rather than boilerplate, site navigation, a \
                 search-results list, an error or bot-check page, or a page with almost no text?",
            ),
        )];
        let a = self
            .hub
            .ask(&self.app_id, Feature::IngestGate, &state, &q, INGEST_BUDGET)
            .await?;
        let p = a.get("worth")?.yes?;
        if p < decider.config.thresholds.ingest_gate_reject_below {
            self.hub.note_acted(&self.app_id, Feature::IngestGate);
            Some(p)
        } else {
            None
        }
    }
}

/// The last `max` characters of `s`, on a char boundary.
fn tail_chars(s: &str, max: usize) -> &str {
    let count = s.chars().count();
    if count <= max {
        return s;
    }
    match s.char_indices().nth(count - max) {
        Some((i, _)) => &s[i..],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decider::hub::testing::Scripted;
    use adaptive_pathway::traits::MemoryGate;
    use memorabilia::traits::WorthinessGate;

    async fn hub(on: &[&str], p: f32) -> Arc<DeciderHub> {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let hub = Arc::new(DeciderHub::new(pool));
        let features: serde_json::Map<String, serde_json::Value> = on
            .iter()
            .map(|f| (f.to_string(), serde_json::Value::Bool(true)))
            .collect();
        let config = serde_json::from_value(serde_json::json!({
            "url": "http://127.0.0.1:1", "features": features,
        }))
        .unwrap();
        hub.install_for_test(
            "kitty",
            config,
            Arc::new(Scripted::yes(move |name, _| if name == "b1" { 0.01 } else { p })),
        );
        hub
    }

    #[tokio::test]
    async fn the_learn_gate_skips_only_below_its_threshold() {
        let low = PathwayGate::new(hub(&["learn_gate"], 0.05).await, "kitty");
        assert_eq!(low.skip_learning("user: thanks").await, Some(0.05));
        let high = PathwayGate::new(hub(&["learn_gate"], 0.6).await, "kitty");
        assert_eq!(high.skip_learning("user: I am vegetarian").await, None);
        let off = PathwayGate::new(hub(&[], 0.05).await, "kitty");
        assert_eq!(off.skip_learning("user: thanks").await, None);
    }

    #[tokio::test]
    async fn relevance_comes_back_by_position() {
        let g = PathwayGate::new(hub(&["recall_rerank"], 0.7).await, "kitty");
        let (ps, drop_below) = g.belief_relevance("help me", &["a", "b", "c"]).await.unwrap();
        assert_eq!(ps, vec![Some(0.7), Some(0.01), Some(0.7)]);
        assert!((drop_below - 0.08).abs() < 1e-6);
    }

    #[tokio::test]
    async fn the_ingest_gate_rejects_low_value_documents() {
        let g = MemorabiliaGate::new(hub(&["ingest_gate"], 0.02).await, "kitty");
        assert_eq!(g.reject("Checking your browser…").await, Some(0.02));
        let keep = MemorabiliaGate::new(hub(&["ingest_gate"], 0.9).await, "kitty");
        assert_eq!(keep.reject("Spec: 25 Nm").await, None);
    }

    #[test]
    fn tails_respect_char_boundaries() {
        assert_eq!(tail_chars("héllo", 3), "llo");
        assert_eq!(tail_chars("hi", 5), "hi");
    }
}
