use super::base::Delta;
use dashmap::DashMap;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use super::anthropic::AnthropicProvider;
use super::base::{HealthStatus, Provider, SamplingParams};
use super::openai_compat::OpenAICompatibleProvider;
use super::sampling;
use crate::config::{CacheConfig, ProviderConfig};
use crate::error::ProviderError;
use crate::network::TailscaleClient;
use crate::storage::providers::ProviderRow;

struct ProviderEntry {
    /// `Arc` rather than `Box` so an awaited network call (`chat_completion`,
    /// `discover_models`, `check_health`) can clone the handle out, drop the
    /// DashMap guard, and only then `.await` — a `Box` can't be shared, so
    /// the guard used to be held across the whole request.
    provider: Arc<dyn Provider>,
    health: HealthStatus,
    health_checked_at: Instant,
    /// This provider's own `-np`/`--parallel` slot count, when set — see
    /// `ProviderConfig::parallel_slots`'s doc comment.
    parallel_slots: Option<u32>,
    /// Already resolved at registration time: the profile's configured
    /// overrides merged onto `sampling::defaults_for` — see that function's
    /// doc comment for why self-hosted providers get a non-empty floor and
    /// hosted ones don't.
    sampling: SamplingParams,
    /// Per-provider override for BigTiny's context-window budgeting,
    /// overriding the daemon-wide `token_management.max_context_tokens` for
    /// sessions on this provider — see `ProviderConfig::context_length`.
    context_length: Option<i32>,
    /// Selection order when no provider was explicitly requested — lower
    /// wins. Kitty writes 1 for the active profile and 100 for every other
    /// (`sync_active_provider`); see `get_provider_id`.
    fallback_priority: i32,
}

const HEALTH_TTL_SECS: u64 = 30;

pub struct ProviderRouter {
    providers: DashMap<String, ProviderEntry>,
    /// Shared across every registered provider so the peer cache (and the
    /// "Tailscale unreachable" warn-once) is discovered/logged at most once
    /// per daemon run, not once per provider.
    tailscale: Arc<TailscaleClient>,
    /// Prompt-cache-affecting behavior (slot pinning, Anthropic
    /// `cache_control`) — see `prompt_determinism.md`. Cloned into every
    /// registered provider the same way `tailscale` is.
    cache: CacheConfig,
}

impl ProviderRouter {
    pub fn new(cache: CacheConfig) -> Self {
        Self {
            providers: DashMap::new(),
            tailscale: Arc::new(TailscaleClient::new()),
            cache,
        }
    }

    /// Resolve the entry's cached `sampling`/`parallel_slots`/`context_length`
    /// from a `ProviderConfig` before it's moved into the concrete provider
    /// constructor — shared by `register_openai`/`register_anthropic` so
    /// both stay in sync.
    fn resolved_fields(config: &ProviderConfig) -> (Option<u32>, SamplingParams, Option<i32>, i32) {
        let configured = SamplingParams {
            temperature: config.temperature,
            top_p: config.top_p,
            top_k: config.top_k,
            min_p: config.min_p,
            presence_penalty: config.presence_penalty,
            frequency_penalty: config.frequency_penalty,
            max_tokens: config.max_tokens,
        };
        let resolved_sampling =
            sampling::resolve(&config.provider_type, &config.model, &configured);
        (
            config.parallel_slots,
            resolved_sampling,
            config.context_length,
            config.fallback_priority,
        )
    }

    pub fn register_openai(&self, provider_id: &str, config: ProviderConfig) {
        let (parallel_slots, resolved_sampling, context_length, fallback_priority) =
            Self::resolved_fields(&config);
        let p: Arc<dyn Provider> = Arc::new(OpenAICompatibleProvider::new(
            provider_id,
            config,
            self.tailscale.clone(),
        ));
        self.providers.insert(
            provider_id.to_string(),
            ProviderEntry {
                provider: p,
                health: HealthStatus {
                    status: "disconnected".into(),
                    latency_ms: None,
                    error: None,
                },
                health_checked_at: Instant::now(),
                parallel_slots,
                sampling: resolved_sampling,
                context_length,
                fallback_priority,
            },
        );
    }

