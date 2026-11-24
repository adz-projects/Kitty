//! Level 1 proposition rows, chunk→proposition support links, and their
//! SQL primitives (plan §5.2). Row types and parameterized SQL only — the
//! §8 confidence math that recomputes `confidence`/`is_disputed` lives in
//! the phases that own it, and uses these primitives in the same
//! transaction as its cause.

use sqlx::FromRow;

use crate::error::Result;
use crate::store::Db;
use super::chunks::Chunk;

#[derive(Debug, Clone, FromRow)]
pub struct Proposition {
    pub node_id: String,
    pub claim: String,
    /// Derived strictly from ACTIVE supporting chunks (plan §8); stored
    /// denormalized for the retrieval hot path.
    pub confidence: f64,
    /// Derived: true iff any supporting chunk has an open DISPUTED edge
    /// (plan §9.1). Stored denormalized for the same reason.
    pub is_disputed: bool,
    /// "active" | "UNSUPPORTED_ARCHIVE" (plan §5.3).
    pub status: String,
    pub importance: String,
    pub urgency: String,
    /// Earliest-deadline rule over the chunk T_expires (plan §7.2).
    pub urgency_expires_at: Option<String>,
    pub last_assessed_at: Option<String>,
    /// Set exactly when (and only when) `status = 'UNSUPPORTED_ARCHIVE'`.
    pub archived_at: Option<String>,
    pub created_at: String,
    /// Who asserts the claim; empty when the source states it as its own
    /// fact (migration 006).
    #[sqlx(default)]
    pub attribution: String,
    /// `states` | `reports` | `argues` | `refutes` (migration 006).
    #[sqlx(default)]
    pub stance: String,
}

impl Proposition {
    /// The claim as recall shows it: attributed when the source does not
    /// state it as fact, so an argument never reads as an established fact.
    pub fn display_claim(&self) -> String {
        let cleaned = crate::core::clean_attribution(&self.attribution);
        let who = cleaned.as_str();
        match (self.stance.as_str(), who.is_empty()) {
            ("argues", false) => format!("{who} argues: {}", self.claim),
            ("reports", false) => format!("{who} reports: {}", self.claim),
            ("refutes", false) => format!("{who} disputes: {}", self.claim),
            ("argues", true) => format!("(argued) {}", self.claim),
            (_, false) => format!("{} (per {who})", self.claim),
            _ => self.claim.clone(),
        }
    }
}

impl Db {
    /// Idempotent: a re-extraction of the same chunk re-derives the same
    /// deterministic `node_id`s (Phase 6), so a retry's insert is a no-op
    /// instead of a hard PK error that would wedge the chunk pending
    /// forever. Targeted `ON CONFLICT (node_id)`, never bare `OR IGNORE`
    /// (a real PK/shape bug must still surface).
    pub async fn insert_proposition(&self, p: &Proposition) -> Result<u64> {
        Ok(sqlx::query(
            "INSERT INTO propositions (
                node_id, claim, confidence, is_disputed, status, importance,
                urgency, urgency_expires_at, last_assessed_at, archived_at,
                created_at, attribution, stance
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (node_id) DO NOTHING",
        )
        .bind(&p.node_id)
        .bind(&p.claim)
        .bind(p.confidence)
        .bind(p.is_disputed)
        .bind(&p.status)
        .bind(&p.importance)
        .bind(&p.urgency)
        .bind(&p.urgency_expires_at)
        .bind(&p.last_assessed_at)
        .bind(&p.archived_at)
        .bind(&p.created_at)
        .bind(&p.attribution)
        .bind(if p.stance.is_empty() { "states" } else { p.stance.as_str() })
        .execute(&*self.write().await)
        .await?
        .rows_affected())
    }

    pub async fn get_proposition(&self, node_id: &str) -> Result<Option<Proposition>> {
        Ok(sqlx::query_as::<_, Proposition>(
            "SELECT * FROM propositions WHERE node_id = ?",
        )
        .bind(node_id)
        .fetch_optional(self.read_pool())
        .await?)
    }

    pub async fn list_propositions_by_status(
        &self,
        status: &str,
    ) -> Result<Vec<Proposition>> {
        Ok(sqlx::query_as::<_, Proposition>(
            "SELECT * FROM propositions WHERE status = ? ORDER BY rowid",
        )
        .bind(status)
        .fetch_all(self.read_pool())
        .await?)
    }

