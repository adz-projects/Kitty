//! Discovering how many requests an endpoint will actually serve at once.
//!
//! V1 guessed. `default_concurrency` returns 1 for `custom_openai`/`ollama`,
//! which is the safe guess — a llama-server built without `--parallel` really
//! does serve one request at a time, and a second arrives, waits with no
//! response headers, and dies at the 30s header timeout while the abandoned
//! request keeps generating server-side.
//!
//! But safe is not free. A `llama-server --parallel 4` is throttled to a
//! quarter of its capacity, and with several apps sharing that endpoint the
//! guess is exactly when it hurts most. Both llama.cpp and Ollama will say what
//! they support if asked, so ask.
//!
//! Order of precedence:
//!
//! 1. a user-set `parallel_slots` — an explicit statement outranks a probe;
//! 2. what the endpoint reports;
//! 3. `default_concurrency`, unchanged.

use std::time::Duration;

/// Where a provider's slot count came from, surfaced on `GET /api/providers`
/// so an operator can tell a measured value from a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotsSource {
    /// A user-set `parallel_slots`.
    Configured,
    /// Reported by the endpoint itself.
    Probed,
    /// `default_concurrency` for the dialect.
    Default,
}

/// Probes must not delay provider registration; an endpoint that is slow or
/// down falls back to the default rather than holding up startup.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// A sane ceiling for a reported value.
///
/// A malformed or absurd response (a server reporting thousands of slots)
/// would otherwise remove the concurrency limit entirely, which is the exact
/// failure the limit exists to prevent.
const MAX_PROBED: u32 = 64;

/// Ask an OpenAI-compatible endpoint how many parallel slots it has.
///
/// Returns `None` for anything unrecognised or unreachable, which is not an
/// error: most endpoints do not answer this, and the caller falls back.
pub async fn probe(client: &reqwest::Client, base_url: &str, dialect: &str) -> Option<u32> {
    match dialect {
        // llama.cpp's `llama-server` exposes `/props`, whose
        // `total_slots` is precisely the `--parallel` value.
        "custom_openai" | "local" => probe_llama_props(client, base_url).await,
        // Ollama's concurrency is set by `OLLAMA_NUM_PARALLEL` and is not
        // reported over the API, so there is nothing to ask.
        _ => None,
    }
}

async fn probe_llama_props(client: &reqwest::Client, base_url: &str) -> Option<u32> {
    let url = format!("{}/props", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    parse_total_slots(&body)
}

/// Pull a slot count out of a `/props` body.
///
/// Split out so the shape can be tested without a live server, and because
/// llama.cpp has moved this field around between releases.
fn parse_total_slots(body: &serde_json::Value) -> Option<u32> {
    let raw = body
        .get("total_slots")
        .or_else(|| body.get("n_parallel"))
        .or_else(|| body.pointer("/default_generation_settings/n_parallel"))?
        .as_u64()?;
    if raw == 0 {
        // A server reporting zero slots is telling us something is wrong, not
        // that it wants to be unreachable.
        return None;
    }
    Some((raw as u32).min(MAX_PROBED))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn total_slots_is_read_from_the_shapes_llama_cpp_has_used() {
        assert_eq!(parse_total_slots(&json!({"total_slots": 4})), Some(4));
        assert_eq!(parse_total_slots(&json!({"n_parallel": 2})), Some(2));
        assert_eq!(
            parse_total_slots(&json!({"default_generation_settings": {"n_parallel": 8}})),
            Some(8)
        );
    }

    #[test]
    fn an_absent_or_unparseable_field_is_not_an_error() {
        // Most endpoints do not answer this; the caller falls back.
        assert_eq!(parse_total_slots(&json!({})), None);
        assert_eq!(parse_total_slots(&json!({"total_slots": "four"})), None);
    }

    #[test]
    fn a_zero_report_is_ignored_rather_than_believed() {
        // Zero would mean "never serve anything" -- a server saying that is
        // reporting a fault, not a capacity.
        assert_eq!(parse_total_slots(&json!({"total_slots": 0})), None);
    }

    #[test]
    fn an_absurd_report_is_clamped() {
        // Believing it would remove the limit entirely, which is the failure
        // the limit exists to prevent.
        assert_eq!(parse_total_slots(&json!({"total_slots": 100_000})), Some(MAX_PROBED));
    }

    #[tokio::test]
    async fn hosted_dialects_are_not_probed() {
        // They have no such endpoint, and a request per registration would be
        // a pointless round-trip against a metered API.
        let client = reqwest::Client::new();
        assert_eq!(probe(&client, "http://127.0.0.1:1", "openai").await, None);
        assert_eq!(probe(&client, "http://127.0.0.1:1", "anthropic").await, None);
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_falls_back_quietly() {
        let client = reqwest::Client::new();
        assert_eq!(
            probe(&client, "http://127.0.0.1:1", "custom_openai").await,
            None
        );
    }
}