    pub fn register_anthropic(&self, provider_id: &str, config: ProviderConfig) {
        let (parallel_slots, resolved_sampling, context_length, fallback_priority) =
            Self::resolved_fields(&config);
        let p: Arc<dyn Provider> = Arc::new(AnthropicProvider::new(
            provider_id,
            config,
            self.tailscale.clone(),
            self.cache.clone(),
        ));
        self.providers.insert(
            provider_id.to_string(),
            ProviderEntry {
                provider: p,
                health: HealthStatus {
                    status: "disconnected".into(),
                    latency_ms: None,
                    error: None,
                },
                health_checked_at: Instant::now(),
                parallel_slots,
                sampling: resolved_sampling,
                context_length,
                fallback_priority,
            },
        );
    }

    pub fn unregister(&self, provider_id: &str) {
        self.providers.remove(provider_id);
    }

    /// Force a provider's cached health, so selection policy can be tested
    /// without standing up real endpoints (the real setter is
    /// `check_all_health`, which needs live HTTP).
    #[cfg(test)]
    fn set_health_for_test(&self, provider_id: &str, status: &str) {
        if let Some(mut entry) = self.providers.get_mut(provider_id) {
            entry.health.status = status.into();
        }
    }

    /// Register (or refresh) a provider from its DB row — the `config` JSON
    /// blob is where `api_key`/`model` live (the `providers` table has no
    /// dedicated columns for either). Shared by startup's `load_providers`
    /// and the `/api/providers` create/update routes so both stay in sync.
    pub fn register_from_row(&self, row: &ProviderRow) {
        let config_json: serde_json::Value = row
            .config
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| serde_json::json!({}));

