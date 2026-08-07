//! Learning: one shared extraction+record function used by both the
//! compaction-piggyback seam (A) and the turn-end seam (B), guarded by a
//! per-session learn lock and a forward-only watermark.

use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::error::{PathwayError, Result};
use crate::store::beliefs::{Belief, Layer, Provenance};
use crate::store::{Db, encode_embedding};
use crate::traits::StructuredChat;

/// JSON-schema for the extraction pass. Schema discipline (mirrors the
/// daemon's `MEMORY_SLOTS_SCHEMA`): every field `required`; empty-string
/// sentinels rather than nullable unions. `layer` must NOT include
/// `identity` -- the extractor can never write a permanent fact, only
/// consolidation promotes.
pub fn extraction_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "observations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "statement": {"type": "string"},
                        "provenance": {"type": "string", "enum": [
                            "direct_statement", "controlled_test", "inferred_pattern", "single_observation"
                        ]},
                        "layer": {"type": "string", "enum": ["context", "conversation"]},
                        "domain": {"type": "string"},
                        "evidence": {"type": "string"},
                        "contradicts": {"type": "string"}
                    },
                    "required": ["statement", "provenance", "layer"]
                }
            },
            "corrections": {
                "type": "array",
                "items": {"type": "string"}
            },
            "tone": {"type": "string"},
            "open_topics": {
                "type": "array",
                "items": {"type": "string"}
            }
        },
        "required": ["observations", "corrections", "tone", "open_topics"]
    })
}

/// Exactly 2 messages framing "learn about the user -- not the task, not the
/// assistant", mirroring the daemon's `build_summarizer_prompt` shape.
pub fn build_extraction_prompt(known_beliefs: &str, chunk: &str) -> Vec<Value> {
    let system = "You are learning about the person in this conversation. \
        Extract beliefs ABOUT THE USER -- their preferences, values, habits, \
        constraints, identity. Never extract facts about the task, the tools, \
        or the assistant. If the text says nothing about the user, return an \
        empty observations list.";
    vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": format!(
            "Already known about this user (do not duplicate):\n{}\n\n\
             Conversation to learn from:\n{}",
            if known_beliefs.is_empty() { "(none)" } else { known_beliefs },
            chunk
        )}),
    ]
}

/// What triggered a learn pass, for the audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnTrigger {
    TurnEnd,
    Compaction,
    IdleClose,
}

/// Input to `extract_and_record`.
#[derive(Clone)]
pub struct LearnRequest<'a> {
    pub session_id: &'a str,
    /// rowids to learn through (exclusive upper bound). Rows must already
    /// have been persisted (Seam B reads persisted rows; Seam A passes the
    /// folded chunk).
    pub through_rowid: i64,
    /// Chunk text to learn from. If None, read `(last_learned, through]`
    /// from `host_pool`.
    pub given_chunk: Option<String>,
}

/// Outcome of a learn pass.
#[derive(Debug, Clone, Default)]
pub struct LearnOutcome {
    pub observations: usize,
    pub corrections: usize,
}

