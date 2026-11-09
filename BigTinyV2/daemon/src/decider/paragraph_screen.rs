//! The decision model as memorabilia's paragraph screen
//! (`docs/MEMORY_OVERNIGHT.md` §2), and the tools for testing other question
//! sets against labelled paragraphs.
//!
//! The questions are **data**: an app sends its question set with its decider
//! configuration (`paragraph_screen`), so a better set is a settings change,
//! not a rebuild. Every verdict is logged with the set's `id`.
//!
//! Unlike every other use of the decision model, this one does not fail open
//! when the model is unreachable: memorabilia holds the document and asks
//! again later ([`ScreenVerdict::Unavailable`]). With the feature off, or no
//! decision model at all, the screen reports itself disabled and the
//! pre-filter alone decides.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use memorabilia::traits::{ParagraphScreen, ScreenVerdict};
use serde::{Deserialize, Serialize};

use super::{Answers, DeciderHub, Feature, Question};

/// One paragraph call. Intake runs after the turn, in the background.
const SCREEN_BUDGET: Duration = Duration::from_secs(4);

/// One yes/no question of a set, with the P(yes) a paragraph needs to pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenQuestion {
    pub name: String,
    pub instructions: String,
    pub keep_at: f32,
    /// The intake lane this question belongs to (`current`, `durable`), or
    /// empty for a question every lane shares. A paragraph is kept when the
    /// shared questions and every question of at least one lane pass; the
    /// first such lane (set order) is the lane that kept it. A set with no
    /// lanes keeps a paragraph only when every question passes, as before.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lane: String,
}

/// A question set. A paragraph is kept only when it clears every question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParagraphScreenConfig {
    /// Names the set in the screen log, so results can be compared per set.
    pub id: String,
    pub questions: Vec<ScreenQuestion>,
    /// Paragraphs per model call. `1` (the default) is one call per paragraph,
    /// the calibrated behavior. A larger value combines that many passages into
    /// a single call (cheaper), but its accuracy has to be measured with the
    /// eval before it is trusted (docs/MEMORY_OVERNIGHT.md, *Testing question
    /// sets*), so it is opt-in via config.json and off by default. Edited there,
    /// never in Settings, like the rest of this set.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
}

fn default_batch_size() -> usize {
    1
}

impl Default for ParagraphScreenConfig {
    /// The starting set. Its thresholds are a guess until calibrated against
    /// labelled paragraphs (`docs/MEMORY_OVERNIGHT.md`, *Testing question
    /// sets*).
    fn default() -> Self {
        // Two lanes (Phase 4.1). `durable-substantive-v1` kept only lasting
        // material and rejected exactly what brings a model up to date
        // (releases, decisions, status changes); that is now its own lane.
        Self {
            id: "two-lane-v1".into(),
            questions: vec![
                ScreenQuestion {
                    name: "current".into(),
                    instructions: "Does this passage report something recent that a well-read model trained a \
                        year ago would probably not know -- a release, decision, change of status, new \
                        figure, event or announcement -- stated concretely?"
                        .into(),
                    keep_at: 0.60,
                    lane: "current".into(),
                },
                ScreenQuestion {
                    name: "durable".into(),
                    instructions: "Will this passage still be informative in a few years, after the specific \
                        products, prices, people or events it mentions are no longer current? Answer \
                        yes for explanations of how or why things work, lasting history, definitions, \
                        durable relationships and reasoned arguments or interpretations; answer no for \
                        specs, prices, schedules and status updates."
                        .into(),
                    keep_at: 0.60,
                    lane: "durable".into(),
                },
                ScreenQuestion {
                    name: "substantive".into(),
                    instructions: "Is this passage substantive and well supported (concrete claims, mechanisms, \
                        evidence or reasoned argument) rather than marketing copy, filler, unsupported \
                        speculation or vague generalities?"
                        .into(),
                    keep_at: 0.60,
                    lane: String::new(),
                },
            ],
            batch_size: default_batch_size(),
        }
    }
}