    /// Idempotent support link (a chunk retried through extraction must not
    /// double-support). Timestamps come from the caller, never the clock —
    /// deterministic test state.
    pub async fn add_support_link(
        &self,
        chunk_id: &str,
        node_id: &str,
        created_at: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO chunk_propositions (chunk_id, node_id, created_at)
             VALUES (?, ?, ?)
             ON CONFLICT (chunk_id, node_id) DO NOTHING",
        )
        .bind(chunk_id)
        .bind(node_id)
        .bind(created_at)
        .execute(&*self.write().await)
        .await?;
        Ok(())
    }

    pub async fn remove_support_link(&self, chunk_id: &str, node_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM chunk_propositions WHERE chunk_id = ? AND node_id = ?")
            .bind(chunk_id)
            .bind(node_id)
            .execute(&*self.write().await)
            .await?;
        Ok(())
    }

    /// The chunk's ACTIVE supporting chunks (input to §8 confidence and to
    /// retrieval rendering).
    /// Several propositions in one read (batched under SQLite's parameter
    /// limit). Missing ids are simply absent.
    pub async fn get_propositions(&self, node_ids: &[String]) -> Result<Vec<Proposition>> {
        let mut out = Vec::with_capacity(node_ids.len());
        for ids in node_ids.chunks(500) {
            let placeholders = vec!["?"; ids.len()].join(", ");
            let sql = format!("SELECT * FROM propositions WHERE node_id IN ({placeholders})");
            let mut q = sqlx::query_as::<_, Proposition>(&sql);
            for id in ids {
                q = q.bind(id);
            }
            out.extend(q.fetch_all(self.read_pool()).await?);
        }
        Ok(out)
    }

    /// `(chunk_id, cluster_citation)` of a proposition's active supporting
    /// chunks, in [`Self::list_active_supporting_chunks`]'s order, without
    /// the chunk text (recall needs only these two).
    pub async fn list_active_support_refs(&self, node_id: &str) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as::<_, (String, String)>(
            "SELECT c.chunk_id, c.cluster_citation FROM chunks AS c
             JOIN chunk_propositions AS cp ON cp.chunk_id = c.chunk_id
             WHERE cp.node_id = ? AND c.status = 'active'
             ORDER BY c.rowid",
        )
        .bind(node_id)
        .fetch_all(self.read_pool())
        .await?)
    }

    pub async fn list_active_supporting_chunks(&self, node_id: &str) -> Result<Vec<Chunk>> {
        Ok(sqlx::query_as::<_, Chunk>(
            "SELECT c.* FROM chunks AS c
             JOIN chunk_propositions AS cp ON cp.chunk_id = c.chunk_id
             WHERE cp.node_id = ? AND c.status = 'active'
             ORDER BY c.rowid",
        )
        .bind(node_id)
        .fetch_all(self.read_pool())
        .await?)
    }

    /// The chunk's ACTIVE propositions (the Stage 4 conflict prompt renders
    /// each probe neighbor's extracted claims — plan §3.5: neighbors go in
    /// with their text AND their extracted propositions, so the LLM compares
    /// claim against claim). Archived/unsupported propositions are excluded:
    /// they are forensic, not live.
    pub async fn list_propositions_for_chunk(
        &self,
        chunk_id: &str,
    ) -> Result<Vec<Proposition>> {
        Ok(sqlx::query_as::<_, Proposition>(
            "SELECT p.* FROM propositions AS p
             JOIN chunk_propositions AS cp ON cp.node_id = p.node_id
             WHERE cp.chunk_id = ? AND p.status = 'active'
             ORDER BY p.rowid",
        )
        .bind(chunk_id)
        .fetch_all(self.read_pool())
        .await?)
    }

    /// COUNT(DISTINCT source_entity) over active support links — N_active
    /// in the §8.2 formula.
    pub async fn count_active_sources(&self, node_id: &str) -> Result<i64> {
        // Distinct after `canonical_source`: copies of one document saved
        // under several names are one source, not independent corroboration.
        let entities: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT c.source_entity
             FROM chunks AS c
             JOIN chunk_propositions AS cp ON cp.chunk_id = c.chunk_id
             WHERE cp.node_id = ? AND c.status = 'active'",
        )
        .bind(node_id)
        .fetch_all(self.read_pool())
        .await?;
        let distinct: std::collections::HashSet<String> =
            entities.iter().map(|e| crate::core::canonical_source(e)).collect();
        Ok(distinct.len() as i64)
    }

    pub async fn set_proposition_confidence(
        &self,
        node_id: &str,
        confidence: f64,
        is_disputed: bool,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE propositions SET confidence = ?, is_disputed = ? WHERE node_id = ?",
        )
        .bind(confidence)
        .bind(is_disputed)
        .bind(node_id)
        .execute(&*self.write().await)
        .await?;
        Ok(())
    }

    /// Hard delete (plan §7.4 / §12.2 cascade): FK cascades remove the
    /// supporting chunk links and every edge incident to the node.
    pub async fn delete_proposition(&self, node_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM propositions WHERE node_id = ?")
            .bind(node_id)
            .execute(&*self.write().await)
            .await?;
        Ok(())
    }

    /// The propositions `chunk_id` supports (the §12.2 cascade needs this
    /// list *before* deleting the chunk row, whose links cascade away).
    pub async fn list_nodes_supported_by_chunk(
        &self,
        chunk_id: &str,
    ) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT node_id FROM chunk_propositions WHERE chunk_id = ? ORDER BY node_id",
        )
        .bind(chunk_id)
        .fetch_all(self.read_pool())
        .await?)
    }

    /// The §5.3 proposition transition, pair-atomic; archiving here is the
    /// UNSUPPORTED_ARCHIVE step (hard delete happens later in maintenance,
    /// plan §7.4).
    pub async fn archive_proposition(
        &self,
        node_id: &str,
        archived_at: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE propositions SET status = 'UNSUPPORTED_ARCHIVE', archived_at = ? \
             WHERE node_id = ?",
        )
        .bind(archived_at)
        .bind(node_id)
        .execute(&*self.write().await)
        .await?;
        Ok(())
    }

    /// Identifiers of all active propositions, in insertion order.
    pub async fn list_active_proposition_ids(&self) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT node_id FROM propositions WHERE status = 'active' ORDER BY rowid",
        )
        .fetch_all(self.read_pool())
        .await?)
    }

    /// Full-text BM25 search over active proposition claims via `propositions_fts`.
    /// Returns `(node_id, score)` ordered strongest-first (higher is better).
    /// Soft-fails to an empty vec on invalid/empty FTS queries so recall never fails.
    pub async fn search_propositions_fts(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, f64)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let Some(match_expr) = build_fts5_match_query(query) else {
            return Ok(Vec::new());
        };
        let rows: Result<Vec<(String, f64)>> = sqlx::query_as(
            "SELECT p.node_id, -bm25(propositions_fts) AS score
             FROM propositions_fts
             JOIN propositions AS p ON p.rowid = propositions_fts.rowid
             WHERE propositions_fts MATCH ? AND p.status = 'active'
             ORDER BY bm25(propositions_fts) ASC, p.node_id ASC
             LIMIT ?",
        )
        .bind(&match_expr)
        .bind(limit as i64)
        .fetch_all(self.read_pool())
        .await
        .map_err(Into::into);
        match rows {
            Ok(r) => Ok(r),
            Err(e) => {
                tracing::debug!("propositions_fts search soft-failed for {match_expr:?}: {e}");
                Ok(Vec::new())
            }
        }
    }

    /// Active claims holding a *rare* identifier-like term from `query` (a
    /// name, model number or code symbol, see [`identifier_terms`]): a term
    /// at most `max_df` active claims contain. Best BM25 first, at most
    /// `limit`. These are what embeddings blur and an exact match settles;
    /// a common word never qualifies. Soft-fails to empty.
    pub async fn rare_term_propositions(
        &self,
        query: &str,
        max_df: i64,
        limit: usize,
    ) -> Result<Vec<String>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut rare: Vec<String> = Vec::new();
        for term in identifier_terms(query).into_iter().take(6) {
            let quoted = format!("\"{term}\"");
            let df: std::result::Result<i64, sqlx::Error> = sqlx::query_scalar(
                "SELECT COUNT(*) FROM propositions_fts \
                 JOIN propositions AS p ON p.rowid = propositions_fts.rowid \
                 WHERE propositions_fts MATCH ? AND p.status = 'active'",
            )
            .bind(&quoted)
            .fetch_one(self.read_pool())
            .await;
            match df {
                Ok(n) if n > 0 && n <= max_df => rare.push(quoted),
                Ok(_) => {}
                Err(e) => tracing::debug!("propositions_fts df soft-failed for {quoted}: {e}"),
            }
        }
        if rare.is_empty() {
            return Ok(Vec::new());
        }
        let expr = rare.join(" OR ");
        let rows: std::result::Result<Vec<String>, sqlx::Error> = sqlx::query_scalar(
            "SELECT p.node_id FROM propositions_fts \
             JOIN propositions AS p ON p.rowid = propositions_fts.rowid \
             WHERE propositions_fts MATCH ? AND p.status = 'active' \
             ORDER BY bm25(propositions_fts) ASC, p.node_id ASC LIMIT ?",
        )
        .bind(&expr)
        .bind(limit as i64)
        .fetch_all(self.read_pool())
        .await;
        Ok(rows.unwrap_or_else(|e| {
            tracing::debug!("propositions_fts rare search soft-failed for {expr:?}: {e}");
            Vec::new()
        }))
    }
}

