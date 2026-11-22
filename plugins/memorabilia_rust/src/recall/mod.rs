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

/// Composite primary sort key combining centered relevance, Reciprocal Rank
/// Fusion across retrieval channels (chunks, claims, FTS5), and proposition
/// confidence. Returns `−∞` when `relevance` is `None` (spread-only /
/// hash-space items) so directly-scored semantic hits always lead.
fn composite_relevance_key(relevance: Option<f64>, rrf: f64, confidence: f64) -> f64 {
    match relevance {
        Some(rel) => (rel * 0.70 + rrf * 0.30) * (0.85 + 0.15 * confidence.clamp(0.0, 1.0)),
        None => f64::NEG_INFINITY,
    }
}

/// RRF damping constant (standard Cormack et al. k = 60).
const RRF_K: f64 = 60.0;

/// Max propositions a rare exact term (a name, model number or identifier
/// in the query) may bring in that neither vector channel found.
const MAX_FTS_ONLY_SEEDS: usize = 3;

/// A query term counts as rare when at most this many active claims hold it.
pub(crate) const FTS_RARE_TERM_MAX_DF: i64 = 3;

/// Output of fusing chunk-vector, claim-vector, and FTS5 seed channels.
struct FusedSeeds {
    activations: HashMap<String, f64>,
    relevance: HashMap<String, f64>,
    rrf: HashMap<String, f64>,
    /// Each vector channel's own effective cut, logged to its own EWMA.
    chunk_cut: Option<f64>,
    claim_cut: Option<f64>,
}

/// Build deterministic descending node ranking from a seed result for RRF.
fn ranked_node_list(seeds: &SeedResult) -> Vec<String> {
    let mut nodes: Vec<(String, f64)> = seeds
        .activations
        .iter()
        .map(|(id, &act)| {
            let score = seeds.relevance.get(id).copied().unwrap_or(act);
            (id.clone(), score)
        })
        .collect();
    nodes.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    nodes.into_iter().map(|(id, _)| id).collect()
}

/// Fuse the chunk and claim vector channels, then let FTS5 re-rank what they
/// found (Reciprocal Rank Fusion).
///
/// Only the vector channels decide what is relevant: an FTS hit adds rank to
/// a node they already admitted and never admits one by itself, because a
/// lexical OR match on one shared word is not relevance. The one exception
/// is `fts_rescues`, claims holding a *rare* exact term from the query (a
/// name, model number or identifier that embeddings blur), admitted at the
/// floor so they rank below every semantically scored item.
fn fuse_seed_channels(
    chunk_seeds: SeedResult,
    claim_seeds: SeedResult,
    fts_hits: &[(String, f64)],
    fts_rescues: &[String],
    floor: f64,
    centered_space: bool,
) -> FusedSeeds {
    let chunk_list = ranked_node_list(&chunk_seeds);
    let claim_list = ranked_node_list(&claim_seeds);

    let mut activations = chunk_seeds.activations;
    for (node, act) in claim_seeds.activations {
        activations
            .entry(node)
            .and_modify(|prev| *prev = prev.max(act))
            .or_insert(act);
    }

    let mut relevance = chunk_seeds.relevance;
    for (node, rel) in claim_seeds.relevance {
        relevance
            .entry(node)
            .and_modify(|prev| *prev = prev.max(rel))
            .or_insert(rel);
    }

    let mut rescued = 0usize;
    for node in fts_rescues {
        if rescued >= MAX_FTS_ONLY_SEEDS {
            break;
        }
        if activations.contains_key(node) {
            continue;
        }
        activations.insert(node.clone(), 0.5);
        if centered_space {
            relevance.insert(node.clone(), floor);
        }
        rescued += 1;
    }

    // RRF over the admitted set only, normalized to [0, 1] across 3 channels.
    let mut rrf_raw: HashMap<String, f64> = HashMap::new();
    for list in [&chunk_list, &claim_list] {
        for (idx, node) in list.iter().enumerate() {
            *rrf_raw.entry(node.clone()).or_default() += 1.0 / (RRF_K + (idx + 1) as f64);
        }
    }
    for (idx, (node, _)) in fts_hits.iter().enumerate() {
        if activations.contains_key(node) {
            *rrf_raw.entry(node.clone()).or_default() += 1.0 / (RRF_K + (idx + 1) as f64);
        }
    }
    let max_rrf = 3.0 / (RRF_K + 1.0);
    let rrf: HashMap<String, f64> = rrf_raw
        .into_iter()
        .map(|(k, v)| (k, (v / max_rrf).clamp(0.0, 1.0)))
        .collect();

    FusedSeeds {
        activations,
        relevance,
        rrf,
        chunk_cut: chunk_seeds.effective_cut,
        claim_cut: claim_seeds.effective_cut,
    }
}

/// The self-tuning floor's key for the claim channel of embedding space
/// `tag`: claims are short and score on a different scale from chunks, so
/// they keep an EWMA (and a corpus mean) of their own.
pub(crate) fn claim_space(tag: &str) -> String {
    format!("claims::{tag}")
}

/// Pass-1 seed output: per-node raw-cosine activation (drives spreading) and
/// per-node centered relevance (the primary ranking key), plus this query's
/// effective cut (the lowest kept centered score) for the self-tuning EWMA.
#[derive(Default)]
struct SeedResult {
    activations: HashMap<String, f64>,
    relevance: HashMap<String, f64>,
    effective_cut: Option<f64>,
    /// Whether a deeper search could add seeds: the search came back full
    /// (`k` hits, so the pool holds more) and its weakest hit still cleared
    /// the floor. When the shortfall came from the floor, every deeper hit
    /// (lower similarity) fails it too.
    could_widen: bool,
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
/// A scored hit: (node id, raw score, centered score if any).
type ScoredHit = (String, f64, Option<f64>);

fn adaptive_cut(scored: Vec<ScoredHit>, floor: f64, gap_min: f64) -> (Vec<ScoredHit>, Option<f64>) {
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
    /// The other side of an open claim-level dispute, `(item_id, claim)`, so
    /// recall shows both instead of a bare "(disputed)".
    pub disputed_by: Option<(String, String)>,
    /// Reached only through an analogy (`RHYMES_WITH`) edge from a direct
    /// hit: why the analogy holds. Shown under "Related by analogy" (4.7).
    pub analogy: Option<String>,
}

/// One turn's injection ([`Engine::recall_turn`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Recalled {
    pub text: String,
    /// Pass back to [`Engine::record_use`] with the items the model used.
    pub query_event_id: String,
    /// The items the block showed, in order.
    pub item_ids: Vec<String>,
}