impl ParagraphScreenConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("paragraph_screen.id must not be empty".into());
        }
        if self.questions.is_empty() {
            return Err("paragraph_screen needs at least one question".into());
        }
        let mut seen = std::collections::HashSet::new();
        for q in &self.questions {
            if q.name.trim().is_empty() || !seen.insert(q.name.as_str()) {
                return Err(format!(
                    "paragraph_screen question names must be unique and non-empty ('{}')",
                    q.name
                ));
            }
            if q.instructions.trim().is_empty() {
                return Err(format!("paragraph_screen question '{}' has no instructions", q.name));
            }
            if !(0.0..=1.0).contains(&q.keep_at) {
                return Err(format!("paragraph_screen question '{}': keep_at must be 0..=1", q.name));
            }
        }
        if self.batch_size == 0 {
            return Err("paragraph_screen.batch_size must be at least 1".into());
        }
        Ok(())
    }

    /// The questions, in the shape the backend asks them.
    pub fn questions(&self) -> Vec<(String, Question)> {
        self.questions
            .iter()
            .map(|q| (q.name.clone(), Question::yes_no(q.instructions.clone())))
            .collect()
    }

    /// Every question's P(yes), in set order; `None` when any is missing (an
    /// answer the backend could not produce is no verdict at all).
    pub fn scores(&self, answers: &Answers) -> Option<Vec<(String, f32)>> {
        self.questions
            .iter()
            .map(|q| Some((q.name.clone(), answers.get(&q.name)?.yes?)))
            .collect()
    }

    /// Whether `scores` (in set order) keep the paragraph (see
    /// [`ScreenQuestion::lane`]).
    pub fn keeps(&self, scores: &[f32]) -> bool {
        self.keep_lane(scores).0
    }

    /// `(kept, lane that kept it)` for `scores` in set order. The lane is
    /// `None` for a set with no lanes.
    pub fn keep_lane(&self, scores: &[f32]) -> (bool, Option<String>) {
        if scores.len() != self.questions.len() {
            return (false, None);
        }
        let passes = |lane: &str| {
            self.questions
                .iter()
                .zip(scores)
                .filter(|(q, _)| q.lane == lane)
                .all(|(q, p)| *p >= q.keep_at)
        };
        let mut lanes: Vec<&str> = Vec::new();
        for q in &self.questions {
            if !q.lane.is_empty() && !lanes.contains(&q.lane.as_str()) {
                lanes.push(&q.lane);
            }
        }
        if !passes("") {
            return (false, None);
        }
        if lanes.is_empty() {
            return (true, None);
        }
        match lanes.into_iter().find(|l| passes(l)) {
            Some(l) => (true, Some(l.to_string())),
            None => (false, None),
        }
    }

    /// Build the combined state and the per-passage questions for a batched
    /// call over `states`: each passage is labelled, and each question is
    /// scoped to one passage by a `p{idx}:` name prefix (recovered by
    /// [`Self::batched_scores`]) and a "considering only passage N" preamble.
    pub fn batched_prompt(&self, states: &[&str]) -> (String, Vec<(String, Question)>) {
        let mut combined = String::new();
        for (p, s) in states.iter().enumerate() {
            combined.push_str(&format!("=== PASSAGE {} ===\n{}\n\n", p + 1, s));
        }
        let mut questions = Vec::with_capacity(self.questions.len() * states.len());
        for p in 0..states.len() {
            for q in &self.questions {
                questions.push((
                    format!("p{p}:{}", q.name),
                    Question::yes_no(format!(
                        "Considering ONLY passage {} above: {}",
                        p + 1,
                        q.instructions
                    )),
                ));
            }
        }
        (combined, questions)
    }

    /// Passage `p`'s scores (in set order) from a batched answer set; `None`
    /// when any is missing — that passage then has no verdict.
    pub fn batched_scores(&self, answers: &Answers, p: usize) -> Option<Vec<(String, f32)>> {
        self.questions
            .iter()
            .map(|q| Some((q.name.clone(), answers.get(&format!("p{p}:{}", q.name))?.yes?)))
            .collect()
    }
}

