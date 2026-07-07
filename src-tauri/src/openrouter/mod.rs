//! OpenRouter's public models list (Round-6 Feature 1): used only to suggest a
//! model's real context window in the Providers form — Kitty never calls
//! OpenRouter for inference itself (that's goosed's job, same as every other
//! provider type).

use serde_json::Value;

/// `GET /api/v1/models` — no API key required for the list itself. Returns the
/// raw `data` array (each entry has `id`, `context_length`, etc.).
pub async fn list_models() -> Result<Vec<Value>, String> {
    let resp = reqwest::get("https://openrouter.ai/api/v1/models")
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
