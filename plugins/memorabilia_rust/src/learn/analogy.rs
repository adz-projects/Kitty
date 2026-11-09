//! Analogy edges (Phase 4.7): indirect relevance.
//!
//! Direct recall finds what is *about* the question. Some of the most useful
//! material is about something else entirely but shares its shape -- a novel
//! about a society that trades struggle for engineered comfort, for a
//! question about students leaning on AI. Embedding the literal claims cannot
//! see that: the two are far apart. So each claim also gets a one-sentence
//! **abstraction** -- the general principle it is an instance of, with its
//! names, places and dates removed -- and claims whose abstractions are close
//! while their literal claims are not, from different sources, become analogy
//! candidates. The extraction model confirms each pair with a one-line
//! rationale, and confirmed pairs get `RHYMES_WITH` edges (both directions).
//! Recall's existing spreading activation follows them from a direct hit, and
//! the index shows what it reached under "Related by analogy", with the
//! rationale.
//!
//! Runs in the overnight pass after extraction, bounded per pass; anything
//! left waits for the next one.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use crate::engine::Engine;
use crate::error::Result;
use crate::learn::overnight::cosine_pub as cosine;
use crate::store::edges::PropositionEdge;
use crate::store::{decode_embedding, encode_embedding};

/// Claims abstracted per model call.
const ABSTRACT_BATCH: usize = 16;

/// What one analogy pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnalogyOutcome {
    pub abstracted: usize,
    pub candidates: usize,
    pub edges: usize,
}

