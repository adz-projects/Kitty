use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_embedding_dim")]
    pub embedding_dim: usize,
    #[serde(default)]
    pub embedding: EmbeddingConfig,
    #[serde(default)]
    pub novelty: NoveltyConfig,
    #[serde(default)]
    pub dpp: DppConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingConfig {
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,
    #[serde(default = "default_ollama_model")]
    pub ollama_model: String,
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u64,
    #[serde(default = "default_probe_interval_s")]
    pub probe_interval_s: u64,
    #[serde(default = "default_cache_size")]
    pub cache_size: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NoveltyConfig {
    #[serde(default = "default_n_hash_tables")]
    pub n_hash_tables: usize,
    #[serde(default = "default_hash_size")]
    pub hash_size: usize,
    #[serde(default = "default_min_count_pessimistic")]
    pub min_count_pessimistic: bool,
    #[serde(default = "default_lambda")]
    pub default_lambda: f64,
    #[serde(default = "default_lambda_floor")]
    pub lambda_floor: f64,
    #[serde(default = "default_ucb_multiplier")]
    pub ucb_multiplier: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DppConfig {
    #[serde(default = "default_diversity_weight")]
    pub default_diversity_weight: f64,
    #[serde(default = "default_max_hints")]
    pub max_hints: usize,
    #[serde(default = "default_token_budget")]
    pub token_budget: usize,
    #[serde(default = "default_epsilon")]
    pub epsilon: f64,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            ollama_url: default_ollama_url(),
            ollama_model: default_ollama_model(),
            timeout_s: default_timeout_s(),
            probe_interval_s: default_probe_interval_s(),
            cache_size: default_cache_size(),
        }
    }
}

impl Default for NoveltyConfig {
    fn default() -> Self {
        Self {
            n_hash_tables: default_n_hash_tables(),
            hash_size: default_hash_size(),
            min_count_pessimistic: default_min_count_pessimistic(),
            default_lambda: default_lambda(),
            lambda_floor: default_lambda_floor(),
            ucb_multiplier: default_ucb_multiplier(),
        }
    }
}

impl Default for DppConfig {
    fn default() -> Self {
        Self {
            default_diversity_weight: default_diversity_weight(),
            max_hints: default_max_hints(),
            token_budget: default_token_budget(),
            epsilon: default_epsilon(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            embedding_dim: default_embedding_dim(),
            embedding: EmbeddingConfig {
                ollama_url: default_ollama_url(),
                ollama_model: default_ollama_model(),
                timeout_s: default_timeout_s(),
                probe_interval_s: default_probe_interval_s(),
                cache_size: default_cache_size(),
            },
            novelty: NoveltyConfig {
                n_hash_tables: default_n_hash_tables(),
                hash_size: default_hash_size(),
                min_count_pessimistic: default_min_count_pessimistic(),
                default_lambda: default_lambda(),
                lambda_floor: default_lambda_floor(),
                ucb_multiplier: default_ucb_multiplier(),
            },
            dpp: DppConfig {
                default_diversity_weight: default_diversity_weight(),
                max_hints: default_max_hints(),
                token_budget: default_token_budget(),
                epsilon: default_epsilon(),
            },
        }
    }
}

impl Config {
    pub fn from_json(s: &str) -> crate::error::Result<Self> {
        serde_json::from_str(s)
            .map_err(|e| crate::error::PathwayError::Config(e.to_string()))
    }
}

fn default_embedding_dim() -> usize {
    384
}
fn default_ollama_url() -> String {
    "http://localhost:11434".into()
}
fn default_ollama_model() -> String {
    "qwen3-embedding:0.6b".into()
}
fn default_timeout_s() -> u64 {
    12
}
fn default_probe_interval_s() -> u64 {
    60
}
fn default_cache_size() -> usize {
    256
}
fn default_n_hash_tables() -> usize {
    3
}
fn default_hash_size() -> usize {
    2048
}
fn default_min_count_pessimistic() -> bool {
    true
}
fn default_lambda() -> f64 {
    0.5
}
fn default_lambda_floor() -> f64 {
    0.05
}
fn default_ucb_multiplier() -> f64 {
    0.15
}
fn default_diversity_weight() -> f64 {
    1.0
}
fn default_max_hints() -> usize {
    5
}
fn default_token_budget() -> usize {
    200
}
fn default_epsilon() -> f64 {
    1e-7
}
