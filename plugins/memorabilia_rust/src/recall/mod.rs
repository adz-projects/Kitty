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
                        qvec = blend_query(&qvec, &pv);
                    }
                }
            }
        }
        // The injection path (unlike read-only search) logs its cut for the
        // self-tuning floor (`cut_log`).
        let items = self.ranked_items_from(qvec, tag, true).await?;
        if items.is_empty() {
            return Ok(None);
        }
        let cap = self.config.injection_token_cap;
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
        let items = self.ranked_items(stance).await.ok()?;
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
        let (qvec, tag) = self.embedder.embed(query).await.map_err(crate::error::Error::Embed)?;
        self.ranked_items_from(qvec, tag, persist_cut).await
    }

    /// [`Self::ranked_items_persist`] from an already-embedded query.
    pub(crate) async fn ranked_items_from(
        &self,
        qvec: Vec<f32>,
        tag: String,
        persist_cut: bool,
    ) -> Result<Vec<RankedItem>> {
        // Pass 1: seed propositions from a vector search over chunks.

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
        let seeded: std::collections::HashSet<String> = seeds.activations.keys().cloned().collect();

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
            // A superseded claim was true but no longer is (4.4): it stays
            // readable through `render_item`, never recalled as current.
            if self.db.superseded_by(&node_id).await?.is_some() {
                continue;
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
            let claim = prop.display_claim();
            items.push(RankedItem {
                item_id: prop.node_id,
                claim,
                confidence: prop.confidence,
                importance: prop.importance,
                urgency: prop.urgency,
                is_disputed: prop.is_disputed,
                priority,
                relevance: relevance.get(&node_id).copied(),
                citations,
                supporting_chunk_ids,
                disputed_by,
                analogy,
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
        if let Some(mean) = self.db.corpus_mean_vector(tag).await? {
            if let Ok(s) = serde_json::to_string(&mean) {
                self.db.set_setting(&key, &s).await?;
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
    fn rel_key_sorts_scored_above_unscored() {
        assert!(rel_key(Some(-0.9)) > rel_key(None), "any real score beats a spread-only item");
    }
}
