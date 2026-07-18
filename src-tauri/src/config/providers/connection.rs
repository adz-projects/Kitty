//! On-demand connectivity+auth probe for a provider profile.

use std::time::Duration;

use crate::lifecycle::ollama_proc;

use super::keyring::get_secret_async;
use super::ProviderProfile;

/// Lightweight, on-demand connectivity+auth probe for a provider profile —
/// never used for a background poll (that was deliberately removed, see
/// `emit_health_from_send_result`'s doc comment); only called from
/// `activate_provider` (reject a switch to a non-functioning provider) and
/// the manual "Retry connection check" command. `Ok(())` means the profile
/// looks usable; `Err(String)` is a human-readable reason to show the user.
pub async fn test_connection(profile: &ProviderProfile) -> Result<(), String> {
    match profile.provider_type.as_str() {
        "ollama" => {
            let client = crate::util::http_client();
            if !ollama_proc::probe_version(&client, &profile.base_url).await {
                return Err(format!("couldn't reach Ollama at {}", profile.base_url));
            }
            if let Some(model) = profile.models.first() {
                if !ollama_proc::has_model_tag(&client, &profile.base_url, model).await {
                    return Err(format!(
                        "Ollama is reachable, but \"{model}\" isn't installed"
                    ));
                }
            }
            Ok(())
        }
        "openrouter" => {
            let key = get_secret_async(&profile.id)
                .await
                .ok_or("no API key stored for this profile — edit it and add one")?;
            crate::openrouter::get_credits(&key).await.map(|_| ())
        }
        "anthropic" => {
            let key = get_secret_async(&profile.id)
                .await
                .ok_or("no API key stored for this profile — edit it and add one")?;
            let client = crate::util::http_client();
            let url = format!("{}/v1/models", profile.base_url.trim_end_matches('/'));
            let resp = client
                .get(url)
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .map_err(|e| format!("could not reach Anthropic: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!(
                    "Anthropic returned {} — check the API key",
                    resp.status()
                ));
            }
            Ok(())
        }
        "openai" | "custom_openai" => {
            let client = crate::util::http_client();
            let url = format!("{}/models", profile.base_url.trim_end_matches('/'));
            let mut req = client.get(url).timeout(Duration::from_secs(10));
            if let Some(key) = get_secret_async(&profile.id).await {
                req = req.bearer_auth(key);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| format!("could not reach {}: {e}", profile.base_url))?;
            if !resp.status().is_success() {
                return Err(format!("{} returned {}", profile.base_url, resp.status()));
            }
            Ok(())
        }
        other => Err(format!("unknown provider type: {other}")),
    }
}
