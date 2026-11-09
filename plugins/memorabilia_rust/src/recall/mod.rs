//! Two-pass retrieval, rendering, and the reinforcement outbox write
//! (project-plan.md §10, §15 Phase 7).
//!
//! Pass 1 seeds propositions from a vector search over **chunks** (a chunk's
//! seed activation = `max CosSim` over its retrieved supporting chunks —
//! max, never sum, so near-duplicate evidence cannot double-count). Pass 2
//! spreads that activation over proposition edges, bounded in depth and
//! pruned below a floor. The ranked set renders three ways:
//!
//! * [`Engine::recall`] — the compact **summary + index** injected into the
//!   model's context each turn, capped at `injection_token_cap`. It also
//!   writes the grounding/reinforcement outbox rows for the chunks backing
//!   the indexed items, in one transaction (plan §6, claude.md principles
//!   3 & 4: every rendered chunk is grounded once per query event; only
//!   undisputed supporting chunks are reinforced).
//! * [`Engine::search_index`] — the same ranked index, paginated, backing the
//!   `memorabilia_search` MCP tool.
//! * [`Engine::render_item`] — one item's full proposition + supporting chunk
//!   text + citations, paginated and byte-budgeted, backing the
//!   `memorabilia_read_item` MCP tool.
//!
//! Soft-fail by construction (claude.md principle 9): any embed/DB error in
//! the recall hot path is logged and swallowed to `None`, so the prompt is
//! byte-identical to no-memory behavior. `pending` chunks carry no active
//! propositions, so they are invisible to retrieval until Stage 4 completes.

use std::collections::HashMap;

pub mod cut_log;

use crate::core;
use crate::engine::Engine;
use crate::error::Result;
use crate::store::outbox::OutboxEntry;

/// Rough token estimate for the cap accounting (plan §10.1). Deterministic
/// and dependency-free: ~4 chars/token is the conventional English ratio, and
/// recall must not exceed the cap, so we round *up* and never undercount.
fn approx_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

/// Elementwise `a - b` (the centering step for the relevance gate). Lengths
/// are checked by the caller; a shorter `b` leaves the tail of `a` unchanged.
fn sub(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter()
        .enumerate()
        .map(|(i, &x)| x - b.get(i).copied().unwrap_or(0.0))
        .collect()
}

/// Sort key for the primary (relevance) ranking: a directly-seeded item's
/// centered relevance, or `−∞` for a spread-only / hash-space item so it sorts
/// after every item that has a real relevance score.
fn rel_key(relevance: Option<f64>) -> f64 {
    relevance.unwrap_or(f64::NEG_INFINITY)
}

/// Pass-1 seed output: per-node raw-cosine activation (drives spreading) and
/// per-node centered relevance (the primary ranking key), plus this query's
/// effective cut (the lowest kept centered score) for the self-tuning EWMA.
struct SeedResult {
    activations: HashMap<String, f64>,
    relevance: HashMap<String, f64>,
    effective_cut: Option<f64>,
}

/// The self-tuning seed filter. Given `(chunk_id, raw_cos, centered_cos?)`:
///
/// 1. **Operational floor** — drop any hit whose centered score is below
///    `floor` (a hit with no centered score fails open and is kept).
/// 2. **Per-query gap cut** — sort the surviving centered scores descending and
///    find the largest gap between adjacent scores; when it clears `gap_min`,
///    keep only the cluster above that gap (the hits genuinely relevant to
///    *this* query) and drop the tail. A tight, gapless distribution is kept
///    whole. Fewer than two centered scores → nothing to cut.
///
/// Returns the kept hits and the lowest kept centered score (this turn's
/// effective cut, fed to the EWMA), or `None` when no centered score survived.
fn adaptive_cut(
    scored: Vec<(String, f64, Option<f64>)>,
    floor: f64,
    gap_min: f64,
) -> (Vec<(String, f64, Option<f64>)>, Option<f64>) {
    // 1. Operational floor (fail open for hits with no centered score).
    let mut survivors: Vec<(String, f64, Option<f64>)> = scored
        .into_iter()
        .filter(|(_, _, cc)| cc.map(|c| c >= floor).unwrap_or(true))
        .collect();

    // 2. Per-query gap cut over the centered scores.
    let mut ccs: Vec<f64> = survivors.iter().filter_map(|(_, _, cc)| *cc).collect();
    ccs.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    if ccs.len() >= 2 {
        let mut max_gap = 0.0f64;
        let mut threshold: Option<f64> = None;
        for w in ccs.windows(2) {
            let gap = w[0] - w[1];
            if gap > max_gap {
                max_gap = gap;
                threshold = Some(w[0]); // keep scores >= the lower edge of the top cluster
            }
        }
        if max_gap >= gap_min {
            if let Some(t) = threshold {
                survivors.retain(|(_, _, cc)| cc.map(|c| c >= t).unwrap_or(true));
            }
        }
    }

    let effective_cut = survivors
        .iter()
        .filter_map(|(_, _, cc)| *cc)
        .fold(f64::INFINITY, f64::min);
    let effective_cut = effective_cut.is_finite().then_some(effective_cut);
    (survivors, effective_cut)
}

