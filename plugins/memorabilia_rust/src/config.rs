//! Single source of truth for every tunable knob (project-plan.md §2).
//!
//! All configuration lives in one YAML file (`config.yaml`, path
//! configurable) loaded at startup via [`Config::load`]. There is no other
//! configuration surface: no env-var shims for the same knobs, no hardcoded
//! fallbacks in code paths. Per-field serde defaults keep older config files
//! loading after new defaults are added — unknown keys are ignored.
//!
//! Rule (claude.md, core principle 8): never a bare literal in a code path.
//! If a number shows up in logic it lives here, with a rationale.

use std::path::Path;

use serde::Deserialize;

use crate::error::{Error, Result};

/// Pinned semantic embedding model (plan §3.3). Vectors produced by any
/// other model are in a different space and must never share a recall or
/// clustering pool.
pub const DEFAULT_EMBEDDING_MODEL: &str = "qwen3-embedding:0.6b";

/// Sentinel tag for vectors produced by the deterministic lexical
/// signed-hash fallback embedder (plan §3.3). That space is incompatible
/// with the semantic model's, so a correctly-tagged hash vector simply
/// never cosine-compares against semantic vectors — the safe outcome for a
/// vector we can't meaningfully compare until `reembed_stale` recovers it.
pub const HASH_EMBED_MODEL: &str = "__lexical_hash__";

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Config {
    /// Vector dimensionality of `embedding_model` (plan §2). 384 matches the
    /// pinned qwen3-embedding:0.6b, keeping vocabulary/vector parity with
    /// the behavioral-memory reference.
    #[serde(default = "default_embedding_dim")]
    pub embedding_dim: usize,
    /// Single pinned embedding model (plan §2 / §3.3).
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    /// Best-effort embed budget per call; timeout → hash fallback, never a
    /// hard failure (plan §2 / §3.3).
    #[serde(default = "default_embedding_timeout_ms")]
    pub embedding_timeout_ms: u64,
    /// Ollama endpoint for the semantic embedding model (plan §2 / §3.3).
    /// The single configuration surface rule (no env-var shims) means the
    /// endpoint lives here, exactly like `extraction.provider_url` for the
    /// extraction model.
    #[serde(default = "default_embedding_provider_url")]
    pub embedding_provider_url: String,
    /// Circuit-breaker re-probe gap for the semantic provider, seconds
    /// (plan §2 / §3.3). While the service is down, embeds fast-fall back to
    /// the hash space without paying the timeout budget again until this
    /// many seconds have passed; a wrong default here costs latency, not
    /// correctness.
    #[serde(default = "default_embedding_probe_interval_s")]
    pub embedding_probe_interval_s: u64,
    /// LRU capacity of the in-process embedding cache, exact text →
    /// (vector, space tag) (plan §2 / §3.3). Ingestion re-embeds identical
    /// text when re-trying `pending` chunks while the service is down, so
    /// the cache bounds the work of outage periods.
    #[serde(default = "default_embedding_cache_size")]
    pub embedding_cache_size: usize,
    /// Lower bound of the chunk size window (plan §2 / §3).
    #[serde(default = "default_chunk_chars_min")]
    pub chunk_chars_min: usize,
    /// Upper bound of the chunk size window (plan §2 / §3).
    #[serde(default = "default_chunk_chars_max")]
    pub chunk_chars_max: usize,
    /// CosSim floor to join an existing provenance cluster (plan §2 / §3).
    #[serde(default = "default_cluster_similarity_threshold")]
    pub cluster_similarity_threshold: f64,
    /// CosSim floor for near-duplicate chunk grouping (plan §2).
    #[serde(default = "default_semantic_dedup_threshold")]
    pub semantic_dedup_threshold: f64,
    /// Minimum LLM confidence to open a DISPUTED edge; below it the edge is
    /// noise (plan §2 / §9.1).
    #[serde(default = "default_dispute_strength_floor")]
    pub dispute_strength_floor: f64,
    /// Decay time constant, days, for `static` chunks (plan §2 / §7.1).
    #[serde(default = "default_tau_static_days")]
    pub tau_static_days: u64,
    /// Decay time constant, days, for `transient` chunks (plan §2 / §7.1).
    #[serde(default = "default_tau_transient_days")]
    pub tau_transient_days: u64,
    /// Pre-deadline salience ramp time constant, days (plan §2 / §7.1).
    #[serde(default = "default_tau_pre_days")]
    pub tau_pre_days: u64,
    /// Post-deadline incident window time constant, hours (plan §2 / §7.1).
    #[serde(default = "default_tau_post_hours")]
    pub tau_post_hours: u64,
    /// Reinforcements a transient chunk needs before it may promote to
    /// static (plan §2 / §6.1 gate 1).
    #[serde(default = "default_reinforcement_promote_threshold")]
    pub reinforcement_promote_threshold: u64,
    /// `reinforcement_count` saturates here; beyond promotion it has no
    /// effect, so the counter is capped rather than grown unbounded (plan
    /// §2 / §6.1).
    #[serde(default = "default_reinforcement_count_max")]
    pub reinforcement_count_max: u64,
    /// Source-reliability floor required for promotion (plan §2 / §6.1
    /// gate 3).
    #[serde(default = "default_promotion_min_reliability")]
    pub promotion_min_reliability: f64,
    /// Reliability up-drift step on corroboration/promotion (plan §2 / §4.2).
    #[serde(default = "default_reliability_drift_up")]
    pub reliability_drift_up: f64,
    /// Reliability down-drift step on dispute-loss (plan §2 / §4.2).
    #[serde(default = "default_reliability_drift_down")]
    pub reliability_drift_down: f64,
    /// Max deviation of a drifted reliability from its tier prior, so a
    /// single event cannot swing it wildly (plan §2 / §4.2).
    #[serde(default = "default_reliability_band")]
    pub reliability_band: f64,
    /// Hard-delete window for archived/unsupported rows, days (plan §2 /
    /// §7.4).
    #[serde(default = "default_archive_retention_days")]
    pub archive_retention_days: u64,
    /// Max graph hops from seed nodes in Pass 2 (plan §2 / §10.1).
    #[serde(default = "default_retrieval_depth_cap")]
    pub retrieval_depth_cap: usize,
    /// Pass 1 top-K chunk seeds (plan §2 / §10.1).
    #[serde(default = "default_retrieval_seed_chunks")]
    pub retrieval_seed_chunks: usize,
    /// Minimum distinct proposition seeds before widening Pass 1 (plan §2 /
    /// §10.1).
    #[serde(default = "default_retrieval_min_seeds")]
    pub retrieval_min_seeds: usize,
    /// Drop any Pass 2 path whose accumulated activation falls below this
    /// (plan §2 / §10.1).
    #[serde(default = "default_activation_prune_threshold")]
    pub activation_prune_threshold: f64,
    /// Per-hop activation decay γ applied at each Pass 2 hop (plan §10.1);
    /// together with edge weights ≤ 0.8 it keeps accumulated activation in
    /// [0,1].
    #[serde(default = "default_activation_hop_decay")]
    pub activation_hop_decay: f64,
    /// Hard cap on the rendered context payload, tokens (plan §2 / §10.1).
    /// This bounds the full recall block (`render_item` / the MCP lookup
    /// tools); the compact per-turn context injection uses the smaller
    /// `injection_token_cap` instead.
    #[serde(default = "default_output_token_cap")]
    pub output_token_cap: usize,
    /// Hard cap on the compact summary+index block injected into the model's
    /// context each turn (plan §10.1, BigTiny-plugin conversion). The
    /// injection is a short summary plus a one-line-per-item index, not the
    /// full recall payload — the model pulls full items on demand through the
    /// `memorabilia_read_item` MCP tool — so this stays well under
    /// `output_token_cap`. A wrong value here only trades per-turn prompt
    /// footprint against how many items the index can advertise.
    #[serde(default = "default_injection_token_cap")]
    pub injection_token_cap: usize,
    /// Ceiling on the multiplicative MetaBoost (plan §2 / §10.2).
    #[serde(default = "default_metaboost_cap")]
    pub metaboost_cap: f64,
    /// Coefficient λ of the §8.2 source-correlation term
    /// `λ·log(1 + |N_S − 1|)`: the extra weight the (N+1)-th active chunk
    /// of an already-represented `source_entity` contributes. Sized so a
    /// second same-source chunk (≈ +0.035 confidence) stays an order of
    /// magnitude below adding one more independent source — correlated
    /// evidence nudges confidence, never dominates it.
    #[serde(default = "default_correlation_lambda")]
    pub correlation_lambda: f64,
    /// MetaBoost addend for `urgency == "high"` (plan §10.2 UrgencyBoost).
    /// A looming deadline should nearly double a proposition's base rank
    /// before importance effects; a wrong default here rescales urgency
    /// ordering, not correctness.
    #[serde(default = "default_metaboost_urgency_high")]
    pub metaboost_urgency_high: f64,
    /// MetaBoost addend for `urgency == "medium"` (plan §10.2).
    #[serde(default = "default_metaboost_urgency_medium")]
    pub metaboost_urgency_medium: f64,
    /// MetaBoost addend for `urgency == "low"` (plan §10.2).
    #[serde(default = "default_metaboost_urgency_low")]
    pub metaboost_urgency_low: f64,
    /// MetaBoost addend for `importance == "high"` (plan §10.2
    /// ImportanceBoost).
    #[serde(default = "default_metaboost_importance_high")]
    pub metaboost_importance_high: f64,
    /// MetaBoost addend for `importance == "medium"` (plan §10.2).
    #[serde(default = "default_metaboost_importance_medium")]
    pub metaboost_importance_medium: f64,
    /// MetaBoost addend for `importance == "low"` (plan §10.2).
    #[serde(default = "default_metaboost_importance_low")]
    pub metaboost_importance_low: f64,
    /// MetaBoost addend for `importance == "unknown"` (plan §10.2
    /// UnknownTriagePremium): surfaces unassessed claims above
    /// confidently-low ones while the extraction model classifies them. An
    /// `importance` outside the four CHECK-enforced levels is treated as
    /// unknown.
    #[serde(default = "default_metaboost_unknown_triage_premium")]
    pub metaboost_unknown_triage_premium: f64,
    /// Neighbor chunks fed to the Stage 4 conflict probe (plan §2 / §3.5).
    #[serde(default = "default_conflict_probe_neighbors")]
    pub conflict_probe_neighbors: usize,
    /// Lower cosine bound of the conflict probe window (plan §2 / §3.5).
    #[serde(default = "default_conflict_probe_similarity_min")]
    pub conflict_probe_similarity_min: f64,
    /// Confidence floor for RHYMES_WITH candidates (plan §2 / §9.3).
    #[serde(default = "default_rhymes_min_confidence")]
    pub rhymes_min_confidence: f64,
    /// Max pairs sampled per RHYMES_WITH run (plan §2 / §9.3).
    #[serde(default = "default_rhymes_batch_cap")]
    pub rhymes_batch_cap: usize,
    /// Min length before a forget tombstone is recorded (plan §2 / §12.2).
    #[serde(default = "default_tombstone_min_chars")]
    pub tombstone_min_chars: usize,
    /// Digit-run length that counts as a high-entropy token for the
    /// tombstone granularity guard (plan §12.2): deleted text below
    /// `tombstone_min_chars` is still tombstoned when it carries a digit
    /// run this long. A wrong default here either poisons short numeric
    /// patterns (too low) or lets short credentials relearn (too high).
    #[serde(default = "default_tombstone_digit_run")]
    pub tombstone_digit_run: usize,
    /// CosSim floor for the delete/forget tool's similarity match against
    /// the user's phrase (plan §12.2: "match the offending content by
    /// similarity"). Conservative by design — a miss leaves the data in
    /// place for a more specific phrase; a false hit deletes the wrong
    /// chunk.
    #[serde(default = "default_forget_match_threshold")]
    pub forget_match_threshold: f64,
    /// Duration of an `outdated` forget suppression in days before the
    /// content may be relearned (plan §2 / §12.2, "time-bounded
    /// suppression, default 90 days").
    #[serde(default = "default_outdated_suppression_days")]
    pub outdated_suppression_days: u64,
    /// Background extraction model settings (plan §2 / §3.6).
    #[serde(default)]
    pub extraction: ExtractionConfig,
    /// Maintenance scheduler cadence, seconds (plan §2 / §11).
    #[serde(default = "default_maintenance_tick_s")]
    pub maintenance_tick_s: u64,
    /// Cadence gate for expensive nightly passes, hours (plan §2 / §11).
    #[serde(default = "default_maintenance_heavy_interval_hours")]
    pub maintenance_heavy_interval_hours: u64,
}

