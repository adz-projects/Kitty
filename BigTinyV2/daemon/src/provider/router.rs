use super::base::Delta;
use dashmap::DashMap;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;
use super::queue::{Priority, ProviderQueue, QueuePermit};

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
    /// How many chat completions may be in flight against this provider at
    /// once — `parallel_slots`, or `default_concurrency` for its dialect.
    /// Resolved at registration so `chat_completion` never has to reason
    /// about dialects.
    concurrency: u32,
    /// Already resolved at registration time: the profile's configured
    /// overrides merged onto `sampling::defaults_for` — see that function's
    /// doc comment for why self-hosted providers get a non-empty floor and
    /// hosted ones don't.
    sampling: SamplingParams,
    /// Per-provider override for BigTiny's context-window budgeting,
    /// overriding the daemon-wide `token_management.max_context_tokens` for
    /// sessions on this provider — see `ProviderConfig::context_length`.
    context_length: Option<i32>,
    /// Tiebreaker *within* one app's visible set — lower wins.
    ///
    /// In V1 this was the whole selection mechanism, and it was daemon-global:
    /// expressing "use this one" meant demoting every other row to 100, which
    /// is why Kitty's `sync_active_provider` rewrote rows it did not own. Here
    /// the primary answer is the app's own `default_provider_id`, and this only
    /// breaks ties among what remains.
    fallback_priority: i32,
    /// Owning app, or `None` for the shared pool every app can see.
    app_id: Option<String>,
}

const HEALTH_TTL_SECS: u64 = 30;

/// Chat completions allowed in flight at once against a provider whose
/// `parallel_slots` is unset.
///
/// One for anything self-hosted. A llama-server or LM Studio built without
/// `--parallel` serves exactly one request at a time; a second arrives, waits
/// behind the first with no response headers, and dies at the 30s header
/// timeout — while the abandoned request keeps generating server-side. Being
/// wrong in the other direction merely queues a request that could have run
/// concurrently, so 1 is the safe default and `parallel_slots` is how a user
/// with `--parallel 4` says so.
fn default_concurrency(dialect: &str) -> u32 {
    match dialect {
        "custom_openai" | "ollama" | "local" => 1,
        // Hosted APIs are built for concurrency; a low cap here would
        // needlessly serialize multiple chat windows.
        _ => 8,
    }
}

/// A `Delta` stream that holds its provider's concurrency permit until the
/// stream itself is dropped.
///
/// The permit must outlive the *whole* response, not just the request:
/// releasing it when headers arrive would free the slot while the server is
/// still generating, which is precisely the overlap this gate exists to
/// prevent.
/// What `GET /api/providers` reports about a provider's queue, so a client
/// can pace itself instead of firing blind.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueueStats {
    pub concurrency: u32,
    pub in_flight: u32,
    pub queue_depth: usize,
    /// This caller's own waiters -- distinct from `queue_depth`, so an app can
    /// tell "the endpoint is busy" from "*I* have a backlog".
    pub my_queue_depth: usize,
}

struct PermitStream {
    inner: Pin<Box<dyn Stream<Item = Delta> + Send>>,
    _permit: QueuePermit,
}

impl Stream for PermitStream {
    type Item = Delta;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Delta>> {
        // Both fields are `Unpin`, so projecting through `get_mut` is safe.
        self.get_mut().inner.as_mut().poll_next(cx)
    }
}