/// Cosine similarity of two equal-length vectors; 0 when either is degenerate.
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (&x, &y) in a.iter().zip(b.iter()) {
        dot += x as f64 * y as f64;
        na += x as f64 * x as f64;
        nb += y as f64 * y as f64;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// One ranked proposition after both retrieval passes.
#[derive(Debug, Clone)]
pub struct RankedItem {
    /// Stable lookup id handed to the model and resolved by
    /// `memorabilia_read_item` — the proposition's `node_id`.
    pub item_id: String,
    pub claim: String,
    pub confidence: f64,
    pub importance: String,
    pub urgency: String,
    pub is_disputed: bool,
    /// `Priority(V) = Activation · MetaBoost` (plan §10.2). Retained as the
    /// *secondary* ranking key: since the centered-cosine relevance gate, the
    /// primary key is `relevance` (how well the item matches *this* query), so
    /// MetaBoost (importance/urgency) only orders items of equal relevance
    /// instead of lifting an off-topic-but-important item over an on-topic one.
    pub priority: f64,
    /// Max centered-cosine relevance of this item's directly-seeded supporting
    /// chunks against the query — the **primary** ranking key. `None` for an
    /// item reached only by Pass-2 spreading activation (it was never a direct
    /// query hit, so it has no relevance of its own) or in the lexical-hash
    /// space where centering is meaningless; `None` sorts after every
    /// directly-relevant item, so spread-only context fills remaining slots
    /// rather than leading.
    pub relevance: Option<f64>,
    /// Deterministic citations of the active supporting chunks (deduped,
    /// order-stable).
    pub citations: Vec<String>,
    /// Active supporting chunk ids (the grounding/reinforcement targets).
    pub supporting_chunk_ids: Vec<String>,
}

/// A compact index row (the injected index / the `memorabilia_search`
/// result): enough to decide whether to look the item up, no full text.
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub item_id: String,
    pub claim: String,
    pub confidence: f64,
    pub is_disputed: bool,
    pub citation: String,
}

/// One supporting chunk of an item's full view.
#[derive(Debug, Clone)]
pub struct ItemChunk {
    pub citation: String,
    pub content: String,
}

/// The full view of one item (the `memorabilia_read_item` payload).
#[derive(Debug, Clone)]
pub struct ItemView {
    pub item_id: String,
    pub claim: String,
    pub confidence: f64,
    pub importance: String,
    pub urgency: String,
    pub is_disputed: bool,
    /// Active supporting chunks in this window.
    pub chunks: Vec<ItemChunk>,
    /// Total active supporting chunks (for pagination metadata).
    pub total_chunks: usize,
    /// Window offset into the supporting chunks.
    pub offset: usize,
}

impl Engine {
    /// Factual recall for `query`: the compact summary+index block injected
    /// into the model's context (`injection_token_cap`), or `None` when the
    /// engine has nothing to say — in which case the caller's prompt is
    /// byte-identical to no-memory behavior (claude.md principle 9).
    ///
    /// Side effect (plan §6): the chunks backing the indexed items are
    /// grounded (and reinforced when undisputed) via the outbox, in one
    /// transaction. A soft failure anywhere logs and yields `None`.
    pub async fn recall(&self, query: &str) -> Option<String> {
        match self.recall_inner(query).await {
            Ok(block) => block,
            Err(e) => {
                tracing::warn!("recall: soft-failed ({e}); injecting nothing");
                None
            }
        }
    }