/// The screen memorabilia's intake calls, for one app.
pub struct MemorabiliaParagraphScreen {
    hub: Arc<DeciderHub>,
    app_id: String,
}

impl MemorabiliaParagraphScreen {
    pub fn new(hub: Arc<DeciderHub>, app_id: &str) -> Self {
        Self {
            hub,
            app_id: app_id.to_string(),
        }
    }
}

#[async_trait]
impl ParagraphScreen for MemorabiliaParagraphScreen {
    async fn enabled(&self) -> bool {
        self.hub.enabled(&self.app_id, Feature::IngestGate).await.is_some()
    }

    async fn screen(&self, state: &str) -> ScreenVerdict {
        let Some(decider) = self.hub.enabled(&self.app_id, Feature::IngestGate).await else {
            return ScreenVerdict::Disabled;
        };
        let set = &decider.config.paragraph_screen;
        let Some(answers) = self
            .hub
            .ask(&self.app_id, Feature::IngestGate, state, &set.questions(), SCREEN_BUDGET)
            .await
        else {
            return ScreenVerdict::Unavailable;
        };
        let Some(scores) = set.scores(&answers) else {
            return ScreenVerdict::Unavailable;
        };
        let ps: Vec<f32> = scores.iter().map(|(_, p)| *p).collect();
        let (keep, lane) = set.keep_lane(&ps);
        if !keep {
            self.hub.note_acted(&self.app_id, Feature::IngestGate);
        }
        ScreenVerdict::Scored { keep, scores, lane }
    }

    async fn question_set(&self) -> String {
        match self.hub.enabled(&self.app_id, Feature::IngestGate).await {
            Some(d) => d.config.paragraph_screen.id.clone(),
            None => String::new(),
        }
    }

    async fn batch_size(&self) -> usize {
        match self.hub.enabled(&self.app_id, Feature::IngestGate).await {
            Some(d) => d.config.paragraph_screen.batch_size.max(1),
            None => 1,
        }
    }

    /// Screen several passages in one model call when the set configures a
    /// batch; otherwise fall back to one call each. Verdicts are returned in
    /// `states` order. A passage whose answers are missing is `Unavailable`
    /// (no verdict) rather than silently kept or dropped.
    async fn screen_batch(&self, states: &[&str]) -> Vec<ScreenVerdict> {
        let Some(decider) = self.hub.enabled(&self.app_id, Feature::IngestGate).await else {
            return vec![ScreenVerdict::Disabled; states.len()];
        };
        let set = &decider.config.paragraph_screen;
        // One-at-a-time unless a real batch is configured and there is more
        // than one passage to combine.
        if set.batch_size <= 1 || states.len() <= 1 {
            let mut out = Vec::with_capacity(states.len());
            for s in states {
                out.push(self.screen(s).await);
            }
            return out;
        }

        let (combined, questions) = set.batched_prompt(states);
        let Some(answers) = self
            .hub
            .ask(&self.app_id, Feature::IngestGate, &combined, &questions, SCREEN_BUDGET)
            .await
        else {
            return vec![ScreenVerdict::Unavailable; states.len()];
        };
        (0..states.len())
            .map(|p| match set.batched_scores(&answers, p) {
                Some(scores) => {
                    let ps: Vec<f32> = scores.iter().map(|(_, v)| *v).collect();
                    let (keep, lane) = set.keep_lane(&ps);
                    if !keep {
                        self.hub.note_acted(&self.app_id, Feature::IngestGate);
                    }
                    ScreenVerdict::Scored { keep, scores, lane }
                }
                None => ScreenVerdict::Unavailable,
            })
            .collect()
    }
}

