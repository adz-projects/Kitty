//! The optional decision model, for two web checks: recognising a scraped
//! page that is not content (a bot challenge, a paywall, an error page) and
//! ordering merged search results by relevance.
//!
//! The host passes the endpoint through this server's configuration
//! (`KITTY_DECIDER_*`, see `config.rs`). Everything here **fails open**: no
//! endpoint, a check switched off, a slow or unreachable server, or an answer
//! in a shape this module doesn't recognise all mean "do what you did before".
//!
//! The endpoint is configured by the user and usually lives on their own
//! machine or LAN, so — unlike every URL a *tool call* names — it does not go
//! through `ssrf::check_url`.
//!
//! Dialects are matched by name (`systemone` for Liquid's d1 family today), so
//! another decision model is one more arm. Duplicated in spirit from the
//! engine's `decider` module rather than shared, for the reason `paths.rs`
//! gives.

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::config::InProcessConfig;

/// Page checks and re-ranking run on the tool's own path; a model waiting on
/// a page should never wait long on a judgement about it.
const BUDGET: Duration = Duration::from_millis(1500);
/// How much of a page the page check reads.
const PAGE_STATE_CHARS: usize = 2_000;
/// Default for `page_classify_min` when the host sends no override.
const DEFAULT_PAGE_MIN: f32 = 0.70;

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(3))
            .build()
            .unwrap_or_default()
    })
}

/// What the page check can conclude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    Content,
    Challenge,
    PaywallOrLogin,
    ErrorPage,
    EmptyShell,
}

const PAGE_OPTIONS: [(&str, &str, PageKind); 5] = [
    ("content", "Real content: an article, documentation, data or a listing", PageKind::Content),
    (
        "bot_challenge",
        "A bot check or CAPTCHA (\"verify you are human\", \"checking your browser\")",
        PageKind::Challenge,
    ),
    (
        "paywall_or_login",
        "A paywall, subscription prompt or login wall instead of the content",
        PageKind::PaywallOrLogin,
    ),
    ("error_page", "An error page: not found, access denied, server error", PageKind::ErrorPage),
    (
        "empty_shell",
        "An empty shell that needs JavaScript to show anything",
        PageKind::EmptyShell,
    ),
];

/// The decision model as this server sees it.
#[derive(Debug, Clone)]
pub struct Decider {
    url: String,
    dialect: String,
    features: Vec<String>,
    page_min: f32,
}

impl Decider {
    /// From the configuration in effect, or `None` when there is no endpoint.
    pub fn from_config(cfg: &InProcessConfig) -> Option<Self> {
        if cfg.decider_url.trim().is_empty() {
            return None;
        }
        let thresholds: Map<String, Value> =
            serde_json::from_str(&cfg.decider_thresholds).unwrap_or_default();
        let page_min = thresholds
            .get("page_classify_min")
            .and_then(Value::as_f64)
            .map(|v| v as f32)
            .unwrap_or(DEFAULT_PAGE_MIN);
        Some(Self {
            url: cfg
                .decider_url
                .trim()
                .trim_end_matches('/')
                .trim_end_matches("/v1/systemone")
                .to_string(),
            dialect: if cfg.decider_dialect.is_empty() {
                "systemone".into()
            } else {
                cfg.decider_dialect.clone()
            },
            features: cfg.decider_features.clone(),
            page_min,
        })
    }

    /// The decision model for the current call, if this check is on.
    pub fn current(feature: &str) -> Option<Self> {
        Self::from_config(&crate::config::current()).filter(|d| d.is_on(feature))
    }

    pub fn is_on(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }

    /// Ask raw questions (Decision Index shape) about `state`. `None` on any
    /// failure.
    async fn ask(&self, state: &str, questions: Map<String, Value>) -> Option<Map<String, Value>> {
        match self.dialect.as_str() {
            "systemone" => {
                let body = json!({ "state": state, "questions": questions });
                let send = client()
                    .post(format!("{}/v1/systemone", self.url))
                    .json(&body)
                    .send();
                let resp = tokio::time::timeout(BUDGET, send).await.ok()?.ok()?;
                if !resp.status().is_success() {
                    return None;
                }
                let v: Value = tokio::time::timeout(BUDGET, resp.json()).await.ok()?.ok()?;
                v.get("answers").and_then(Value::as_object).cloned()
            }
            _ => None,
        }
    }

    /// Classify a page from its title and opening text. `Some` only when a
    /// label clears the configured confidence; `None` = no opinion.
    pub async fn classify_page(&self, title: &str, text: &str) -> Option<(PageKind, f32)> {
        let opening: String = text.chars().take(PAGE_STATE_CHARS).collect();
        let state = format!("Title: {title}\n\n{opening}");
        let criteria: Map<String, Value> = PAGE_OPTIONS
            .iter()
            .map(|(k, d, _)| (k.to_string(), Value::String(d.to_string())))
            .collect();
        let mut qs = Map::new();
        qs.insert(
            "page".into(),
            json!({
                "type": "choice",
                "instructions": "What is this fetched web page?",
                "criteria": criteria,
            }),
        );
        let answers = self.ask(&state, qs).await?;
        let (label, p) = parse_choice(answers.get("page")?)?;
        let kind = PAGE_OPTIONS.iter().find(|(k, _, _)| *k == label)?.2;
        (p >= self.page_min).then_some((kind, p))
    }

