use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Top-level config: merged from defaults + user overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BigTinyConfig {
    #[serde(default)]
    pub fallback: FallbackConfig,
    #[serde(default)]
    pub token_management: TokenManagementConfig,
    #[serde(default)]
    pub summarizer: SummarizerConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub hitl: HITLConfig,
    #[serde(default)]
    pub recipes: RecipesConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub cache: CacheConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProviderConfig {
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,
    #[serde(default)]
    pub fallback_priority: i32,
    pub model: String,
    /// The `-np`/`--parallel` slot count this provider's own server was
    /// started with, when it's a llama-server-compatible endpoint that
    /// supports pinning a session to a specific KV-cache slot via `id_slot`
    /// (see `prompt_determinism.md`). `None` (the default — most providers,
    /// including Ollama and anything not deliberately configured for this)
    /// means: never send `id_slot` at all. Per-provider rather than a single
    /// daemon-wide setting because different remote llama-server instances
    /// are commonly started with different `--parallel` values, and a
    /// client-side slot count that doesn't match the real server-side one
    /// silently thrashes the KV cache instead of pinning it.
    #[serde(default)]
    pub parallel_slots: Option<u32>,
    /// Sampling overrides for this provider. All `None` by default — serde
    /// silently drops unknown fields on this struct, which is what let these
    /// settings round-trip through Kitty's UI for months without ever
    /// reaching the wire (see `provider::sampling` for the model-aware
    /// defaults applied when a field is left unset on a self-hosted
    /// provider).
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    /// llama.cpp/Ollama extension; dropped on the wire for hosted
    /// OpenAI-compatible endpoints and unsupported by Anthropic.
    #[serde(default)]
    pub top_k: Option<i32>,
    /// llama.cpp/Ollama extension; dropped on the wire for hosted
    /// OpenAI-compatible endpoints and unsupported by Anthropic.
    #[serde(default)]
    pub min_p: Option<f64>,
    /// Repetition control. llama-server's own default is 0.0 (disabled) —
    /// quantized local models (observed: Qwen3.6 27B via llama-server) can
    /// stream an unbounded repetition loop without this set. See
    /// `provider::sampling::defaults_for`.
    #[serde(default)]
    pub presence_penalty: Option<f64>,
    #[serde(default)]
    pub frequency_penalty: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<i32>,
    /// Per-provider override for BigTiny's context-window budgeting
    /// (`agent::loop_`'s compaction threshold calc), overriding the
    /// daemon-wide `token_management.max_context_tokens` for sessions on
    /// this provider.
    #[serde(default)]
    pub context_length: Option<i32>,
    /// Per-provider "response timeout" — seconds of *idle* time allowed on an
    /// SSE stream before the daemon treats the provider as stuck and aborts
    /// the turn with a transient error (default 300s). Only gates on bytes
    /// not arriving; a long, actively-streaming turn is never capped.
    /// Mirrored out of the transport `config` JSON blob as `idle_timeout_secs`
    /// (see `ProviderRouter::register_from_row`), and resolved to a
    /// `Duration` by `ProviderConfig::idle_timeout` (invalid/missing → 300s).
    #[serde(default)]
    pub idle_timeout_secs: Option<f64>,
}

impl ProviderConfig {
    /// Resolve the per-provider SSE idle-read timeout (see
    /// `idle_timeout_secs`). A missing, zero, negative, or non-finite value
    /// falls back to the 300s default so a malformed blob can never produce a
    /// zero/instant timeout that kills otherwise-healthy turns.
    pub fn idle_timeout(&self) -> Duration {
        match self.idle_timeout_secs {
            Some(s) if s.is_finite() && s > 0.0 => Duration::from_secs_f64(s),
            _ => Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FallbackConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_retry_delay_ms")]
    pub retry_delay_ms: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            retry_delay_ms: default_retry_delay_ms(),
            max_retries: default_max_retries(),
        }
    }
}

fn default_retry_delay_ms() -> u64 {
    1000
}

fn default_max_retries() -> u32 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenManagementConfig {
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: i32,
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: f64,
    #[serde(default = "default_compaction_target_ratio")]
    pub compaction_target_ratio: f64,
    #[serde(default = "default_min_compaction_tokens")]
    pub min_compaction_tokens: i32,
    #[serde(default = "default_max_live_tail_tokens")]
    pub max_live_tail_tokens: i32,
    #[serde(default = "default_message_mask_head_lines")]
    pub message_mask_head_lines: i32,
    #[serde(default = "default_message_mask_tail_lines")]
    pub message_mask_tail_lines: i32,
    #[serde(default = "default_tool_mask_head")]
    pub tool_mask_head: i32,
    #[serde(default = "default_tool_mask_tail")]
    pub tool_mask_tail: i32,
}

fn default_max_context_tokens() -> i32 {
    64000
}
fn default_compaction_threshold() -> f64 {
    0.6
}
fn default_compaction_target_ratio() -> f64 {
    0.4
}
fn default_min_compaction_tokens() -> i32 {
    16000
}
fn default_max_live_tail_tokens() -> i32 {
    24000
}
fn default_message_mask_head_lines() -> i32 {
    10
}
fn default_message_mask_tail_lines() -> i32 {
    10
}
fn default_tool_mask_head() -> i32 {
    400
}
fn default_tool_mask_tail() -> i32 {
    400
}

impl Default for TokenManagementConfig {
    fn default() -> Self {
        Self {
            max_context_tokens: default_max_context_tokens(),
            compaction_threshold: default_compaction_threshold(),
            compaction_target_ratio: default_compaction_target_ratio(),
            min_compaction_tokens: default_min_compaction_tokens(),
            max_live_tail_tokens: default_max_live_tail_tokens(),
            message_mask_head_lines: default_message_mask_head_lines(),
            message_mask_tail_lines: default_message_mask_tail_lines(),
            tool_mask_head: default_tool_mask_head(),
            tool_mask_tail: default_tool_mask_tail(),
        }
    }
}

impl TokenManagementConfig {
    /// Clamp the masking thresholds to `>= 0` at load time. A negative value
    /// in a user-supplied YAML used to become a huge `usize` index at the
    /// `as usize` conversion sites in `agent::compaction` (masking tool
    /// output / fenced code blocks), panicking on an out-of-bounds slice.
    /// Line counts can't meaningfully be negative, so clamping is the correct
    /// normalization rather than rejecting the config.
    pub fn sanitize(&mut self) {
        self.message_mask_head_lines = self.message_mask_head_lines.max(0);
        self.message_mask_tail_lines = self.message_mask_tail_lines.max(0);
        self.tool_mask_head = self.tool_mask_head.max(0);
        self.tool_mask_tail = self.tool_mask_tail.max(0);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SummarizerConfig {
    #[serde(default = "default_summarizer_enabled")]
    pub enabled: bool,
    #[serde(default = "default_summarizer_model")]
    pub model: String,
    #[serde(default = "default_summarizer_base_url")]
    pub base_url: String,
    #[serde(default = "default_summarizer_keep_alive")]
    pub keep_alive: String,
    #[serde(default = "default_summarizer_temperature")]
    pub temperature: f64,
    #[serde(default = "default_summarizer_timeout_s")]
    pub timeout_s: f64,
    #[serde(default = "default_reserve_exchanges")]
    pub reserve_exchanges: i32,
    #[serde(default = "default_max_slot_items")]
    pub max_slot_items: i32,
}

fn default_summarizer_enabled() -> bool {
    true
}
fn default_summarizer_model() -> String {
    "qwen3.5:0.8b".into()
}
fn default_summarizer_base_url() -> String {
    "http://127.0.0.1:11434".into()
}
fn default_summarizer_keep_alive() -> String {
    "5m".into()
}
fn default_summarizer_temperature() -> f64 {
    0.1
}
fn default_summarizer_timeout_s() -> f64 {
    30.0
}
fn default_reserve_exchanges() -> i32 {
    3
}
fn default_max_slot_items() -> i32 {
    20
}

impl Default for SummarizerConfig {
    fn default() -> Self {
        Self {
            enabled: default_summarizer_enabled(),
            model: default_summarizer_model(),
            base_url: default_summarizer_base_url(),
            keep_alive: default_summarizer_keep_alive(),
            temperature: default_summarizer_temperature(),
            timeout_s: default_summarizer_timeout_s(),
            reserve_exchanges: default_reserve_exchanges(),
            max_slot_items: default_max_slot_items(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentConfig {
    #[serde(default = "default_max_concurrent_tool_calls")]
    pub max_concurrent_tool_calls: i32,
    /// Seconds to keep a turn running after its SSE receiver disconnects
    /// before cancelling it, so a client that reconnects promptly (a mobile
    /// network handoff, an app briefly backgrounded) doesn't lose in-flight
    /// work — while a genuinely abandoned turn still stops burning tokens
    /// against a paid provider rather than running unobserved forever. See
    /// `Agent::run_turn`'s disconnect watcher.
    #[serde(default = "default_disconnect_grace_secs")]
    pub disconnect_grace_secs: u64,
    /// Governs what `agent::sandbox::check_containment` does when a tool
    /// call's arguments contain *no* path candidates it recognizes (an
    /// unrecognized key name, or a genuinely non-filesystem tool). Default
    /// `false` preserves the historical fail-open behavior documented on
    /// `check_containment` — appropriate for a single-user desktop, where an
    /// escalation-to-approval on every unrecognized call would be pure
    /// friction. Set `true` when the daemon's data root is itself the
    /// security boundary (an app sandbox, e.g. Android) and false positives
    /// from an incomplete `extract_candidate_paths` key list are the safer
    /// failure mode than a silent bypass.
    #[serde(default)]
    pub sandbox_strict: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_concurrent_tool_calls: default_max_concurrent_tool_calls(),
            disconnect_grace_secs: default_disconnect_grace_secs(),
            sandbox_strict: false,
        }
    }
}

fn default_max_concurrent_tool_calls() -> i32 {
    5
}

fn default_disconnect_grace_secs() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HITLConfig {
    #[serde(default = "default_hitl_policy")]
    pub default_policy: String,
    #[serde(default)]
    pub always_allow_patterns: Vec<String>,
    #[serde(default = "default_auto_reject_patterns")]
    pub auto_reject_patterns: Vec<String>,
}

fn default_hitl_policy() -> String {
    "always_ask".into()
}

fn default_auto_reject_patterns() -> Vec<String> {
    vec![
        "rm -rf /".into(),
        "chmod 777".into(),
        "dd if=".into(),
        "mkfs".into(),
    ]
}

impl Default for HITLConfig {
    fn default() -> Self {
        Self {
            default_policy: default_hitl_policy(),
            always_allow_patterns: vec![],
            auto_reject_patterns: default_auto_reject_patterns(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecipesConfig {
    #[serde(default = "default_recipes_directory")]
    pub directory: String,
}

impl Default for RecipesConfig {
    fn default() -> Self {
        Self {
            directory: default_recipes_directory(),
        }
    }
}

pub fn default_recipes_directory() -> String {
    "~/.bigtiny/recipes".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchedulerConfig {
    #[serde(default = "default_scheduler_enabled")]
    pub enabled: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: default_scheduler_enabled(),
        }
    }
}

fn default_scheduler_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default)]
    pub json_format: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            json_format: false,
        }
    }
}

fn default_log_level() -> String {
    "info".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerConfig {
    #[serde(default = "default_server_host")]
    pub host: String,
    #[serde(default = "default_server_port")]
    pub port: u16,
    #[serde(default)]
    pub reload: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_server_host(),
            port: default_server_port(),
            reload: false,
        }
    }
}

fn default_server_host() -> String {
    "127.0.0.1".into()
}

fn default_server_port() -> u16 {
    8080
}

/// Prompt-cache-affecting behavior — see `prompt_determinism.md` at the repo
/// root. Slot pinning itself (`id_slot`) moved to a per-provider setting
/// (`ProviderConfig::parallel_slots`) since different remote llama-server
/// instances are commonly started with different `--parallel` values — a
/// single daemon-wide slot count couldn't be correct for more than one of
/// them at once.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CacheConfig {
    #[serde(default = "default_sort_tools")]
    pub sort_tools: bool,
    #[serde(default)]
    pub anthropic_cache_control: bool,
}

fn default_sort_tools() -> bool {
    true
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            sort_tools: default_sort_tools(),
            anthropic_cache_control: false,
        }
    }
}

