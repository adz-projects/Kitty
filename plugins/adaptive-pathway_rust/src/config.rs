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
    #[serde(default)]
    pub diffusion: DiffusionConfig,
    #[serde(default)]
    pub trajectory: TrajectoryConfig,
    #[serde(default)]
    pub recall: RecallConfig,
}

/// The query-relevance gate applied to belief selection every turn.
///
/// Raw embedding cosine has almost no dynamic range in this space (unrelated
/// beliefs still score ~0.8), so the gate works on **mean-centered** cosine:
/// the corpus mean of the candidate beliefs is subtracted from both the query
/// and each belief before cosine, which restores the range. A belief is
/// dropped when its centered relevance falls below the floor, so a turn
/// injects only beliefs genuinely aligned with the current message instead of
/// whatever carries the most standing weight. Pinned beliefs are always
/// exempt; identity beliefs use the gentler `min_cosine_identity`.
#[derive(Debug, Clone, Deserialize)]
pub struct RecallConfig {
    /// Centered-cosine floor for ordinary (context/conversation) beliefs. 0.0
    /// keeps only beliefs that, after discounting the direction every belief
    /// shares, still point the same way as the query. Raise toward ~0.05–0.1
    /// to inject more selectively.
    #[serde(default = "default_recall_min_cosine")]
    pub min_cosine: f64,
    /// Gentler floor for `Layer::Identity` beliefs: slow-moving profile facts
    /// are worth surfacing on a slightly looser match, but no longer wholly
    /// exempt from relevance (they used to inject unconditionally via the
    /// reserved slots, which is how a stale identity belief leaked into every
    /// turn).
    #[serde(default = "default_recall_min_cosine_identity")]
    pub min_cosine_identity: f64,
    /// Minimum centered-cosine gap that triggers the **per-query gap cut**:
    /// after the floor, the surviving candidates are sorted by centered
    /// relevance and, when the largest gap between adjacent scores clears this,
    /// the tail below it is dropped — keeping only the cluster genuinely
    /// relevant to this turn. Pinned and identity beliefs are exempt (same as
    /// the floor). Self-correcting per turn with no stored state. Set >1.0 to
    /// disable.
    #[serde(default = "default_recall_gap_min")]
    pub gap_min: f64,
    /// Smoothing of the running cut estimate (`recall_cut_ewma::<space>` in
    /// `app_settings`), updated once per injected turn from that turn's lowest
    /// kept centered score. Low = slow; the estimate is a property of the
    /// embedding space, not any one query.
    #[serde(default = "default_recall_ewma_alpha")]
    pub ewma_alpha: f64,
    /// How far below the running EWMA cut a turn may still admit a belief. The
    /// operational floor each turn is `max(min_cosine, ewma − slack)`; never
    /// lowers the hard `min_cosine`.
    #[serde(default = "default_recall_ewma_slack")]
    pub ewma_slack: f64,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            min_cosine: default_recall_min_cosine(),
            min_cosine_identity: default_recall_min_cosine_identity(),
            gap_min: default_recall_gap_min(),
            ewma_alpha: default_recall_ewma_alpha(),
            ewma_slack: default_recall_ewma_slack(),
        }
    }
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
    // `max_hints` (5) and `token_budget` (200) used to live here. Both were
    // defined, defaulted, and read by nothing: the real slot count is
    // `recall::MAX_BELIEFS` (6) and the real budget is
    // `recall::RECALL_MAX_TOKENS` (350), which is what actually caps the
    // rendered block. Removed rather than wired up so there is exactly one
    // source of truth per knob -- serde ignores unknown keys, so an existing
    // config JSON that still sets them keeps loading.
    #[serde(default = "default_epsilon")]
    pub epsilon: f64,
}

/// Spreading-activation tunables for `vector::spread::diffuse_activation`,
/// run between blended-score computation and DPP selection in
/// `recall::select_beliefs_relevant`. See that module's doc comment for the
/// mechanism. Defaults are deliberately conservative: a moderate
/// `edge_threshold` (only genuinely similar beliefs propagate energy) and a
/// modest `boost_weight` (diffusion nudges DPP's input scores, it does not
/// dominate them) — this only reweights ranking among candidates already in
/// play, so a wrong default degrades gracefully rather than corrupting
/// selection outright.
#[derive(Debug, Clone, Deserialize)]
pub struct DiffusionConfig {
    #[serde(default = "default_diffusion_enabled")]
    pub enabled: bool,
    /// Per-hop energy decay multiplier.
    #[serde(default = "default_diffusion_gamma")]
    pub gamma: f64,
    /// Number of diffusion hops from each anchor.
    #[serde(default = "default_diffusion_hops")]
    pub hops: usize,
    /// Minimum cosine similarity an edge must clear to carry energy.
    #[serde(default = "default_diffusion_edge_threshold")]
    pub edge_threshold: f64,
    /// Multiplier applied to activation when boosting a candidate's DPP
    /// input score: `score * (1 + boost_weight * activation)`.
    #[serde(default = "default_diffusion_boost_weight")]
    pub boost_weight: f64,
    /// Flat energy weight for co-occurrence edges -- beliefs whose
    /// observations share an `extract_and_record` batch (migration 006).
    /// Unlike cosine edges these carry no similarity term and ignore
    /// `edge_threshold`, since co-occurring facts are typically semantically
    /// distant; see `vector::spread::diffuse_activation`. Set to `0.0` to
    /// disable co-occurrence pull without touching cosine diffusion.
    #[serde(default = "default_diffusion_cooccurrence_weight")]
    pub cooccurrence_weight: f64,
}