pub struct ProviderRouter {
    providers: DashMap<String, ProviderEntry>,
    /// Per-provider admission queues, `provider id -> queue`.
    ///
    /// Replaces V1's `(limit, Semaphore)` pair. A semaphore is FIFO, which is
    /// correct for one client and a starvation bug for several: against a
    /// one-slot endpoint, an app that queues fifty turns puts fifty entries in
    /// front of the next interactive message. `ProviderQueue` serves apps
    /// round-robin instead, so the wait is bounded by the app count rather
    /// than the queue depth.
    ///
    /// Deliberately NOT a field on `ProviderEntry`: `register_from_row` runs on
    /// every provider PATCH and re-`insert`s the entry wholesale, which would
    /// swap the queue out from under everything waiting on it. Keyed
    /// separately, the queue survives re-registration.
    queues: DashMap<String, Arc<ProviderQueue>>,
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
            queues: DashMap::new(),
            tailscale: Arc::new(TailscaleClient::new()),
            cache,
        }
    }

    /// Resolve the entry's cached `sampling`/`parallel_slots`/`context_length`
    /// from a `ProviderConfig` before it's moved into the concrete provider
    /// constructor — shared by `register_openai`/`register_anthropic` so
    /// both stay in sync.
    fn resolved_fields(
        config: &ProviderConfig,
    ) -> (Option<u32>, u32, SamplingParams, Option<i32>, i32) {
        let configured = SamplingParams {
            temperature: config.temperature,
            top_p: config.top_p,
            top_k: config.top_k,
            min_p: config.min_p,
            presence_penalty: config.presence_penalty,
            frequency_penalty: config.frequency_penalty,
            max_tokens: config.max_tokens,
            // Effort is a per-turn request applied by the agent loop, never a
            // provider-config default — there's nothing to resolve here.
            effort: None,
        };
        let resolved_sampling =
            sampling::resolve(&config.provider_type, &config.model, &configured);
        let concurrency = config
            .parallel_slots
            .filter(|&n| n > 0)
            .unwrap_or_else(|| default_concurrency(&config.provider_type));
        (
            config.parallel_slots,
            concurrency,
            resolved_sampling,
            config.context_length,
            config.fallback_priority,
        )
    }

    pub fn register_openai(&self, provider_id: &str, config: ProviderConfig) {
        self.register_openai_owned(provider_id, config, None)
    }

    /// As [`Self::register_openai`], but recording which app owns the row
    /// (`None` = the shared pool). Ownership decides *visibility* only; the
    /// connection itself is cached once and reused across apps.
    pub fn register_openai_owned(
        &self,
        provider_id: &str,
        config: ProviderConfig,
        app_id: Option<String>,
    ) {
        let (parallel_slots, concurrency, resolved_sampling, context_length, fallback_priority) =
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
                concurrency,
                sampling: resolved_sampling,
                context_length,
                fallback_priority,
                app_id,
            },
        );
    }

    pub fn register_anthropic(&self, provider_id: &str, config: ProviderConfig) {
        self.register_anthropic_owned(provider_id, config, None)
    }

    /// As [`Self::register_anthropic`], but recording which app owns the row
    /// (`None` = the shared pool). Ownership decides *visibility* only; the
    /// connection itself is cached once and reused across apps.
    pub fn register_anthropic_owned(
        &self,
        provider_id: &str,
        config: ProviderConfig,
        app_id: Option<String>,
    ) {
        let (parallel_slots, concurrency, resolved_sampling, context_length, fallback_priority) =
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
                concurrency,
                sampling: resolved_sampling,
                context_length,
                fallback_priority,
                app_id,
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
            // See `ProviderConfig::experimental_prefill`'s doc comment --
            // an explicit user opt-in, off by default.
            experimental_prefill: config_json
                .get("experimental_prefill")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        };

        // Carry the row's owner into the registry. Registering as `None`
        // here would silently publish every private provider into the shared
        // pool the moment it was loaded from the database -- the DB would say
        // "app A's", the router would say "everyone's", and `is_visible_to`
        // trusts the router. Caught by
        // `tenancy::another_apps_provider_can_be_neither_read_patched_nor_deleted`.
        if row.provider_type == "anthropic" {
            self.register_anthropic_owned(&row.id, runtime_config, row.app_id.clone());
        } else {
            self.register_openai_owned(&row.id, runtime_config, row.app_id.clone());
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
    /// Whether `app_id` may use `provider_id` -- its own, or the shared pool.
    pub fn is_visible_to(&self, provider_id: &str, app_id: &str) -> bool {
        self.providers
            .get(provider_id)
            .map(|e| match &e.app_id {
                None => true,
                Some(owner) => owner == app_id,
            })
            .unwrap_or(false)
    }

    /// Resolve which provider a turn should use, for a specific app.
    ///
    /// This replaces V1's daemon-global selection. The order is:
    ///
    /// 1. an explicitly requested provider -- returned unconditionally,
    ///    healthy or not, *provided the app can see it*. A pinned provider must
    ///    fail loudly as itself rather than silently substituting another (see
    ///    [`Self::get_provider_id`] for the failure that motivated this).
    /// 2. the app's own `default_provider_id`, when it is still visible and
    ///    registered.
    /// 3. the healthiest remaining provider the app can see.
    ///
    /// A provider owned by *another* app is treated exactly as one that does
    /// not exist -- no fallback, no warning naming it. Anything else would let
    /// one app's traffic silently run through another app's credentials and
    /// billing account.
    pub fn resolve_provider_for_app(
        &self,
        app_id: &str,
        preferred_id: Option<&str>,
        app_default: Option<&str>,
    ) -> Result<String, ProviderError> {
        if let Some(id) = preferred_id {
            if self.is_visible_to(id, app_id) {
                return Ok(id.to_string());
            }
            tracing::warn!(
                pinned_provider = id,
                app_id,
                "pinned provider is not registered or not visible to this app;                  falling back to this app's own providers"
            );
        }

        if let Some(id) = app_default {
            if self.is_visible_to(id, app_id) {
                return Ok(id.to_string());
            }
            tracing::warn!(
                default_provider = id,
                app_id,
                "app's default provider is not registered; falling back"
            );
        }

        let mut candidates: Vec<(bool, i32, String)> = self
            .providers
            .iter()
            .filter(|e| match &e.app_id {
                None => true,
                Some(owner) => owner == app_id,
            })
            // Known-broken providers are excluded when nothing was explicitly
            // requested; `disconnected` (never probed) is not, or the first
            // turn after startup would fail before any probe ran.
            .filter(|e| e.health.status != "unhealthy")
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
                user_message: "No providers are configured or reachable for this app.".into(),
            })
    }

    pub fn get_provider_id(&self, preferred_id: Option<&str>) -> Result<String, ProviderError> {
        if let Some(id) = preferred_id {
            if self.providers.contains_key(id) {
                return Ok(id.to_string());
            }
            // A session pinned to a provider that isn't registered — fall
            // through to the health-sorted fallback below, but say so. Silent
            // here is what let a bad provider stamp run every turn on a
            // *different* engine (e.g. the in-process `local`) with no signal.
            tracing::warn!(
                pinned_provider = id,
                "session's pinned provider is not registered; falling back to another provider"
            );
        }

        let mut candidates: Vec<(bool, i32, String)> = self
            .providers
            .iter()
            // Known-broken providers are excluded outright when nothing was
            // explicitly requested: running a turn against an endpoint the
            // daemon already knows is down wastes the attempt budget and, for
            // a failing failover, doubles the latency of the eventual error.
            // `disconnected` (never probed yet — e.g. at daemon startup) is
            // NOT excluded; with all providers fresh, the first turn would
            // otherwise fail with `NoHealthyProvider` before any probe ran.
            .filter(|e| e.health.status != "unhealthy")
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
                user_message: "No providers are configured or reachable.".into(),
            })
    }

    /// Passive circuit breaker — called by the agent loop after a
    /// transport-class failure (`ProviderError::is_transport_error`: connect
    /// failure, header timeout, or a mid-stream drop surfacing as
    /// `error_type == "request"`). Marks the provider `unhealthy` with the
    /// failure reason and starts a cooldown (`health_checked_at = now`):
    /// `check_all_health` won't re-probe it for `HEALTH_TTL_SECS`, and
    /// unpinned selection filters it out in the meantime. The next probe
    /// after the cooldown flips it back to healthy if it recovered.
    ///
    /// `AuthFailed`/`InsufficientCredits`/`ContextExceeded` deliberately do
    /// NOT mark a provider down — a bad key or empty billing account says
    /// nothing about connectivity, and the failed-over provider would likely
    /// fail the same way.
    pub fn mark_unhealthy(&self, provider_id: &str, reason: &str) {
        if let Some(mut entry) = self.providers.get_mut(provider_id) {
            entry.health = HealthStatus {
                status: "unhealthy".into(),
                latency_ms: None,
                error: Some(reason.into()),
            };
            entry.health_checked_at = Instant::now();
        }
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

        // Probe all due providers CONCURRENTLY — probing serially let a single
        // stalled endpoint (up to its own timeout, often 30s+) gate the rest,
        // and `GET /api/status` calls this synchronously on every request, so
        // a daemon restart (all providers "disconnected" → all due) could
        // block the first status poll for N×timeout seconds.
        let results: Vec<(String, HealthStatus)> = futures::future::join_all(
            due.into_iter().map(|(id, provider)| async move {
                let status = provider.check_health().await;
                (id, status)
            }),
        )
        .await;

        for (id, status) in results {
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

    /// Correct this provider's cached context length from ground truth.
    ///
    /// The only caller is the context-overflow path in the tool loop: when a
    /// provider rejects a request it reports its *own* view of the window
    /// (llama.cpp sends `n_ctx`), and that number outranks anything discovery
    /// guessed or a user typed into a profile. Without this, a provider whose
    /// configured `context_length` is too generous keeps budgeting against the
    /// wrong number and blows the window again on the very next turn.
    ///
    /// In-memory only — deliberately not written back to the DB profile, so a
    /// user's explicit setting is never silently rewritten under them; the
    /// correction lasts for this daemon's lifetime, which is what the valve
    /// needs. Ignores non-positive values.
    pub fn set_context_length(&self, provider_id: &str, context_length: i32) {
        if context_length <= 0 {
            return;
        }
        if let Some(mut entry) = self.providers.get_mut(provider_id) {
            entry.context_length = Some(context_length);
        }
    }

    /// Resolve model for a specific provider.
    pub fn resolve_model(&self, provider_id: &str, override_model: Option<&str>) -> String {
        if let Some(entry) = self.providers.get(provider_id) {
            entry.provider.resolve_model(override_model)
        } else {
            "unknown".to_string()
        }
    }

    /// See `Provider::supports_assistant_prefill`. `false` for an unknown
    /// provider id -- the safe default, same as the trait's own.
    pub fn supports_assistant_prefill(&self, provider_id: &str) -> bool {
        self.providers
            .get(provider_id)
            .map(|e| e.provider.supports_assistant_prefill())
            .unwrap_or(false)
    }

    /// See `Provider::supports_tools`. `true` for an unknown provider id,
    /// matching the trait default — assuming a provider *can* take tools is
    /// the recoverable guess (it errors visibly); assuming it can't would
    /// silently strip them from a provider that works fine.
    pub fn supports_tools(&self, provider_id: &str) -> bool {
        self.providers
            .get(provider_id)
            .map(|e| e.provider.supports_tools())
            .unwrap_or(true)
    }

    /// Call chat_completion on a specific provider.
    /// The concurrency gate for one provider, created on first use.
    ///
    /// Replaced when the configured limit changes, so editing `parallel_slots`
    /// takes effect without a daemon restart. Anything already holding a
    /// permit keeps it (its `Arc` outlives the map entry), so a limit change
    /// can briefly admit more than the new limit — acceptable for a rare,
    /// user-initiated event, and far better than ignoring the setting until
    /// restart.
    /// This provider's queue, created on first use at `concurrency`.
    ///
    /// An existing queue is reused even if `concurrency` has since changed, so
    /// a re-registration cannot strand callers already waiting on the old one.
    fn queue_for(&self, provider_id: &str, concurrency: u32) -> Arc<ProviderQueue> {
        self.queues
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(ProviderQueue::new(concurrency)))
            .clone()
    }

    /// Apply a provider's current concurrency to its live queue.
    ///
    /// Called after re-registration so editing `parallel_slots` takes effect
    /// without a restart. Adjusts in place rather than replacing the queue,
    /// which would strand anyone already waiting on it.
    pub async fn sync_queue_limit(&self, provider_id: &str) {
        let Some(concurrency) = self.providers.get(provider_id).map(|e| e.concurrency) else {
            return;
        };
        let queue = self.queue_for(provider_id, concurrency);
        queue.set_limit(concurrency).await;
    }

    /// A snapshot of one provider's queue, for `GET /api/providers`.
    pub async fn queue_stats(&self, provider_id: &str, app_id: &str) -> Option<QueueStats> {
        let q = self.queues.get(provider_id)?.clone();
        Some(QueueStats {
            concurrency: q.limit().await,
            in_flight: q.in_flight().await,
            queue_depth: q.queue_depth().await,
            my_queue_depth: q.queue_depth_for(app_id).await,
        })
    }

    /// `app_id` and `priority` drive fair admission: every caller funnels
    /// through here -- the tool loop, its retry/failover attempts, and the
    /// three fire-and-forget turn-end tasks -- so the queue is enforced in one
    /// place rather than relying on each of them to remember.
    #[allow(clippy::too_many_arguments)]
    pub async fn chat_completion(
        &self,
        provider_id: &str,
        messages: &[serde_json::Value],
        tools: Option<Vec<serde_json::Value>>,
        sampling: SamplingParams,
        model: Option<String>,
        id_slot: Option<i32>,
        app_id: &str,
        priority: Priority,
    ) -> Result<Pin<Box<dyn Stream<Item = Delta> + Send>>, ProviderError> {
        // Clone the provider's Arc out, drop the DashMap guard, then await —
        // a chat completion can run for minutes and must never hold the shard
        // lock (which would block health checks and other completions).
        let (provider, concurrency) = {
            let entry =
                self.providers
                    .get(provider_id)
                    .ok_or_else(|| ProviderError::NoHealthyProvider {
                        user_message: format!("Provider '{}' not found", provider_id),
                    })?;
            (entry.provider.clone(), entry.concurrency)
        };

        // Wait for a slot before sending anything. Every caller funnels
        // through here — the tool loop, its retry/failover attempts, and the
        // three fire-and-forget turn-end tasks — so the queue is enforced in
        // one place rather than relying on each of them to remember.
        let permit = self
            .queue_for(provider_id, concurrency)
            .acquire(app_id, priority)
            .await;

        let inner = provider
            .chat_completion(messages, tools, sampling, model, id_slot)
            .await?;
        Ok(Box::pin(PermitStream {
            inner,
            _permit: permit,
        }))
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
            app_id: None,
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
            app_id: None,
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
    /// All-unhealthy registry: unpinned selection must fail rather than run
    /// the turn against a provider the daemon already knows is down — this
    /// was the pre-#6 behavior (`NoHealthyProvider` only fired on an *empty*
    /// map).
    #[test]
    fn unpinned_selection_rejects_an_all_unhealthy_registry() {
        let router = ProviderRouter::default();
        router.register_openai(
            "down-a",
            ProviderConfig {
                fallback_priority: 1,
                ..Default::default()
            },
        );
        router.register_openai(
            "down-b",
            ProviderConfig {
                fallback_priority: 2,
                ..Default::default()
            },
        );
        router.set_health_for_test("down-a", "unhealthy");
        router.set_health_for_test("down-b", "unhealthy");
        assert!(router.get_provider_id(None).is_err());
    }

    /// A provider marked down by the circuit breaker (or a probe) is skipped
    /// on failover re-resolution, even when it outranks the healthy backup by
    /// `fallback_priority` — this is what lets the loop actually fail over
    /// instead of re-picking the same broken endpoint.
    #[test]
    fn an_unhealthy_provider_is_filtered_out_of_unpinned_selection() {
        let router = ProviderRouter::default();
        router.register_openai(
            "circuit-broken",
            ProviderConfig {
                fallback_priority: 1,
                ..Default::default()
            },
        );
        router.register_openai(
            "healthy-backup",
            ProviderConfig {
                fallback_priority: 50,
                ..Default::default()
            },
        );
        router.mark_unhealthy("circuit-broken", "connection reset");

        assert_eq!(router.get_provider_id(None).unwrap(), "healthy-backup");
    }

    /// `disconnected` (never probed — e.g. right after daemon startup) must
    /// stay selectable: with every provider fresh, an unpinned first turn
    /// would otherwise fail before any health probe ran.
    #[test]
    fn disconnected_providers_remain_selectable_unpinned() {
        let router = ProviderRouter::default();
        router.register_openai("fresh", ProviderConfig::default());
        assert_eq!(router.get_provider_id(None).unwrap(), "fresh");
    }

    /// The circuit breaker must not be tripped by non-connectivity errors:
    /// a pinned provider with a bad key stays selectable (the explicit
    /// choice must still win and fail loudly as itself).
    #[test]
    fn a_pinned_provider_is_never_filtered_out() {
        let router = ProviderRouter::default();
        router.register_openai(
            "pinned",
            ProviderConfig {
                fallback_priority: 1,
                ..Default::default()
            },
        );
        router.register_openai(
            "healthy",
            ProviderConfig {
                fallback_priority: 100,
                ..Default::default()
            },
        );
        router.set_health_for_test("pinned", "unhealthy");
        router.set_health_for_test("healthy", "healthy");

        assert_eq!(
            router.get_provider_id(Some("pinned")).unwrap(),
            "pinned",
            "an explicit choice must win even when known to be down"
        );
    }

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

    // -----------------------------------------------------------------
    // Per-app resolution
    //
    // The V1 behaviour these replace: selection was a daemon-global sort, so
    // "use this one" could only be expressed by demoting every other app's
    // rows. The property that matters most here is that a provider owned by
    // another app is indistinguishable from one that does not exist -- anything
    // softer would route one app's traffic through another's credentials.
    // -----------------------------------------------------------------

    fn owned(router: &ProviderRouter, id: &str, app: Option<&str>, priority: i32) {
        router.register_openai_owned(
            id,
            ProviderConfig {
                fallback_priority: priority,
                ..Default::default()
            },
            app.map(String::from),
        );
    }

    #[test]
    fn an_app_never_resolves_to_another_apps_provider() {
        let router = ProviderRouter::default();
        owned(&router, "theirs", Some("app-b"), 1);

        // Not even as a fallback, and not even when it is the only provider
        // registered: routing here would spend another app's credentials.
        assert!(router
            .resolve_provider_for_app("app-a", None, None)
            .is_err());
        assert!(router
            .resolve_provider_for_app("app-a", Some("theirs"), None)
            .is_err());
        assert!(router
            .resolve_provider_for_app("app-a", None, Some("theirs"))
            .is_err());
    }

    #[test]
    fn the_shared_pool_is_visible_to_everyone() {
        let router = ProviderRouter::default();
        owned(&router, "shared", None, 1);

        for app in ["app-a", "app-b"] {
            assert_eq!(
                router.resolve_provider_for_app(app, None, None).unwrap(),
                "shared"
            );
        }
    }

    #[test]
    fn each_apps_default_wins_for_that_app_only() {
        // Two apps, two defaults, one registry -- and neither disturbs the
        // other. In V1 the second app's activation would have demoted the
        // first's provider to priority 100.
        let router = ProviderRouter::default();
        owned(&router, "for-a", Some("app-a"), 50);
        owned(&router, "for-b", Some("app-b"), 50);
        owned(&router, "shared", None, 1);

        assert_eq!(
            router
                .resolve_provider_for_app("app-a", None, Some("for-a"))
                .unwrap(),
            "for-a"
        );
        assert_eq!(
            router
                .resolve_provider_for_app("app-b", None, Some("for-b"))
                .unwrap(),
            "for-b"
        );
    }

    #[test]
    fn an_explicit_pin_beats_the_app_default() {
        let router = ProviderRouter::default();
        owned(&router, "pinned", Some("app-a"), 100);
        owned(&router, "default", Some("app-a"), 1);

        assert_eq!(
            router
                .resolve_provider_for_app("app-a", Some("pinned"), Some("default"))
                .unwrap(),
            "pinned"
        );
    }

    #[test]
    fn a_pinned_provider_is_used_even_when_unhealthy() {
        // Carried over from V1's `get_provider_id`: a provider the user chose
        // must fail loudly as itself rather than silently substituting another.
        let router = ProviderRouter::default();
        owned(&router, "pinned", Some("app-a"), 100);
        owned(&router, "other", Some("app-a"), 1);
        router.set_health_for_test("pinned", "unhealthy");

        assert_eq!(
            router
                .resolve_provider_for_app("app-a", Some("pinned"), None)
                .unwrap(),
            "pinned"
        );
    }

    #[test]
    fn a_stale_app_default_falls_back_within_the_app() {
        // The default names a provider that has since been deleted.
        let router = ProviderRouter::default();
        owned(&router, "still-here", Some("app-a"), 1);

        assert_eq!(
            router
                .resolve_provider_for_app("app-a", None, Some("deleted-long-ago"))
                .unwrap(),
            "still-here"
        );
    }

    #[test]
    fn an_apps_own_provider_is_preferred_over_shared_by_priority_only() {
        let router = ProviderRouter::default();
        owned(&router, "shared", None, 1);
        owned(&router, "mine", Some("app-a"), 50);

        // With no default set, ordinary priority applies across the visible
        // set -- shared is not special-cased in either direction.
        assert_eq!(
            router.resolve_provider_for_app("app-a", None, None).unwrap(),
            "shared"
        );
        // ...and naming it as the app default is how "prefer mine" is said.
        assert_eq!(
            router
                .resolve_provider_for_app("app-a", None, Some("mine"))
                .unwrap(),
            "mine"
        );
    }

    #[test]
    fn unhealthy_providers_are_skipped_but_disconnected_ones_are_not() {
        let router = ProviderRouter::default();
        owned(&router, "down", Some("app-a"), 1);
        owned(&router, "fresh", Some("app-a"), 2);
        router.set_health_for_test("down", "unhealthy");

        // `fresh` is "disconnected" (never probed) -- excluding it too would
        // make the first turn after startup fail before any probe ran.
        assert_eq!(
            router.resolve_provider_for_app("app-a", None, None).unwrap(),
            "fresh"
        );
    }

    #[test]
    fn visibility_is_the_same_question_resolution_asks() {
        let router = ProviderRouter::default();
        owned(&router, "mine", Some("app-a"), 1);
        owned(&router, "shared", None, 1);

        assert!(router.is_visible_to("mine", "app-a"));
        assert!(!router.is_visible_to("mine", "app-b"));
        assert!(router.is_visible_to("shared", "app-a"));
        assert!(router.is_visible_to("shared", "app-b"));
        assert!(!router.is_visible_to("never-registered", "app-a"));
    }

    #[test]
    fn an_empty_registry_still_errors() {
        let router = ProviderRouter::default();
        assert!(router.get_provider_id(None).is_err());
        assert!(router.get_provider_id(Some("anything")).is_err());
    }

    /// A self-hosted endpoint is one request at a time unless its operator
    /// says otherwise. Getting this wrong is what produced "Provider sent no
    /// response headers within 30s": a second call queued behind the first on
    /// a single-slot llama-server, received nothing, and timed out — while the
    /// abandoned request kept generating.
    #[test]
    fn self_hosted_dialects_default_to_one_slot_hosted_ones_do_not() {
        for dialect in ["custom_openai", "ollama", "local"] {
            assert_eq!(default_concurrency(dialect), 1, "{dialect}");
        }
        for dialect in ["openai", "openrouter", "anthropic"] {
            assert!(default_concurrency(dialect) > 1, "{dialect}");
        }
    }

    /// `parallel_slots` finally means what its name says. It used to produce
    /// only an advisory `id_slot` hint on the request body — a note to the
    /// server about which slot to use, with nothing actually limiting how many
    /// requests were in flight.
    #[test]
    fn parallel_slots_overrides_the_dialect_default() {
        let router = ProviderRouter::default();
        router.register_openai(
            "many",
            ProviderConfig {
                provider_type: "custom_openai".into(),
                parallel_slots: Some(4),
                ..Default::default()
            },
        );
        router.register_openai(
            "one",
            ProviderConfig {
                provider_type: "custom_openai".into(),
                ..Default::default()
            },
        );
        assert_eq!(
            futures::executor::block_on(router.queue_for("many", 4).limit()),
            4
        );
        assert_eq!(
            futures::executor::block_on(router.queue_for("one", 1).limit()),
            1
        );
    }

    /// The queue must survive `register_from_row`, which re-inserts the whole
    /// `ProviderEntry` on every provider PATCH (activating a profile, changing
    /// a model). A queue living on the entry would be swapped out from under
    /// everything waiting on it — which is why the map is keyed separately.
    #[tokio::test]
    async fn re_registering_a_provider_keeps_the_same_queue() {
        let router = ProviderRouter::default();
        let cfg = || ProviderConfig {
            provider_type: "custom_openai".into(),
            ..Default::default()
        };
        router.register_openai("p", cfg());
        let first = router.queue_for("p", 1);
        let permit = first.acquire("app", Priority::Interactive).await;

        router.register_openai("p", cfg());
        let second = router.queue_for("p", 1);
        assert!(
            Arc::ptr_eq(&first, &second),
            "re-registration must not replace the queue"
        );
        assert_eq!(
            second.in_flight().await,
            1,
            "the in-flight call's slot must still be held"
        );
        drop(permit);
    }

    /// Editing `parallel_slots` takes effect without a daemon restart — and,
    /// unlike V1's swap-the-semaphore approach, without stranding waiters.
    #[tokio::test]
    async fn changing_the_limit_adjusts_the_queue_in_place() {
        let router = ProviderRouter::default();
        let a = router.queue_for("p", 1);
        assert_eq!(a.limit().await, 1);

        a.set_limit(3).await;
        let b = router.queue_for("p", 1);
        assert!(
            Arc::ptr_eq(&a, &b),
            "the queue is adjusted, not replaced"
        );
        assert_eq!(b.limit().await, 3);
    }

    /// The permit has to outlive the whole response, not just the request:
    /// releasing it when headers arrive would free the slot while the server
    /// is still generating — exactly the overlap the queue exists to prevent.
    #[tokio::test]
    async fn a_permit_is_held_until_its_stream_is_dropped() {
        let queue = Arc::new(ProviderQueue::new(1));
        let permit = queue.acquire("app", Priority::Interactive).await;
        let inner: Pin<Box<dyn Stream<Item = Delta> + Send>> =
            Box::pin(futures::stream::iter(Vec::<Delta>::new()));
        let stream = PermitStream {
            inner,
            _permit: permit,
        };
        assert_eq!(queue.in_flight().await, 1);

        drop(stream);
        // `QueuePermit::drop` releases on a spawned task, so the slot frees
        // shortly after rather than synchronously.
        for _ in 0..50 {
            if queue.in_flight().await == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("dropping the stream must release the slot");
    }
}