    async fn recall_inner(&self, query: &str) -> Result<Option<String>> {
        // The injection path (unlike read-only search) updates the running
        // EWMA cut estimate, so the self-tuning floor tracks what real turns
        // actually find relevant.
        let items = self.ranked_items_persist(query, true).await?;
        if items.is_empty() {
            return Ok(None);
        }
        let cap = self.config.injection_token_cap;
        let (block, grounded) = render_index_block(&items, cap);
        if grounded.is_empty() {
            return Ok(None);
        }
        self.write_grounding(&grounded).await?;
        Ok(Some(block))
    }

    /// The ranked index for `query`, paginated — the `memorabilia_search`
    /// tool's data (read-only: search never grounds/reinforces; only the
    /// per-turn injection does, so one query event increments a chunk once).
    pub async fn search_index(
        &self,
        query: &str,
        offset: usize,
        limit: usize,
    ) -> Vec<IndexEntry> {
        let items = match self.ranked_items(query).await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("search_index: soft-failed ({e})");
                return Vec::new();
            }
        };
        items
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|i| IndexEntry {
                item_id: i.item_id,
                claim: i.claim,
                confidence: i.confidence,
                is_disputed: i.is_disputed,
                citation: i.citations.first().cloned().unwrap_or_default(),
            })
            .collect()
    }

    /// One item's full proposition + supporting chunk text + citations,
    /// windowed over the supporting chunks — the `memorabilia_read_item`
    /// tool's data. `None` when the id is unknown or its proposition is not
    /// active (archived/deleted items never resurface — plan §13.6).
    pub async fn render_item(
        &self,
        item_id: &str,
        offset: usize,
        limit: usize,
    ) -> Option<ItemView> {
        match self.render_item_inner(item_id, offset, limit).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("render_item: soft-failed for {item_id} ({e})");
                None
            }
        }
    }

    async fn render_item_inner(
        &self,
        item_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Option<ItemView>> {
        let Some(prop) = self.db.get_proposition(item_id).await? else {
            return Ok(None);
        };
        if prop.status != "active" {
            return Ok(None);
        }
        let supporters = self.db.list_active_supporting_chunks(item_id).await?;
        let total = supporters.len();
        // Byte budget over the full recall cap so an item id / pagination
        // metadata always survives (plan §10.1); keep whole chunks.
        let mut token_budget = self.config.output_token_cap;
        let mut chunks = Vec::new();
        for ch in supporters.into_iter().skip(offset).take(limit) {
            let cost = approx_tokens(&ch.content) + approx_tokens(&ch.cluster_citation);
            if !chunks.is_empty() && cost > token_budget {
                break;
            }
            token_budget = token_budget.saturating_sub(cost);
            chunks.push(ItemChunk {
                citation: ch.cluster_citation,
                content: ch.content,
            });
        }
        Ok(Some(ItemView {
            item_id: prop.node_id,
            claim: prop.claim,
            confidence: prop.confidence,
            importance: prop.importance,
            urgency: prop.urgency,
            is_disputed: prop.is_disputed,
            chunks,
            total_chunks: total,
            offset,
        }))
    }

    /// The two-pass retrieval core, shared by recall and search. Read-only
    /// variant (does not update the EWMA); see [`Self::ranked_items_persist`].
    pub(crate) async fn ranked_items(&self, query: &str) -> Result<Vec<RankedItem>> {
        self.ranked_items_persist(query, false).await
    }

    /// The two-pass retrieval core. Ranked by **centered relevance** descending
    /// (the primary key since the relevance gate), then `Priority` (MetaBoost),
    /// then `item_id` so repeated calls agree byte-for-byte (plan §13.4).
    ///
    /// `persist_cut` is set only on the injection path: it folds this turn's
    /// effective cut into the running EWMA estimate the self-tuning floor reads
    /// (`recall_gap_min` / `recall_ewma_*`). Search leaves the estimate
    /// untouched so browsing never moves the live floor.
    pub(crate) async fn ranked_items_persist(
        &self,
        query: &str,
        persist_cut: bool,
    ) -> Result<Vec<RankedItem>> {
        // Pass 1: seed propositions from a vector search over chunks.
        let (qvec, tag) = self.embedder.embed(query).await.map_err(crate::error::Error::Embed)?;

        // Corpus mean for the centered-cosine relevance gate (see
        // `recall_min_cosine`). Skipped in the lexical-hash fallback space,
        // where cosine is keyword overlap and mean-centering is meaningless;
        // soft-fail to `None` (no gate) on any error or a dim mismatch, so
        // recall never hard fails on the hot path (core principle 9).
        let mean = if tag == crate::config::HASH_EMBED_MODEL {
            None
        } else {
            self.corpus_mean(&tag).await.unwrap_or(None)
        };
        let mean = mean.filter(|m| m.len() == qvec.len());
        let mean_ref = mean.as_deref();

        // Operational floor = hard floor raised toward where relevance usually
        // separates in this space (the running EWMA cut), minus a little slack.
        // Cold (no EWMA yet) or hash space → just the hard floor.
        let floor = self.operational_floor(&tag, mean_ref.is_some()).await;

        let mut seeds = self
            .seed_from_chunks(&qvec, &tag, self.config.retrieval_seed_chunks, mean_ref, floor)
            .await?;
        if seeds.activations.len() < self.config.retrieval_min_seeds {
            // Widen Pass 1 over the whole active pool once (plan §10.1) — the
            // brute-force index already pays a full scan per search.
            let wider = (self.db.count_active_chunks().await?).max(1) as usize;
            if wider > self.config.retrieval_seed_chunks {
                seeds = self.seed_from_chunks(&qvec, &tag, wider, mean_ref, floor).await?;
            }
        }
        if seeds.activations.is_empty() {
            return Ok(Vec::new());
        }

        // Self-tuning: fold this turn's effective cut into the running estimate
        // (injection path only). Best-effort — a settings-write failure must
        // never fail recall (core principle 9).
        // Logged only: the snapshot the floor reads moves in the heavy
        // maintenance pass (`cut_log`), so two recalls of the same query see
        // the same floor.
        if persist_cut {
            if let Some(cut) = seeds.effective_cut {
                if let Err(e) = cut_log::log(&self.db, &tag, cut).await {
                    tracing::debug!("recall: could not log the recall cut for {tag}: {e}");
                }
            }
        }

        let relevance = seeds.relevance;

        // Pass 2: bounded spreading activation over proposition edges.
        let activations = self.spread_activation(seeds.activations).await?;

        // Rank: fetch each activated proposition, compute Priority.
        let mut items: Vec<RankedItem> = Vec::new();
        for (node_id, activation) in activations {
            let Some(prop) = self.db.get_proposition(&node_id).await? else {
                continue;
            };
            if prop.status != "active" {
                continue;
            }
            let boost = core::metaboost(&prop.urgency, &prop.importance, &self.config);
            let priority = core::priority(activation, boost, self.config.metaboost_cap);
            let supporters = self.db.list_active_supporting_chunks(&node_id).await?;
            let mut citations: Vec<String> = Vec::new();
            let mut supporting_chunk_ids: Vec<String> = Vec::new();
            for ch in &supporters {
                if !citations.contains(&ch.cluster_citation) {
                    citations.push(ch.cluster_citation.clone());
                }
                supporting_chunk_ids.push(ch.chunk_id.clone());
            }
            // A proposition with no active support has left the graph (plan
            // §5.3); never surface it.
            if supporting_chunk_ids.is_empty() {
                continue;
            }
            items.push(RankedItem {
                item_id: prop.node_id,
                claim: prop.claim,
                confidence: prop.confidence,
                importance: prop.importance,
                urgency: prop.urgency,
                is_disputed: prop.is_disputed,
                priority,
                relevance: relevance.get(&node_id).copied(),
                citations,
                supporting_chunk_ids,
            });
        }
        // Primary: centered relevance (a directly-seeded on-topic item leads;
        // spread-only / hash-space items — `relevance = None` — fall to the
        // back, ordered among themselves by Priority then id). This is what
        // makes MetaBoost a tiebreak rather than a lever that lifts off-topic
        // items (fix: relevance-primary ranking).
        items.sort_by(|a, b| {
            rel_key(b.relevance)
                .partial_cmp(&rel_key(a.relevance))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    b.priority
                        .partial_cmp(&a.priority)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.item_id.cmp(&b.item_id))
        });
        Ok(items)
    }

    /// The per-turn operational floor: the hard `recall_min_cosine` raised
    /// toward the running EWMA cut (minus slack) when one exists for this
    /// space. Returns the hard floor unchanged in the hash space or before any
    /// EWMA has been recorded. Soft-fails to the hard floor on any read error.
    async fn operational_floor(&self, tag: &str, centered: bool) -> f64 {
        let hard = self.config.recall_min_cosine;
        if !centered {
            return hard;
        }
        match cut_log::snapshot(&self.db, tag).await {
            Some(ewma) => hard.max(ewma - self.config.recall_ewma_slack),
            None => hard,
        }
    }

    /// Pass 1: seed proposition activations from the top-`k` chunk search.
    /// Each proposition's seed activation is `max CosSim` over its retrieved
    /// supporting chunks (plan §10.1). `pending` chunks carry no active
    /// propositions, so they contribute nothing (invisible to recall).
    ///
    /// When `mean` is present, each hit is scored by **centered cosine** (the
    /// corpus mean removed from both the query and the chunk, so only chunks
    /// genuinely aligned with the query — not merely sharing EmbeddingGemma's
    /// compressed common direction — rank highly). The hits then pass through
    /// [`adaptive_cut`]: a hard/operational `floor`, then a per-query gap cut
    /// that keeps only the top relevance cluster for this query. A hit whose
    /// vector can't be fetched is kept (fail open, never suppress on a lookup
    /// miss) and carries no centered score.
    ///
    /// Returns, per surviving node: the raw-cosine seed activation (what drives
    /// Pass-2 spreading dynamics, unchanged) **and** the max centered relevance
    /// (the primary ranking key), plus this call's effective cut (the lowest
    /// kept centered score) for the self-tuning EWMA. In the lexical-hash space
    /// (`mean = None`) there are no centered scores: activation is the raw
    /// cosine exactly as before and relevance is empty, so hash-space recall is
    /// byte-for-byte unchanged.
    async fn seed_from_chunks(
        &self,
        qvec: &[f32],
        tag: &str,
        k: usize,
        mean: Option<&[f32]>,
        floor: f64,
    ) -> Result<SeedResult> {
        let hits = self
            .vectors
            .search(qvec, tag, k)
            .await
            .map_err(crate::error::Error::Internal)?;

        // (chunk_id, raw cosine, centered cosine?) — centered only when we can
        // fetch the chunk's vector in a matching space.
        let scored: Vec<(String, f64, Option<f64>)> = match mean {
            Some(mu) if mu.len() == qvec.len() => {
                let ids: Vec<String> = hits.iter().map(|(id, _)| id.clone()).collect();
                let vecs = self.db.fetch_vectors(&ids).await?;
                let qc = sub(qvec, mu);
                hits.into_iter()
                    .map(|(id, raw)| {
                        let cc = match vecs.get(&id) {
                            Some(v) if v.len() == mu.len() => Some(cosine(&qc, &sub(v, mu))),
                            _ => None,
                        };
                        (id, raw as f64, cc)
                    })
                    .collect()
            }
            _ => hits.into_iter().map(|(id, raw)| (id, raw as f64, None)).collect(),
        };

        // Apply the floor + per-query gap cut only where centered scores exist.
        let centered_present = scored.iter().any(|(_, _, cc)| cc.is_some());
        let (kept, effective_cut) = if centered_present {
            adaptive_cut(scored, floor, self.config.recall_gap_min)
        } else {
            (scored, None)
        };

        // node_id -> (raw cosines, centered cosines) of its retrieved chunks.
        let mut raw_by_node: HashMap<String, Vec<f64>> = HashMap::new();
        let mut cc_by_node: HashMap<String, Vec<f64>> = HashMap::new();
        for (chunk_id, raw, cc) in &kept {
            for prop in self.db.list_propositions_for_chunk(chunk_id).await? {
                raw_by_node.entry(prop.node_id.clone()).or_default().push(*raw);
                if let Some(cc) = cc {
                    cc_by_node.entry(prop.node_id).or_default().push(*cc);
                }
            }
        }
        let activations: HashMap<String, f64> = raw_by_node
            .into_iter()
            .map(|(node, cosines)| (node, core::seed_activation(&cosines)))
            .collect();
        // Max centered cosine per node (same "max, not sum" rule as activation).
        let relevance: HashMap<String, f64> = cc_by_node
            .into_iter()
            .map(|(node, ccs)| (node, ccs.into_iter().fold(f64::NEG_INFINITY, f64::max)))
            .collect();
        Ok(SeedResult { activations, relevance, effective_cut })
    }

    /// The centered-cosine relevance gate's corpus mean (`recall_min_cosine`),
    /// cached in `app_settings` per embedding space. Returns `None` when the
    /// space has no active vectors yet. The cache is refreshed by the
    /// maintenance heavy pass; recall only reads (computing+storing once if the
    /// cache is cold) so the hot path never scans the whole pool.
    pub(crate) async fn corpus_mean(&self, tag: &str) -> Result<Option<Vec<f32>>> {
        let key = format!("corpus_mean::{tag}");
        if let Some(s) = self.db.get_setting(&key).await? {
            if let Ok(v) = serde_json::from_str::<Vec<f32>>(&s) {
                if !v.is_empty() {
                    return Ok(Some(v));
                }
            }
        }
        let Some(mean) = self.db.corpus_mean_vector(tag).await? else {
            return Ok(None);
        };
        if let Ok(s) = serde_json::to_string(&mean) {
            self.db.set_setting(&key, &s).await?;
        }
        Ok(Some(mean))
    }

    /// Recompute and persist the corpus mean for `tag` (maintenance heavy
    /// pass), so the centered-cosine gate tracks the growing/decaying pool
    /// instead of freezing at the first query's snapshot.
    pub(crate) async fn refresh_corpus_mean(&self, tag: &str) -> Result<()> {
        let key = format!("corpus_mean::{tag}");
        match self.db.corpus_mean_vector(tag).await? {
            Some(mean) => {
                if let Ok(s) = serde_json::to_string(&mean) {
                    self.db.set_setting(&key, &s).await?;
                }
            }
            None => {}
        }
        Ok(())
    }

    /// Pass 2: bounded spreading activation over proposition edges (plan
    /// §10.1). Depth ≤ `retrieval_depth_cap`; each hop multiplies by the
    /// edge weight (≤ 0.8) and the per-hop decay γ; paths below
    /// `activation_prune_threshold` are pruned. Keeps each node's best
    /// activation.
    async fn spread_activation(
        &self,
        seeds: HashMap<String, f64>,
    ) -> Result<HashMap<String, f64>> {
        let gamma = self.config.activation_hop_decay;
        let prune = self.config.activation_prune_threshold;
        let mut best: HashMap<String, f64> = HashMap::new();
        let mut frontier: Vec<(String, f64)> = Vec::new();
        for (node, act) in seeds {
            let act = act.clamp(0.0, 1.0);
            if core::should_prune(act, prune) {
                continue;
            }
            best.insert(node.clone(), act);
            frontier.push((node, act));
        }
        for _ in 0..self.config.retrieval_depth_cap {
            if frontier.is_empty() {
                break;
            }
            let mut next: Vec<(String, f64)> = Vec::new();
            for (node, act) in frontier.drain(..) {
                for edge in self.db.list_proposition_edges_from(&node).await? {
                    let spread = core::hop(act, edge.weight, gamma);
                    if core::should_prune(spread, prune) {
                        continue;
                    }
                    let improved = best.get(&edge.to_node).map(|b| spread > *b).unwrap_or(true);
                    if improved {
                        best.insert(edge.to_node.clone(), spread);
                        next.push((edge.to_node, spread));
                    }
                }
            }
            frontier = next;
        }
        Ok(best)
    }

    /// Write the deduplicated grounding/reinforcement outbox rows for the
    /// chunks backing the indexed items, in one transaction (plan §6,
    /// principles 3 & 4). A chunk supporting several rendered items is
    /// grounded exactly once per query event (the `UNIQUE(chunk_id,
    /// query_event_id)` makes it idempotent); a disputed chunk is grounded
    /// but never reinforced.
    async fn write_grounding(&self, grounded_chunk_ids: &[String]) -> Result<()> {
        if grounded_chunk_ids.is_empty() {
            return Ok(());
        }
        let query_event_id = uuid::Uuid::new_v4().to_string();
        // The outbox `created_at` is FIFO bookkeeping for the drain, not decay
        // math (which is anchored on chunks), so the query-event wall clock is
        // the right stamp here — recall takes no caller `now`.
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        // Resolve dispute status outside the transaction (reads only).
        let mut rows: Vec<OutboxEntry> = Vec::new();
        for chunk_id in grounded_chunk_ids {
            let disputed = self.db.max_open_dispute_strength(chunk_id).await?.is_some();
            rows.push(OutboxEntry {
                chunk_id: chunk_id.clone(),
                query_event_id: query_event_id.clone(),
                grounded: true,
                reinforced: !disputed,
                created_at: now.clone(),
            });
        }
        let db = &self.db;
        db.run_in_transaction(|| async move {
            for row in &rows {
                db.enqueue_outbox(row).await?;
            }
            Ok(())
        })
        .await
    }
}