impl Engine {
    /// Abstract the claims that have none yet, then propose, confirm and
    /// write analogy edges for them. `keep_going` is checked between model
    /// calls. Soft-fail: a model or embedder failure ends the pass early.
    pub async fn build_analogies(&self, keep_going: &(dyn Fn() -> bool + Sync)) -> AnalogyOutcome {
        let mut out = AnalogyOutcome::default();
        let new = match self.abstract_new_claims(keep_going, &mut out).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("analogy: abstraction failed ({e})");
                return out;
            }
        };
        if new.is_empty() || !keep_going() {
            return out;
        }
        if let Err(e) = self.link_analogies(&new, &mut out).await {
            tracing::warn!("analogy: linking failed ({e})");
        }
        out
    }

    /// Write abstractions for claims without one (up to the pass budget).
    /// Returns the node ids abstracted this pass.
    async fn abstract_new_claims(
        &self,
        keep_going: &(dyn Fn() -> bool + Sync),
        out: &mut AnalogyOutcome,
    ) -> Result<Vec<String>> {
        let pending: Vec<(String, String)> = sqlx::query_as(
            "SELECT p.node_id, p.claim FROM propositions p \
             LEFT JOIN claim_abstracts a ON a.node_id = p.node_id \
             WHERE p.status = 'active' AND a.node_id IS NULL ORDER BY p.created_at, p.node_id LIMIT ?",
        )
        .bind(self.config.analogy_claims_per_pass as i64)
        .fetch_all(self.db.pool())
        .await?;
        let mut done = Vec::new();
        for batch in pending.chunks(ABSTRACT_BATCH) {
            if !keep_going() {
                break;
            }
            let lines: Vec<String> = batch.iter().enumerate().map(|(i, (_, c))| format!("{i}. {c}")).collect();
            let prompt = vec![
                json!({"role": "system", "content": "CLAIMS TO ABSTRACT. For each numbered claim, write one sentence stating the general principle, mechanism or pattern it is an instance of -- with its specific names, places, products and dates removed, so that claims from very different fields that share a shape read alike. Return JSON only."}),
                json!({"role": "user", "content": lines.join("\n")}),
            ];
            let schema = json!({
                "type": "object",
                "properties": {"abstracts": {"type": "array", "items": {"type": "object",
                    "properties": {"claim": {"type": "integer"}, "abstract": {"type": "string"}},
                    "required": ["claim", "abstract"]}}},
                "required": ["abstracts"]
            });
            let Some(v) = self.ask_model(prompt, &schema).await else {
                break;
            };
            for a in v.get("abstracts").and_then(Value::as_array).into_iter().flatten() {
                let (Some(i), Some(text)) = (
                    a.get("claim").and_then(Value::as_u64).map(|i| i as usize),
                    a.get("abstract").and_then(Value::as_str).map(str::trim),
                ) else {
                    continue;
                };
                let Some((node, _)) = batch.get(i) else { continue };
                if text.is_empty() {
                    continue;
                }
                let Ok((vec, tag)) = self.embedder.embed(text).await else {
                    return Ok(done);
                };
                if tag == crate::config::HASH_EMBED_MODEL {
                    continue; // analogy needs meaning, not shared words
                }
                sqlx::query(
                    "INSERT OR REPLACE INTO claim_abstracts (node_id, abstract, embedding_model, embedding) \
                     VALUES (?, ?, ?, ?)",
                )
                .bind(node)
                .bind(text)
                .bind(&tag)
                .bind(encode_embedding(&vec))
                .execute(self.db.pool())
                .await?;
                done.push(node.clone());
                out.abstracted += 1;
            }
        }
        Ok(done)
    }

    /// Propose pairs for the newly abstracted claims, confirm them with the
    /// model, write `RHYMES_WITH` edges for the confirmed ones.
    async fn link_analogies(&self, new: &[String], out: &mut AnalogyOutcome) -> Result<()> {
        let rows: Vec<(String, String, String, Vec<u8>)> = sqlx::query_as(
            "SELECT a.node_id, a.abstract, a.embedding_model, a.embedding FROM claim_abstracts a \
             JOIN propositions p ON p.node_id = a.node_id WHERE p.status = 'active' ORDER BY a.node_id",
        )
        .fetch_all(self.db.pool())
        .await?;
        let abstracts: HashMap<String, (String, String, Vec<f32>)> = rows
            .into_iter()
            .map(|(id, text, model, blob)| (id, (text, model, decode_embedding(&blob))))
            .collect();
        let mut claim_vecs: HashMap<String, Vec<f32>> = HashMap::new();
        let mut sources: HashMap<String, HashSet<String>> = HashMap::new();
        for id in abstracts.keys() {
            let srcs: HashSet<String> = self
                .db
                .list_active_supporting_chunks(id)
                .await?
                .into_iter()
                .map(|c| c.source_entity)
                .collect();
            sources.insert(id.clone(), srcs);
        }
        let models: HashSet<&String> = abstracts.values().map(|(_, m, _)| m).collect();
        for model in models {
            for (n, v) in self.db.active_claim_vectors(model).await? {
                if abstracts.contains_key(&n) {
                    claim_vecs.insert(n, v);
                }
            }
        }

        // Candidates: close abstractions, distant claims, no shared source.
        let mut pairs: Vec<(String, String, f32)> = Vec::new();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for a in new {
            let Some((_, model_a, va)) = abstracts.get(a) else { continue };
            let mut mine: Vec<(String, f32)> = Vec::new();
            for (b, (_, model_b, vb)) in &abstracts {
                if a == b || model_a != model_b {
                    continue;
                }
                let key = if a < b { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) };
                if seen.contains(&key) {
                    continue;
                }
                if sources.get(a).zip(sources.get(b)).is_some_and(|(x, y)| !x.is_disjoint(y)) {
                    continue;
                }
                let abs = cosine(va, vb);
                if f64::from(abs) < self.config.analogy_abstract_min {
                    continue;
                }
                let lit = match (claim_vecs.get(a), claim_vecs.get(b)) {
                    (Some(x), Some(y)) => cosine(x, y),
                    _ => 0.0,
                };
                if f64::from(lit) > self.config.analogy_claim_max {
                    continue; // directly related: plain recall already finds it
                }
                if self.db.has_edge("RHYMES_WITH", a, b).await? {
                    continue;
                }
                mine.push((b.clone(), abs));
            }
            mine.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| x.0.cmp(&y.0)));
            for (b, s) in mine.into_iter().take(3) {
                let key = if *a < b { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) };
                seen.insert(key);
                pairs.push((a.clone(), b, s));
            }
        }
        pairs.truncate(self.config.analogy_pairs_per_pass);
        out.candidates = pairs.len();
        if pairs.is_empty() {
            return Ok(());
        }

        let mut lines = Vec::new();
        for (k, (a, b, _)) in pairs.iter().enumerate() {
            let (Ok(Some(pa)), Ok(Some(pb))) = (self.db.get_proposition(a).await, self.db.get_proposition(b).await) else {
                continue;
            };
            let (ca, cb) = (pa.claim, pb.claim);
            lines.push(format!("{k}. A: {ca}\n   B: {cb}"));
        }
        let prompt = vec![
            json!({"role": "system", "content": "ANALOGY PAIRS. For each numbered pair, say whether A would genuinely help someone think about B (or B about A) by analogy -- the same mechanism or pattern at work in a different setting -- and give a one-line reason naming the shared pattern. Mere topical overlap is not an analogy. Return JSON only."}),
            json!({"role": "user", "content": lines.join("\n")}),
        ];
        let schema = json!({
            "type": "object",
            "properties": {"pairs": {"type": "array", "items": {"type": "object",
                "properties": {"pair": {"type": "integer"}, "analogous": {"type": "boolean"}, "why": {"type": "string"}},
                "required": ["pair", "analogous", "why"]}}},
            "required": ["pairs"]
        });
        let Some(v) = self.ask_model(prompt, &schema).await else {
            return Ok(());
        };
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        for r in v.get("pairs").and_then(Value::as_array).into_iter().flatten() {
            if r.get("analogous").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let Some((a, b, _)) = r.get("pair").and_then(Value::as_u64).and_then(|k| pairs.get(k as usize)) else {
                continue;
            };
            let why = r.get("why").and_then(Value::as_str).unwrap_or("").trim().to_string();
            for (from, to) in [(a, b), (b, a)] {
                let edge = PropositionEdge {
                    edge_id: format!("r_{}", &crate::text::sha256_hex(&format!("{from}|{to}"))[..24]),
                    edge_type: "RHYMES_WITH".into(),
                    from_node: from.clone(),
                    to_node: to.clone(),
                    weight: self.config.analogy_edge_weight.clamp(0.01, 0.8),
                    revalidate_at: None,
                    ttl_days: Some(self.config.analogy_ttl_days),
                    created_at: now.clone(),
                };
                if self.db.insert_proposition_edge(&edge).await.is_ok() {
                    sqlx::query("INSERT OR REPLACE INTO edge_rationales (edge_id, rationale) VALUES (?, ?)")
                        .bind(&edge.edge_id)
                        .bind(&why)
                        .execute(self.db.pool())
                        .await?;
                    out.edges += 1;
                }
            }
        }
        Ok(())
    }

    /// One structured call to the extraction model, under the global
    /// semaphore and the extraction timeout. `None` on any failure.
    async fn ask_model(&self, prompt: Vec<Value>, schema: &Value) -> Option<Value> {
        let _permit = self.extraction_semaphore.clone().acquire_owned().await.ok()?;
        let budget = std::time::Duration::from_secs(self.config.extraction.timeout_s);
        tokio::time::timeout(budget, self.chat.structured_chat(prompt, schema))
            .await
            .ok()?
            .ok()
    }

    /// The rationale stored with an analogy edge, if any.
    pub async fn analogy_rationale(&self, from: &str, to: &str) -> Option<String> {
        sqlx::query_scalar(
            "SELECT r.rationale FROM proposition_edges e JOIN edge_rationales r ON r.edge_id = e.edge_id \
             WHERE e.edge_type = 'RHYMES_WITH' AND e.from_node = ? AND e.to_node = ?",
        )
        .bind(from)
        .bind(to)
        .fetch_optional(self.db.pool())
        .await
        .ok()
        .flatten()
    }
}