impl BigTinyConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config {}: {}", path.display(), e))?;

let mut config: Self = serde_yaml::from_str(&contents)
    .map_err(|e| format!("Failed to parse config {}: {}", path.display(), e))?;

// Clamp raw user input (see `TokenManagementConfig::sanitize`) so a
// negative masking threshold can never reach the `as usize` use sites.
config.token_management.sanitize();

Ok(config)
    }

    /// Merge non-default values from `other` into `self`.
    pub fn merge_non_default(&mut self, other: &Self) {
        if other.fallback != FallbackConfig::default() {
            self.fallback = other.fallback.clone();
        }
        if other.token_management != TokenManagementConfig::default() {
            self.token_management = other.token_management.clone();
        }
        if other.summarizer != SummarizerConfig::default() {
            self.summarizer = other.summarizer.clone();
        }
        if other.agent != AgentConfig::default() {
            self.agent = other.agent.clone();
        }
        if other.hitl != HITLConfig::default() {
            self.hitl = other.hitl.clone();
        }
        if other.recipes != RecipesConfig::default() {
            self.recipes = other.recipes.clone();
        }
        if other.scheduler != SchedulerConfig::default() {
            self.scheduler = other.scheduler.clone();
        }
        if other.logging != LoggingConfig::default() {
            self.logging = other.logging.clone();
        }
        if other.server != ServerConfig::default() {
            self.server = other.server.clone();
        }
        if !other.providers.is_empty() {
            self.providers = other.providers.clone();
        }
        if other.cache != CacheConfig::default() {
            self.cache = other.cache.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_yaml(content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bigtiny_cfg_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join(format!("config_{}.yaml", uuid::Uuid::new_v4()));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", content).unwrap();
        path
    }

    #[test]
    fn test_default_config() {
        let cfg = BigTinyConfig::default();
        assert_eq!(cfg.logging.level, "info");
        assert_eq!(cfg.agent.max_concurrent_tool_calls, 5);
        assert_eq!(cfg.hitl.default_policy, "always_ask");
        assert_eq!(cfg.token_management.max_context_tokens, 64000);
        assert_eq!(cfg.summarizer.reserve_exchanges, 3);
        assert!(cfg.providers.is_empty());
    }

    #[test]
    fn test_load_yaml() {
        let path = temp_yaml(
            r#"logging:
  level: "debug"
hitl:
  default_policy: "require_confirmation"
server:
  host: "127.0.0.1"
  port: 8080
"#,
        );
        let cfg = BigTinyConfig::load(&path).unwrap();
        assert_eq!(cfg.logging.level, "debug");
        assert_eq!(cfg.hitl.default_policy, "require_confirmation");
        assert_eq!(cfg.server.port, 8080);
    }

    #[test]
    fn test_merge_non_default() {
        let mut base = BigTinyConfig::default();
        base.logging.level = "info".into();

        let other = BigTinyConfig {
            agent: AgentConfig {
                max_concurrent_tool_calls: 10,
                ..Default::default()
            },
            ..Default::default()
        };

        base.merge_non_default(&other);
        assert_eq!(base.logging.level, "info"); // preserved
        assert_eq!(base.agent.max_concurrent_tool_calls, 10); // merged
    }

    #[test]
    fn test_load_providers() {
        let path = temp_yaml(
            r#"providers:
  - name: "openai"
    provider_type: "openai_compat"
    base_url: "https://api.openai.com"
    api_key: "sk-test123"
    fallback_priority: 1
    model: "gpt-4"
  - name: "anthropic"
    provider_type: "anthropic"
    base_url: "https://api.anthropic.com"
    api_key: "sk-ant456"
    fallback_priority: 2
    model: "claude-3-opus"
"#,
        );
        let cfg = BigTinyConfig::load(&path).unwrap();
        assert_eq!(cfg.providers.len(), 2);
        assert_eq!(cfg.providers[0].name, "openai");
        assert_eq!(cfg.providers[1].provider_type, "anthropic");
    }

    /// Regression: negative masking thresholds in the YAML must be clamped to
    /// 0 at load, never flow to the `as usize` slicing sites in compaction.
    #[test]
    fn test_load_clamps_negative_masking_thresholds() {
        let path = temp_yaml(
            "token_management:\n  message_mask_head_lines: -5\n  message_mask_tail_lines: -3\n  tool_mask_head: -1\n  tool_mask_tail: -2\n",
        );
        let cfg = BigTinyConfig::load(&path).unwrap();
        assert_eq!(cfg.token_management.message_mask_head_lines, 0);
        assert_eq!(cfg.token_management.message_mask_tail_lines, 0);
        assert_eq!(cfg.token_management.tool_mask_head, 0);
        assert_eq!(cfg.token_management.tool_mask_tail, 0);
    }

    /// WS2: the per-provider idle-read timeout resolves from `idle_timeout_secs`.
    #[test]
    fn test_idle_timeout_resolution() {
        // A valid seconds value becomes the corresponding Duration.
        let cfg = ProviderConfig {
            idle_timeout_secs: Some(120.0),
            ..Default::default()
        };
        assert_eq!(cfg.idle_timeout(), Duration::from_secs(120));

        // Missing (None) falls back to the 300s default.
        let cfg = ProviderConfig::default();
        assert_eq!(cfg.idle_timeout(), Duration::from_secs(300));

        // Invalid values (zero, negative, non-finite) also fall back to 300s.
        for invalid in [Some(0.0), Some(-5.0), Some(f64::NAN), Some(f64::INFINITY)] {
            let cfg = ProviderConfig {
                idle_timeout_secs: invalid,
                ..Default::default()
            };
            assert_eq!(cfg.idle_timeout(), Duration::from_secs(300));
        }
    }
}