/// A documented counter-view for the counterpoint channel
/// ([`Engine::counterpoint`]).
#[derive(Debug, Clone, PartialEq)]
pub struct CounterView {
    pub item_id: String,
    pub claim: String,
    pub citation: String,
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
        self.recall_turn(query, None).await.map(|r| r.text)
    }

    /// The per-turn injection with what the host needs to report use later:
    /// the block, the query event id, and the item ids it showed.
    ///
    /// `previous` is the chat's previous user message. A follow-up ("what
    /// about the second one?") says little on its own, so the query vector
    /// leans on the previous message too (current 0.75, previous 0.25),
    /// both read from the transcript, so the same turn always queries the
    /// same way.
    ///
    /// Showing items only *grounds* their chunks; it does not reinforce
    /// them. Reinforcement -- what keeps a chunk from decaying -- comes from
    /// [`Self::record_use`] once the host knows the model used an item, so
    /// a claim that is shown every turn but never used still fades.
    pub async fn recall_turn(&self, query: &str, previous: Option<&str>) -> Option<Recalled> {
        match self.recall_inner(query, previous).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("recall: soft-failed ({e}); injecting nothing");
                None
            }
        }
    }

    async fn recall_inner(&self, query: &str, previous: Option<&str>) -> Result<Option<Recalled>> {
        let (mut qvec, tag) = self.embedder.embed(query).await.map_err(crate::error::Error::Embed)?;
        if let Some(prev) = previous.map(str::trim).filter(|p| !p.is_empty()) {
            if tag != crate::config::HASH_EMBED_MODEL {
                if let Ok((pv, ptag)) = self.embedder.embed(prev).await {
                    if ptag == tag && pv.len() == qvec.len() {
                        let mean = if query.split_whitespace().count() <= SHORT_FOLLOWUP_MAX_WORDS {
                            None
                        } else {
                            self.corpus_mean(&tag).await.unwrap_or(None)
                        };
                        if should_blend_previous(query, &qvec, &pv, mean.as_deref()) {
                            qvec = blend_query(&qvec, &pv);
                        }
                    }
                }
            }
        }
        // The injection path (unlike read-only search) logs its cut for the
        // self-tuning floor (`cut_log`).
        let cap = self.config.injection_token_cap;
        // An item line costs well over `INDEX_ITEM_MIN_TOKENS`, so no more
        // than this many can ever fit in the block.
        let max_items = (cap / INDEX_ITEM_MIN_TOKENS).max(1);
        let items = self
            .ranked_items_from(qvec, tag, Some(query), true, max_items)
            .await?;
        if items.is_empty() {
            return Ok(None);
        }
        let (block, grounded, shown) = render_index_block(&items, cap);
        if grounded.is_empty() {
            return Ok(None);
        }
        let query_event_id = uuid::Uuid::new_v4().to_string();
        self.write_grounding(&grounded, &query_event_id).await?;
        Ok(Some(Recalled { text: block, query_event_id, item_ids: shown }))
    }

    /// Which of the `shown` items the model used this turn: looked up with
    /// `memorabilia_read_item` (`looked_up`), cited by id in `answer`, or
    /// carried into `answer` (at least 60% of the claim's content words, four
    /// letters or more, appear in it). Deterministic and offline.
    pub async fn used_items(&self, shown: &[String], answer: &str, looked_up: &[String]) -> Vec<String> {
        let lower = answer.to_lowercase();
        let words_of = |s: &str| -> Vec<String> {
            s.to_lowercase()
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.chars().count() >= 4)
                .map(str::to_string)
                .collect()
        };
        let answer_words: std::collections::HashSet<String> = words_of(answer).into_iter().collect();
        let mut used = Vec::new();
        for id in shown {
            if looked_up.contains(id) || lower.contains(&id.to_lowercase()) {
                used.push(id.clone());
                continue;
            }
            let Ok(Some(p)) = self.db.get_proposition(id).await else {
                continue;
            };
            let words = words_of(&p.claim);
            if words.len() >= 3 {
                let hits = words.iter().filter(|w| answer_words.contains(*w)).count();
                if hits * 10 >= words.len() * 6 {
                    used.push(id.clone());
                }
            }
        }
        used
    }

    /// The model used these items from an injection (looked one up, cited
    /// its id, or its answer carries the claim): reinforce their undisputed
    /// supporting chunks, which resets their decay clock. Disputed evidence
    /// is never reinforced. Idempotent per (chunk, event); best-effort.
    pub async fn record_use(&self, query_event_id: &str, item_ids: &[String]) {
        if let Err(e) = self.record_use_inner(query_event_id, item_ids).await {
            tracing::warn!("record_use: soft-failed ({e})");
        }
    }

    async fn record_use_inner(&self, query_event_id: &str, item_ids: &[String]) -> Result<()> {
        let event = format!("{query_event_id}:use");
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut rows: Vec<OutboxEntry> = Vec::new();
        let mut seen: std::collections::HashSet<String> = Default::default();
        for id in item_ids {
            for ch in self.db.list_active_supporting_chunks(id).await? {
                if !seen.insert(ch.chunk_id.clone()) {
                    continue;
                }
                if self.db.max_open_dispute_strength(&ch.chunk_id).await?.is_some()
                    || self.db.max_open_claim_dispute(id).await?.is_some()
                {
                    continue;
                }
                rows.push(OutboxEntry {
                    chunk_id: ch.chunk_id,
                    query_event_id: event.clone(),
                    grounded: false,
                    reinforced: true,
                    created_at: now.clone(),
                });
            }
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

    /// The ranked index for `query`, paginated — the `memorabilia_search`
    /// tool's data (read-only: search never grounds/reinforces; only the
    /// per-turn injection does, so one query event increments a chunk once).
    pub async fn search_index(
        &self,
        query: &str,
        offset: usize,
        limit: usize,
    ) -> Vec<IndexEntry> {
        let items = match self.ranked_items(query, offset.saturating_add(limit)).await {
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
        // Readable, but labelled: what replaced it, and when.
        let mut claim = prop.display_claim();
        if let Some(rel) = self.db.superseded_by(item_id).await? {
            if let Some(newer) = self.db.get_proposition(&rel.from_node).await? {
                claim = format!(
                    "{claim} [superseded {} by [{}]: {}]",
                    rel.opened_at.get(..10).unwrap_or(&rel.opened_at),
                    newer.node_id,
                    newer.display_claim()
                );
            }
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
            claim,
            confidence: prop.confidence,
            importance: prop.importance,
            urgency: prop.urgency,
            is_disputed: prop.is_disputed,
            chunks,
            total_chunks: total,
            offset,
        }))
    }

    /// The other side of `node_id`'s strongest open claim-level dispute, if
    /// it is still current: `(item_id, display claim)`.
    async fn dispute_partner(&self, node_id: &str) -> Result<Option<(String, String)>> {
        let mut best: Option<(f64, String)> = None;
        for r in self.db.open_relations_of(node_id).await? {
            if r.kind != "disputes" {
                continue;
            }
            let other = if r.from_node == node_id { r.to_node } else { r.from_node };
            if best.as_ref().is_none_or(|(s, _)| r.strength > *s) {
                best = Some((r.strength, other));
            }
        }
        let Some((_, other)) = best else {
            return Ok(None);
        };
        match self.db.get_proposition(&other).await? {
            Some(p) if p.status == "active" && self.db.superseded_by(&other).await?.is_none() => {
                Ok(Some((p.node_id.clone(), p.display_claim())))
            }
            _ => Ok(None),
        }
    }

    /// A documented counter-view to `stance` for the counterpoint channel
    /// (`[Worth challenging]`): among the items most relevant to it, the
    /// first with a current claim that disputes it, with its citation.
    /// Read-only. `None` when memory holds no such material -- the model
    /// then offers the other side from its own knowledge.
    pub async fn counterpoint(&self, stance: &str) -> Option<CounterView> {
        let items = self.ranked_items(stance, 5).await.ok()?;
        for item in items.iter().take(5) {
            let Some((other_id, other_claim)) = item.disputed_by.clone() else {
                continue;
            };
            let citation = self
                .db
                .list_active_supporting_chunks(&other_id)
                .await
                .ok()?
                .first()
                .map(|c| c.cluster_citation.clone())
                .unwrap_or_default();
            return Some(CounterView { item_id: other_id, claim: other_claim, citation });
        }
        None
    }

    /// The two-pass retrieval core, shared by recall and search. Read-only
    /// variant (does not update the EWMA); see [`Self::ranked_items_persist`].
    pub(crate) async fn ranked_items(&self, query: &str, max_items: usize) -> Result<Vec<RankedItem>> {
        self.ranked_items_persist(query, false, max_items).await
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
        max_items: usize,
    ) -> Result<Vec<RankedItem>> {
        let (qvec, tag) = self.embedder.embed(query).await.map_err(crate::error::Error::Embed)?;
        self.ranked_items_from(qvec, tag, Some(query), persist_cut, max_items)
            .await
    }

    /// [`Self::ranked_items_persist`] from an already-embedded query.
    ///
    /// At most `max_items` items: the first `max_items` of the full ranking.
    pub(crate) async fn ranked_items_from(
        &self,
        qvec: Vec<f32>,
        tag: String,
        query: Option<&str>,
        persist_cut: bool,
        max_items: usize,
    ) -> Result<Vec<RankedItem>> {
        // Pass 1: hybrid proposition seeding from three channels:
        // (1) dense vector search over chunks, (2) dense vector search directly
        // over extracted claim vectors (avoiding long-chunk dilution), and
        // (3) FTS5 BM25 over proposition claims (exact proper nouns/symbols).

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

        let mut chunk_seeds = self
            .seed_from_chunks(&qvec, &tag, self.config.retrieval_seed_chunks, mean_ref, floor)
            .await?;
        if chunk_seeds.activations.len() < self.config.retrieval_min_seeds && chunk_seeds.could_widen {
            let wider = (self.db.count_active_chunks().await?).max(1) as usize;
            if wider > self.config.retrieval_seed_chunks {
                chunk_seeds = self
                    .seed_from_chunks(&qvec, &tag, wider, mean_ref, floor)
                    .await?;
            }
        }

        // The claim channel is centered on the claim pool's own mean and
        // gated by its own self-tuning floor (claims are short and score on a
        // different scale from chunks).
        let claim_tag = claim_space(&tag);
        let claim_mean = if tag == crate::config::HASH_EMBED_MODEL {
            None
        } else {
            self.claim_mean(&tag).await.unwrap_or(None)
        };
        let claim_mean = claim_mean.filter(|m| m.len() == qvec.len());
        let claim_floor = self
            .operational_floor(&claim_tag, claim_mean.is_some())
            .await;
        let claim_seeds = self
            .seed_from_claims(
                &qvec,
                &tag,
                self.config.retrieval_seed_chunks,
                claim_mean.as_deref(),
                claim_floor,
            )
            .await?;

        let (fts_hits, fts_rescues) = match query {
            Some(q) if !q.trim().is_empty() => (
                self.db.search_propositions_fts(q, 20).await.unwrap_or_default(),
                self.db
                    .rare_term_propositions(q, FTS_RARE_TERM_MAX_DF, MAX_FTS_ONLY_SEEDS)
                    .await
                    .unwrap_or_default(),
            ),
            _ => (Vec::new(), Vec::new()),
        };

        let chunk_found = !chunk_seeds.activations.is_empty();
        let claim_found = !claim_seeds.activations.is_empty();
        let seeds = fuse_seed_channels(
            chunk_seeds,
            claim_seeds,
            &fts_hits,
            &fts_rescues,
            floor,
            mean_ref.is_some(),
        );

        // Each channel logs its own cut; a channel whose elevated floor
        // rejected everything logs the base floor instead, so empty turns
        // gently relax an over-tight floor back down.
        if persist_cut {
            let hard = self.config.recall_min_cosine;
            let logs = [
                (&tag, mean_ref.is_some(), chunk_found, floor, seeds.chunk_cut),
                (&claim_tag, claim_mean.is_some(), claim_found, claim_floor, seeds.claim_cut),
            ];
            for (space, centered, found, space_floor, cut) in logs {
                if !centered {
                    continue;
                }
                let value = if found {
                    cut
                } else if space_floor > hard {
                    Some(hard)
                } else {
                    None
                };
                if let Some(v) = value {
                    if let Err(e) = cut_log::log(&self.db, space, v).await {
                        tracing::debug!("recall: could not log the recall cut for {space}: {e}");
                    }
                }
            }
        }

        if seeds.activations.is_empty() {
            return Ok(Vec::new());
        }

        let relevance = seeds.relevance;
        let rrf_scores = seeds.rrf;
        let seeded: std::collections::HashSet<String> = seeds.activations.keys().cloned().collect();

        // Pass 2: bounded spreading activation over proposition edges.
        let activations = self.spread_activation(seeds.activations).await?;

        let ids: Vec<String> = activations.keys().cloned().collect();
        let props: HashMap<String, crate::store::propositions::Proposition> = self
            .db
            .get_propositions(&ids)
            .await?
            .into_iter()
            .map(|p| (p.node_id.clone(), p))
            .collect();
        let mut candidates: Vec<(crate::store::propositions::Proposition, f64, Option<f64>, f64)> =
            Vec::new();
        for (node_id, activation) in activations {
            let Some(prop) = props.get(&node_id) else {
                continue;
            };
            if prop.status != "active" {
                continue;
            }
            let conf_factor = 0.75 + 0.25 * prop.confidence.clamp(0.0, 1.0);
            let boost = core::metaboost(&prop.urgency, &prop.importance, &self.config);
            let priority = core::priority(activation, boost, self.config.metaboost_cap) * conf_factor;
            let rrf = rrf_scores.get(&node_id).copied().unwrap_or(0.0);
            candidates.push((prop.clone(), priority, relevance.get(&node_id).copied(), rrf));
        }
        // Primary: relevance + RRF fusion modulated mildly by claim confidence
        // (for directly-seeded centered items). Uncentered / spread-only items
        // (`relevance = None`) fall after directly-scored items and sort by
        // confidence-weighted Priority, then RRF, then id.
        candidates.sort_by(|(pa, prio_a, rel_a, rrf_a), (pb, prio_b, rel_b, rrf_b)| {
            let key_a = composite_relevance_key(*rel_a, *rrf_a, pa.confidence);
            let key_b = composite_relevance_key(*rel_b, *rrf_b, pb.confidence);
            key_b
                .partial_cmp(&key_a)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| prio_b.partial_cmp(prio_a).unwrap_or(std::cmp::Ordering::Equal))
                .then_with(|| rrf_b.partial_cmp(rrf_a).unwrap_or(std::cmp::Ordering::Equal))
                .then_with(|| pa.node_id.cmp(&pb.node_id))
        });

        let mut items: Vec<RankedItem> = Vec::new();
        let mut analogies = 0usize;
        for (prop, priority, rel, _rrf) in candidates {
            let node_id = prop.node_id.clone();
            // Past the cut, only analogies are still wanted (the block sets
            // room aside for up to `ANALOGY_SLOTS` of them, wherever they
            // rank), and only spread-reached items can be one.
            let past_cut = items.len() >= max_items;
            if past_cut && (analogies >= ANALOGY_SLOTS || seeded.contains(&node_id)) {
                if analogies >= ANALOGY_SLOTS {
                    break;
                }
                continue;
            }
            // A superseded claim was true but no longer is (4.4): it stays
            // readable through `render_item`, never recalled as current.
            if self.db.superseded_by(&node_id).await?.is_some() {
                continue;
            }
            let supporters = self.db.list_active_support_refs(&node_id).await?;
            // A proposition with no active support has left the graph (plan
            // §5.3); never surface it.
            if supporters.is_empty() {
                continue;
            }
            let mut citations: Vec<String> = Vec::new();
            let mut supporting_chunk_ids: Vec<String> = Vec::new();
            for (chunk_id, citation) in supporters {
                if !citations.contains(&citation) {
                    citations.push(citation);
                }
                supporting_chunk_ids.push(chunk_id);
            }
            let disputed_by = self.dispute_partner(&node_id).await?;
            // Reached only by spreading: through an analogy edge from a
            // direct hit, carrying that edge's rationale.
            let analogy = if seeded.contains(&node_id) {
                None
            } else {
                let mut why = None;
                for e in self.db.list_proposition_edges_to(&node_id).await? {
                    if e.edge_type == "RHYMES_WITH" && seeded.contains(&e.from_node) {
                        why = Some(self.analogy_rationale(&e.from_node, &node_id).await.unwrap_or_default());
                        break;
                    }
                }
                why
            };
            if past_cut && analogy.is_none() {
                continue;
            }
            if analogy.is_some() {
                analogies += 1;
            }
            let claim = prop.display_claim();
            items.push(RankedItem {
                item_id: prop.node_id,
                claim,
                confidence: prop.confidence,
                importance: prop.importance,
                urgency: prop.urgency,
                is_disputed: prop.is_disputed,
                priority,
                relevance: rel,
                citations,
                supporting_chunk_ids,
                disputed_by,
                analogy,
            });
        }
        Ok(items)
    }

    /// Maximum amount the self-tuning EWMA may raise the operational floor
    /// above the configured `recall_min_cosine`, preventing a one-way ratchet
    /// from locking out normal queries.
    const MAX_EWMA_FLOOR_LIFT: f64 = 0.10;

    /// The per-turn operational floor: the hard `recall_min_cosine` raised
    /// toward the running EWMA cut (minus slack, capped at `hard + MAX_EWMA_FLOOR_LIFT`)
    /// when one exists for this space. Returns the hard floor unchanged in the
    /// hash space or before any EWMA has been recorded. Soft-fails to the hard
    /// floor on any read error.
    async fn operational_floor(&self, tag: &str, centered: bool) -> f64 {
        let hard = self.config.recall_min_cosine;
        if !centered {
            return hard;
        }
        let max_floor = hard + Self::MAX_EWMA_FLOOR_LIFT;
        match cut_log::snapshot(&self.db, tag).await {
            Some(ewma) => hard.max(ewma - self.config.recall_ewma_slack).min(max_floor),
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
        // When mean-centering is active, over-fetch raw-cosine candidates so
        // anisotropic common-direction crowding doesn't push relevant hits out
        // of the raw top-k before centering runs.
        let fetch_k = if mean.is_some() { k.max(64) } else { k };
        // Each hit comes back with its vector, so the centered re-score below
        // does not read the same vectors a second time.
        let hits_with_vectors = self
            .vectors
            .search_with_vectors(qvec, tag, fetch_k)
            .await
            .map_err(crate::error::Error::Internal)?;
        let mut hit_vectors: HashMap<String, Vec<f32>> = HashMap::new();
        let hits: Vec<(String, f32)> = hits_with_vectors
            .into_iter()
            .map(|(id, cos, v)| {
                if let Some(v) = v {
                    hit_vectors.insert(id.clone(), v);
                }
                (id, cos)
            })
            .collect();

        // (chunk_id, raw cosine, centered cosine?) — centered only when we can
        // fetch the chunk's vector in a matching space.
        let scored: Vec<(String, f64, Option<f64>)> = match mean {
            Some(mu) if mu.len() == qvec.len() => {
                // Only vectors the search could not return are fetched.
                let missing: Vec<String> = hits
                    .iter()
                    .map(|(id, _)| id.clone())
                    .filter(|id| !hit_vectors.contains_key(id))
                    .collect();
                let mut vecs = hit_vectors;
                if !missing.is_empty() {
                    vecs.extend(self.db.fetch_vectors(&missing).await?);
                }
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
        let weakest_centered = scored
            .iter()
            .filter_map(|(_, _, cc)| *cc)
            .fold(f64::INFINITY, f64::min);
        let could_widen = scored.len() >= fetch_k && (!centered_present || weakest_centered >= floor);
        let (mut kept, effective_cut) = if centered_present {
            adaptive_cut(scored, floor, self.config.recall_gap_min)
        } else {
            (scored, None)
        };
        if centered_present {
            kept.sort_by(|a, b| {
                let sa = a.2.unwrap_or(a.1);
                let sb = b.2.unwrap_or(b.1);
                sb.partial_cmp(&sa)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.0.cmp(&b.0))
            });
            kept.truncate(k.max(self.config.retrieval_seed_chunks));
        }

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
        Ok(SeedResult { activations, relevance, effective_cut, could_widen })
    }

    /// Pass-1 claim-level seed channel over `claim_vectors`: matches the query
    /// directly against short extracted claims, so a concise fact is not
    /// diluted by the long chunk it came from.
    ///
    /// The nearest claims by raw cosine come from SQL (sqlite-vec scores them
    /// there, so no vector crosses into Rust but the over-fetched few), then
    /// are re-scored centered on the claim pool's cached mean and cut at the
    /// claim channel's own floor, exactly like the chunk channel.
    async fn seed_from_claims(
        &self,
        qvec: &[f32],
        tag: &str,
        k: usize,
        mean: Option<&[f32]>,
        floor: f64,
    ) -> Result<SeedResult> {
        let is_hash = tag == crate::config::HASH_EMBED_MODEL;
        // A semantic space with no mean yet (fewer than two claims) has no
        // meaningful relevance scale: leave it to the chunk channel.
        if !is_hash && mean.is_none() {
            return Ok(SeedResult::default());
        }
        let fetch_k = if mean.is_some() { k.max(64) } else { k };
        let hits = self.db.nearest_claims(qvec, tag, fetch_k).await?;
        let qc = mean.map(|mu| sub(qvec, mu));
        let mut scored: Vec<ScoredHit> = Vec::with_capacity(hits.len());
        for (node_id, raw, v) in hits {
            if is_hash && raw <= 0.0 {
                continue;
            }
            let cc = match (&qc, mean) {
                (Some(q_centered), Some(mu)) if v.len() == mu.len() => {
                    Some(cosine(q_centered, &sub(&v, mu)))
                }
                _ => None,
            };
            scored.push((node_id, raw, cc));
        }
        if scored.is_empty() {
            return Ok(SeedResult::default());
        }

        let centered_present = mean.is_some();
        let (mut kept, effective_cut) = if centered_present {
            adaptive_cut(scored, floor, self.config.recall_gap_min)
        } else {
            (scored, None)
        };
        kept.sort_by(|a, b| {
            let sa = a.2.unwrap_or(a.1);
            let sb = b.2.unwrap_or(b.1);
            sb.partial_cmp(&sa)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        kept.truncate(k.max(1));

        let mut activations = HashMap::new();
        let mut relevance = HashMap::new();
        for (node_id, raw, cc) in kept {
            activations.insert(node_id.clone(), raw.clamp(0.0, 1.0));
            if let Some(c) = cc {
                relevance.insert(node_id, c);
            }
        }
        Ok(SeedResult {
            activations,
            relevance,
            effective_cut,
            could_widen: false,
        })
    }

    /// The claim pool's mean in `tag`'s space, cached in `app_settings` and
    /// refreshed on the same rules as [`Self::corpus_mean`]. `None` under two
    /// claims.
    pub(crate) async fn claim_mean(&self, tag: &str) -> Result<Option<Vec<f32>>> {
        let count = self.db.count_active_claim_vectors(tag).await?;
        if count < Self::MIN_CORPUS_MEAN_CHUNKS {
            return Ok(None);
        }
        let space = claim_space(tag);
        let key = format!("corpus_mean::{space}");
        let count_key = format!("corpus_mean_count::{space}");
        let cached_count = self
            .db
            .get_setting(&count_key)
            .await?
            .and_then(|s| s.parse::<usize>().ok());
        if cached_count.is_some_and(|c| mean_cache_fresh(c, count)) {
            if let Some(s) = self.db.get_setting(&key).await? {
                if let Ok(v) = serde_json::from_str::<Vec<f32>>(&s) {
                    if !v.is_empty() {
                        return Ok(Some(v));
                    }
                }
            }
        }
        self.refresh_claim_mean(tag).await
    }

    /// Recompute and cache the claim pool's mean (maintenance, or a stale
    /// cache on the hot path). Clears the cache under two claims.
    pub(crate) async fn refresh_claim_mean(&self, tag: &str) -> Result<Option<Vec<f32>>> {
        let space = claim_space(tag);
        let key = format!("corpus_mean::{space}");
        let count_key = format!("corpus_mean_count::{space}");
        match self.db.claim_mean_vector_with_count(tag).await? {
            Some((mean, n)) if n >= Self::MIN_CORPUS_MEAN_CHUNKS => {
                if let Ok(s) = serde_json::to_string(&mean) {
                    self.db.set_setting(&key, &s).await?;
                    self.db.set_setting(&count_key, &n.to_string()).await?;
                }
                Ok(Some(mean))
            }
            _ => {
                let _ = self.db.delete_setting(&key).await;
                let _ = self.db.delete_setting(&count_key).await;
                Ok(None)
            }
        }
    }

    /// Minimum number of active vectors needed before mean-centering is
    /// meaningful (with 1 vector, `v - μ = 0` and centered cosine is always 0).
    const MIN_CORPUS_MEAN_CHUNKS: usize = 2;

    /// The centered-cosine relevance gate's corpus mean (`recall_min_cosine`),
    /// cached in `app_settings` per embedding space. Returns `None` when the
    /// space has fewer than 2 active vectors (preventing 1-vector self-cancelling
    /// degeneracy). Automatically refreshes when a small pool (`< 50` chunks)
    /// changes size or a larger pool drifts by `>= 25%`.
    pub(crate) async fn corpus_mean(&self, tag: &str) -> Result<Option<Vec<f32>>> {
        let count = self.db.count_active_chunks_for_model(tag).await?;
        if count < Self::MIN_CORPUS_MEAN_CHUNKS {
            return Ok(None);
        }
        let key = format!("corpus_mean::{tag}");
        let count_key = format!("corpus_mean_count::{tag}");
        let cached_count = self
            .db
            .get_setting(&count_key)
            .await?
            .and_then(|s| s.parse::<usize>().ok());
        let cache_fresh = cached_count.is_some_and(|c| mean_cache_fresh(c, count));
        if cache_fresh {
            if let Some(s) = self.db.get_setting(&key).await? {
                if let Ok(v) = serde_json::from_str::<Vec<f32>>(&s) {
                    if !v.is_empty() {
                        return Ok(Some(v));
                    }
                }
            }
        }
        let Some((mean, n)) = self.db.corpus_mean_vector_with_count(tag).await? else {
            return Ok(None);
        };
        if n < Self::MIN_CORPUS_MEAN_CHUNKS {
            return Ok(None);
        }
        if let Ok(s) = serde_json::to_string(&mean) {
            self.db.set_setting(&key, &s).await?;
            self.db.set_setting(&count_key, &n.to_string()).await?;
        }
        Ok(Some(mean))
    }

    /// Recompute and persist the corpus mean for `tag` (maintenance heavy
    /// pass), so the centered-cosine gate tracks the growing/decaying pool
    /// instead of freezing at the first query's snapshot.
    pub(crate) async fn refresh_corpus_mean(&self, tag: &str) -> Result<()> {
        let key = format!("corpus_mean::{tag}");
        let count_key = format!("corpus_mean_count::{tag}");
        match self.db.corpus_mean_vector_with_count(tag).await? {
            Some((mean, n)) if n >= Self::MIN_CORPUS_MEAN_CHUNKS => {
                if let Ok(s) = serde_json::to_string(&mean) {
                    self.db.set_setting(&key, &s).await?;
                    self.db.set_setting(&count_key, &n.to_string()).await?;
                }
            }
            _ => {
                let _ = self.db.delete_setting(&key).await;
                let _ = self.db.delete_setting(&count_key).await;
            }
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
    async fn write_grounding(&self, grounded_chunk_ids: &[String], query_event_id: &str) -> Result<()> {
        if grounded_chunk_ids.is_empty() {
            return Ok(());
        }
        let query_event_id = query_event_id.to_string();
        // The outbox `created_at` is FIFO bookkeeping for the drain, not decay
        // math (which is anchored on chunks), so the query-event wall clock is
        // the right stamp here — recall takes no caller `now`.
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        // Grounding only: being shown is not being used. Reinforcement comes
        // from `record_use` (4.5).
        let mut rows: Vec<OutboxEntry> = Vec::new();
        for chunk_id in grounded_chunk_ids {
            rows.push(OutboxEntry {
                chunk_id: chunk_id.clone(),
                query_event_id: query_event_id.clone(),
                grounded: true,
                reinforced: false,
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
fn render_index_block(items: &[RankedItem], token_cap: usize) -> (String, Vec<String>, Vec<String>) {
    // Up to two analogies get their own section; room for it is set aside
    // before the direct items fill the cap, so they are not the first thing
    // cut.
    let analogies: Vec<&RankedItem> = items.iter().filter(|i| i.analogy.is_some()).take(ANALOGY_SLOTS).collect();
    let analogy_lines: Vec<String> = analogies
        .iter()
        .map(|i| {
            let why = i.analogy.as_deref().filter(|w| !w.is_empty()).map(|w| format!(" — why: {w}")).unwrap_or_default();
            format!(
                "- [{}] {}{} — {}\n",
                i.item_id,
                i.claim,
                why,
                i.citations.first().map(String::as_str).unwrap_or("(uncited)"),
            )
        })
        .collect();
    let analogy_header = "\nRelated by analogy (a different setting, the same pattern):\n";
    let reserved = if analogy_lines.is_empty() {
        0
    } else {
        approx_tokens(analogy_header) + analogy_lines.iter().map(|l| approx_tokens(l)).sum::<usize>()
    };
    let direct: Vec<&RankedItem> = items.iter().filter(|i| i.analogy.is_none()).collect();
    if direct.is_empty() && analogies.is_empty() {
        return (String::new(), Vec::new(), Vec::new());
    }
    let top = direct.first().copied().unwrap_or(&items[0]);
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
    let mut shown: Vec<String> = Vec::new();
    let direct_cap = token_cap.saturating_sub(reserved);
    for item in direct {
        let disputed = match (&item.disputed_by, item.is_disputed) {
            (Some((id, claim)), _) => format!(" (disputed by [{id}]: {claim})"),
            (None, true) => " (disputed)".to_string(),
            (None, false) => String::new(),
        };
        let line = format!(
            "- [{}] {}{} — conf {:.2} — {}\n",
            item.item_id,
            item.claim,
            disputed,
            item.confidence,
            item.citations.first().map(String::as_str).unwrap_or("(uncited)"),
        );
        // Keep whole items while they fit; stop before exceeding the cap.
        if approx_tokens(&out) + approx_tokens(&line) > direct_cap {
            break;
        }
        out.push_str(&line);
        shown.push(item.item_id.clone());
        for chunk_id in &item.supporting_chunk_ids {
            if !grounded.contains(chunk_id) {
                grounded.push(chunk_id.clone());
            }
        }
    }
    if !analogy_lines.is_empty() && approx_tokens(&out) + reserved <= token_cap {
        out.push_str(analogy_header);
        for (item, line) in analogies.iter().zip(&analogy_lines) {
            out.push_str(line);
            shown.push(item.item_id.clone());
            for chunk_id in &item.supporting_chunk_ids {
                if !grounded.contains(chunk_id) {
                    grounded.push(chunk_id.clone());
                }
            }
        }
    }
    (out, grounded, shown)
}

/// Index slots reserved for items reached by analogy (4.7).
const ANALOGY_SLOTS: usize = 2;

/// A lower bound on one index line's cost (`- [<26-char id>] claim — citation`
/// is over 40 characters, `approx_tokens` counts four per token), so a block
/// of `injection_token_cap` tokens never holds more than `cap / this` items.
const INDEX_ITEM_MIN_TOKENS: usize = 10;

/// Whether a mean cached over `cached` vectors still describes a pool of
/// `count`: exactly, under 50 (a small pool moves with every item); within
/// 25% drift above that.
fn mean_cache_fresh(cached: usize, count: usize) -> bool {
    if cached < 2 {
        return false;
    }
    if count < 50 {
        cached == count
    } else {
        count.abs_diff(cached) * 4 < cached
    }
}

/// Queries up to this many words are treated as follow-ups that always lean on
/// the previous user message; longer queries only blend when topic-continuous.
const SHORT_FOLLOWUP_MAX_WORDS: usize = 8;

/// Whether `previous` should be blended into the recall query vector.
/// Short follow-ups ("what about the second one?") always blend; longer queries
/// only blend when they remain topically close to the previous message.
fn should_blend_previous(
    query: &str,
    qvec: &[f32],
    pvec: &[f32],
    mean: Option<&[f32]>,
) -> bool {
    if query.split_whitespace().count() <= SHORT_FOLLOWUP_MAX_WORDS {
        return true;
    }
    match mean {
        Some(mu) if mu.len() == qvec.len() && mu.len() == pvec.len() => {
            cosine(&sub(qvec, mu), &sub(pvec, mu)) >= 0.15
        }
        _ => cosine(qvec, pvec) >= 0.65,
    }
}

/// The follow-up query vector: the current message, leaning a quarter on
/// the previous one, renormalised.
fn blend_query(current: &[f32], previous: &[f32]) -> Vec<f32> {
    let mut v: Vec<f32> = current.iter().zip(previous).map(|(c, p)| 0.75 * c + 0.25 * p).collect();
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
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
    fn composite_relevance_key_sorts_scored_above_unscored() {
        assert!(
            composite_relevance_key(Some(-0.9), 0.0, 0.5)
                > composite_relevance_key(None, 1.0, 1.0),
            "any real score beats a spread-only item"
        );
    }

    #[test]
    fn previous_blend_gates_on_length_or_topic_continuity() {
        let q = [1.0f32, 0.0, 0.0];
        let p_other = [0.0f32, 1.0, 0.0];
        let p_same = [0.9f32, 0.43, 0.0];
        // Short follow-up always blends even if orthogonal.
        assert!(should_blend_previous("what about the second one?", &q, &p_other, None));
        // Long query with topic switch does not blend.
        assert!(!should_blend_previous(
            "Now let us switch topics completely to baking sourdough bread at home",
            &q,
            &p_other,
            None
        ));
        // Long query on the same topic blends.
        assert!(should_blend_previous(
            "Can you tell me more details about that same database replication setup",
            &q,
            &p_same,
            None
        ));
    }

    #[tokio::test]
    async fn corpus_mean_skips_single_chunk_and_refreshes_when_small_pool_grows() {
        use std::sync::Arc;
        use crate::store::chunks::Chunk;
        use crate::store::vectors::SqliteVectorIndex;
        use crate::store::Db;
        use crate::traits::MockChat;

        let cfg = crate::config::Config::default();
        let db = Db::open_in_memory().await.unwrap();
        db.init_vectors(3).await.unwrap();
        let vectors = Arc::new(SqliteVectorIndex::new(db.pool().clone()));
        let engine = Engine::new(
            cfg,
            db,
            Arc::new(MockChat { response: serde_json::json!({}) }),
            Arc::new(crate::embed::HashEmbedder::new(3)),
            vectors,
        );

        use crate::store::documents::Document;

        engine
            .db
            .insert_document(&Document {
                document_hash: "doc1".into(),
                source_entity: "src".into(),
                source_type: "Scraped".into(),
                source_name: "src".into(),
                captured_at: "2026-08-01T10:00:00Z".into(),
                intent_factor: 1.0,
                chunk_count: 3,
                created_at: "2026-08-01T10:00:00Z".into(),
            })
            .await
            .unwrap();

        let make_chunk = |id: &str| Chunk {
            chunk_id: id.into(),
            content: id.into(),
            content_hash: id.into(),
            document_hash: "doc1".into(),
            source_entity: "src".into(),
            source_reliability: 0.8,
            provenance_cluster_id: "cl".into(),
            cluster_citation: "src".into(),
            status: "active".into(),
            extraction_status: "done".into(),
            extraction_error_at: None,
            embedding_model: "m1".into(),
            decay_class: "static".into(),
            anchor_at: "2026-08-01T10:00:00Z".into(),
            urgency_expires_at: None,
            reinforcement_count: 0,
            grounding_count: 0,
            archived_at: None,
            created_at: "2026-08-01T10:00:00Z".into(),
        };

        engine.db.insert_chunk(&make_chunk("c1")).await.unwrap();
        engine.vectors.upsert("c1", &[1.0, 0.0, 0.0], "m1").await.unwrap();
        assert!(
            engine.corpus_mean("m1").await.unwrap().is_none(),
            "single-chunk pool must not return a degenerate self-cancelling mean"
        );

        engine.db.insert_chunk(&make_chunk("c2")).await.unwrap();
        engine.vectors.upsert("c2", &[0.0, 1.0, 0.0], "m1").await.unwrap();
        let m2 = engine.corpus_mean("m1").await.unwrap().expect("2-chunk pool computes mean");
        assert!((m2[0] - 0.5).abs() < 1e-5 && (m2[1] - 0.5).abs() < 1e-5);

        engine.db.insert_chunk(&make_chunk("c3")).await.unwrap();
        engine.vectors.upsert("c3", &[0.0, 0.0, 1.0], "m1").await.unwrap();
        let m3 = engine.corpus_mean("m1").await.unwrap().expect("small pool refreshes mean");
        assert!((m3[2] - (1.0 / 3.0)).abs() < 1e-4);
    }

    #[tokio::test]
    async fn operational_floor_caps_ewma_lift() {
        use std::sync::Arc;
        use crate::store::vectors::SqliteVectorIndex;
        use crate::store::Db;
        use crate::traits::MockChat;

        let cfg = crate::config::Config::default();
        let hard = cfg.recall_min_cosine;
        let db = Db::open_in_memory().await.unwrap();
        db.init_vectors(cfg.embedding_dim).await.unwrap();
        let vectors = Arc::new(SqliteVectorIndex::new(db.pool().clone()));
        let engine = Engine::new(
            cfg,
            db,
            Arc::new(MockChat { response: serde_json::json!({}) }),
            Arc::new(crate::embed::HashEmbedder::new(8)),
            vectors,
        );
        cut_log::log(&engine.db, "m1", 0.90).await.unwrap();
        cut_log::fold_pending(&engine.db, 1.0).await.unwrap();
        let floor = engine.operational_floor("m1", true).await;
        assert!(
            (floor - (hard + Engine::MAX_EWMA_FLOOR_LIFT)).abs() < 1e-12,
            "EWMA lift must be capped at +0.10 above hard floor, got {floor}"
        );
    }

    #[test]
    fn rrf_fusion_reranks_admitted_items_and_rewards_multi_channel_overlap() {
        let mut chunk_seeds = SeedResult::default();
        chunk_seeds.activations.insert("p_chunk_only".into(), 0.7);
        chunk_seeds.activations.insert("p_multi".into(), 0.65);
        chunk_seeds.relevance.insert("p_chunk_only".into(), 0.40);
        chunk_seeds.relevance.insert("p_multi".into(), 0.35);

        let mut claim_seeds = SeedResult::default();
        claim_seeds.activations.insert("p_multi".into(), 0.85);
        claim_seeds.relevance.insert("p_multi".into(), 0.55);

        let fts_hits = vec![
            ("p_fts_only".into(), 3.2),
            ("p_multi".into(), 2.1),
        ];

        let fused = fuse_seed_channels(chunk_seeds, claim_seeds, &fts_hits, &[], 0.0, true);
        assert!(
            !fused.activations.contains_key("p_fts_only"),
            "a lexical hit the vector channels did not admit stays out"
        );
        assert!(
            fused.rrf["p_multi"] > fused.rrf["p_chunk_only"],
            "multi-channel overlap receives higher RRF score"
        );
        assert!(
            composite_relevance_key(fused.relevance.get("p_multi").copied(), fused.rrf["p_multi"], 0.9)
                > composite_relevance_key(fused.relevance.get("p_chunk_only").copied(), fused.rrf["p_chunk_only"], 0.9)
        );
        assert!(
            composite_relevance_key(Some(0.4), 0.5, 0.95)
                > composite_relevance_key(Some(0.4), 0.5, 0.25),
            "higher proposition confidence improves composite score at equal relevance"
        );
    }

    #[test]
    fn rare_term_rescue_enters_at_the_floor_below_scored_items() {
        let mut chunk_seeds = SeedResult::default();
        chunk_seeds.activations.insert("semantic".into(), 0.7);
        chunk_seeds.relevance.insert("semantic".into(), 0.30);
        let fused = fuse_seed_channels(
            chunk_seeds,
            SeedResult::default(),
            &[("rare".into(), 4.0)],
            &["rare".to_string()],
            0.12,
            true,
        );
        assert_eq!(fused.relevance["rare"], 0.12, "admitted exactly at the floor");
        assert!(
            composite_relevance_key(Some(fused.relevance["semantic"]), fused.rrf["semantic"], 0.5)
                > composite_relevance_key(Some(fused.relevance["rare"]), fused.rrf["rare"], 0.5)
        );
    }

    #[test]
    fn mean_cache_freshness_rules() {
        assert!(!mean_cache_fresh(1, 1));
        assert!(mean_cache_fresh(10, 10));
        assert!(!mean_cache_fresh(10, 11));
        assert!(mean_cache_fresh(100, 120));
        assert!(!mean_cache_fresh(100, 130));
    }
}
