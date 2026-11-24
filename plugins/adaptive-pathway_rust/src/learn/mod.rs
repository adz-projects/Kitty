//! Learning: one shared extraction+record function used by both the
//! compaction-piggyback seam (A) and the turn-end seam (B), guarded by a
//! per-session learn lock and a forward-only watermark.

use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::error::{PathwayError, Result};
use crate::store::beliefs::{Belief, BeliefKind, BeliefPatch, Layer, Provenance};
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
                        "statement": {"type": "string", "description": "One durable fact about the USER that passes all four tests (about the person, durable, transferable, evidenced), third person, present tense. Never a one-off lookup or errand, and never about the topic, code, tools or the assistant."},
                        "provenance": {"type": "string", "enum": [
                            "direct_statement", "controlled_test", "inferred_pattern", "single_observation"
                        ]},
                        "layer": {"type": "string", "enum": ["context", "conversation"]},
                        "domain": {"type": "string", "description": "Short topic label such as 'coding', 'writing', 'research' when it holds only in that area. Empty string only when it plainly holds across every kind of request."},
                        "evidence": {"type": "string", "description": "A short quote from the exchanges that supports this."},
                        "contradicts": {"type": "string", "description": "If this conflicts with a line in KNOWN BELIEFS, that line's exact text. Empty string otherwise."},
                        "kind": {"type": "string", "enum": ["preference", "stance", "context", "trait"], "description": "preference: how they like things done, explained or sourced. stance: a view on a contested question. context: a project, role or situation lasting weeks or more (never a single lookup or errand). trait: an enduring orientation or background."},
                        "relates_to": {
                            "type": "array",
                            "description": "Lines in KNOWN BELIEFS this one is meaningfully connected to (not mere topic overlap). Empty if none.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "known_belief": {"type": "string", "description": "That line's exact text, without the [layer] tag."},
                                    "relation": {"type": "string", "enum": ["explains", "in_tension_with", "specializes"], "description": "explains: this belief explains or motivates that one. in_tension_with: they pull in different directions without flatly contradicting. specializes: this is a special case of that one."}
                                },
                                "required": ["known_belief", "relation"]
                            }
                        }
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
                    "required": ["statement", "provenance", "layer", "domain", "evidence", "contradicts", "kind", "relates_to"]
                }
            },
            "corrections": {
                "type": "array",
                "items": {"type": "string", "description": "Exact text of a KNOWN BELIEF the user explicitly denied or overrode."}
            },
            "test_results": {
                "type": "array",
                "description": "For each line in TESTS IN FLIGHT: did the exchanges confirm or refute that assumption about the user? Report no_signal when they say nothing about it.",
                "items": {
                    "type": "object",
                    "properties": {
                        "assumption": {"type": "string", "description": "The TESTS IN FLIGHT line's exact text."},
                        "outcome": {"type": "string", "enum": ["confirmed", "refuted", "no_signal"]}
                    },
                    "required": ["assumption", "outcome"]
                }
            }
        },
        "required": ["observations", "corrections", "test_results"]
    })
}

/// Exactly 2 messages framing "learn about the user -- not the task, not the
/// assistant", mirroring the daemon's `build_summarizer_prompt` shape.
pub fn build_extraction_prompt(known_beliefs: &str, chunk: &str) -> Vec<Value> {
    build_extraction_prompt_with_tests(known_beliefs, &[], chunk)
}

/// The extractor's instructions: four tests every candidate must pass, then
/// provenance and kind, then a few made-up examples deliberately spread over
/// unrelated fields. The examples show the shape of a keep and a drop and
/// carry nothing of any real user, so a small model that copies one learns
/// nothing false about the person.
pub const EXTRACTION_SYSTEM_PROMPT: &str = "You are learning how to work better with the person in this conversation. \
Extract only what they reveal about themselves that would still help in a different conversation weeks from now.

