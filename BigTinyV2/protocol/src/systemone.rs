//! The System One decision-model wire format (`POST /v1/systemone`), shared by
//! the engine's decider and kitty-web's page check so the two cannot drift.
//!
//! Request: `{"state", "questions": {name: {type, instructions, criteria?}},
//! "model"?}`. Response: `{"answers": {name: answer}}`, or the same under
//! `result` (Cloudflare Workers AI's envelope). Pure functions, no I/O.

use serde_json::{json, Map, Value};

/// The self-hosted dialect: a server root, `/v1/systemone` appended, no auth.
pub const DIALECT: &str = "systemone";
/// The hosted dialect: the URL used as given, a bearer token, the model id
/// in the body.
pub const DIALECT_URL: &str = "systemone-url";

/// The endpoint to POST to. `append_route` for the self-hosted dialect.
pub fn endpoint(url: &str, append_route: bool) -> String {
    let clean = url.trim().trim_end_matches('/');
    if append_route {
        let base = clean.trim_end_matches("/v1/systemone").trim_end_matches('/');
        format!("{base}/v1/systemone")
    } else {
        clean.to_string()
    }
}

/// The server root a self-hosted endpoint was given as (for discovery
/// routes beside `/v1/systemone`).
pub fn base_url(url: &str) -> String {
    url.trim()
        .trim_end_matches('/')
        .trim_end_matches("/v1/systemone")
        .trim_end_matches('/')
        .to_string()
}

/// The request body. `model`, when set, selects the model on a hosted
/// gateway (a single-model self-hosted server ignores it).
pub fn request_body(state: &str, questions: Map<String, Value>, model: Option<&str>) -> Value {
    let mut body = json!({ "state": state, "questions": questions });
    if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
        body["model"] = Value::String(m.to_string());
    }
    body
}

/// A yes/no question.
pub fn yes_no(instructions: &str) -> Value {
    json!({"type": "noul", "instructions": instructions})
}

/// A choice among named options (`(name, description)`).
pub fn choice<'a>(instructions: &str, options: impl IntoIterator<Item = (&'a str, &'a str)>) -> Value {
    let criteria: Map<String, Value> = options
        .into_iter()
        .map(|(k, d)| (k.to_string(), Value::String(d.to_string())))
        .collect();
    json!({"type": "choice", "instructions": instructions, "criteria": criteria})
}

/// The answers object of a response, at the top level or under `result`.
pub fn answers(body: &Value) -> Option<&Map<String, Value>> {
    body.get("answers")
        .or_else(|| body.get("result").and_then(|r| r.get("answers")))
        .and_then(Value::as_object)
}

/// A probability in `[0, 1]`, or `None` for anything else.
pub fn as_prob(v: &Value) -> Option<f32> {
    v.as_f64()
        .map(|p| p as f32)
        .filter(|p| p.is_finite())
        .map(|p| p.clamp(0.0, 1.0))
}

/// A yes/no answer: a bare P(yes), `{"noul": p}` or `{"probability": p}`.
pub fn parse_yes(raw: &Value) -> Option<f32> {
    as_prob(raw)
        .or_else(|| raw.get("noul").and_then(as_prob))
        .or_else(|| raw.get("probability").and_then(as_prob))
}

/// Probabilities as `(name, p)` in the order of `names`, from either a map
/// keyed by name or an array in that order. Empty when neither fits.
pub fn named_probs<'a>(
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

/// The index of the most probable entry.
pub fn argmax(probs: &[(String, f32)]) -> Option<usize> {
    probs
        .iter()
        .enumerate()
        .max_by(|a, b| a.1 .1.total_cmp(&b.1 .1))
        .map(|(i, _)| i)
}

/// `(chosen option, its confidence, all probabilities)`.
pub type ChoiceAnswer = (String, Option<f32>, Vec<(String, f32)>);

/// A choice answer: `(chosen option, its confidence, all probabilities)`.
///
/// The choice is the server's `choice`, else the most probable option. Its
/// confidence is the chosen option's own probability when the server gave
/// one: the server's `confidence` is a separate calibration score, not
/// P(choice) (d1 answers billing at P 0.984, confidence 0.976), and is used
/// only when there are no probabilities.
pub fn parse_choice(raw: &Value, options: &[String]) -> Option<ChoiceAnswer> {
    let probabilities = named_probs(raw.get("probabilities"), options.iter());
    let choice = raw
        .get("choice")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| argmax(&probabilities).map(|i| probabilities[i].0.clone()))?;
    let confidence = probabilities
        .iter()
        .find(|(k, _)| *k == choice)
        .map(|(_, p)| *p)
        .or_else(|| raw.get("confidence").and_then(as_prob));
    Some((choice, confidence, probabilities))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_for_both_dialects() {
        assert_eq!(endpoint("http://h:8080/", true), "http://h:8080/v1/systemone");
        assert_eq!(endpoint("http://h:8080/v1/systemone/", true), "http://h:8080/v1/systemone");
        assert_eq!(endpoint(" https://gw/x/run ", false), "https://gw/x/run");
        assert_eq!(base_url("http://h:8080/d1/v1/systemone"), "http://h:8080/d1");
    }

    #[test]
    fn answers_at_the_top_or_under_result() {
        let top = json!({"answers": {"a": 0.2}});
        let cf = json!({"result": {"answers": {"a": 0.3}}});
        assert_eq!(parse_yes(&answers(&top).unwrap()["a"]), Some(0.2));
        assert_eq!(parse_yes(&answers(&cf).unwrap()["a"]), Some(0.3));
        assert_eq!(parse_yes(&json!({"noul": 0.8})), Some(0.8));
    }

    #[test]
    fn a_choice_takes_the_chosen_options_probability() {
        let opts = vec!["x".to_string(), "y".to_string()];
        let raw = json!({"choice": "y", "confidence": 0.5, "probabilities": {"x": 0.1, "y": 0.9}});
        let (c, p, _) = parse_choice(&raw, &opts).unwrap();
        assert_eq!((c.as_str(), p), ("y", Some(0.9)));
        let raw = json!({"probabilities": [0.7, 0.3]});
        assert_eq!(parse_choice(&raw, &opts).unwrap().0, "x");
    }

    #[test]
    fn the_model_is_sent_only_when_set() {
        assert!(request_body("s", Map::new(), None).get("model").is_none());
        assert_eq!(request_body("s", Map::new(), Some("clef"))["model"], "clef");
    }
}