/// The single shared extraction+record function. Sequence (from the plan):
/// paused -> skip; CAS the per-session learn lock; read watermark;
/// `through <= watermark` -> skip (double-count guard); build chunk; render
/// KNOWN BELIEFS; acquire global 1-permit semaphore; structured_chat;
/// truncate to 5 observations in Rust; per observation embed/route/merge/
/// upsert + assumption + contradiction in one transaction; process
/// corrections as `forget(wrong)`; `watermark = MAX(watermark, through)`.
///
/// Errors are swallowed by the caller at the seams (`tracing::warn!`,
/// matching compaction). A failed extraction never fails a turn.
pub async fn extract_and_record<S: StructuredChat>(
    engine: &crate::engine::PathwayEngine,
    host_pool: &SqlitePool,
    chat: &S,
    req: LearnRequest<'_>,
    trigger: LearnTrigger,
) -> Result<LearnOutcome> {
    let _acquired = engine.learn_lock(req.session_id).await?;
    if let Some(paused) = engine.learn_paused(req.session_id).await {
        if paused {
            return Ok(LearnOutcome::default());
        }
    }

    let db = &engine.db;
    let watermark = db.last_learned_rowid(req.session_id).await?;
    if req.through_rowid <= watermark {
        return Ok(LearnOutcome::default());
    }

    // Build the chunk: use the given one, else read rows (watermark, through]
    // from the host db, dropping role='system', tool-masking, truncating.
    let chunk = match &req.given_chunk {
        Some(c) => c.clone(),
        None => host::read_unlearned_chunk(host_pool, req.session_id, watermark, req.through_rowid).await?,
    };
    if chunk.trim().is_empty() {
        // Nothing learnable; still advance the watermark so we don't rescan.
        db.advance_learned_rowid(req.session_id, req.through_rowid).await?;
        return Ok(LearnOutcome::default());
    }

    // Render KNOWN BELIEFS (top 20 by effective weight).
    let known = render_known_beliefs(db).await?;

    let _permit = engine.chat_semaphore().acquire_owned().await.map_err(
        |_| PathwayError::Internal("semaphore closed".into()),
    )?;

    let prompt = build_extraction_prompt(&known, &chunk);
    let parsed = chat
        .structured_chat(prompt, &extraction_schema())
        .await
        .map_err(PathwayError::Extract)?;

    // Truncate to 5 observations IN RUST (hard cap).
    let observations = parsed
        .get("observations")
        .and_then(|o| o.as_array())
        .cloned()
        .unwrap_or_default();
    let observations: Vec<Value> = observations.into_iter().take(5).collect();

    // Process observations -> beliefs in one transaction.
    let obs_count = observations.len();
    let mut outcome = LearnOutcome { observations: obs_count, ..Default::default() };

    for obs in &observations {
        let statement = obs.get("statement").and_then(|s| s.as_str()).unwrap_or("").to_string();
        if statement.trim().is_empty() {
            continue;
        }
        let provenance = Provenance::parse(
            obs.get("provenance").and_then(|p| p.as_str()).unwrap_or("single_observation"),
        );
        let layer = match obs.get("layer").and_then(|l| l.as_str()) {
            Some("identity") => Layer::Context, // never written by extractor
            Some("context") => Layer::Context,
            _ => Layer::Conversation,
        };
        let domain = obs.get("domain").and_then(|d| d.as_str()).map(|s| s.to_string());
        let evidence = obs.get("evidence").and_then(|e| e.as_str()).map(|s| s.to_string());
        let contradicts = obs.get("contradicts").and_then(|c| c.as_str()).map(|s| s.to_string());

        let embedding = engine.embed.embed(&statement).await;
        let now = chrono::Utc::now();
        crate::belief::synthesis::route_observation(
            db,
            &statement,
            &embedding,
            provenance,
            layer,
            domain.as_deref(),
            evidence.as_deref(),
            contradicts.as_deref(),
            Some(req.session_id.to_string()),
            now,
        )
        .await?;
    }

    // Process corrections (forget(wrong) semantics).
    let corrections = parsed
        .get("corrections")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    for c in corrections {
        if let Some(text) = c.as_str() {
            if db
                .forget_by_text(text, &[], crate::store::suppressions::SuppressReason::Wrong)
                .await
                .is_ok()
            {
                outcome.corrections += 1;
            }
        }
    }

    // Advance the watermark forward only (MAX guard).
    db.advance_learned_rowid(req.session_id, req.through_rowid).await?;
    db.audit(
        match trigger {
            LearnTrigger::TurnEnd => "learn_turn_end",
            LearnTrigger::Compaction => "learn_compaction",
            LearnTrigger::IdleClose => "learn_idle_close",
        },
        Some(&format!(
            "session={} through={} observations={}",
            req.session_id, req.through_rowid, outcome.observations
        )),
    )
    .await?;
    Ok(outcome)
}

/// Render the top-20 known beliefs by effective weight for the prompt.
async fn render_known_beliefs(db: &Db) -> Result<String> {
    let all = db.list_beliefs(None).await?;
    let now = chrono::Utc::now();
    let mut scored: Vec<(f64, &Belief)> = all
        .iter()
        .map(|b| (crate::belief::effective_weight(b, None, now), b))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let lines: Vec<String> = scored
        .iter()
        .take(20)
        .map(|(_, b)| format!("- [{}] {}", b.layer.as_str(), b.text))
        .collect();
    Ok(lines.join("\n"))
}

/// Helper to keep embeddings off the BLOB write path in tests etc.
pub fn blob(embedding: &[f32]) -> Vec<u8> {
    encode_embedding(embedding)
}

pub mod host;