Keep a candidate only if it passes all four tests:
1. ABOUT THE PERSON: it describes them (how they think, decide, learn, weigh evidence, communicate, what they value, \
what they are working toward over weeks or months, or an instruction for how to work with them), not the topic, the task, \
the tools or the assistant.
2. DURABLE: it would plausibly still be true in a month. One question or errand shows only that they asked once.
3. TRANSFERABLE: knowing it would change how you answer an unrelated future request.
4. EVIDENCED: you can quote the words that show it. If you would have to guess, leave it out.

Provenance: direct_statement when they say it about themselves; inferred_pattern when the conversation shows it more \
than once; single_observation when it shows once.
Kind: preference (how they like things done or explained), stance (a view on a contested question), \
context (a project, role or situation lasting weeks or more), trait (an enduring orientation or background).
How they write is not how they want to be answered: a fact about their own prose (voice, tone, habits in \
their drafts) is a trait, worded \"In their own writing, …\", never a preference about replies.

Examples of the shape only. They are invented; never copy them:
- Keep: \"Wants the recommendation first, then the reasoning\" (said \"just tell me what to do first\" about a garden plan).
- Keep: \"Asks where statistics come from before accepting them\" (asked for the source of two different figures).
- Keep: \"Is writing a thesis on medieval trade routes\" (context; mentioned chapters and a deadline months out).
- Drop: \"Asked how long to boil an egg\" (one task; says nothing about how they work).
- Drop: \"Is comparing two phone models\" (a one-off lookup).
- Drop: \"The assistant explained recursion\" (about the assistant).

Most exchanges contain nothing that passes all four tests: return an empty observations list then.";

