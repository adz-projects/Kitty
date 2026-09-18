//! Synchronous ingestion, Stages 0–3 + reliability seeding (plan §15
//! Phase 4, §3).
//!
//! Stage 0  PII gate — reject before any disk write (§12.1).
//! Stage 1  document-hash catalog — whole-payload SHA-256, abort on seen
//!          (or tombstoned) (§3.1).
//! Stage 2  deterministic parse & chunk — fixed `chunk_chars_max` char
//!          window, `chunk_chars_min` char stride (§3).
//! Stage 3  normalize → `content_hash` skip-if-seen → embed → provenance
//!          clustering (CosSim ≥ `cluster_similarity_threshold`, same
//!          vector space) → deterministic citation + slug (§3.1/§3.4).
//!
//! Chunks are written `extraction_status = "pending"`: Stage 4 (LLM
//! extraction) is Phase 6 and fully asynchronous, so `ingest()` never
//! performs an LLM call (Phase 4 exit criterion). Every failure is
//! soft-failed (claude.md principle 9).

use crate::engine::Engine;
use crate::error::Error;
use crate::privacy::{audit_pii_rejection, pii_categories};
use crate::reliability::{ReliabilityData, TIER_PERSONAL};
use crate::store::chunks::Chunk;
use crate::store::documents::Document;
use crate::text::{citation_date, normalize_text, sha256_hex, slug};

// Attachment intent factors (plan §4.2). Web scrapes and user notes carry
// 1.0 ("no attach reason") and never set `intent`.
pub const INTENT_EVIDENCE: f64 = 1.0; // cited as authoritative support
pub const INTENT_REFERENCE: f64 = 0.85; // background / context
pub const INTENT_ARTIFACT: f64 = 0.70; // the user's own produced work
pub const INTENT_CASUAL: f64 = 0.50; // shared FYI

/// Why an attachment was sent (plan §4.2 intent ladder).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentIntent {
    Evidence,
    Reference,
    Artifact,
    Casual,
}

impl AttachmentIntent {
    pub fn factor(self) -> f64 {
        match self {
            AttachmentIntent::Evidence => INTENT_EVIDENCE,
            AttachmentIntent::Reference => INTENT_REFERENCE,
            AttachmentIntent::Artifact => INTENT_ARTIFACT,
            AttachmentIntent::Casual => INTENT_CASUAL,
        }
    }
}

/// Input to [`Engine::ingest`]: the incoming source (web scrape or
/// attachment) plus the structured metadata the deterministic citation
/// and reliability seed are derived from (plan §3.4 / §4.1 / §4.2).
#[derive(Debug, Clone)]
pub struct IngestInput {
    /// Raw payload (scrape body or attachment text).
    pub content: String,
    /// Canonical source-type label in the citation: "Scraped",
    /// "Attachment", "Slack", "UserNote", … (plan §3.4).
    pub source_type: String,
    /// Canonical identity in the citation: URL host, channel name, or
    /// file name (plan §3.4).
    pub source_name: String,
    /// Correlated origin (plan §4.1): host, author/org, channel, or "user".
    pub source_entity: String,
    /// Capture timestamp, ISO-8601 UTC ("…Z") — caller-supplied so test
    /// state stays deterministic (migration 002 convention).
    pub captured_at: String,
    /// Attachment intent (plan §4.2); `None` → factor 1.0.
    pub intent: Option<AttachmentIntent>,
}

/// Outcome of an ingest call. `rejected_pii` carries **categories only** —
/// never the PII value (claude.md principle 6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestOutcome {
    /// Stage 0 rejected this payload (nothing was written); the categories
    /// found, in scan order.
    pub rejected_pii: Vec<String>,
    /// Stage 1 aborted: the document hash was already seen, or is
    /// tombstoned.
    pub aborted_document_seen: bool,
    /// Chunk rows written (Stage 3).
    pub chunks_written: usize,
    /// Chunks skipped at Stage 3 (content hash already seen or
    /// tombstoned).
    pub chunks_skipped: usize,
}

/// The pre-extraction decay class (plan §5.1). Stage 4 assigns the real
/// profile; until then `transient` (the shorter τ) is the conservative
/// default — a chunk whose extraction never completes still decays out on
/// schedule instead of sitting at full salience forever.
const INITIAL_DECAY_CLASS: &str = "transient";