impl Default for DiffusionConfig {
    fn default() -> Self {
        Self {
            enabled: default_diffusion_enabled(),
            gamma: default_diffusion_gamma(),
            hops: default_diffusion_hops(),
            edge_threshold: default_diffusion_edge_threshold(),
            boost_weight: default_diffusion_boost_weight(),
            cooccurrence_weight: default_diffusion_cooccurrence_weight(),
        }
    }
}

/// Momentum-extrapolated query embedding -- a cheap, non-ML stand-in for a
/// trained predictive (JEPA-style) recall model: given the previous and
/// current turn's query embeddings, extrapolate a "where this conversation
/// is heading" vector (`current + momentum·(current − previous)`,
/// renormalized) and use *that* for domain inference and belief cosine
/// ranking instead of the raw current-turn embedding. See
/// `engine::PathwayEngine::recall`'s trajectory step. Held in-memory only
/// (`PathwayEngine.trajectory_embeddings`) — losing it across a daemon
/// restart just means the very next turn falls back to the raw embedding,
/// which is fine for a soft ranking nudge.
#[derive(Debug, Clone, Deserialize)]
pub struct TrajectoryConfig {
    #[serde(default = "default_trajectory_enabled")]
    pub enabled: bool,
    /// How strongly the previous-turn delta is extrapolated forward.
    #[serde(default = "default_trajectory_momentum")]
    pub momentum: f64,
}

impl Default for TrajectoryConfig {
    fn default() -> Self {
        Self {
            enabled: default_trajectory_enabled(),
            momentum: default_trajectory_momentum(),
        }
    }
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
                epsilon: default_epsilon(),
            },
            diffusion: DiffusionConfig {
                enabled: default_diffusion_enabled(),
                gamma: default_diffusion_gamma(),
                hops: default_diffusion_hops(),
                edge_threshold: default_diffusion_edge_threshold(),
                boost_weight: default_diffusion_boost_weight(),
                cooccurrence_weight: default_diffusion_cooccurrence_weight(),
            },
            trajectory: TrajectoryConfig {
                enabled: default_trajectory_enabled(),
                momentum: default_trajectory_momentum(),
            },
            recall: RecallConfig::default(),
        }
    }
}

