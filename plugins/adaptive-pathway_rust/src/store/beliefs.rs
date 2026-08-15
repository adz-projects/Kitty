//! `beliefs` table access. A belief is the atomic unit of behavioral memory.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{decode_embedding, encode_embedding, Db};
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Identity,
    Context,
    Conversation,
}

impl Layer {
    pub fn as_str(&self) -> &'static str {
        match self {
            Layer::Identity => "identity",
            Layer::Context => "context",
            Layer::Conversation => "conversation",
        }
    }

    /// The extractor can never write identity -- only consolidation promotes
    /// (this is a schema-level guard in 001_init.sql CHECK, mirrored here so
    /// the enum can be used before any SQL is written).
    pub fn extractor_writable(self) -> bool {
        matches!(self, Layer::Context | Layer::Conversation)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Correction,
    DirectStatement,
    ControlledTest,
    InferredPattern,
    SingleObservation,
}

impl Provenance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provenance::Correction => "correction",
            Provenance::DirectStatement => "direct_statement",
            Provenance::ControlledTest => "controlled_test",
            Provenance::InferredPattern => "inferred_pattern",
            Provenance::SingleObservation => "single_observation",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "correction" => Provenance::Correction,
            "direct_statement" => Provenance::DirectStatement,
            "controlled_test" => Provenance::ControlledTest,
            "inferred_pattern" => Provenance::InferredPattern,
            _ => Provenance::SingleObservation,
        }
    }

    /// Initial confidence a new belief starts at for this provenance.
    pub fn initial_confidence(self) -> f64 {
        match self {
            Provenance::Correction => 0.75,
            Provenance::DirectStatement => 0.70,
            Provenance::ControlledTest => 0.65,
            Provenance::InferredPattern => 0.30,
            Provenance::SingleObservation => 0.15,
        }
    }

    /// Whether this provenance counts as "tested" evidence (sets a belief's
    /// `tested` flag, lifting the untested ×0.625 recall discount). Matches
    /// the plan's provenance table: correction/direct_statement/
    /// controlled_test are tested; inferred_pattern/single_observation are
    /// not.
    pub fn is_tested(self) -> bool {
        matches!(
            self,
            Provenance::Correction | Provenance::DirectStatement | Provenance::ControlledTest
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Belief {
    pub id: String,
    pub text: String,
    pub embedding: Vec<f32>,
    pub confidence: f64,
    pub provenance: Provenance,
    pub layer: Layer,
    pub tested: bool,
    pub domain: Option<String>,
    pub tier: String,
    pub support_count: i64,
    pub distinct_sessions: i64,
    pub contradict_count: i64,
    pub pinned: bool,
    pub last_confirmed_at: Option<DateTime<Utc>>,
    pub consolidated_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Owning session for a `layer == Conversation` belief ("Lives for the
    /// session" per the three-layer model). Always `None` for `Context`/
    /// `Identity` beliefs, which are cross-session by design -- consolidation
    /// clears this when a belief is promoted out of the conversation layer.
    pub session_id: Option<String>,
    /// The embedding model that produced `embedding`. Empty string is the
    /// "needs re-embedding" sentinel (pre-migration rows, or a genuine
    /// mismatch against the currently configured model) -- see
    /// `list_recall_candidates`'s filter and `Db::update_embedding`.
    pub embedding_model: String,
}

#[derive(Debug, Clone, Default)]
pub struct BeliefPatch {
    pub confidence: Option<f64>,
    pub tested: Option<bool>,
    pub support_count: Option<i64>,
    pub distinct_sessions: Option<i64>,
    pub contradict_count: Option<i64>,
    pub pinned: Option<bool>,
    pub domain: Option<Option<String>>,
    pub layer: Option<Layer>,
    pub provenance: Option<Provenance>,
    pub last_confirmed_at: Option<DateTime<Utc>>,
    pub consolidated_at: Option<DateTime<Utc>>,
    /// `Some(Some(id))` sets the owning session; `Some(None)` clears it
    /// (promotion out of the conversation layer); `None` leaves it alone.
    /// Double-Option, same COALESCE-can't-null-a-column shape as `domain` --
    /// see `update_belief`'s dedicated handling below (it does NOT go
    /// through the blind `COALESCE(?, col)` pattern the other fields use).
    pub session_id: Option<Option<String>>,
}

impl Db {
    pub async fn insert_belief(&self, b: &Belief) -> Result<()> {
        sqlx::query(
            "INSERT INTO beliefs (id, text, embedding, confidence, provenance, layer, tested, \
             domain, tier, support_count, distinct_sessions, contradict_count, pinned, \
             last_confirmed_at, consolidated_at, created_at, updated_at, session_id, \
             embedding_model) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&b.id)
        .bind(&b.text)
        .bind(encode_embedding(&b.embedding))
        .bind(b.confidence)
        .bind(b.provenance.as_str())
        .bind(b.layer.as_str())
        .bind(b.tested)
        .bind(&b.domain)
        .bind(&b.tier)
        .bind(b.support_count)
        .bind(b.distinct_sessions)
        .bind(b.contradict_count)
        .bind(b.pinned)
        .bind(b.last_confirmed_at)
        .bind(b.consolidated_at)
        .bind(b.created_at)
        .bind(b.updated_at)
        .bind(&b.session_id)
        .bind(&b.embedding_model)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Overwrite a belief's embedding after a re-embedding pass (embedding-
    /// model change). Deliberately does **not** touch `updated_at` or
    /// `last_confirmed_at` -- re-embedding isn't a reconfirmation event, and
    /// touching either would corrupt `effective_weight`'s recency-decay math
    /// the same way a stray `updated_at` write on an unrelated field once
    /// did elsewhere in this crate. Only `embedding`/`embedding_model` move.
    pub async fn update_embedding(&self, id: &str, embedding: &[f32], model: &str) -> Result<()> {
        sqlx::query("UPDATE beliefs SET embedding = ?, embedding_model = ? WHERE id = ?")
            .bind(encode_embedding(embedding))
            .bind(model)
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Up to `limit` beliefs whose `embedding_model` doesn't match
    /// `current_model` -- the re-embedding queue a belief row's own column
    /// value *is*, no separate "migration pending" flag needed. Ordered by
    /// `updated_at` ascending (oldest-touched first) purely for deterministic
    /// batch ordering across ticks, not because recency matters here.
    pub async fn list_stale_embedding_beliefs(&self, current_model: &str, limit: i64) -> Result<Vec<Belief>> {
        let rows = sqlx::query_as::<_, BeliefRow>(
            "SELECT * FROM beliefs WHERE embedding_model != ? ORDER BY updated_at ASC LIMIT ?",
        )
        .bind(current_model)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(map_belief).collect())
    }

    /// Count of beliefs still awaiting re-embedding against `current_model`
    /// -- surfaced through `/api/pathway/stats` for the Settings belief-
    /// health view.
    pub async fn count_stale_embedding_beliefs(&self, current_model: &str) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM beliefs WHERE embedding_model != ?")
            .bind(current_model)
            .fetch_one(self.pool())
            .await?;
        Ok(count)
    }

    /// Apply a partial update. Most fields use `COALESCE(?, col)` ("`None`
    /// means leave alone"), which works for scalar fields but cannot express
    /// "explicitly set this nullable column to NULL" -- `COALESCE(NULL,
    /// domain)` just keeps the old value. `domain` and `session_id` are
    /// double-`Option` for exactly that reason (`Some(None)` = clear it,
    /// `None` = leave alone) and are resolved against the current row in
    /// Rust first, then written unconditionally rather than through
    /// `COALESCE`, so a real NULL can actually be written.
    pub async fn update_belief(&self, id: &str, p: &BeliefPatch, updated_at: DateTime<Utc>) -> Result<()> {
        let current = self.get_belief(id).await?;
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => current.as_ref().and_then(|c| c.domain.clone()),
        };
        let session_id = match &p.session_id {
            Some(s) => s.clone(),
            None => current.as_ref().and_then(|c| c.session_id.clone()),
        };
        sqlx::query("UPDATE beliefs SET confidence = COALESCE(?, confidence), \
                     tested = COALESCE(?, tested), \
                     support_count = COALESCE(?, support_count), \
                     distinct_sessions = COALESCE(?, distinct_sessions), \
                     contradict_count = COALESCE(?, contradict_count), \
                     pinned = COALESCE(?, pinned), \
                     domain = ?, \
                     layer = COALESCE(?, layer), \
                     provenance = COALESCE(?, provenance), \
                     last_confirmed_at = COALESCE(?, last_confirmed_at), \
                     consolidated_at = COALESCE(?, consolidated_at), \
                     session_id = ?, \
                     updated_at = ? WHERE id = ?")
            .bind(p.confidence)
            .bind(p.tested)
            .bind(p.support_count)
            .bind(p.distinct_sessions)
            .bind(p.contradict_count)
            .bind(p.pinned)
            .bind(domain)
            .bind(p.layer.map(|l| l.as_str()))
            .bind(p.provenance.map(|pr| pr.as_str()))
            .bind(p.last_confirmed_at)
            .bind(p.consolidated_at)
            .bind(session_id)
            .bind(updated_at)
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn get_belief(&self, id: &str) -> Result<Option<Belief>> {
        let row = sqlx::query_as::<_, BeliefRow>("SELECT * FROM beliefs WHERE id = ?")
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(map_belief))
    }

    pub async fn list_beliefs(&self, layer: Option<Layer>) -> Result<Vec<Belief>> {
        let rows = match layer {
            Some(l) => {
                sqlx::query_as::<_, BeliefRow>("SELECT * FROM beliefs WHERE layer = ?")
                    .bind(l.as_str())
                    .fetch_all(self.pool())
                    .await?
            }
            None => {
                sqlx::query_as::<_, BeliefRow>("SELECT * FROM beliefs")
                    .fetch_all(self.pool())
                    .await?
            }
        };
        Ok(rows.into_iter().map(map_belief).collect())
    }

    /// The `limit` most-recently-touched beliefs across all layers. Backs
    /// `store::contradictions::run_contradiction_pass`, whose O(n²) pairwise
    /// scan must be bounded the same way `list_recall_candidates` bounds the
    /// recall hot path (audit #131).
    pub async fn list_recent_beliefs(&self, limit: i64) -> Result<Vec<Belief>> {
        let rows = sqlx::query_as::<_, BeliefRow>(
            "SELECT * FROM beliefs ORDER BY updated_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(map_belief).collect())
    }

    /// Conversation-layer beliefs owned by exactly this session. Used by
    /// consolidation, which must never touch another session's still-fast-
    /// decaying conversation memory.
    pub async fn list_conversation_beliefs_for_session(&self, session_id: &str) -> Result<Vec<Belief>> {
        let rows = sqlx::query_as::<_, BeliefRow>(
            "SELECT * FROM beliefs WHERE layer = 'conversation' AND session_id = ?",
        )
        .bind(session_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(map_belief).collect())
    }

    /// The full recall candidate set for a turn in `session_id`: every
    /// context/identity belief (cross-session by design) plus only *this*
    /// session's conversation-layer beliefs. Without the session filter,
    /// one session's transient conversational memory ("I'm currently
    /// debugging X") would leak into every other session's recall block.
    /// Bounds the read at `RECALL_CANDIDATE_ROW_LIMIT` rows, most-recently-
    /// touched first -- this is the recall hot path, called every turn, and
    /// was previously a full unbounded `SELECT *` (decoding every belief's
    /// embedding) with no upper bound at all. `select_beliefs_relevant`
    /// already caps its own working set at `recall::MAX_CANDIDATES` (64)
    /// *after* this read, so realistic belief stores (the common case is
    /// dozens to a few hundred) are entirely unaffected either way; this
    /// only changes behavior once a store has grown past
    /// `RECALL_CANDIDATE_ROW_LIMIT`, where it trades "always read
    /// everything" for "read the most recently touched/reinforced subset" --
    /// recency is already how `effective_weight` biases selection, so this
    /// is consistent with, not a departure from, the existing ranking.
    /// `current_model` scopes candidates to beliefs already embedded under
    /// the currently configured model -- a belief mid-re-embedding (its
    /// `embedding_model` still tags the old model) is excluded rather than
    /// having its stale-space embedding compared against a fresh query
    /// embedding via cosine, which would be a meaningless comparison. See
    /// `migrations/005_belief_embedding_model.sql` and
    /// `background::reembed_stale_beliefs`. False negatives here (a belief
    /// briefly unavailable for recall while its re-embed is pending) are
    /// fine, matching this crate's existing "never fabricate, prefer a gap"
    /// stance elsewhere (e.g. `domains::infer_query_domain`).
    pub async fn list_recall_candidates(&self, session_id: &str, current_model: &str) -> Result<Vec<Belief>> {
        const RECALL_CANDIDATE_ROW_LIMIT: i64 = 500;
        let rows = sqlx::query_as::<_, BeliefRow>(
            "SELECT * FROM beliefs WHERE (layer != 'conversation' OR session_id = ?) \
             AND embedding_model = ? \
             ORDER BY updated_at DESC LIMIT ?",
        )
        .bind(session_id)
        .bind(current_model)
        .bind(RECALL_CANDIDATE_ROW_LIMIT)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(map_belief).collect())
    }

    pub async fn delete_belief(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM beliefs WHERE id = ?").bind(id).execute(self.pool()).await?;
        Ok(())
    }

    /// All beliefs with embeddings loaded, for in-memory vector search.
    pub async fn load_embeddings(&self, layer: Option<Layer>) -> Result<Vec<Belief>> {
        self.list_beliefs(layer).await
    }

    /// Resolve a textual `contradicts` reference to a belief id. The
    /// extraction schema's contract for that field is "that KNOWN BELIEF
    /// line's *exact text*" -- the model is expected to quote, not
    /// paraphrase -- so this is a plain case-insensitive substring match in
    /// both directions, deliberately with no embedding involved.
    ///
    /// This previously matched via cosine similarity against
    /// `hashing::hash_embed(what, 384)` -- the *lexical hashing fallback*
    /// embedder -- compared against belief embeddings produced by the
    /// engine's real embedder (Ollama-semantic when available). Those are
    /// two different, incompatible vector spaces; the 0.80 cosine threshold
    /// was comparing noise. Since the field is a quote, not a paraphrase, no
    /// embedding was ever actually needed here -- see `Db::forget_by_text`'s
    /// cosine fallback for the case (a user's own paraphrase) where semantic
    /// matching genuinely is the right tool, using a real embedding supplied
    /// by the caller.
    pub async fn best_text_match(&self, what: &str) -> Result<Option<String>> {
        // An empty needle contains-matches every belief (audit #116).
        if what.trim().is_empty() {
            return Ok(None);
        }
        let what_lower = what.to_lowercase();
        let all = self.list_beliefs(None).await?;
        Ok(all
            .into_iter()
            .find(|b| {
                let text_lower = b.text.to_lowercase();
                text_lower.contains(&what_lower) || what_lower.contains(&text_lower)
            })
            .map(|b| b.id))
    }
}

struct BeliefRow {
    id: String,
    text: String,
    embedding: Vec<u8>,
    confidence: f64,
    provenance: String,
    layer: String,
    tested: bool,
    domain: Option<String>,
    tier: String,
    support_count: i64,
    distinct_sessions: i64,
    contradict_count: i64,
    pinned: bool,
    last_confirmed_at: Option<DateTime<Utc>>,
    consolidated_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    session_id: Option<String>,
    embedding_model: String,
}

impl sqlx::FromRow<'_, sqlx::sqlite::SqliteRow> for BeliefRow {
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> sqlx::Result<Self> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            text: row.try_get("text")?,
            embedding: row.try_get("embedding")?,
            confidence: row.try_get("confidence")?,
            provenance: row.try_get("provenance")?,
            layer: row.try_get("layer")?,
            tested: row.try_get("tested")?,
            domain: row.try_get("domain")?,
            tier: row.try_get("tier")?,
            support_count: row.try_get("support_count")?,
            distinct_sessions: row.try_get("distinct_sessions")?,
            contradict_count: row.try_get("contradict_count")?,
            pinned: row.try_get("pinned")?,
            last_confirmed_at: row.try_get("last_confirmed_at")?,
            consolidated_at: row.try_get("consolidated_at")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            session_id: row.try_get("session_id")?,
            embedding_model: row.try_get("embedding_model")?,
        })
    }
}