/// Stage 2 (plan §3): deterministic fixed-window chunking — a
/// `chunk_chars_max` char window sliding by `chunk_chars_min` chars (a 50%
/// overlap at the defaults) over the trimmed payload. Identical text
/// always splits identically, which is the precondition for the Stage 3
/// `content_hash` boundary (plan §3.1).
pub fn chunk_text(
    text: &str,
    chunk_chars_min: usize,
    chunk_chars_max: usize,
) -> Vec<String> {
    debug_assert!(chunk_chars_min > 0 && chunk_chars_min <= chunk_chars_max);
    let text = text.trim();
    let cs: Vec<char> = text.chars().collect();
    let len = cs.len();
    if len == 0 {
        return Vec::new();
    }
    if len <= chunk_chars_max {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < len {
        let end = (start + chunk_chars_max).min(len);
        out.push(cs[start..end].iter().collect());
        if end == len {
            break;
        }
        start += chunk_chars_min;
    }
    out
}

fn clamp01(x: f64) -> f64 {
    x.clamp(0.0, 1.0)
}

/// Tier + prior for a source (plan §4.2): scrapes classify by host domain
/// against the data file (unknown domains → `personal`); any other source
/// type (channel, file, user note) is known without a domain signal and
/// defaults to `personal` — the conservative tier.
fn tier_and_prior(data: &ReliabilityData, input: &IngestInput) -> (&'static str, f64) {
    let tier = if input.source_type.eq_ignore_ascii_case("scraped")
        || input.source_type.eq_ignore_ascii_case("scrape")
    {
        data.tier_for_domain(&input.source_name)
    } else {
        TIER_PERSONAL
    };
    let prior = data
        .prior_for(tier)
        .unwrap_or(0.0); // a missing tier row is a data-file bug; 0.0 is the conservative side
    (tier, prior)
}

impl Engine {
    /// Synchronous ingestion through Stage 3 (plan §15 Phase 4). Soft-fail:
    /// every internal error is `tracing::warn`-logged and the outcome
    /// reflects what was written — the caller never sees a hard failure.
    pub async fn ingest(&self, input: &IngestInput) -> IngestOutcome {
        let mut outcome = IngestOutcome::default();
        let content = input.content.trim();
        if content.is_empty() {
            return outcome; // nothing to remember — no-op, nothing written
        }

        // Stage 0 — PII gate, before ANY write (plan §12.1).
        let categories = pii_categories(self.pii_classifier.as_ref(), content).await;
        if !categories.is_empty() {
            let created = input.captured_at.clone();
            audit_pii_rejection(&self.db, &categories, &created).await;
            outcome.rejected_pii = categories;
            return outcome;
        }

        // Stage 1 — document-hash catalog (plan §3.1): a tombstone blocks
        // re-ingestion of forgotten text at zero compute; a catalog hit
        // aborts a whole-document duplicate.
        let doc_hash = sha256_hex(content);
        match self.db.has_tombstone(&doc_hash).await {
            Ok(true) => {
                tracing::warn!("ingest: document is tombstoned -- re-ingestion skipped (plan §12.2)");
                outcome.aborted_document_seen = true;
                return outcome;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!("ingest: tombstone lookup failed ({e})");
                return outcome;
            }
        }
        match self.db.get_document(&doc_hash).await {
            Ok(Some(_)) => {
                outcome.aborted_document_seen = true;
                return outcome;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("ingest: document lookup failed ({e})");
                return outcome;
            }
        }

        // Stage 2 — deterministic chunking (plan §3).
        let windows = chunk_text(
            content,
            self.config.chunk_chars_min,
            self.config.chunk_chars_max,
        );

        // Reliability seed (plan §4.2): tier prior × attachment intent,
        // seed-once per source_entity; the chunk then carries the registry
        // value (drifted if the source is already known).
        let intent_factor = input
            .intent
            .map(|i| clamp01(i.factor()))
            .unwrap_or(INTENT_EVIDENCE);
        let (tier, tier_prior) = tier_and_prior(&self.reliability, input);
        let seed = clamp01(tier_prior * intent_factor);
        if let Err(e) = self
            .db
            .seed_source(
                &input.source_entity,
                tier,
                tier_prior,
                seed,
                &input.captured_at,
            )
            .await
        {
            tracing::warn!("ingest: source seed failed ({e})");
            return outcome;
        }
        let reliability = match self.db.get_source(&input.source_entity).await {
            Ok(Some(s)) => s.reliability,
            Ok(None) => seed,
            Err(e) => {
                tracing::warn!("ingest: source registry read failed ({e})");
                return outcome;
            }
        };

        // Deterministic citation + slug (plan §3.4): the per-chunk
        // citation carries the capture date; the cluster slug does NOT, so
        // a source maps to one cluster idempotently across re-ingestions.
        let citation = format!(
            "{}: {} / {}",
            input.source_type,
            input.source_name,
            citation_date(&input.captured_at)
        );
        let own_cluster = slug(&format!("{}: {}", input.source_type, input.source_name));

        // Stage 3 — one transaction for the whole document. The document
        // row is the FK parent of its chunks (migration 002), so it is
        // written first inside the transaction, then each chunk's row +
        // vector row, then the completion count. The transaction is what
        // makes a half-written document invisible: a rolled-back attempt
        // leaves no rows at all, so a retry is a fresh run rendered
        // idempotent by the Stage 3 content hashes, while an identical
        // whole-document re-ingest aborts at Stage 1 at zero cost.
        let db = &self.db;
        let vectors = self.vectors.clone();
        let (written, skipped) = match db
            .run_in_transaction(
                || async move {
                    let mut skipped = 0usize;
                    // FK parent first: the rows its chunks will reference
                    let doc = Document {
                        document_hash: doc_hash.clone(),
                        source_entity: input.source_entity.clone(),
                        source_type: input.source_type.clone(),
                        source_name: input.source_name.clone(),
                        captured_at: input.captured_at.clone(),
                        intent_factor,
                        chunk_count: 0,
                        created_at: input.captured_at.clone(),
                    };
                    db.insert_document(&doc).await?;

                    let mut written = 0usize;
                    for window in windows {
                        let norm = normalize_text(&window);
                        if norm.is_empty() {
                            continue; // a pure-whitespace/punctuation window is not content
                        }
                        let chash = sha256_hex(&norm);
                        if self.db.has_tombstone(&chash).await.unwrap_or(false) {
                            skipped += 1;
                            tracing::warn!("ingest: chunk is tombstoned -- skipped (plan §12.2)");
                            continue;
                        }
                        match self.db.get_chunk_by_content_hash(&chash).await {
                            Ok(Some(_)) => {
                                skipped += 1; // cross-document duplicate (plan §3.1)
                                continue;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                tracing::warn!("ingest: content-hash lookup failed ({e}); chunk skipped");
                                continue;
                            }
                        }
                        let (vec, tag) = match self.embedder.embed(&norm).await {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(
                                    "ingest: embedding failed ({e}); chunk skipped (soft-fail)"
                                );
                                continue;
                            }
                        };
                        let cluster_id = self.resolve_cluster(&vec, &tag, &own_cluster).await;
                        // Deterministic id from the content hash: the same
                        // content always lands on the same id (and dedup
                        // makes a collision impossible).
                        let chunk_id = format!("c_{}", &chash[..24]);
                        let chunk = Chunk {
                            chunk_id,
                            content: norm,
                            content_hash: chash,
                            document_hash: doc_hash.clone(),
                            source_entity: input.source_entity.clone(),
                            source_reliability: reliability,
                            provenance_cluster_id: cluster_id,
                            cluster_citation: citation.clone(),
                             status: "active".into(),
                             extraction_status: "pending".into(),
                             extraction_error_at: None,
                             embedding_model: tag.clone(),
                            decay_class: INITIAL_DECAY_CLASS.into(),
                            anchor_at: input.captured_at.clone(),
                            urgency_expires_at: None,
                            reinforcement_count: 0,
                            grounding_count: 0,
                            archived_at: None,
                            created_at: input.captured_at.clone(),
                        };
                        db.insert_chunk(&chunk).await?;
                        vectors
                            .upsert(&chunk.chunk_id, &vec, &tag)
                            .await
                            .map_err(Error::Internal)?;
                        written += 1;
                    }

                    // The completion count seals the document row: the
                    // catalog now reflects exactly what was written.
                    db.set_document_chunk_count(&doc_hash, written as i64)
                        .await?;
                    Ok((written, skipped))
                },
            )
            .await
        {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("ingest: document write failed ({e}); nothing written");
                (0, 0)
            }
        };

        outcome.chunks_written = written;
        outcome.chunks_skipped = skipped;
        outcome
    }

    /// Provenance cluster for a fresh chunk (plan §3.4 / §15 Phase 4):
    /// join the cluster of the best active same-space match when its cosine
    /// clears `cluster_similarity_threshold` (near-duplicate content
    /// inherits its origin's cluster); otherwise the chunk's own source
    /// slug — the id the source's first chunk created, which is also what
    /// every other lower-cosine chunk of the same source resolves to, so
    /// **the same citation never splits a cluster**.
    async fn resolve_cluster(&self, vec: &[f32], tag: &str, own_cluster: &str) -> String {
        match self.vectors.search(vec, tag, 1).await {
            Ok(hits) => match hits.into_iter().next() {
                Some((id, cos))
                    if (cos as f64) >= self.config.cluster_similarity_threshold =>
                {
                    match self.db.get_chunk(&id).await {
                        Ok(Some(c)) => c.provenance_cluster_id,
                        Ok(None) | Err(_) => own_cluster.to_string(),
                    }
                }
                _ => own_cluster.to_string(),
            },
            Err(e) => {
                tracing::warn!("ingest: cluster search failed ({e}); new source cluster");
                own_cluster.to_string()
            }
        }
    }
}