/// The identifier-like words of a query, lowercased and de-duplicated: a
/// word with a digit or underscore (`B550M`, `gemma_3`), an inner capital
/// (`LiteRT`, `AORUS`), or a capital that does not open a sentence (a proper
/// name). Ordinary words never qualify, whatever their case.
pub fn identifier_terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut sentence_start = true;
    for raw in query.split_whitespace() {
        let ends_sentence = raw.ends_with(['.', '!', '?', ':']);
        for word in raw.split(|c: char| !c.is_alphanumeric() && c != '_') {
            let word = word.trim_matches('_');
            if word.chars().count() < 2 {
                continue;
            }
            let mut chars = word.chars();
            let first_upper = chars.next().is_some_and(|c| c.is_uppercase());
            let inner_upper = chars.any(|c| c.is_uppercase());
            let identifier = word.chars().any(|c| c.is_ascii_digit())
                || word.contains('_')
                || inner_upper
                || (first_upper && !sentence_start);
            if identifier {
                let lower = word.to_lowercase();
                if !out.contains(&lower) {
                    out.push(lower);
                }
            }
            sentence_start = false;
        }
        if ends_sentence {
            sentence_start = true;
        }
    }
    out
}

/// Tokenize a free-text user query into safe quoted FTS5 OR terms (alphanumeric
/// tokens >= 2 chars, capped to 12 tokens to bound SQLite FTS work).
fn build_fts5_match_query(query: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for raw in query.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let trimmed = raw.trim_matches('_');
        if trimmed.chars().count() < 2 {
            continue;
        }
        let lower = trimmed.to_lowercase();
        if matches!(
            lower.as_str(),
            "the" | "and" | "for" | "with" | "that" | "this" | "from" | "what" | "when" | "where"
                | "which" | "about" | "into" | "your" | "have" | "does" | "are" | "was" | "were"
                | "will" | "would" | "could" | "should" | "can" | "not" | "but" | "you"
        ) {
            continue;
        }
        if seen.insert(lower.clone()) {
            terms.push(format!("\"{lower}\""));
            if terms.len() >= 12 {
                break;
            }
        }
    }
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_prop(id: &str, claim: &str) -> Proposition {
        Proposition {
            node_id: id.into(),
            claim: claim.into(),
            confidence: 0.8,
            is_disputed: false,
            status: "active".into(),
            importance: "medium".into(),
            urgency: "low".into(),
            urgency_expires_at: None,
            last_assessed_at: None,
            archived_at: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            attribution: String::new(),
            stance: "states".into(),
        }
    }

    #[tokio::test]
    async fn propositions_fts_tracks_insert_update_delete_and_handles_punctuation() {
        let db = Db::open_in_memory().await.unwrap();
        db.insert_proposition(&test_prop("p1", "Gigabyte B550M AORUS PRO uses AM4 socket"))
            .await
            .unwrap();
        db.insert_proposition(&test_prop("p2", "ArXiv preprints publish daily"))
            .await
            .unwrap();

        let hits = db
            .search_propositions_fts("What socket does B550M AORUS use?! OR \"test\"", 5)
            .await
            .unwrap();
        assert_eq!(hits.first().map(|h| h.0.as_str()), Some("p1"));

        // Archived propositions must not appear in FTS recall.
        db.archive_proposition("p1", "2026-02-01T00:00:00Z").await.unwrap();
        let after_archive = db.search_propositions_fts("B550M AORUS", 5).await.unwrap();
        assert!(after_archive.is_empty());

        // Deleting p2 clears it from FTS index too.
        db.delete_proposition("p2").await.unwrap();
        let after_del = db.search_propositions_fts("ArXiv preprints", 5).await.unwrap();
        assert!(after_del.is_empty());
    }

    #[test]
    fn identifier_terms_pick_names_and_codes_not_ordinary_words() {
        assert_eq!(
            identifier_terms("What socket does the B550M AORUS use? Ask Priya about it."),
            vec!["b550m", "aorus", "priya"]
        );
        assert_eq!(identifier_terms("How do I make good bread"), Vec::<String>::new());
        assert_eq!(identifier_terms("call gemma_3 via LiteRT"), vec!["gemma_3", "litert"]);
    }

    #[tokio::test]
    async fn rare_terms_rescue_only_uncommon_identifiers() {
        let db = Db::open_in_memory().await.unwrap();
        db.insert_proposition(&test_prop("p1", "The B550M board takes AM4 chips"))
            .await
            .unwrap();
        for i in 0..5 {
            db.insert_proposition(&test_prop(&format!("q{i}"), "Priya maintains this cluster"))
                .await
                .unwrap();
        }
        let hits = db
            .rare_term_propositions("Is the B550M still sold, Priya?", 3, 5)
            .await
            .unwrap();
        assert_eq!(hits, vec!["p1".to_string()], "Priya is in 5 claims: not rare");
    }
}