/// Render the compact summary+index block within `token_cap`, returning the
/// block and the deduplicated chunk ids that backed the rendered items (the
/// grounding set). Whole items are kept while they fit; the accounting rounds
/// up so the cap is never exceeded (plan §10.1, pinned by a test).
fn render_index_block(items: &[RankedItem], token_cap: usize) -> (String, Vec<String>) {
    let top = &items[0];
    let mut out = String::new();
    // Summary line (deterministic — no LLM in the recall hot path).
    out.push_str("## Relevant memory\n");
    out.push_str(&format!(
        "Most relevant: {}{}\n",
        top.claim,
        if top.is_disputed { " (disputed)" } else { "" }
    ));
    out.push_str(
        "Look any item up in full with the memorabilia_read_item tool (item_id), \
         or search with memorabilia_search.\n\nIndex:\n",
    );

    let mut grounded: Vec<String> = Vec::new();
    for item in items {
        let line = format!(
            "- [{}] {}{} — conf {:.2} — {}\n",
            item.item_id,
            item.claim,
            if item.is_disputed { " (disputed)" } else { "" },
            item.confidence,
            item.citations.first().map(String::as_str).unwrap_or("(uncited)"),
        );
        // Keep whole items while they fit; stop before exceeding the cap.
        if approx_tokens(&out) + approx_tokens(&line) > token_cap {
            break;
        }
        out.push_str(&line);
        for chunk_id in &item.supporting_chunk_ids {
            if !grounded.contains(chunk_id) {
                grounded.push(chunk_id.clone());
            }
        }
    }
    (out, grounded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: &str, cc: Option<f64>) -> (String, f64, Option<f64>) {
        // raw cosine is irrelevant to the cut (only the centered score gates);
        // use a fixed placeholder.
        (id.into(), 0.8, cc)
    }

    #[test]
    fn floor_drops_sub_floor_hits_and_keeps_fail_open() {
        let scored = vec![
            hit("a", Some(0.30)),
            hit("b", Some(-0.10)), // below a 0.0 floor
            hit("c", None),        // no centered score: fail open, kept
        ];
        // gap_min huge → no gap cut, floor only.
        let (kept, cut) = adaptive_cut(scored, 0.0, 9.9);
        let ids: Vec<&str> = kept.iter().map(|(id, _, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["a", "c"], "sub-floor dropped, fail-open kept");
        assert!((cut.unwrap() - 0.30).abs() < 1e-12, "effective cut is the lowest kept centered score");
    }

    #[test]
    fn gap_cut_keeps_only_the_top_cluster() {
        // Two tight relevant hits, then a wide gap to a weak tail.
        let scored = vec![
            hit("x1", Some(0.30)),
            hit("x2", Some(0.28)),
            hit("t1", Some(0.05)),
            hit("t2", Some(0.02)),
        ];
        let (kept, cut) = adaptive_cut(scored, 0.0, 0.08);
        let ids: Vec<&str> = kept.iter().map(|(id, _, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["x1", "x2"], "the weak tail past the largest gap is dropped");
        assert!((cut.unwrap() - 0.28).abs() < 1e-12);
    }

    #[test]
    fn a_tight_distribution_is_kept_whole() {
        // All gaps below gap_min → nothing is cut (the query genuinely matches
        // several things about equally well).
        let scored = vec![
            hit("a", Some(0.30)),
            hit("b", Some(0.27)),
            hit("c", Some(0.25)),
        ];
        let (kept, _) = adaptive_cut(scored, 0.0, 0.08);
        assert_eq!(kept.len(), 3, "no significant gap, so keep the whole cluster");
    }

    #[test]
    fn nothing_above_floor_yields_no_cut() {
        let scored = vec![hit("a", Some(-0.2)), hit("b", Some(-0.3))];
        let (kept, cut) = adaptive_cut(scored, 0.0, 0.08);
        assert!(kept.is_empty());
        assert!(cut.is_none(), "no centered survivor → no EWMA observation");
    }

    #[test]
    fn rel_key_sorts_scored_above_unscored() {
        assert!(rel_key(Some(-0.9)) > rel_key(None), "any real score beats a spread-only item");
    }
}