        // `row.provider_type` is the DB column, constrained by a CHECK to
        // `('openai_compat', 'anthropic')` — it only ever distinguishes the
        // wire *format*, not the dialect. Kitty collapses `ollama`/
        // `openai`/`openrouter`/`custom_openai` all into `openai_compat`
        // before it ever reaches this row (`bigtiny_provider_target`), so
        // that column can't tell a self-hosted llama-server/Ollama endpoint
        // apart from hosted OpenAI/OpenRouter — which matters here because
        // `sampling::defaults_for` and the `top_k`/`min_p` wire gate in
        // `openai_compat.rs` both need exactly that distinction. `Kitty`
        // (and any other caller who cares) instead stores the granular type
        // in the unconstrained `config` JSON blob as `provider_dialect`;
        // fall back to the DB column when it's absent (BigTiny used
        // directly, with no Kitty in front of it).
        let provider_dialect = config_json
            .get("provider_dialect")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| row.provider_type.clone());

        let runtime_config = ProviderConfig {
            name: row.name.clone(),
            provider_type: provider_dialect,
            base_url: row.base_url.clone(),
            api_key: config_json
                .get("api_key")
                .and_then(|v| v.as_str())
                .map(crate::crypto::decrypt)
                .unwrap_or_default(),
            fallback_priority: row.fallback_priority,
            model: config_json
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            parallel_slots: config_json
                .get("parallel_slots")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            temperature: config_json.get("temperature").and_then(|v| v.as_f64()),
            top_p: config_json.get("top_p").and_then(|v| v.as_f64()),
            top_k: config_json
                .get("top_k")
                .and_then(|v| v.as_i64())
                .map(|v| v as i32),
            min_p: config_json.get("min_p").and_then(|v| v.as_f64()),
            presence_penalty: config_json.get("presence_penalty").and_then(|v| v.as_f64()),
            frequency_penalty: config_json
                .get("frequency_penalty")
                .and_then(|v| v.as_f64()),
            max_tokens: config_json
                .get("max_tokens")
                .and_then(|v| v.as_i64())
                .map(|v| v as i32),
            context_length: config_json
                .get("context_length")
                .and_then(|v| v.as_i64())
                .map(|v| v as i32),
            idle_timeout_secs: config_json.get("idle_timeout_secs").and_then(|v| v.as_f64()),
        };

        if row.provider_type == "anthropic" {
            self.register_anthropic(&row.id, runtime_config);
        } else {
            self.register_openai(&row.id, runtime_config);
        }
    }

    /// Register every provider persisted in the DB — mirrors Python's
    /// `ProviderRouter.load_providers()`, called once at daemon startup.
    pub async fn load_providers(
        &self,
        pool: &sqlx::SqlitePool,
    ) -> Result<(), crate::error::StorageError> {
        let rows = crate::storage::providers::list_providers(pool).await?;
        for row in &rows {
            self.register_from_row(row);
        }
        Ok(())
    }

    /// Get the provider ID to use.
    ///
    /// An explicitly requested `preferred_id` that is registered is returned
    /// **unconditionally**, healthy or not. This used to fall through to "any
    /// healthy provider" whenever the requested one wasn't marked healthy,
    /// which silently ran the turn against a completely different endpoint,
    /// model and API key — with no event, no log line, and nothing in the UI
    /// to say it had happened.
    ///
    /// That is much worse than it sounds, because `check_health` just GETs
    /// `{base_url}/v1/models`: OpenRouter serves that route publicly with no
    /// auth, so an OpenRouter profile with *no API key configured at all*
    /// reports `healthy`, while a self-hosted box that is merely offline (a
    /// sleeping Tailscale peer, say) reports `unhealthy`. The substitution
    /// therefore preferred the broken provider, and the turn died on a
    /// confusing `401 Missing Authentication header` from a provider the
    /// user never selected. A provider the user explicitly chose must fail
    /// loudly as itself instead.
    ///
    /// Cross-provider failover still exists — `agent::loop_` opts into it via
    /// `fallback.enabled` and calls this with `None` on retry, which is the
    /// one path that *should* look elsewhere.
    ///
    /// With no preference, selection is ordered by `fallback_priority`
    /// (ascending, healthy first, ties broken by id) rather than by DashMap
    /// iteration order, which is arbitrary and varies run to run. Kitty has
    /// always assumed this: `sync_active_provider` promotes the active
    /// profile to priority 1 and demotes every other to 100 specifically to
    /// express "use this one" — an intent the router previously ignored
    /// outright.
    pub fn get_provider_id(&self, preferred_id: Option<&str>) -> Result<String, ProviderError> {
        if let Some(id) = preferred_id {
            if self.providers.contains_key(id) {
                return Ok(id.to_string());
            }
        }

        let mut candidates: Vec<(bool, i32, String)> = self
            .providers
            .iter()
            .map(|e| {
                (
                    e.health.status != "healthy",
                    e.fallback_priority,
                    e.key().clone(),
                )
            })
            .collect();
        candidates.sort();

        candidates
            .into_iter()
            .next()
            .map(|(_, _, id)| id)
            .ok_or_else(|| ProviderError::NoHealthyProvider {
                user_message: "No providers are configured.".into(),
            })
    }

    pub async fn check_all_health(&self) {
        // Collect which providers are due first (guards dropped), then probe
        // each one holding no shard lock — `iter_mut` used to hold each
        // shard's write lock across `check_health().await`, blocking the whole
        // map for the duration of every network call and stalling any
        // concurrent `chat_completion`.
        let due: Vec<(String, Arc<dyn Provider>)> = self
            .providers
            .iter()
            .filter(|e| {
                e.health_checked_at.elapsed().as_secs() >= HEALTH_TTL_SECS
                    || e.health.status == "disconnected"
            })
            .map(|e| (e.key().clone(), e.value().provider.clone()))
            .collect();

        for (id, provider) in due {
            let status = provider.check_health().await;
            if let Some(mut entry) = self.providers.get_mut(&id) {
                entry.health = status;
                entry.health_checked_at = Instant::now();
            }
        }
    }

    pub fn provider_ids(&self) -> Vec<String> {
        self.providers.iter().map(|e| e.key().clone()).collect()
    }

    /// Id + cached `HealthStatus` for every registered provider — used by
    /// `GET /api/status`, which previously reported only `{"id": id}` per
    /// provider despite `check_all_health` having just computed (or reused a
    /// cached) status for each one right before building the response.
    pub fn provider_health(&self) -> Vec<(String, HealthStatus)> {
        self.providers
            .iter()
            .map(|e| (e.key().clone(), e.value().health.clone()))
            .collect()
    }

    /// Check and refresh health for a single provider (bypasses the TTL
    /// cache), returning the fresh status. Used by `POST /api/providers/{id}/test`.
    pub async fn check_health(&self, provider_id: &str) -> Result<HealthStatus, ProviderError> {
        // Clone the provider handle out and drop the guard before the await —
        // the network call can take seconds and must not hold the shard lock.
        let provider = self
            .providers
            .get(provider_id)
            .ok_or_else(|| ProviderError::NoHealthyProvider {
                user_message: format!("Provider '{}' not found", provider_id),
            })?
            .provider
            .clone();
        let status = provider.check_health().await;
        if let Some(mut entry) = self.providers.get_mut(provider_id) {
            entry.health = status.clone();
            entry.health_checked_at = Instant::now();
        }
        Ok(status)
    }

    pub async fn discover_models(
        &self,
        provider_id: &str,
    ) -> Result<Vec<super::base::ModelInfo>, ProviderError> {
        let provider = self
            .providers
            .get(provider_id)
            .ok_or_else(|| ProviderError::NoHealthyProvider {
                user_message: format!("Provider '{}' not found", provider_id),
            })?
            .provider
            .clone();
        provider.discover_models().await
    }

    /// This provider's own configured `-np`/`--parallel` slot count, if set
    /// — see `ProviderConfig::parallel_slots`. `None` for an unknown
    /// provider id, same as an unconfigured one: no `id_slot` gets sent.
    pub fn parallel_slots(&self, provider_id: &str) -> Option<u32> {
        self.providers
            .get(provider_id)
            .and_then(|e| e.parallel_slots)
    }

    /// This provider's resolved sampling parameters (configured overrides
    /// merged onto its model-aware defaults at registration time — see
    /// `sampling::resolve`). `SamplingParams::default()` (all `None`, so
    /// nothing extra is sent) for an unknown provider id.
    pub fn sampling(&self, provider_id: &str) -> SamplingParams {
        self.providers
            .get(provider_id)
            .map(|e| e.sampling.clone())
            .unwrap_or_default()
    }

    /// This provider's context-length override, if set — see
    /// `ProviderConfig::context_length`.
    pub fn context_length(&self, provider_id: &str) -> Option<i32> {
        self.providers
            .get(provider_id)
            .and_then(|e| e.context_length)
    }

    /// Resolve model for a specific provider.
    pub fn resolve_model(&self, provider_id: &str, override_model: Option<&str>) -> String {
        if let Some(entry) = self.providers.get(provider_id) {
            entry.provider.resolve_model(override_model)
        } else {
            "unknown".to_string()
        }
    }

    /// Call chat_completion on a specific provider.
    pub async fn chat_completion(
        &self,
        provider_id: &str,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        sampling: SamplingParams,
        model: Option<String>,
        id_slot: Option<i32>,
    ) -> Result<Pin<Box<dyn Stream<Item = Delta> + Send>>, ProviderError> {
        // Clone the provider's Arc out, drop the DashMap guard, then await —
        // a chat completion can run for minutes and must never hold the shard
        // lock (which would block health checks and other completions).
        let provider = self
            .providers
            .get(provider_id)
            .ok_or_else(|| ProviderError::NoHealthyProvider {
                user_message: format!("Provider '{}' not found", provider_id),
            })?
            .provider
            .clone();
        provider
            .chat_completion(messages, tools, sampling, model, id_slot)
            .await
    }

    /// Get the provider ID to use, preferring healthy ones.
    /// Returns provider_id and model override from config.
    pub async fn resolve_provider(
        &self,
        preferred_id: Option<&str>,
    ) -> Result<(String, Option<String>), ProviderError> {
        let id = self.get_provider_id(preferred_id)?;
        let model = self
            .providers
            .get(&id)
            .map(|entry| entry.provider.resolve_model(None));
        Ok((id, model))
    }
}

