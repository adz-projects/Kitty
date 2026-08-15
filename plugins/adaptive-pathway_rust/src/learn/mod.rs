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
                        "statement": {"type": "string", "description": "One behavioral fact about the USER, third person, present tense: 'User prefers ...', 'User is working on ...'. Never about the assistant. Never about code or files."},
                        "provenance": {"type": "string", "enum": [
                            "direct_statement", "controlled_test", "inferred_pattern", "single_observation"
                        ]},
                        "layer": {"type": "string", "enum": ["context", "conversation"]},
                        "domain": {"type": "string", "description": "Short topic label such as 'coding', 'writing', 'cooking'. Empty string if not topic-specific."},
                        "evidence": {"type": "string", "description": "A short quote from the exchanges that supports this."},
                        "contradicts": {"type": "string", "description": "If this conflicts with a line in KNOWN BELIEFS, that line's exact text. Empty string otherwise."}
                    },
                    // Every field required, with empty-string sentinels rather
                    // than nullable/optional ones: llama.cpp's grammar-
                    // constrained decoding is far more reliable against a
                    // *total* grammar at 1.2B-scale models -- a partial
                    // `required` list (as this previously was, missing
                    // domain/evidence/contradicts) lets the grammar treat
                    // those three as optional, which is exactly the case
                    // where small models most often emit a truncated or
                    // malformed object.
                    "required": ["statement", "provenance", "layer", "domain", "evidence", "contradicts"]
                }
            },
            "corrections": {
                "type": "array",
                "items": {"type": "string", "description": "Exact text of a KNOWN BELIEF the user explicitly denied or overrode."}
            },
            "tone": {"type": "string", "description": "The user's tone in these exchanges, one or two words."},
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
    if engine.learn_paused(req.session_id).await? {
        return Ok(LearnOutcome::default());
    }

    let db = &engine.db;
    let watermark = db.last_learned_rowid(req.session_id).await?;
    if req.through_rowid <= watermark {
        return Ok(LearnOutcome::default());
    }

    // Bump the global exchange counter -- the clock assumption scheduling
    // runs against (`Db::global_exchange_count`, `belief::lifecycle`). Once
    // per genuine learn pass (past the pause/lock/watermark guards above),
    // matching the plan's "exchanges_at_flag = <global exchange counter>"
    // framing: ~20 *learn-worthy* exchanges, not calendar time or raw turn
    // count. Best-effort -- a failed bump must never fail the learn pass.
    let _ = db.bump_global_exchange().await;

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

    // The permit is scoped to exactly this call -- it used to be bound at
    // function level, which meant it stayed held through every subsequent
    // per-observation embed call and DB write below (potentially several
    // seconds of unrelated work), blocking every *other* session's learn
    // pass from even starting its own `structured_chat` call for that whole
    // window. The semaphore exists to keep concurrent constrained-decode
    // requests off Ollama, not to serialize embedding/DB work too.
    //
    // A defensive timeout (independent of whatever timeout the concrete
    // `StructuredChat` implementation may or may not have internally --
    // this trait is also implemented by test mocks with none) guards
    // against a hung call holding the permit, and therefore every other
    // session's learn pass, forever.
    const STRUCTURED_CHAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    let prompt = build_extraction_prompt(&known, &chunk);
    let parsed = {
        let _permit = engine.chat_semaphore().acquire_owned().await.map_err(
            |_| PathwayError::Internal("semaphore closed".into()),
        )?;
        tokio::time::timeout(STRUCTURED_CHAT_TIMEOUT, chat.structured_chat(prompt, &extraction_schema()))
            .await
            .map_err(|_| PathwayError::Extract("structured_chat timed out".into()))?
            .map_err(PathwayError::Extract)?
    };

    // Truncate to 5 observations IN RUST (hard cap).
    let observations = parsed
        .get("observations")
        .and_then(|o| o.as_array())
        .cloned()
        .unwrap_or_default();
    let observations: Vec<Value> = observations.into_iter().take(5).collect();

    // Process observations -> beliefs in one transaction. Counted as they're
    // actually processed below, not from the raw parsed length -- an
    // empty-statement entry is skipped (`continue`, never routed to a
    // belief), so counting before that filter over-reported by however many
    // entries were skipped.
    let mut outcome = LearnOutcome::default();

    // One batch id shared by every observation this pass produces. These came
    // out of a single stretch of conversation and are jointly meaningful --
    // co-occurring constraints on one problem, typically semantically distant
    // from each other and so invisible to the cosine graph recall otherwise
    // relies on. Recording the relation lets `select_for_turn` pull siblings
    // in behind an anchor belief (see migration 006 and
    // `vector::spread::diffuse_activation`'s co-occurrence adjacency).
    //
    // Allocated unconditionally rather than only when this pass yields more
    // than one observation: a singleton batch simply contributes no edges
    // (the sibling self-join requires two *distinct* belief ids sharing a
    // batch), so there's nothing to guard against and one code path is
    // cheaper to keep correct than two.
    let batch_id = crate::store::audit::uuid_string();

    for obs in &observations {
        let statement = obs.get("statement").and_then(|s| s.as_str()).unwrap_or("").to_string();
        if statement.trim().is_empty() {
            continue;
        }
        outcome.observations += 1;
        let provenance = Provenance::parse(
            obs.get("provenance").and_then(|p| p.as_str()).unwrap_or("single_observation"),
        );
        let layer = match obs.get("layer").and_then(|l| l.as_str()) {
            Some("identity") => Layer::Context, // never written by extractor
            Some("context") => Layer::Context,
            _ => Layer::Conversation,
        };
        // Required-with-empty-string-sentinel fields (see extraction_schema's
        // doc comment) -- normalize "" to None so "the model had nothing to
        // say for this field" and "the model said something" stay distinct
        // downstream (an empty-string domain would otherwise behave like a
        // real, if useless, domain tag).
        let non_empty = |s: Option<&str>| s.filter(|s| !s.is_empty()).map(str::to_string);
        let domain = non_empty(obs.get("domain").and_then(|d| d.as_str()));
        let evidence = non_empty(obs.get("evidence").and_then(|e| e.as_str()));
        let contradicts = non_empty(obs.get("contradicts").and_then(|c| c.as_str()));

        let (embedding, semantic) = engine.embed.embed_with_space(&statement).await;
        let now = chrono::Utc::now();
        // Tag with the embedding space ACTUALLY used — a lexical hash-fallback
        // vector must not be labeled as the semantic model (it would join the
        // same recall/merge pool as real embeddings and compare garbage, and
        // never be flagged stale by `list_stale_embedding_beliefs`).
        let embedding_model = if semantic {
            engine.cfg.embedding.ollama_model.as_str()
        } else {
            crate::config::HASH_EMBED_MODEL
        };
        crate::belief::synthesis::route_observation(
            db,
            &statement,
            &embedding,
            embedding_model,
            provenance,
            layer,
            domain.as_deref(),
            evidence.as_deref(),
            contradicts.as_deref(),
            Some(req.session_id.to_string()),
            Some(batch_id.as_str()),
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
            // Skip empty/whitespace corrections (audit #116): every
            // text-resolution step downstream of here matches on `contains`,
            // and `""` is a substring of *every* belief — a junk `[""]`
            // correction would otherwise tombstone the first belief in the
            // table.
            if text.trim().is_empty() {
                continue;
            }
            // `forget_by_text`'s cosine fallback needs a real embedding in
            // the same space as the stored beliefs -- the exact/substring
            // resolution steps ahead of it don't need one, but a correction
            // that doesn't textually match anything (the model paraphrased
            // the KNOWN BELIEFS line rather than quoting it) would
            // otherwise silently fail to resolve.
            let embedding = engine.embed.embed(text).await;
            if db
                .forget_by_text(text, &embedding, &[], crate::store::suppressions::SuppressReason::Wrong)
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
        .map(|(_, b)| format!("- [{}] {}", b.layer.as_str(), cap_belief_line(&b.text)))
        .collect();
    Ok(lines.join("\n"))
}

/// Cap on a single belief's rendered length in the KNOWN BELIEFS block
/// (audit #131): belief text is never length-capped at write time, so 20
/// pathologically long texts would otherwise be inlined whole into every
/// extraction prompt.
const KNOWN_BELIEF_LINE_MAX_CHARS: usize = 300;

fn cap_belief_line(text: &str) -> String {
    if text.chars().count() <= KNOWN_BELIEF_LINE_MAX_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(KNOWN_BELIEF_LINE_MAX_CHARS).collect();
    format!("{kept}…")
}

/// Helper to keep embeddings off the BLOB write path in tests etc.
pub fn blob(embedding: &[f32]) -> Vec<u8> {
    encode_embedding(embedding)
}

pub mod host;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn belief_lines_are_capped_for_the_prompt() {
        // Audit #131: belief text is never length-capped at write time, so
        // the KNOWN BELIEFS block caps each rendered line instead of
        // inlining pathologically long texts whole.
        let long = "x".repeat(1000);
        let capped = cap_belief_line(&long);
        assert_eq!(capped.chars().count(), KNOWN_BELIEF_LINE_MAX_CHARS + 1); // + the ellipsis
        assert!(capped.ends_with('…'));

        let short = "short belief";
        assert_eq!(cap_belief_line(short), short);
    }
}