    /// P(relevant) for each item about `query`, by position.
    pub async fn relevance(&self, query: &str, items: &[String]) -> Option<Vec<Option<f32>>> {
        if items.is_empty() {
            return None;
        }
        let mut qs = Map::new();
        for (i, item) in items.iter().enumerate() {
            qs.insert(
                format!("r{i}"),
                json!({
                    "type": "noul",
                    "instructions": format!(
                        "Does this search result answer or directly help with the query? {item}"
                    ),
                }),
            );
        }
        let answers = self.ask(&format!("Query: {query}"), qs).await?;
        Some(
            (0..items.len())
                .map(|i| answers.get(&format!("r{i}")).and_then(parse_yes))
                .collect(),
        )
    }
}

fn as_prob(v: &Value) -> Option<f32> {
    v.as_f64()
        .filter(|p| p.is_finite())
        .map(|p| p.clamp(0.0, 1.0) as f32)
}

/// A yes/no answer: bare P(yes) or `{"noul": p}`.
fn parse_yes(v: &Value) -> Option<f32> {
    as_prob(v).or_else(|| v.get("noul").and_then(as_prob))
}

/// A choice answer: `{"choice", "confidence"}`, or the argmax of
/// `probabilities`.
fn parse_choice(v: &Value) -> Option<(String, f32)> {
    let probs = v.get("probabilities").and_then(Value::as_object);
    if let Some(choice) = v.get("choice").and_then(Value::as_str) {
        // P(choice) first: the server's `confidence` is a separate
        // calibration score (see the engine's `decider::systemone`).
        let p = probs
            .and_then(|m| m.get(choice))
            .and_then(as_prob)
            .or_else(|| v.get("confidence").and_then(as_prob))?;
        return Some((choice.to_string(), p));
    }
    probs?
        .iter()
        .filter_map(|(k, p)| as_prob(p).map(|p| (k.clone(), p)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

/// Stable sort of `indices` by descending score; unscored items keep their
/// place relative to each other and sort as if scored 0.5, so a partial answer
/// never pushes an unjudged result to the bottom.
pub fn order_by(scores: &[Option<f32>]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..scores.len()).collect();
    idx.sort_by(|&a, &b| {
        let sa = scores[a].unwrap_or(0.5);
        let sb = scores[b].unwrap_or(0.5);
        sb.total_cmp(&sa)
    });
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str, features: &[&str], thresholds: &str) -> InProcessConfig {
        InProcessConfig {
            decider_url: url.into(),
            decider_dialect: String::new(),
            decider_features: features.iter().map(|s| s.to_string()).collect(),
            decider_thresholds: thresholds.into(),
            ..InProcessConfig::default()
        }
    }

    #[test]
    fn no_url_means_no_decider_and_features_are_individual() {
        assert!(Decider::from_config(&cfg("", &["page_classify"], "")).is_none());
        let d = Decider::from_config(&cfg("http://box:8080/", &["search_rerank"], "")).unwrap();
        assert!(d.is_on("search_rerank"));
        assert!(!d.is_on("page_classify"));
        assert_eq!(d.url, "http://box:8080");
        assert_eq!(d.page_min, DEFAULT_PAGE_MIN);
        let d = Decider::from_config(&cfg("http://b", &[], r#"{"page_classify_min":0.9}"#)).unwrap();
        assert!((d.page_min - 0.9).abs() < 1e-6);
    }

    #[test]
    fn choice_answers_parse_with_or_without_a_named_choice() {
        assert_eq!(
            parse_choice(&json!({"choice": "bot_challenge", "confidence": 0.93})),
            Some(("bot_challenge".into(), 0.93))
        );
        assert_eq!(
            parse_choice(&json!({"probabilities": {"content": 0.2, "paywall_or_login": 0.8}})),
            Some(("paywall_or_login".into(), 0.8))
        );
        assert_eq!(parse_choice(&json!({})), None);
        // A real d1 answer: the option's probability, not `confidence`.
        assert_eq!(
            parse_choice(&json!({"type": "choice", "choice": "content",
                                 "probabilities": {"content": 0.75, "bot_challenge": 0.25},
                                 "confidence": 0.4})),
            Some(("content".into(), 0.75))
        );
    }

    #[test]
    fn ordering_is_stable_and_keeps_unjudged_items_in_the_middle() {
        assert_eq!(order_by(&[Some(0.1), Some(0.9), None, Some(0.9)]), vec![1, 3, 2, 0]);
        assert_eq!(order_by(&[None, None]), vec![0, 1]);
    }

    /// Against a live server: `D1_URL=http://host:port cargo test -- --ignored live_d1`.
    #[tokio::test]
    #[ignore]
    async fn live_d1() {
        let url = std::env::var("D1_URL").expect("set D1_URL");
        let d = Decider::from_config(&cfg(&url, &["page_classify", "search_rerank"], "")).unwrap();
        let page = d
            .classify_page(
                "Just a moment...",
                "Checking if the site connection is secure. Verify you are human.",
            )
            .await;
        println!("page: {page:?}");
        assert_eq!(page.map(|p| p.0), Some(PageKind::Challenge));
        let scores = d
            .relevance(
                "rust trait object safety",
                &["How to remove rust from tools".into(), "Object safety - The Rust Reference".into()],
            )
            .await
            .unwrap();
        println!("relevance: {scores:?}");
        assert_eq!(order_by(&scores), vec![1, 0]);
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_has_no_opinion() {
        let d = Decider::from_config(&cfg("http://127.0.0.1:1", &["page_classify"], "")).unwrap();
        assert_eq!(d.classify_page("t", "x").await, None);
        assert_eq!(d.relevance("q", &["a".into()]).await, None);
    }
}
