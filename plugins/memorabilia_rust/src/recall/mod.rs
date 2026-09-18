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
    /// `Priority(V) = Activation · MetaBoost` (plan §10.2).
    pub priority: f64,
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
        let items = self.ranked_items(query).await?;
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

    /// The two-pass retrieval core, shared by recall and search. Ranked by
    /// `Priority` descending, ties broken by `item_id` so repeated calls
    /// agree byte-for-byte (plan §13.4).
    pub(crate) async fn ranked_items(&self, query: &str) -> Result<Vec<RankedItem>> {
        // Pass 1: seed propositions from a vector search over chunks.
        let (qvec, tag) = self.embedder.embed(query).await.map_err(crate::error::Error::Embed)?;

        let mut seeds = self.seed_from_chunks(&qvec, &tag, self.config.retrieval_seed_chunks).await?;
        if seeds.len() < self.config.retrieval_min_seeds {
            // Widen Pass 1 over the whole active pool once (plan §10.1) — the
            // brute-force index already pays a full scan per search.
            let wider = (self.db.count_active_chunks().await?).max(1) as usize;
            if wider > self.config.retrieval_seed_chunks {
                seeds = self.seed_from_chunks(&qvec, &tag, wider).await?;
            }
        }
        if seeds.is_empty() {
            return Ok(Vec::new());
        }

        // Pass 2: bounded spreading activation over proposition edges.
        let activations = self.spread_activation(seeds).await?;

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
                citations,
                supporting_chunk_ids,
            });
        }
        items.sort_by(|a, b| {
            b.priority
                .partial_cmp(&a.priority)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.item_id.cmp(&b.item_id))
        });
        Ok(items)
    }

    /// Pass 1: seed proposition activations from the top-`k` chunk search.
    /// Each proposition's seed activation is `max CosSim` over its retrieved
    /// supporting chunks (plan §10.1). `pending` chunks carry no active
    /// propositions, so they contribute nothing (invisible to recall).
    async fn seed_from_chunks(
        &self,
        qvec: &[f32],
        tag: &str,
        k: usize,
    ) -> Result<HashMap<String, f64>> {
        let hits = self
            .vectors
            .search(qvec, tag, k)
            .await
            .map_err(crate::error::Error::Internal)?;
        // node_id -> cosines of the retrieved chunks that support it.
        let mut per_node: HashMap<String, Vec<f64>> = HashMap::new();
        for (chunk_id, cos) in hits {
            for prop in self.db.list_propositions_for_chunk(&chunk_id).await? {
                per_node.entry(prop.node_id).or_default().push(cos as f64);
            }
        }
        Ok(per_node
            .into_iter()
            .map(|(node, cosines)| (node, core::seed_activation(&cosines)))
            .collect())
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