/// Stage 4 extraction runs on a separate local model behind the
/// `StructuredChat` seam — never the calling LLM (plan §3.6).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ExtractionConfig {
    /// Endpoint of the local extraction model (plan §2).
    #[serde(default = "default_extraction_provider_url")]
    pub provider_url: String,
    /// Local instruct model that performs extraction; plan §3.6 calls for a
    /// qwen3 4B-class local model (plan §2).
    #[serde(default = "default_extraction_model")]
    pub model: String,
    /// Extraction call budget, seconds (plan §2).
    #[serde(default = "default_extraction_timeout_s")]
    pub timeout_s: u64,
    /// Backoff before retrying a failed extraction, seconds (plan §2). A
    /// minute-scale wait keeps the pending queue from hammering a cold or
    /// down model.
    #[serde(default = "default_extraction_retry_backoff_s")]
    pub retry_backoff_s: u64,
    /// Global semaphore: one extraction completion at a time (plan §2 /
    /// §3.6).
    #[serde(default = "default_extraction_max_concurrent")]
    pub max_concurrent: u64,
    /// Bounded batch of `pending` chunks per extraction drain (plan §11
    /// "a bounded batch of `pending` chunks"). The extraction LLM call is
    /// the slow step in the loop, so one drain must never hold the
    /// maintenance tick (or a manual drain) open across a whole queue — a
    /// wrong default here only paces throughput, never correctness.
    #[serde(default = "default_extraction_batch_size")]
    pub batch_size: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            embedding_dim: default_embedding_dim(),
            embedding_model: default_embedding_model(),
            embedding_timeout_ms: default_embedding_timeout_ms(),
            embedding_provider_url: default_embedding_provider_url(),
            embedding_probe_interval_s: default_embedding_probe_interval_s(),
            embedding_cache_size: default_embedding_cache_size(),
            chunk_chars_min: default_chunk_chars_min(),
            chunk_chars_max: default_chunk_chars_max(),
            cluster_similarity_threshold: default_cluster_similarity_threshold(),
            semantic_dedup_threshold: default_semantic_dedup_threshold(),
            dispute_strength_floor: default_dispute_strength_floor(),
            tau_static_days: default_tau_static_days(),
            tau_transient_days: default_tau_transient_days(),
            tau_pre_days: default_tau_pre_days(),
            tau_post_hours: default_tau_post_hours(),
            reinforcement_promote_threshold: default_reinforcement_promote_threshold(),
            reinforcement_count_max: default_reinforcement_count_max(),
            promotion_min_reliability: default_promotion_min_reliability(),
            reliability_drift_up: default_reliability_drift_up(),
            reliability_drift_down: default_reliability_drift_down(),
            reliability_band: default_reliability_band(),
            archive_retention_days: default_archive_retention_days(),
            retrieval_depth_cap: default_retrieval_depth_cap(),
            retrieval_seed_chunks: default_retrieval_seed_chunks(),
            retrieval_min_seeds: default_retrieval_min_seeds(),
            activation_prune_threshold: default_activation_prune_threshold(),
            activation_hop_decay: default_activation_hop_decay(),
            output_token_cap: default_output_token_cap(),
            injection_token_cap: default_injection_token_cap(),
            metaboost_cap: default_metaboost_cap(),
            correlation_lambda: default_correlation_lambda(),
            metaboost_urgency_high: default_metaboost_urgency_high(),
            metaboost_urgency_medium: default_metaboost_urgency_medium(),
            metaboost_urgency_low: default_metaboost_urgency_low(),
            metaboost_importance_high: default_metaboost_importance_high(),
            metaboost_importance_medium: default_metaboost_importance_medium(),
            metaboost_importance_low: default_metaboost_importance_low(),
            metaboost_unknown_triage_premium: default_metaboost_unknown_triage_premium(),
            conflict_probe_neighbors: default_conflict_probe_neighbors(),
            conflict_probe_similarity_min: default_conflict_probe_similarity_min(),
            rhymes_min_confidence: default_rhymes_min_confidence(),
            rhymes_batch_cap: default_rhymes_batch_cap(),
            tombstone_min_chars: default_tombstone_min_chars(),
            tombstone_digit_run: default_tombstone_digit_run(),
            forget_match_threshold: default_forget_match_threshold(),
            outdated_suppression_days: default_outdated_suppression_days(),
            extraction: ExtractionConfig::default(),
            maintenance_tick_s: default_maintenance_tick_s(),
            maintenance_heavy_interval_hours: default_maintenance_heavy_interval_hours(),
        }
    }
}