// ── Testing question sets ───────────────────────────────────────────────

/// One labelled paragraph: a screen-log export line with `keep` filled in.
#[derive(Debug, Clone, Deserialize)]
pub struct Labelled {
    pub text: String,
    #[serde(default)]
    pub heading: Option<String>,
    #[serde(default)]
    pub source_type: String,
    #[serde(default)]
    pub source_name: String,
    /// The label; unlabelled lines are skipped.
    #[serde(default)]
    pub keep: Option<bool>,
}

impl Labelled {
    /// The exact text production screens for this paragraph.
    pub fn state(&self) -> String {
        memorabilia::learn::capture::screen_state(
            &self.source_type,
            &self.source_name,
            &memorabilia::learn::paragraphs::Paragraph {
                heading: self.heading.clone(),
                text: self.text.clone(),
            },
        )
    }
}

/// How a rule did against the labels. Precision is what "too selective
/// rather than too inclusive" protects, so the summary score is F0.5.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Metrics {
    pub kept: usize,
    pub total: usize,
    pub true_pos: usize,
    pub false_pos: usize,
    pub false_neg: usize,
}

impl Metrics {
    pub fn keep_rate(&self) -> f64 {
        ratio(self.kept, self.total)
    }
    pub fn precision(&self) -> f64 {
        ratio(self.true_pos, self.true_pos + self.false_pos)
    }
    pub fn recall(&self) -> f64 {
        ratio(self.true_pos, self.true_pos + self.false_neg)
    }
    /// F-beta with beta = 0.5: precision weighted twice recall.
    pub fn f05(&self) -> f64 {
        let (p, r) = (self.precision(), self.recall());
        if p + r == 0.0 {
            0.0
        } else {
            1.25 * p * r / (0.25 * p + r)
        }
    }
}

fn ratio(a: usize, b: usize) -> f64 {
    if b == 0 {
        0.0
    } else {
        a as f64 / b as f64
    }
}

/// Score keep/drop decisions at `thresholds` (one per question, set order)
/// against `labels`. `scores[i]` is paragraph i's P(yes) per question.
pub fn metrics(scores: &[Vec<f32>], labels: &[bool], thresholds: &[f32]) -> Metrics {
    let mut m = Metrics::default();
    for (s, &label) in scores.iter().zip(labels) {
        let keep = s.len() == thresholds.len() && s.iter().zip(thresholds).all(|(p, t)| p >= t);
        m.total += 1;
        if keep {
            m.kept += 1;
        }
        match (keep, label) {
            (true, true) => m.true_pos += 1,
            (true, false) => m.false_pos += 1,
            (false, true) => m.false_neg += 1,
            (false, false) => {}
        }
    }
    m
}

/// The thresholds (0.05 to 0.95 in steps of 0.05, per question) with the best
/// F0.5. Exhaustive, so only for sets of up to three questions; `None` for
/// larger sets or no data.
pub fn best_thresholds(scores: &[Vec<f32>], labels: &[bool], questions: usize) -> Option<(Vec<f32>, Metrics)> {
    if questions == 0 || questions > 3 || scores.is_empty() {
        return None;
    }
    let grid: Vec<f32> = (1..20).map(|i| i as f32 * 0.05).collect();
    let mut best: Option<(Vec<f32>, Metrics)> = None;
    let mut idx = vec![0usize; questions];
    loop {
        let t: Vec<f32> = idx.iter().map(|&i| grid[i]).collect();
        let m = metrics(scores, labels, &t);
        // Ties go to the higher keep rate: as precise, but learns more.
        let better = match &best {
            None => true,
            Some((_, b)) => m.f05() > b.f05() + 1e-12 || ((m.f05() - b.f05()).abs() <= 1e-12 && m.kept > b.kept),
        };
        if better {
            best = Some((t, m));
        }
        // Next grid point.
        let mut k = 0;
        loop {
            if k == questions {
                return best;
            }
            idx[k] += 1;
            if idx[k] < grid.len() {
                break;
            }
            idx[k] = 0;
            k += 1;
        }
    }
}