impl Config {
    pub fn from_json(s: &str) -> crate::error::Result<Self> {
        let cfg: Self = serde_json::from_str(s)
            .map_err(|e| crate::error::PathwayError::Config(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Rejects values the math downstream cannot survive (audit #122):
    /// `embedding_dim = 0` panics `hash_embed`/`project` (`rem_euclid(0)`,
    /// `i % dim`) and `novelty.hash_size = 0` panics the count-min sketch
    /// the same way — both on the in-process recall path, where a panic is
    /// a daemon crash, not a tool error. Better to refuse the config at
    /// load with a message that names the key.
    pub fn validate(&self) -> crate::error::Result<()> {
        if self.embedding_dim == 0 {
            return Err(crate::error::PathwayError::Config(
                "embedding_dim must be >= 1".into(),
            ));
        }
        if self.novelty.hash_size == 0 {
            return Err(crate::error::PathwayError::Config(
                "novelty.hash_size must be >= 1".into(),
            ));
        }
        if self.novelty.n_hash_tables == 0 {
            return Err(crate::error::PathwayError::Config(
                "novelty.n_hash_tables must be >= 1".into(),
            ));
        }
        Ok(())
    }
}

fn default_embedding_dim() -> usize {
    384
}
fn default_ollama_url() -> String {
    "http://localhost:11434".into()
}
/// Public so callers that need to tag data with "whatever model a fresh
/// default `Config` would use" (chiefly test fixtures constructing `Belief`
/// rows directly, and `store::beliefs::list_recall_candidates`'s embedding-
/// model filter's single source of truth) don't have to duplicate the
/// literal string.
pub const DEFAULT_EMBEDDING_MODEL: &str = "qwen3-embedding:0.6b";

/// Sentry label for vectors produced by the LEXICAL signed-hash fallback
/// embedder (Ollama unavailable/erroring). This space is incompatible with
/// the semantic embedder's — cosine across the two means nothing — so a
/// hash-space belief MUST be tagged with this instead of the configured
/// Ollama model, or it lands in the same recall/merge pool as genuine
/// semantic embeddings and compares garbage forever. Because
/// `list_recall_candidates` filters by the configured model, a correctly-
/// tagged hash-space belief simply won't be recalled — the safe outcome for
/// a vector we can't meaningfully compare.
pub const HASH_EMBED_MODEL: &str = "__lexical_hash__";

fn default_ollama_model() -> String {
    DEFAULT_EMBEDDING_MODEL.into()
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
fn default_epsilon() -> f64 {
    1e-7
}
fn default_diffusion_enabled() -> bool {
    true
}
fn default_diffusion_gamma() -> f64 {
    0.5
}
fn default_diffusion_hops() -> usize {
    1
}
fn default_diffusion_edge_threshold() -> f64 {
    0.55
}
fn default_diffusion_boost_weight() -> f64 {
    0.3
}

/// Deliberately below `gamma`-scaled cosine energy at the `edge_threshold`
/// floor (0.5 * 0.55 = 0.275): a co-occurrence edge should be a nudge that
/// breaks near-ties, not something that outranks genuine topical relevance.
fn default_diffusion_cooccurrence_weight() -> f64 {
    0.25
}
fn default_trajectory_enabled() -> bool {
    true
}
fn default_trajectory_momentum() -> f64 {
    // Lowered from 0.35: momentum extrapolates the ranking query off the
    // current message in the direction of recent drift, which pulls recall
    // toward whatever the last few turns were about. A gentler nudge still
    // tracks a genuine topic shift without overshooting a one-off question.
    0.15
}
fn default_recall_min_cosine() -> f64 {
    // Centered cosine (corpus mean removed), not raw cosine. Tuned with the
    // relevance-floor sweep (`memory_eval_relevance_floor`,
    // docs/memory-eval/README.md) on EmbeddingGemma 2: at 0.0 (the old value)
    // 38% of off-topic beliefs passed and no off-topic question injected
    // nothing; 0.10 keeps ~93% of expected beliefs at ~3% leakage, and 0.117
    // reaches zero leakage at 90%. The running EWMA can only raise it.
    0.10
}
fn default_recall_min_cosine_identity() -> f64 {
    // Gentler than the general floor, so a slow-moving profile belief can
    // surface on a looser match, but never a free pass: the old -0.05 let a
    // stale profile belief inject on essentially any turn via the reserved
    // identity slots. Half the general floor; the sweep has no identity cases,
    // so this is judgement, not measurement -- add identity cases to the eval
    // before tuning it.
    0.05
}
fn default_recall_gap_min() -> f64 {
    // Centered-cosine units; ~1/9 of the typical centered span — a real break
    // between the relevant cluster and the tail, above sampling jitter.
    0.08
}
fn default_recall_ewma_alpha() -> f64 {
    // Deliberately slow: the running cut estimate tracks the embedding space,
    // not any single query.
    0.1
}
fn default_recall_ewma_slack() -> f64 {
    // A turn may reach this far under the running EWMA cut before being
    // floored; never lowers the hard `min_cosine`.
    0.05
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_json_rejects_zero_embedding_dim() {
        // Audit #122: `embedding_dim = 0` panicked `hash_embed`/`project`
        // (`rem_euclid(0)`, `i % dim`) on the in-process recall path.
        let err = Config::from_json(r#"{"embedding_dim": 0}"#).unwrap_err();
        assert!(err.to_string().contains("embedding_dim"), "{err}");
    }

    #[test]
    fn from_json_rejects_zero_novelty_dims() {
        let err = Config::from_json(r#"{"novelty": {"hash_size": 0}}"#).unwrap_err();
        assert!(err.to_string().contains("hash_size"), "{err}");
        let err = Config::from_json(r#"{"novelty": {"n_hash_tables": 0}}"#).unwrap_err();
        assert!(err.to_string().contains("n_hash_tables"), "{err}");
    }

    #[test]
    fn from_json_accepts_defaults_and_valid_values() {
        assert!(Config::from_json("{}").is_ok());
        let cfg = Config::from_json(r#"{"embedding_dim": 768, "novelty": {"hash_size": 1024}}"#).unwrap();
        assert_eq!(cfg.embedding_dim, 768);
        assert_eq!(cfg.novelty.hash_size, 1024);
    }
}
