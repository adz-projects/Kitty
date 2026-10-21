//! Liquid AI's `system_one` dialect, as served by llama-server
//! (`POST /v1/systemone`) for the d1 family.
//!
//! Request: `{"state": "...", "questions": {name: {type, instructions,
//! criteria}}}` where `type` is `noul` (yes/no), `choice` (criteria is a
//! `{name: description}` map) or `score` (criteria is a list of level
//! descriptions, lowest first).
//!
//! Response: `{"answers": {name: answer}, "usage": {...}}`. A `noul` answer is
//! P(yes), either bare or as `{"noul": p}`; `choice` and `score` answers carry
//! `confidence` and `probabilities`. The parser is deliberately lenient about
//! the exact nesting, because the server's shape has to be read off a model
//! card rather than a schema, and a shape it does not recognise only costs an
//! omitted answer — which callers already treat as "no opinion".

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use super::backend::{DeciderBackend, DeciderError, ProbeResult};
use super::{Answer, Answers, Question};

pub const DIALECT: &str = "systemone";

/// A ceiling for one request. Callers impose their own, much shorter budgets
/// with `tokio::time::timeout`; this only stops a wedged socket from living
/// forever once its caller has given up.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default()
    })
}

pub struct SystemOne {
    base: String,
}

impl SystemOne {
    pub fn new(url: &str) -> Self {
        // Accept either the server root or the full route.
        let base = url
            .trim()
            .trim_end_matches('/')
            .trim_end_matches("/v1/systemone")
            .trim_end_matches('/')
            .to_string();
        Self { base }
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/systemone", self.base)
    }

    async fn model_name(&self) -> Option<String> {
        // `/v1/models` names the loaded model; `/props` is llama-server's own
        // and carries the path it was loaded from.
        let models: Value = client()
            .get(format!("{}/v1/models", self.base))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        if let Some(id) = models["data"][0]["id"].as_str() {
            return Some(id.to_string());
        }
        let props: Value = client()
            .get(format!("{}/props", self.base))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        props["model_path"].as_str().map(|p| {
            std::path::Path::new(p)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string())
        })
    }
}

/// The request body for one call.
pub fn request_body(state: &str, questions: &[(String, Question)]) -> Value {
    let mut qs = Map::new();
    for (name, q) in questions {
        let v = match q {
            Question::YesNo { instructions } => json!({
                "type": "noul",
                "instructions": instructions,
            }),
            Question::Choice {
                instructions,
                options,
            } => {
                let criteria: Map<String, Value> = options
                    .iter()
                    .map(|(k, d)| (k.clone(), Value::String(d.clone())))
                    .collect();
                json!({"type": "choice", "instructions": instructions, "criteria": criteria})
            }
            Question::Score {
                instructions,
                levels,
            } => json!({"type": "score", "instructions": instructions, "criteria": levels}),
        };
        qs.insert(name.clone(), v);
    }
    json!({ "state": state, "questions": qs })
}

/// Parse a response body against the questions that were asked.
pub fn parse_answers(body: &Value, questions: &[(String, Question)]) -> Answers {
    let mut out = Answers::new();
    let Some(answers) = body.get("answers").and_then(Value::as_object) else {
        return out;
    };
    for (name, q) in questions {
        let Some(raw) = answers.get(name) else {
            continue;
        };
        if let Some(a) = parse_one(raw, q) {
            out.insert(name.clone(), a);
        }
    }
    out
}

fn as_prob(v: &Value) -> Option<f32> {
    v.as_f64()
        .map(|p| p as f32)
        .filter(|p| p.is_finite())
        .map(|p| p.clamp(0.0, 1.0))
}

fn parse_one(raw: &Value, q: &Question) -> Option<Answer> {
    match q {
        Question::YesNo { .. } => {
            let p = as_prob(raw)
                .or_else(|| raw.get("noul").and_then(as_prob))
                .or_else(|| raw.get("probability").and_then(as_prob))?;
            Some(Answer {
                yes: Some(p),
                ..Answer::default()
            })
        }
        Question::Choice { options, .. } => {
            let probabilities = named_probs(raw.get("probabilities"), options.iter().map(|o| &o.0));
            let choice = raw
                .get("choice")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| argmax(&probabilities).map(|i| probabilities[i].0.clone()))?;
            let confidence = raw
                .get("confidence")
                .and_then(as_prob)
                .or_else(|| probabilities.iter().find(|(k, _)| *k == choice).map(|(_, p)| *p));
            Some(Answer {
                choice: Some(choice),
                confidence,
                probabilities,
                ..Answer::default()
            })
        }
        Question::Score { levels, .. } => {
            let names: Vec<String> = (0..levels.len()).map(|i| i.to_string()).collect();
            let mut probabilities = named_probs(raw.get("probabilities"), names.iter());
            // A map keyed by level description rather than index.
            if probabilities.is_empty() {
                probabilities = named_probs(raw.get("probabilities"), levels.iter())
                    .into_iter()
                    .enumerate()
                    .map(|(i, (_, p))| (i.to_string(), p))
                    .collect();
            }
            let level = argmax(&probabilities).or_else(|| {
                // Fall back to the expected level the server reports. Its base
                // is not documented; with no probabilities to check it against,
                // round and clamp into range.
                raw.get("score")
                    .and_then(Value::as_f64)
                    .map(|s| (s.round().max(0.0) as usize).min(levels.len().saturating_sub(1)))
            })?;
            let confidence = raw
                .get("confidence")
                .and_then(as_prob)
                .or_else(|| probabilities.get(level).map(|(_, p)| *p));
            Some(Answer {
                level: Some(level),
                confidence,
                probabilities,
                ..Answer::default()
            })
        }
    }
}