fn map_belief(r: BeliefRow) -> Belief {
    Belief {
        id: r.id,
        text: r.text,
        embedding: decode_embedding(&r.embedding),
        confidence: r.confidence,
        provenance: Provenance::parse(&r.provenance),
        layer: if r.layer == "identity" {
            Layer::Identity
        } else if r.layer == "context" {
            Layer::Context
        } else {
            Layer::Conversation
        },
        tested: r.tested,
        domain: r.domain,
        tier: r.tier,
        support_count: r.support_count,
        distinct_sessions: r.distinct_sessions,
        contradict_count: r.contradict_count,
        pinned: r.pinned,
        last_confirmed_at: r.last_confirmed_at,
        consolidated_at: r.consolidated_at,
        created_at: r.created_at,
        updated_at: r.updated_at,
        session_id: r.session_id,
        embedding_model: r.embedding_model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn belief(id: &str, updated_at: DateTime<Utc>) -> Belief {
        Belief {
            id: id.into(),
            text: format!("belief {id}"),
            embedding: vec![1.0, 0.0],
            confidence: 0.5,
            provenance: Provenance::InferredPattern,
            layer: Layer::Context,
            tested: false,
            domain: None,
            tier: "context".into(),
            support_count: 1,
            distinct_sessions: 1,
            contradict_count: 0,
            pinned: false,
            last_confirmed_at: None,
            consolidated_at: None,
            created_at: updated_at,
            updated_at,
            session_id: None,
            embedding_model: crate::config::DEFAULT_EMBEDDING_MODEL.into(),
        }
    }

    #[tokio::test]
    async fn list_recall_candidates_caps_at_the_row_limit_keeping_the_newest() {
        let db = crate::store::Db::open_in_memory().await.unwrap();
        let base = Utc::now();
        // 510 beliefs, each with a distinct updated_at -- more than the
        // internal 500-row cap.
        for i in 0..510 {
            let b = belief(&format!("b{i}"), base + chrono::Duration::seconds(i));
            db.insert_belief(&b).await.unwrap();
        }

        let candidates = db
            .list_recall_candidates("s1", crate::config::DEFAULT_EMBEDDING_MODEL)
            .await
            .unwrap();
        assert_eq!(candidates.len(), 500, "must cap at the row limit, not return every belief");
        // Newest-first: the 10 oldest (b0..b9) must have been dropped.
        let ids: std::collections::HashSet<&str> = candidates.iter().map(|b| b.id.as_str()).collect();
        assert!(!ids.contains("b0"), "the oldest belief must be dropped once over the cap");
        assert!(ids.contains("b509"), "the newest belief must survive the cap");
    }

    #[tokio::test]
    async fn best_text_match_ignores_an_empty_needle() {
        // Audit #116: `contains("")` is true for every belief, so an empty
        // needle resolved to the first row in the table.
        let db = crate::store::Db::open_in_memory().await.unwrap();
        db.insert_belief(&belief("b1", Utc::now())).await.unwrap();
        assert_eq!(db.best_text_match("").await.unwrap(), None);
        assert_eq!(db.best_text_match("   ").await.unwrap(), None);
        // A real needle still resolves.
        assert_eq!(db.best_text_match("belief b1").await.unwrap().as_deref(), Some("b1"));
    }
}