/// Mean P(yes) for the paragraphs labelled keep and drop, per question: how
/// well a question separates the two on its own.
pub fn separation(scores: &[Vec<f32>], labels: &[bool], questions: usize) -> Vec<(f64, f64)> {
    (0..questions)
        .map(|q| {
            let (mut k, mut nk, mut d, mut nd) = (0f64, 0usize, 0f64, 0usize);
            for (s, &l) in scores.iter().zip(labels) {
                let Some(p) = s.get(q) else { continue };
                if l {
                    k += *p as f64;
                    nk += 1;
                } else {
                    d += *p as f64;
                    nd += 1;
                }
            }
            (k / nk.max(1) as f64, d / nd.max(1) as f64)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decider::hub::testing::Scripted;
    use crate::decider::Answer;

    #[test]
    fn batched_prompt_and_scores_route_answers_by_passage() {
        let set = ParagraphScreenConfig::default(); // current | durable, + substantive
        let states = ["first passage", "second passage"];
        let (combined, questions) = set.batched_prompt(&states);
        assert!(combined.contains("=== PASSAGE 1 ===") && combined.contains("=== PASSAGE 2 ==="));
        assert_eq!(questions.len(), set.questions.len() * states.len());

        // Passage 0 clears everything; passage 1 fails the shared
        // `substantive` question, which no lane can make up for.
        let mut answers = Answers::new();
        for (name, _) in &questions {
            let yes = if name.starts_with("p0:") {
                0.9
            } else if name.ends_with(":substantive") {
                0.1
            } else {
                0.9
            };
            answers.insert(name.clone(), Answer { yes: Some(yes), ..Answer::default() });
        }
        let p0: Vec<f32> = set.batched_scores(&answers, 0).unwrap().iter().map(|(_, v)| *v).collect();
        let p1: Vec<f32> = set.batched_scores(&answers, 1).unwrap().iter().map(|(_, v)| *v).collect();
        assert!(set.keeps(&p0), "passage 0 clears every threshold");
        assert!(!set.keeps(&p1), "passage 1 fails the shared question");

        // A missing answer for a passage means no verdict for it.
        answers.remove("p0:substantive");
        assert!(set.batched_scores(&answers, 0).is_none());
    }

    async fn hub(on: bool, p: f32, set: Option<serde_json::Value>) -> Arc<DeciderHub> {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let hub = Arc::new(DeciderHub::new(pool));
        let mut config = serde_json::json!({
            "url": "http://127.0.0.1:1", "features": {"ingest_gate": on},
        });
        if let Some(set) = set {
            config["paragraph_screen"] = set;
        }
        hub.install_for_test(
            "kitty",
            serde_json::from_value(config).unwrap(),
            // `p` for the lane questions, a pass for the shared one.
            Arc::new(Scripted::yes(move |name, _| if name == "durable" || name == "current" { p } else { 0.9 })),
        );
        hub
    }

    #[tokio::test]
    async fn a_paragraph_must_clear_every_question() {
        let s = MemorabiliaParagraphScreen::new(hub(true, 0.3, None).await, "kitty");
        assert!(s.enabled().await);
        match s.screen("The 4090 has 24 GB.").await {
            ScreenVerdict::Scored { keep, scores, .. } => {
                assert!(!keep);
                assert_eq!(scores.len(), 3);
            }
            v => panic!("{v:?}"),
        }
        let s = MemorabiliaParagraphScreen::new(hub(true, 0.8, None).await, "kitty");
        assert!(matches!(s.screen("x").await, ScreenVerdict::Scored { keep: true, .. }));
        assert_eq!(s.question_set().await, "two-lane-v1");
    }

    #[tokio::test]
    async fn switched_off_it_is_disabled_not_unavailable() {
        let s = MemorabiliaParagraphScreen::new(hub(false, 0.8, None).await, "kitty");
        assert!(!s.enabled().await);
        assert_eq!(s.screen("x").await, ScreenVerdict::Disabled);
    }

    #[tokio::test]
    async fn an_app_can_send_its_own_question_set() {
        let set = serde_json::json!({
            "id": "only-durable",
            "questions": [{"name": "durable", "instructions": "Lasting?", "keep_at": 0.5}],
        });
        let s = MemorabiliaParagraphScreen::new(hub(true, 0.55, Some(set)).await, "kitty");
        assert!(matches!(s.screen("x").await, ScreenVerdict::Scored { keep: true, .. }));
        assert_eq!(s.question_set().await, "only-durable");
    }

    #[test]
    fn a_paragraph_is_kept_by_the_first_lane_it_clears() {
        let set = ParagraphScreenConfig::default(); // current, durable, substantive (shared)
        // News: current yes, durable no, substantive yes -> kept as current.
        assert_eq!(set.keep_lane(&[0.9, 0.2, 0.8]), (true, Some("current".into())));
        // An explanation: current no, durable yes -> kept as durable.
        assert_eq!(set.keep_lane(&[0.1, 0.9, 0.8]), (true, Some("durable".into())));
        // Neither lane -> dropped.
        assert_eq!(set.keep_lane(&[0.1, 0.2, 0.9]), (false, None));
        // A lane passes but the shared question fails -> dropped.
        assert_eq!(set.keep_lane(&[0.9, 0.9, 0.1]), (false, None));
        // A set with no lanes behaves as before: every question must pass.
        let flat = ParagraphScreenConfig {
            id: "flat".into(),
            questions: vec![
                ScreenQuestion { name: "a".into(), instructions: "a?".into(), keep_at: 0.5, lane: String::new() },
                ScreenQuestion { name: "b".into(), instructions: "b?".into(), keep_at: 0.5, lane: String::new() },
            ],
            batch_size: 1,
        };
        assert_eq!(flat.keep_lane(&[0.9, 0.9]), (true, None));
        assert_eq!(flat.keep_lane(&[0.9, 0.1]), (false, None));
    }

    #[test]
    fn validation_rejects_bad_sets() {
        assert!(ParagraphScreenConfig::default().validate().is_ok());
        let mut s = ParagraphScreenConfig::default();
        s.questions[1].name = s.questions[0].name.clone();
        assert!(s.validate().is_err());
        let mut s = ParagraphScreenConfig::default();
        s.questions[0].keep_at = 1.5;
        assert!(s.validate().is_err());
        let s = ParagraphScreenConfig { id: "x".into(), questions: vec![], batch_size: 1 };
        assert!(s.validate().is_err());
        let s = ParagraphScreenConfig { batch_size: 0, ..Default::default() };
        assert!(s.validate().is_err(), "batch_size must be >= 1");
    }

    #[test]
    fn metrics_and_the_threshold_search() {
        // Question 0 separates perfectly at 0.5; question 1 is noise.
        let scores = vec![
            vec![0.9, 0.2],
            vec![0.8, 0.9],
            vec![0.7, 0.5],
            vec![0.3, 0.9],
            vec![0.1, 0.4],
        ];
        let labels = vec![true, true, true, false, false];
        let m = metrics(&scores, &labels, &[0.5, 0.0]);
        assert_eq!((m.true_pos, m.false_pos, m.false_neg), (3, 0, 0));
        assert!((m.f05() - 1.0).abs() < 1e-9);
        let (t, best) = best_thresholds(&scores, &labels, 2).unwrap();
        assert!((best.f05() - 1.0).abs() < 1e-9);
        assert!(t[0] > 0.3 && t[0] <= 0.7);
        assert!(t[1] <= 0.2, "the noisy question must not cut anything: {t:?}");
        let sep = separation(&scores, &labels, 2);
        assert!(sep[0].0 > sep[0].1);
    }

    /// Score question sets against labelled paragraphs on a live decision
    /// model. See `docs/MEMORY_OVERNIGHT.md`, *Testing question sets*:
    ///
    /// `D1_URL=... SCREEN_EVAL_LABELS=labels.jsonl [SCREEN_EVAL_SETS=sets.json]
    ///  cargo test --lib -- --ignored live_d1_screen_eval --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_d1_screen_eval() {
        use super::super::backend::DeciderBackend;
        let url = std::env::var("D1_URL").expect("set D1_URL");
        let labels_path = std::env::var("SCREEN_EVAL_LABELS").expect("set SCREEN_EVAL_LABELS");
        let items: Vec<Labelled> = std::fs::read_to_string(&labels_path)
            .expect("read SCREEN_EVAL_LABELS")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Labelled>(l).expect("a JSON object per line"))
            .filter(|l| l.keep.is_some())
            .collect();
        assert!(!items.is_empty(), "no labelled lines (set \"keep\": true/false)");
        let sets: Vec<ParagraphScreenConfig> = match std::env::var("SCREEN_EVAL_SETS") {
            Ok(p) => serde_json::from_str(&std::fs::read_to_string(p).expect("read SCREEN_EVAL_SETS"))
                .expect("a JSON array of question sets"),
            Err(_) => vec![ParagraphScreenConfig::default()],
        };
        let labels: Vec<bool> = items.iter().map(|i| i.keep.unwrap_or(false)).collect();
        let positives = labels.iter().filter(|l| **l).count();
        println!(
            "{} labelled paragraphs, {} keep / {} drop\n",
            items.len(),
            positives,
            items.len() - positives
        );
        let backend = super::super::systemone::SystemOne::new(&url);
        for set in &sets {
            set.validate().expect("valid question set");
            let qs = set.questions();
            let mut scores: Vec<Vec<f32>> = Vec::new();
            let mut kept_labels: Vec<bool> = Vec::new();
            let started = std::time::Instant::now();
            for (item, &label) in items.iter().zip(&labels) {
                match backend.ask(&item.state(), &qs).await {
                    Ok(a) => match set.scores(&a) {
                        Some(s) => {
                            scores.push(s.into_iter().map(|(_, p)| p).collect());
                            kept_labels.push(label);
                        }
                        None => println!("  (no answer for one paragraph; skipped)"),
                    },
                    Err(e) => println!("  (call failed: {e}; skipped)"),
                }
            }
            let per_call = started.elapsed().as_millis() as f64 / scores.len().max(1) as f64;
            let own: Vec<f32> = set.questions.iter().map(|q| q.keep_at).collect();
            let m = metrics(&scores, &kept_labels, &own);
            println!("set {}  ({:.0} ms per paragraph)", set.id, per_call);
            for (q, (k, d)) in set.questions.iter().zip(separation(&scores, &kept_labels, qs.len())) {
                println!("  {:<14} mean P(yes): keep {:.2}  drop {:.2}", q.name, k, d);
            }
            let line = |label: &str, t: &[f32], m: &Metrics| {
                println!(
                    "  {label:<10} thresholds {:?}  keep rate {:.0}%  precision {:.2}  recall {:.2}  F0.5 {:.2}",
                    t.iter().map(|x| (x * 100.0).round() / 100.0).collect::<Vec<_>>(),
                    m.keep_rate() * 100.0,
                    m.precision(),
                    m.recall(),
                    m.f05()
                );
            };
            line("as set", &own, &m);
            if let Some((t, b)) = best_thresholds(&scores, &kept_labels, qs.len()) {
                line("best", &t, &b);
            }
            println!();
        }
    }
}