/// Probabilities as `(name, p)` in the order of `names`, from either a map
/// keyed by name or an array in that order. Empty when neither fits.
fn named_probs<'a>(
    v: Option<&Value>,
    names: impl Iterator<Item = &'a String> + Clone,
) -> Vec<(String, f32)> {
    match v {
        Some(Value::Object(m)) => names
            .filter_map(|n| m.get(n).and_then(as_prob).map(|p| (n.clone(), p)))
            .collect(),
        Some(Value::Array(a)) => names
            .zip(a.iter())
            .filter_map(|(n, p)| as_prob(p).map(|p| (n.clone(), p)))
            .collect(),
        _ => Vec::new(),
    }
}

fn argmax(probs: &[(String, f32)]) -> Option<usize> {
    probs
        .iter()
        .enumerate()
        .max_by(|a, b| a.1 .1.total_cmp(&b.1 .1))
        .map(|(i, _)| i)
}

#[async_trait]
impl DeciderBackend for SystemOne {
    async fn ask(
        &self,
        state: &str,
        questions: &[(String, Question)],
    ) -> Result<Answers, DeciderError> {
        let resp = client()
            .post(self.endpoint())
            .json(&request_body(state, questions))
            .send()
            .await
            .map_err(|e| DeciderError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(DeciderError::Status(status.as_u16()));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| DeciderError::Shape(e.to_string()))?;
        if body.get("answers").is_none() {
            return Err(DeciderError::Shape("no `answers` field".into()));
        }
        Ok(parse_answers(&body, questions))
    }

    async fn probe(&self) -> ProbeResult {
        let started = Instant::now();
        let q = vec![(
            "probe".to_string(),
            Question::yes_no("Is this text a greeting?"),
        )];
        let asked = tokio::time::timeout(PROBE_TIMEOUT, self.ask("Hello there!", &q)).await;
        let rtt_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (ok, error) = match asked {
            Ok(Ok(a)) if a.contains_key("probe") => (true, None),
            Ok(Ok(_)) => (false, Some("the endpoint answered without the probe answer".into())),
            Ok(Err(e)) => (false, Some(e.to_string())),
            Err(_) => (false, Some("timed out".into())),
        };
        let model = if ok { self.model_name().await } else { None };
        ProbeResult {
            ok,
            model,
            rtt_ms,
            error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qs() -> Vec<(String, Question)> {
        vec![
            ("refund".into(), Question::yes_no("Asking for a refund?")),
            (
                "team".into(),
                Question::Choice {
                    instructions: "Which team?".into(),
                    options: vec![
                        ("billing".into(), "Charges".into()),
                        ("technical".into(), "Faults".into()),
                    ],
                },
            ),
            (
                "urgency".into(),
                Question::Score {
                    instructions: "How urgent?".into(),
                    levels: vec!["Can wait".into(), "Today".into(), "Now".into()],
                },
            ),
        ]
    }

    #[test]
    fn the_request_uses_the_decision_index_shape() {
        let body = request_body("charged twice", &qs());
        assert_eq!(body["state"], "charged twice");
        assert_eq!(body["questions"]["refund"]["type"], "noul");
        assert_eq!(body["questions"]["team"]["criteria"]["billing"], "Charges");
        assert_eq!(body["questions"]["urgency"]["criteria"][2], "Now");
    }

    #[test]
    fn answers_parse_in_object_and_bare_shapes() {
        let body = json!({"answers": {
            "refund": {"noul": 0.93},
            "team": {"choice": "billing", "confidence": 0.88,
                     "probabilities": {"billing": 0.88, "technical": 0.12}},
            "urgency": {"score": 1.2, "confidence": 0.6, "probabilities": [0.2, 0.6, 0.2]},
        }});
        let a = parse_answers(&body, &qs());
        assert_eq!(a["refund"].yes, Some(0.93));
        assert_eq!(a["team"].choice.as_deref(), Some("billing"));
        assert_eq!(a["team"].prob_of("technical"), Some(0.12));
        assert_eq!(a["urgency"].level, Some(1));

        let bare = json!({"answers": {"refund": 0.2}});
        assert_eq!(parse_answers(&bare, &qs())["refund"].yes, Some(0.2));
    }

    #[test]
    fn unrecognised_answers_are_omitted_not_guessed() {
        let body = json!({"answers": {"refund": "yes", "team": {}}});
        let a = parse_answers(&body, &qs());
        assert!(a.is_empty());
    }

    #[test]
    fn a_full_route_url_is_accepted() {
        assert_eq!(
            SystemOne::new("http://h:8080/v1/systemone/").endpoint(),
            "http://h:8080/v1/systemone"
        );
    }
}