/// [`build_extraction_prompt`] plus the assumptions put to this chat that
/// no pass has judged yet ("TESTS IN FLIGHT"), for `test_results`.
pub fn build_extraction_prompt_with_tests(known_beliefs: &str, tests: &[String], chunk: &str) -> Vec<Value> {
    let system = EXTRACTION_SYSTEM_PROMPT;
    vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": format!(
            "Already known about this user (do not duplicate):\n{}\n\n\
             TESTS IN FLIGHT (assumptions the assistant was asked to check in this chat; \
             judge each from the conversation):\n{}\n\n\
             Conversation to learn from:\n{}",
            if known_beliefs.is_empty() { "(none)" } else { known_beliefs },
            if tests.is_empty() {
                "(none)".to_string()
            } else {
                tests.iter().map(|t| format!("- {t}")).collect::<Vec<_>>().join("\n")
            },
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
    // Bumped where the pass lands (with its watermark), not here: an
    // interrupted pass is re-run over the same range, and bumping up front
    // advanced the clock twice for one exchange.

    // Build the chunk: use the given one, else read rows (watermark, through]
    // from the host db, dropping role='system', tool-masking, truncating.
    let chunk = match &req.given_chunk {
        Some(c) => c.clone(),
        None => host::read_unlearned_chunk(host_pool, req.session_id, watermark, req.through_rowid).await?,
    };
    if chunk.trim().is_empty() {
        // Nothing learnable; still advance the watermark so we don't rescan.
        let _ = db.bump_global_exchange().await;
        db.advance_learned_rowid(req.session_id, req.through_rowid).await?;
        return Ok(LearnOutcome::default());
    }

    // The host's gate, when it has one, may judge the chunk not worth a pass
    // (nothing durable about the user in it). Same as an empty chunk: the
    // watermark advances so the range is never re-sent, and nothing is
    // learned. The exchange clock above still moved -- it counts exchanges,
    // not extractions.
    if let Some(gate) = engine.gate() {
        if let Some(p) = gate.skip_learning(&chunk).await {
            let _ = db.bump_global_exchange().await;
            db.advance_learned_rowid(req.session_id, req.through_rowid).await?;
            let detail = format!("session={} through={} p={p:.2}", req.session_id, req.through_rowid);
            let _ = db.audit("learn_gated", Some(&detail)).await;
            return Ok(LearnOutcome::default());
        }
    }

    // Render KNOWN BELIEFS (top 20 by effective weight).
    // KNOWN BELIEFS are the ones most relevant to this chunk (weight x
    // similarity), not just the heaviest: what the conversation contradicts,
    // explains or pulls against is usually a related belief, and the
    // extractor can only report relations to lines it is shown.
    let chunk_vec = match engine.embed.embed_with_space(&chunk).await {
        (v, true) => v,
        _ => Vec::new(),
    };
    let known = render_known_beliefs(db, &chunk_vec, &engine.cfg.embedding.ollama_model).await?;

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
    //
    // 300s, matching the daemon's summarizer chain wall-clock cap
    // (`SUMMARIZER_OVERALL_TIMEOUT`), so the chain -- not this budget --
    // decides when a call has failed. The turn's own provider does this pass
    // (a remote or reasoning model routinely needs well over the old 30s), and
    // at 30s every pass on such a provider timed out and nothing was learned.
    const STRUCTURED_CHAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
    let in_flight = db.tests_in_flight(req.session_id).await.unwrap_or_default();
    let test_lines: Vec<String> = in_flight.iter().map(|(_, _, t)| t.clone()).collect();
    let prompt = build_extraction_prompt_with_tests(&known, &test_lines, &chunk);
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

    // Observations are counted as they're actually processed below, not from
    // the raw parsed length -- an empty-statement entry is skipped (never
    // routed to a belief), so counting before that filter over-reported.

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

    // Everything slow happens first, outside the transaction: one embedding
    // per observation and per correction. Then the whole pass is written in
    // one transaction (observations, test results, corrections, the exchange
    // clock and the watermark), so a pass interrupted by the next turn lands
    // whole or not at all. Committed per observation with the watermark last,
    // a re-run merged the observations already recorded a second time.
    struct Prepared {
        obs: Value,
        statement: String,
        embedding: Vec<f32>,
        embedding_model: String,
        provenance: Provenance,
        layer: Layer,
        domain: Option<String>,
        evidence: Option<String>,
        contradicts: Option<String>,
        kind: crate::store::beliefs::BeliefKind,
    }
    let mut prepared: Vec<Prepared> = Vec::new();
    for obs in &observations {
        let statement = obs.get("statement").and_then(|s| s.as_str()).unwrap_or("").to_string();
        if statement.trim().is_empty() {
            continue;
        }
        let provenance = Provenance::parse(
            obs.get("provenance").and_then(|p| p.as_str()).unwrap_or("single_observation"),
        );
        let has_links = obs
            .get("relates_to")
            .and_then(|r| r.as_array())
            .is_some_and(|a| !a.is_empty());
        let layer = match (obs.get("layer").and_then(|l| l.as_str()), provenance, has_links) {
            (_, Provenance::SingleObservation, _) => Layer::Conversation,
            (_, Provenance::InferredPattern, false) => Layer::Conversation,
            (Some("identity" | "context"), _, _) => Layer::Context, // extractor never writes Identity
            _ => Layer::Conversation,
        };
        // Required-with-empty-string-sentinel fields (see extraction_schema's
        // doc comment) -- normalize "" to None so "the model had nothing to
        // say for this field" and "the model said something" stay distinct
        // downstream (an empty-string domain would otherwise behave like a
        // real, if useless, domain tag).
        let non_empty = |s: Option<&str>| s.filter(|s| !s.is_empty()).map(str::to_string);
        let (embedding, semantic) = engine.embed.embed_with_space(&statement).await;
        // Tag with the embedding space ACTUALLY used — a lexical hash-fallback
        // vector must not be labeled as the semantic model (it would join the
        // same recall/merge pool as real embeddings and compare garbage, and
        // never be flagged stale by `list_stale_embedding_beliefs`).
        let embedding_model = if semantic {
            engine.cfg.embedding.ollama_model.clone()
        } else {
            crate::config::HASH_EMBED_MODEL.to_string()
        };
        prepared.push(Prepared {
            obs: obs.clone(),
            domain: non_empty(obs.get("domain").and_then(|d| d.as_str())),
            evidence: non_empty(obs.get("evidence").and_then(|e| e.as_str())),
            contradicts: non_empty(obs.get("contradicts").and_then(|c| c.as_str())),
            kind: crate::store::beliefs::BeliefKind::parse(
                obs.get("kind").and_then(|k| k.as_str()).unwrap_or("context"),
            ),
            statement,
            embedding,
            embedding_model,
            provenance,
            layer,
        });
    }

    // Process corrections (forget(wrong) semantics). Skip empty/whitespace
    // corrections (audit #116): every text-resolution step downstream
    // matches on `contains`, and `""` is a substring of *every* belief — a
    // junk `[""]` correction would otherwise tombstone the first belief in
    // the table. `forget_by_text`'s cosine fallback needs a real embedding
    // in the same space as the stored beliefs (a correction that paraphrased
    // the KNOWN BELIEFS line rather than quoting it would otherwise silently
    // fail to resolve).
    let mut corrections: Vec<(String, Vec<f32>)> = Vec::new();
    for c in parsed
        .get("corrections")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default()
    {
        if let Some(text) = c.as_str().filter(|t| !t.trim().is_empty()) {
            let embedding = engine.embed.embed(text).await;
            corrections.push((text.to_string(), embedding));
        }
    }

    // Judge the assumption tests this chat had in flight.
    let mut tests: Vec<(String, Option<String>, String)> = Vec::new();
    if let Some(results) = parsed.get("test_results").and_then(|r| r.as_array()) {
        for r in results {
            let (Some(text), Some(result)) = (
                r.get("assumption").and_then(|v| v.as_str()),
                r.get("outcome").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let Some((assumption_id, belief_id, _)) = in_flight
                .iter()
                .find(|(_, _, t)| t.trim() == text || t.contains(text) || text.contains(t.trim()))
            else {
                continue;
            };
            tests.push((assumption_id.clone(), belief_id.clone(), result.to_string()));
        }
    }

    let session_id = req.session_id;
    let through = req.through_rowid;
    let outcome = db
        .run_in_transaction(|| async {
            let mut outcome = LearnOutcome::default();
            let now = chrono::Utc::now();
            for p in &prepared {
                outcome.observations += 1;
                let landed = crate::belief::synthesis::route_observation_with(
                    db,
                    &p.statement,
                    &p.embedding,
                    &p.embedding_model,
                    p.provenance,
                    p.layer,
                    p.domain.as_deref(),
                    p.evidence.as_deref(),
                    p.contradicts.as_deref(),
                    Some(session_id.to_string()),
                    Some(batch_id.as_str()),
                    now,
                    p.kind,
                )
                .await?;
                if let Some(id) = landed {
                    record_relations(db, &id, &p.obs).await;
                }
            }
            for (assumption_id, belief_id, result) in &tests {
                apply_test_result(db, session_id, assumption_id, belief_id.as_deref(), result).await;
            }
            for (text, embedding) in &corrections {
                if db
                    .forget_by_text(text, embedding, &[], crate::store::suppressions::SuppressReason::Wrong)
                    .await
                    .is_ok()
                {
                    outcome.corrections += 1;
                }
            }
            let _ = db.bump_global_exchange().await;
            // Advance the watermark forward only (MAX guard).
            db.advance_learned_rowid(session_id, through).await?;
            db.audit(
                match trigger {
                    LearnTrigger::TurnEnd => "learn_turn_end",
                    LearnTrigger::Compaction => "learn_compaction",
                    LearnTrigger::IdleClose => "learn_idle_close",
                },
                Some(&format!(
                    "session={session_id} through={through} observations={}",
                    outcome.observations
                )),
            )
            .await?;
            Ok(outcome)
        })
        .await?;
    Ok(outcome)
}

/// Record what extraction said this belief relates to (`relates_to`):
/// typed links, and for `in_tension_with` an open contradiction, so
/// `[In tension]` can surface it. Best-effort.
async fn record_relations(db: &Db, belief_id: &str, obs: &Value) {
    let Some(rels) = obs.get("relates_to").and_then(|r| r.as_array()) else {
        return;
    };
    for rel in rels {
        let (Some(text), Some(relation)) = (
            rel.get("known_belief").and_then(|v| v.as_str()),
            rel.get("relation")
                .and_then(|v| v.as_str())
                .and_then(crate::store::links::Relation::parse),
        ) else {
            continue;
        };
        let Ok(Some(other)) = db.best_text_match(text).await else {
            continue;
        };
        if other == belief_id {
            continue;
        }
        let _ = db.insert_link(belief_id, &other, relation).await;
        if relation == crate::store::links::Relation::InTensionWith
            && !db.has_contradiction(belief_id, &other).await.unwrap_or(true)
        {
            let _ = db
                .insert_contradiction(&crate::store::contradictions::Contradiction {
                    id: crate::store::audit::uuid_string(),
                    belief_a: belief_id.to_string(),
                    belief_b: other,
                    status: "open".into(),
                    resolved_b: None,
                    reason: Some("model_tension".into()),
                    created_at: chrono::Utc::now(),
                    resolved_at: None,
                })
                .await;
        }
    }
}

/// Apply one judged assumption test. `confirmed`: the belief is now
/// validated (controlled-test evidence, `tested`) and the assumption passes.
/// `refuted`: confidence falls by the direct-test step and the assumption
/// fails. `no_signal` (or anything else): nothing; it stays in flight.
/// Best-effort.
async fn apply_test_result(
    db: &Db,
    session_id: &str,
    assumption_id: &str,
    belief_id: Option<&str>,
    outcome: &str,
) {
    use crate::belief::provenance::{reinforce_toward, reinforcement_step};
    use crate::belief::EvidenceKind;
    let passed = match outcome {
        "confirmed" => true,
        "refuted" => false,
        _ => return,
    };
    if let Some(bid) = belief_id {
        if let Ok(Some(b)) = db.get_belief(bid).await {
            let now = chrono::Utc::now();
            let patch = if passed {
                crate::store::beliefs::BeliefPatch {
                    confidence: Some(reinforce_toward(
                        b.confidence,
                        true,
                        reinforcement_step(EvidenceKind::ControlledTest),
                    )),
                    tested: Some(true),
                    provenance: Some(crate::belief::synthesis::max_provenance(
                        b.provenance,
                        Provenance::ControlledTest,
                    )),
                    last_confirmed_at: Some(now),
                    ..Default::default()
                }
            } else {
                crate::store::beliefs::BeliefPatch {
                    confidence: Some(reinforce_toward(
                        b.confidence,
                        false,
                        reinforcement_step(EvidenceKind::DirectTest),
                    )),
                    ..Default::default()
                }
            };
            let _ = db.update_belief(bid, &patch, now).await;
        }
        let _ = db.resolve_assumption_for_belief(bid, passed).await;
    }
    let _ = db.mark_test_judged(assumption_id, session_id).await;
    let _ = db
        .audit(
            "assumption_test",
            Some(&format!("assumption={assumption_id} outcome={outcome}")),
        )
        .await;
}

/// Render the 20 known beliefs most relevant to the chunk (effective weight
/// times similarity; weight alone when the chunk has no semantic vector).
async fn render_known_beliefs(db: &Db, chunk_vec: &[f32], space: &str) -> Result<String> {
    let now = chrono::Utc::now();
    let suppressed = db.active_suppressed_text_hashes(now).await.unwrap_or_default();
    let all = crate::belief::filter_suppressed(db.list_beliefs(None).await?, &suppressed);
    let mut scored: Vec<(f64, &Belief)> = all
        .iter()
        .map(|b| {
            let w = crate::belief::effective_weight(b, None, now);
            let sim = if !chunk_vec.is_empty() && b.embedding_model == space {
                crate::vector::ops::cosine(chunk_vec, &b.embedding).max(0.0)
            } else {
                0.0
            };
            (w * (0.5 + 0.5 * sim), b)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.id.cmp(&b.1.id))
    });
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

/// JSON schema for cross-session inductive synthesis of staged evidence.
pub fn synthesis_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "patterns": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "statement": {
                            "type": "string",
                            "description": "One abstract, durable behavioral or epistemic trait induced across multiple staged observations (not a restatement of one single errand or lookup)."
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["preference", "stance", "trait", "context"]
                        },
                        "domain": {
                            "type": "string",
                            "description": "Short topic label if topic-bound, or empty string if cross-cutting."
                        },
                        "evidence_ids": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Exact [id] values from STAGED OBSERVATIONS that jointly support this generalized pattern (must span >= 2 distinct sessions)."
                        }
                    },
                    "required": ["statement", "kind", "domain", "evidence_ids"]
                }
            }
        },
        "required": ["patterns"]
    })
}