impl Default for ExtractionConfig {
    fn default() -> Self {
        Self {
            provider_url: default_extraction_provider_url(),
            model: default_extraction_model(),
            timeout_s: default_extraction_timeout_s(),
            retry_backoff_s: default_extraction_retry_backoff_s(),
            max_concurrent: default_extraction_max_concurrent(),
            batch_size: default_extraction_batch_size(),
        }
    }
}

impl Config {
    /// Load config from `path`.
    ///
    /// * `None`, or a `path` whose file does not exist → [`Config::default`]
    ///   (plan §13.5: defaults load with no file present).
    /// * A present, non-empty file overrides only the keys it sets; unknown
    ///   keys are ignored so older config files keep loading after new
    ///   defaults are added (mirrors the reference's serde tolerance).
    /// * A present file with malformed YAML is a `Config` error.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        match path {
            Some(p) if p.exists() => {
                let raw = std::fs::read_to_string(p)?;
                if raw.trim().is_empty() {
                    return Ok(Self::default());
                }
                serde_yaml::from_str(&raw).map_err(|e| Error::Config(e.to_string()))
            }
            _ => Ok(Self::default()),
        }
    }
}

// --------------------------------------------------------------------------
// Default values. Each must match the plan §2 table — tests pin all of them
// (tests/config.rs::defaults_match_plan_section2).

