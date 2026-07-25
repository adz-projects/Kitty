//! OpenRouter's public models list (Round-6 Feature 1): used only to suggest a
//! model's real context window in the Providers form — Kitty never calls
//! OpenRouter for inference itself (that's BigTiny's job, same as every other
//! provider type).

use serde_json::Value;

use crate::util::http_client;

/// `GET /api/v1/models` — no API key required for the list itself. Returns the
/// raw `data` array (each entry has `id`, `context_length`, etc.).
pub async fn list_models() -> Result<Vec<Value>, String> {
    let resp = http_client()
        .get("https://openrouter.ai/api/v1/models")
        .send()
        .await
        .map_err(|e| format!("could not reach OpenRouter: {e}"))?;
    let json: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(json
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default())
}

/// Find `model_id`'s `context_length` in an already-fetched models list.
pub fn context_length_for(models: &[Value], model_id: &str) -> Option<u32> {
    models
        .iter()
        .find(|m| m.get("id").and_then(|i| i.as_str()) == Some(model_id))
        .and_then(|m| m.get("context_length"))
        .and_then(|c| c.as_u64())
        .map(|n| n as u32)
}

/// `GET /api/v1/key` — the API key's own credit balance/usage. Requires the
/// key itself as a bearer token (unlike `list_models`, which is public).
/// Returns the raw `data` object (`label`, `limit`, `limit_remaining`,
/// `usage`, `is_free_tier`, etc. — Kitty only reads a few of these fields,
/// pass the rest through unparsed rather than modeling the whole shape).
pub async fn get_credits(api_key: &str) -> Result<Value, String> {
    let resp = http_client()
        .get("https://openrouter.ai/api/v1/key")
        .bearer_auth(api_key)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("could not reach OpenRouter: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("OpenRouter returned {}", resp.status()));
    }
    let json: Value = resp.json().await.map_err(|e| e.to_string())?;
    json.get("data")
        .cloned()
        .ok_or_else(|| "unexpected response shape from OpenRouter".to_string())
}