pub mod extraction;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking_is_deterministic_fixed_window() {
        let text: String = std::iter::repeat('a').take(2500).collect();
        let chunks = chunk_text(&text, 512, 1024);
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].len(), 1024);
        assert_eq!(chunks[1].len(), 1024);
        assert_eq!(chunks[2].len(), 1024);
        assert_eq!(chunks[3].len(), 2500 - 1536);
        // windows cover the whole text with min-stride overlap
        assert_eq!(chunks[0][512..1024].to_string(), chunks[1][..512].to_string());
    }

    #[test]
    fn chunking_short_text_is_a_single_chunk() {
        assert_eq!(chunk_text("hello", 512, 1024), vec!["hello".to_string()]);
        assert_eq!(chunk_text("", 512, 1024), Vec::<String>::new());
        assert_eq!(chunk_text("   ", 512, 1024), Vec::<String>::new());
    }

    #[test]
    fn chunking_respects_injected_window() {
        let text: String = std::iter::repeat('x').take(500).collect();
        let chunks = chunk_text(&text, 100, 200);
        assert_eq!(chunks.len(), 4);
        assert!(chunks.iter().all(|c| c.len() == 200));
    }

    #[test]
    fn intent_factors_match_plan() {
        assert_eq!(AttachmentIntent::Evidence.factor(), 1.0);
        assert_eq!(AttachmentIntent::Reference.factor(), 0.85);
        assert_eq!(AttachmentIntent::Artifact.factor(), 0.70);
        assert_eq!(AttachmentIntent::Casual.factor(), 0.5);
    }

    #[test]
    fn tier_assignment_requires_domain_signal_for_scrapes_only() {
        let data = ReliabilityData::load_default().unwrap();
        let base = IngestInput {
            content: String::new(),
            source_type: "Slack".into(),
            source_name: "#infra".into(),
            source_entity: "slack#infra".into(),
            captured_at: "2026-08-01T00:00:00Z".into(),
            intent: None,
        };
        // non-scrape sources default to personal
        let (tier, _) = tier_and_prior(&data, &base);
        assert_eq!(tier, TIER_PERSONAL);
        // scrapes classify by domain
        let scrape = IngestInput {
            source_type: "Scraped".into(),
            source_name: "nytimes.com".into(),
            ..base
        };
        let (tier, prior) = tier_and_prior(&data, &scrape);
        assert_eq!(tier, "established");
        assert!((prior - 0.80).abs() < 1e-9);
    }
}
