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

#[cfg(test)]
mod tests {
    use super::*;

    /// Deliberately omits `id`/`name`/`provider_type`/`base_url`/`models` —
    /// callers fill those in per test. Every other field is a plausible
    /// unconfigured default (`ProviderProfile` has no `Default` impl).
    fn profile(
        id: &str,
        provider_type: &str,
        base_url: &str,
        models: Vec<&str>,
    ) -> ProviderProfile {
        ProviderProfile {
            id: id.to_string(),
            name: "test".to_string(),
            provider_type: provider_type.to_string(),
            base_url: base_url.to_string(),
            models: models.into_iter().map(String::from).collect(),
            is_trusted: false,
            temperature: None,
            top_p: None,
            top_k: None,
            min_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            context_length: None,
            strip_reasoning: false,
            system_prompt: None,
            prompt_idle_timeout_secs: None,
            parallel_slots: None,
            created_at: String::new(),
        }
    }

    /// A fresh, never-`set_secret`-called profile id — `get_secret_async`
    /// reads Windows Credential Manager for real (there's no injectable
    /// fake), but a lookup for an id that was never stored is a fast, local,
    /// deterministic "not found", not a flaky/networked call, so this is
    /// safe to rely on directly rather than needing a mock (unlike
    /// `keyring.rs`'s own tests, which specifically exercise error
    /// classification paths that real Credential Manager calls can't
    /// reliably reproduce on demand).
    fn unconfigured_id() -> String {
        format!("test-connection-unused-{}", uuid_like())
    }

    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        format!(
            "{}-{:?}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::thread::current().id()
        )
    }

    #[tokio::test]
    async fn unknown_provider_type_is_rejected_without_any_network_call() {
        let p = profile("p1", "not-a-real-provider", "http://127.0.0.1:1", vec![]);
        let result = test_connection(&p).await;
        assert_eq!(
            result,
            Err("unknown provider type: not-a-real-provider".to_string())
        );
    }

    #[tokio::test]
    async fn ollama_unreachable_server_is_a_clear_error() {
        // Nothing is listening on this port — the connection itself fails,
        // distinct from a server that responds with an error status.
        let p = profile("p1", "ollama", "http://127.0.0.1:1", vec![]);
        let result = test_connection(&p).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("couldn't reach Ollama"));
    }

    #[tokio::test]
    async fn ollama_reachable_but_model_not_installed() {
        let mut server = mockito::Server::new_async().await;
        let _version = server
            .mock("GET", "/api/version")
            .with_status(200)
            .create_async()
            .await;
        let _tags = server
            .mock("GET", "/api/tags")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"models": [{"name": "other-model"}]}"#)
            .create_async()
            .await;

        let p = profile("p1", "ollama", &server.url(), vec!["missing-model"]);
        let result = test_connection(&p).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("isn't installed"));
    }

    #[tokio::test]
    async fn ollama_reachable_and_model_installed_is_ok() {
        let mut server = mockito::Server::new_async().await;
        let _version = server
            .mock("GET", "/api/version")
            .with_status(200)
            .create_async()
            .await;
        let _tags = server
            .mock("GET", "/api/tags")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"models": [{"name": "llama3"}]}"#)
            .create_async()
            .await;

        let p = profile("p1", "ollama", &server.url(), vec!["llama3"]);
        let result = test_connection(&p).await;
        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn ollama_reachable_with_no_model_configured_only_checks_reachability() {
        let mut server = mockito::Server::new_async().await;
        let _version = server
            .mock("GET", "/api/version")
            .with_status(200)
            .create_async()
            .await;

        let p = profile("p1", "ollama", &server.url(), vec![]);
        let result = test_connection(&p).await;
        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn custom_openai_reachable_with_no_key_stored_is_ok() {
        let mut server = mockito::Server::new_async().await;
        let _models = server
            .mock("GET", "/models")
            .with_status(200)
            .create_async()
            .await;

        let p = profile(&unconfigured_id(), "custom_openai", &server.url(), vec![]);
        let result = test_connection(&p).await;
        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn custom_openai_non_success_status_is_a_clear_error() {
        let mut server = mockito::Server::new_async().await;
        let _models = server
            .mock("GET", "/models")
            .with_status(401)
            .create_async()
            .await;

        let p = profile(&unconfigured_id(), "custom_openai", &server.url(), vec![]);
        let result = test_connection(&p).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("401"));
    }

    #[tokio::test]
    async fn openrouter_without_a_stored_key_is_rejected_before_any_network_call() {
        let p = profile(
            &unconfigured_id(),
            "openrouter",
            "http://127.0.0.1:1",
            vec![],
        );
        let result = test_connection(&p).await;
        assert_eq!(
            result,
            Err("no API key stored for this profile — edit it and add one".to_string())
        );
    }

    #[tokio::test]
    async fn anthropic_without_a_stored_key_is_rejected_before_any_network_call() {
        let p = profile(
            &unconfigured_id(),
            "anthropic",
            "http://127.0.0.1:1",
            vec![],
        );
        let result = test_connection(&p).await;
        assert_eq!(
            result,
            Err("no API key stored for this profile — edit it and add one".to_string())
        );
    }
}