impl Default for ProviderRouter {
    fn default() -> Self {
        Self::new(CacheConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_slots_reflects_the_registered_providers_own_config() {
        let router = ProviderRouter::default();
        router.register_openai(
            "pinned",
            ProviderConfig {
                parallel_slots: Some(4),
                ..Default::default()
            },
        );
        router.register_openai("unpinned", ProviderConfig::default());

        assert_eq!(router.parallel_slots("pinned"), Some(4));
        assert_eq!(router.parallel_slots("unpinned"), None);
        assert_eq!(router.parallel_slots("does-not-exist"), None);
    }

    #[test]
    fn register_from_row_reads_parallel_slots_out_of_the_config_json_blob() {
        let router = ProviderRouter::default();
        let row = ProviderRow {
            id: "p1".into(),
            name: "llama-server".into(),
            provider_type: "openai_compat".into(),
            base_url: "http://192.168.1.199:8081".into(),
            fallback_priority: 0,
            config: Some(r#"{"model":"qwen3.6","parallel_slots":2}"#.into()),
            status: "disconnected".into(),
            error_message: None,
            created_at: None,
            updated_at: None,
        };
        router.register_from_row(&row);
        assert_eq!(router.parallel_slots("p1"), Some(2));
    }

    #[test]
    fn self_hosted_provider_with_no_sampling_config_gets_the_repetition_safe_defaults() {
        let router = ProviderRouter::default();
        router.register_openai(
            "llama-server",
            ProviderConfig {
                provider_type: "custom_openai".into(),
                ..Default::default()
            },
        );
        let s = router.sampling("llama-server");
        assert_eq!(s.presence_penalty, Some(1.0));
        assert_eq!(s.top_k, Some(20));
    }

    #[test]
    fn a_configured_sampling_field_overrides_the_default_but_others_still_apply() {
        let router = ProviderRouter::default();
        router.register_openai(
            "llama-server",
            ProviderConfig {
                provider_type: "custom_openai".into(),
                temperature: Some(0.1),
                ..Default::default()
            },
        );
        let s = router.sampling("llama-server");
        assert_eq!(s.temperature, Some(0.1));
        assert_eq!(s.presence_penalty, Some(1.0));
    }

    #[test]
    fn hosted_anthropic_provider_gets_no_sampling_defaults() {
        let router = ProviderRouter::default();
        router.register_anthropic("claude", ProviderConfig::default());
        assert_eq!(router.sampling("claude"), SamplingParams::default());
    }

    #[test]
    fn context_length_reflects_the_registered_providers_own_config() {
        let router = ProviderRouter::default();
        router.register_openai(
            "with-ctx",
            ProviderConfig {
                context_length: Some(32000),
                ..Default::default()
            },
        );
        router.register_openai("no-ctx", ProviderConfig::default());

        assert_eq!(router.context_length("with-ctx"), Some(32000));
        assert_eq!(router.context_length("no-ctx"), None);
        assert_eq!(router.context_length("does-not-exist"), None);
    }

    #[test]
    fn register_from_row_reads_sampling_and_context_length_out_of_the_config_json_blob() {
        let router = ProviderRouter::default();
        let row = ProviderRow {
            id: "p1".into(),
            name: "llama-server".into(),
            provider_type: "custom_openai".into(),
            base_url: "http://192.168.1.199:8081".into(),
            fallback_priority: 0,
            config: Some(
                r#"{"model":"qwen3.6","temperature":0.2,"presence_penalty":1.3,"context_length":32000}"#
                    .into(),
            ),
            status: "disconnected".into(),
            error_message: None,
            created_at: None,
            updated_at: None,
        };
        router.register_from_row(&row);
        let s = router.sampling("p1");
        assert_eq!(s.temperature, Some(0.2));
        assert_eq!(s.presence_penalty, Some(1.3));
        assert_eq!(router.context_length("p1"), Some(32000));
    }

    /// Reproduces the real registry that produced a baffling
    /// `401 Missing Authentication header`: a self-hosted box the user had
    /// actually selected was offline (`unhealthy`), while a *keyless*
    /// OpenRouter profile sitting alongside it reported `healthy` — because
    /// `check_health` only GETs `{base_url}/v1/models`, which OpenRouter
    /// serves publicly without auth. The router substituted the keyless
    /// profile and sent it an empty bearer token. An explicit choice must
    /// win even when it is known to be down.
    #[test]
    fn an_explicitly_requested_provider_is_never_swapped_for_a_healthy_one() {
        let router = ProviderRouter::default();
        router.register_openai(
            "qwen-selfhosted",
            ProviderConfig {
                provider_type: "custom_openai".into(),
                base_url: "http://100.82.113.84:8081".into(),
                fallback_priority: 1,
                ..Default::default()
            },
        );
        router.register_openai(
            "openrouter-no-key",
            ProviderConfig {
                provider_type: "openrouter".into(),
                base_url: "https://openrouter.ai/api".into(),
                fallback_priority: 100,
                ..Default::default()
            },
        );
        router.set_health_for_test("qwen-selfhosted", "unhealthy");
        router.set_health_for_test("openrouter-no-key", "healthy");

        assert_eq!(
            router.get_provider_id(Some("qwen-selfhosted")).unwrap(),
            "qwen-selfhosted"
        );
    }

    /// With no explicit preference, `fallback_priority` decides — this is the
    /// contract Kitty's `sync_active_provider` has always assumed when it
    /// promotes the active profile to 1 and demotes the rest to 100. Before
    /// this fix nothing read the field at all and selection followed
    /// arbitrary DashMap order.
    #[test]
    fn unpreferred_selection_follows_fallback_priority_not_map_order() {
        let router = ProviderRouter::default();
        for (id, priority) in [("demoted-a", 100), ("active", 1), ("demoted-b", 100)] {
            router.register_openai(
                id,
                ProviderConfig {
                    provider_type: "custom_openai".into(),
                    fallback_priority: priority,
                    ..Default::default()
                },
            );
            router.set_health_for_test(id, "healthy");
        }
        assert_eq!(router.get_provider_id(None).unwrap(), "active");
    }

    /// Healthy still beats unhealthy when nothing was explicitly requested —
    /// that part of the old behavior was right and is what `fallback.enabled`
    /// retries depend on (`agent::loop_` calls this with `None` on retry).
    #[test]
    fn unpreferred_selection_prefers_healthy_over_a_better_priority_thats_down() {
        let router = ProviderRouter::default();
        router.register_openai(
            "preferred-but-down",
            ProviderConfig {
                fallback_priority: 1,
                ..Default::default()
            },
        );
        router.register_openai(
            "backup-thats-up",
            ProviderConfig {
                fallback_priority: 50,
                ..Default::default()
            },
        );
        router.set_health_for_test("preferred-but-down", "unhealthy");
        router.set_health_for_test("backup-thats-up", "healthy");

        assert_eq!(router.get_provider_id(None).unwrap(), "backup-thats-up");
    }

    /// A `preferred_id` naming a provider that isn't registered at all (a
    /// stale id on an old session, say) still falls back rather than failing.
    #[test]
    fn an_unregistered_preferred_id_falls_back_by_priority() {
        let router = ProviderRouter::default();
        router.register_openai(
            "active",
            ProviderConfig {
                fallback_priority: 1,
                ..Default::default()
            },
        );
        assert_eq!(
            router.get_provider_id(Some("deleted-long-ago")).unwrap(),
            "active"
        );
    }

    #[test]
    fn an_empty_registry_still_errors() {
        let router = ProviderRouter::default();
        assert!(router.get_provider_id(None).is_err());
        assert!(router.get_provider_id(Some("anything")).is_err());
    }
}