/// Prompt asking the model to induce cross-session abstract behavioral traits
/// from multiple single-session staged observations.
pub fn build_synthesis_prompt(
    known_beliefs: &str,
    staged: &[crate::store::staged::StagedEvidence],
) -> Vec<Value> {
    let system = "You are synthesizing durable, cross-session traits about the user from \
        single-session observations. Emit ONLY abstract behavioral, epistemic, or creative \
        patterns supported by evidence from AT LEAST TWO distinct sessions. Ignore isolated \
        lookups, transient errands, or one-off questions. Return an empty list if no true \
        cross-session pattern exists.";
    let mut lines = Vec::with_capacity(staged.len());
    for item in staged {
        let domain = item.domain.as_deref().filter(|d| !d.is_empty()).unwrap_or("general");
        lines.push(format!(
            "- [id={}] [session={}] [kind={}] [domain={}] {}",
            item.id,
            item.session_id,
            item.kind.as_str(),
            domain,
            item.statement
        ));
    }
    vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": format!(
            "Already known about this user (do not duplicate):\n{}\n\n\
             STAGED OBSERVATIONS across sessions:\n{}",
            if known_beliefs.is_empty() { "(none)" } else { known_beliefs },
            lines.join("\n")
        )}),
    ]
}

/// Settings key: the newest staged item's `created_at` the last synthesis
/// attempt saw, so an unchanged pending set is not sent to the model again.
const SYNTHESIS_SEEN_KEY: &str = "staged_synthesis_seen_through";