fn default_embedding_dim() -> usize {
    384
}
fn default_embedding_model() -> String {
    DEFAULT_EMBEDDING_MODEL.into()
}
fn default_embedding_timeout_ms() -> u64 {
    1500
}
fn default_embedding_provider_url() -> String {
    "http://localhost:11434".into()
}
fn default_embedding_probe_interval_s() -> u64 {
    60
}
fn default_embedding_cache_size() -> usize {
    256
}
fn default_chunk_chars_min() -> usize {
    512
}
fn default_chunk_chars_max() -> usize {
    1024
}
fn default_cluster_similarity_threshold() -> f64 {
    0.92
}
fn default_semantic_dedup_threshold() -> f64 {
    0.92
}
fn default_dispute_strength_floor() -> f64 {
    0.5
}
fn default_tau_static_days() -> u64 {
    365
}
fn default_tau_transient_days() -> u64 {
    7
}
fn default_tau_pre_days() -> u64 {
    7
}
fn default_tau_post_hours() -> u64 {
    48
}
fn default_reinforcement_promote_threshold() -> u64 {
    5
}
fn default_reinforcement_count_max() -> u64 {
    5
}
fn default_promotion_min_reliability() -> f64 {
    0.6
}
fn default_reliability_drift_up() -> f64 {
    0.15
}
fn default_reliability_drift_down() -> f64 {
    0.25
}
fn default_reliability_band() -> f64 {
    0.2
}
fn default_archive_retention_days() -> u64 {
    90
}
fn default_retrieval_depth_cap() -> usize {
    3
}
fn default_retrieval_seed_chunks() -> usize {
    8
}
fn default_retrieval_min_seeds() -> usize {
    3
}
fn default_activation_prune_threshold() -> f64 {
    0.05
}
fn default_activation_hop_decay() -> f64 {
    0.5
}
fn default_output_token_cap() -> usize {
    1500
}
fn default_injection_token_cap() -> usize {
    500
}
fn default_metaboost_cap() -> f64 {
    3.0
}
fn default_correlation_lambda() -> f64 {
    0.05
}
fn default_metaboost_urgency_high() -> f64 {
    1.0
}
fn default_metaboost_urgency_medium() -> f64 {
    0.4
}
fn default_metaboost_urgency_low() -> f64 {
    0.1
}
fn default_metaboost_importance_high() -> f64 {
    0.8
}
fn default_metaboost_importance_medium() -> f64 {
    0.3
}
fn default_metaboost_importance_low() -> f64 {
    0.1
}
fn default_metaboost_unknown_triage_premium() -> f64 {
    0.5
}
fn default_conflict_probe_neighbors() -> usize {
    5
}
fn default_conflict_probe_similarity_min() -> f64 {
    0.55
}
fn default_rhymes_min_confidence() -> f64 {
    0.7
}
fn default_rhymes_batch_cap() -> usize {
    50
}
fn default_tombstone_min_chars() -> usize {
    20
}
fn default_tombstone_digit_run() -> usize {
    8
}
fn default_forget_match_threshold() -> f64 {
    0.8
}
fn default_outdated_suppression_days() -> u64 {
    90
}
fn default_extraction_provider_url() -> String {
    "http://localhost:11434".into()
}
fn default_extraction_model() -> String {
    "qwen3:4b".into()
}
fn default_extraction_timeout_s() -> u64 {
    12
}
fn default_extraction_retry_backoff_s() -> u64 {
    60
}
fn default_extraction_max_concurrent() -> u64 {
    1
}
fn default_extraction_batch_size() -> u64 {
    5
}
fn default_maintenance_tick_s() -> u64 {
    60
}
fn default_maintenance_heavy_interval_hours() -> u64 {
    24
}