/// Inductive synthesis over `staged_evidence`: expires items older than
/// `STAGED_RETENTION_DAYS`, skips when fewer than 3 pending items span at
/// least 2 distinct sessions or nothing was staged since the last attempt,
/// and promotes model-proposed patterns verified here to span >= 2 sessions
/// into `Layer::Context`. Only the items a written pattern cites are
/// consumed: the rest stay pending, so evidence from one chat can still
/// combine with a chat that comes later. Returns the patterns written.
pub async fn synthesize_staged_evidence<S: StructuredChat>(
    engine: &crate::engine::PathwayEngine,
    chat: &S,
) -> Result<usize> {
    let db = &engine.db;
    let now = chrono::Utc::now();
    let _ = db
        .expire_old_staged_evidence(
            now - chrono::Duration::days(crate::consolidate::STAGED_RETENTION_DAYS),
        )
        .await?;

    // Nothing new since the last attempt: the model already judged this set.
    let newest = db.newest_pending_staged_at().await?;
    let seen = db
        .get_setting(SYNTHESIS_SEEN_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.with_timezone(&chrono::Utc));
    if let (Some(n), Some(s)) = (newest, seen) {
        if n <= s {
            return Ok(0);
        }
    }

    let pending = db.list_pending_staged_evidence(40).await?;
    if pending.len() < 3 {
        return Ok(0);
    }
    let distinct_sessions: std::collections::HashSet<&str> =
        pending.iter().map(|s| s.session_id.as_str()).collect();
    if distinct_sessions.len() < 2 {
        return Ok(0);
    }

    let known = render_known_beliefs(db, &[], &engine.cfg.embedding.ollama_model).await?;
    let prompt = build_synthesis_prompt(&known, &pending);
    const STRUCTURED_CHAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
    let parsed = {
        let _permit = engine
            .chat_semaphore()
            .acquire_owned()
            .await
            .map_err(|_| PathwayError::Internal("semaphore closed".into()))?;
        tokio::time::timeout(
            STRUCTURED_CHAT_TIMEOUT,
            chat.structured_chat(prompt, &synthesis_schema()),
        )
        .await
        .map_err(|_| PathwayError::Extract("synthesize_staged_evidence timed out".into()))?
        .map_err(PathwayError::Extract)?
    };

    let by_id: std::collections::HashMap<&str, &crate::store::staged::StagedEvidence> =
        pending.iter().map(|s| (s.id.as_str(), s)).collect();
    let patterns = parsed
        .get("patterns")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();

    // Recorded once the model has answered, so a failed call is retried.
    if let Some(n) = newest {
        let _ = db.set_setting(SYNTHESIS_SEEN_KEY, &n.to_rfc3339()).await;
    }

    let mut written = 0usize;
    let mut consumed: Vec<String> = Vec::new();
    for pat in patterns.into_iter().take(5) {
        let statement = pat
            .get("statement")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if statement.is_empty() {
            continue;
        }
        let hash = crate::belief::synthesis::text_hash(statement);
        if db.has_permanent_tombstone(&hash).await? {
            continue;
        }
        let cited_ids: Vec<&str> = pat
            .get("evidence_ids")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .collect();
        let cited_sessions: std::collections::HashSet<&str> = cited_ids
            .iter()
            .filter_map(|id| by_id.get(id).map(|s| s.session_id.as_str()))
            .collect();
        if cited_sessions.len() < 2 {
            continue;
        }

        let kind = pat
            .get("kind")
            .and_then(|v| v.as_str())
            .map(BeliefKind::parse)
            .unwrap_or(BeliefKind::Trait);
        let domain = pat
            .get("domain")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let (embedding, semantic) = engine.embed.embed_with_space(statement).await;
        // Tag with the space actually used (see `extract_and_record`): a hash
        // fallback vector must never join the semantic pool under its name.
        let model = if semantic {
            engine.cfg.embedding.ollama_model.clone()
        } else {
            crate::config::HASH_EMBED_MODEL.to_string()
        };
        if let Some(belief_id) = crate::belief::synthesis::route_observation_with(
            db,
            statement,
            &embedding,
            &model,
            Provenance::InferredPattern,
            Layer::Context,
            domain,
            Some("cross-session synthesis"),
            None,
            None,
            None,
            now,
            kind,
        )
        .await?
        {
            if let Ok(Some(existing)) = db.get_belief(&belief_id).await {
                let target_sessions = existing.distinct_sessions.max(cited_sessions.len() as i64);
                let target_support = existing.support_count.max(cited_ids.len() as i64);
                let _ = db
                    .update_belief(
                        &belief_id,
                        &BeliefPatch {
                            distinct_sessions: Some(target_sessions),
                            support_count: Some(target_support),
                            ..Default::default()
                        },
                        now,
                    )
                    .await;
            }
            written += 1;
            consumed.extend(
                cited_ids
                    .iter()
                    .filter(|id| by_id.contains_key(*id))
                    .map(|id| id.to_string()),
            );
        }
    }

    consumed.sort();
    consumed.dedup();
    let _ = db.mark_staged_synthesized(&consumed, now).await?;
    if written > 0 {
        let _ = db
            .audit(
                "synthesize_staged",
                Some(&format!(
                    "evaluated={} consumed={} synthesized={written}",
                    pending.len(),
                    consumed.len()
                )),
            )
            .await;
    }
    Ok(written)
}

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
