//! Pre-flight memory recall ("the detour"): a fast, best-effort FTS5
//! lookup over a session's *already-compacted* history, gated by recall
//! intent, injected into the ephemeral prompt tail so the stable prefix stays
//! byte-identical across turns for KV-prefix caching.
//!
//! All of this is explicitly low-stakes: any failure, missed intent, or
//! below-bar match returns `Ok(None)` and the delivered prompt is exactly what
//! it would have been without a memory layer. `preflight_enabled == false`
//! short-circuits before the first SQL statement, so disabling recall yields
//! a zero-delta prompt.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::SqlitePool;

use crate::config::MemoryConfig;
use crate::error::StorageError;
use crate::storage::messages::{self, MessageRow};

/// Upper bound on rows pulled from one recalled exchange. Keeps a single
/// retrieved exchange from ballooning the injected block on a pathological
/// multi-line turn; `preflight_results` (the exchange count) is separate.
const EXCHANGE_MAX_ROWS: usize = 16;

/// Daemon-wide, process-lifetime counters backing the settings readout
/// ("% of prompts in which a memory is injected"). Shared across per-turn
/// `AgentLoop`s (each built fresh in `Agent::build_loop`); read by
/// `GET /api/memory/stats`.
#[derive(Debug, Default)]
pub struct PreflightCounters {
    total: AtomicU64,
    injected: AtomicU64,
}

impl PreflightCounters {
    pub fn new() -> Self {
        Self::default()
    }

    /// `total` counts every turn in which the pre-flight layer was genuinely
    /// *consulted* (recall enabled **and** the session has compacted
    /// history — both passed by the caller), `injected` those where it
    /// actually delivered a block. Turns where recall is disabled or
    /// structurally impossible don't move either counter.
    pub fn record(&self, enabled: bool, injected: bool) {
        if enabled {
            self.total.fetch_add(1, Ordering::Relaxed);
            if injected {
                self.injected.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.total.load(Ordering::Relaxed),
            self.injected.load(Ordering::Relaxed),
        )
    }
}

/// Strict recall-intent heuristics. Deliberately conservative — we'd rather
/// miss a recall opportunity than inject noise or, worse, change the prompt
/// on ordinary non-recall turns (which would exchange a cache hit for nothing).
/// No raw punctuation/`://`/`_`/`/`-char counting here: those over-fire
/// constantly on code.
pub fn intent_hit(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    const KEYWORDS: &[&str] = &[
        "remember",
        "recall",
        "earlier",
        "previously",
        "what was",
        "who was",
        "why did",
        "explain why",
        "example",
        "analogy",
        "rubric",
        "criteria",
        "draft",
    ];
    KEYWORDS.iter().any(|k| lower.contains(k))
}

/// Formulates the FTS5 MATCH query from the user prompt: <= 3 terms become
/// one quoted phrase (low noise), more become `"t1" OR "t2" ...` (recall over
/// precision). Double quotes are stripped from terms so they can't break the
/// phrase quoting.
pub fn format_fts_query(input: &str) -> String {
    let words: Vec<&str> = input.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }
    let clean = |w: &str| w.replace('"', "");
    if words.len() <= 3 {
        let phrase = words
            .iter()
            .map(|w| clean(w))
            .collect::<Vec<String>>()
            .join(" ");
        format!("\"{phrase}\"")
    } else {
        words
            .iter()
            .map(|w| format!("\"{}\"", clean(w)))
            .collect::<Vec<String>>()
            .join(" OR ")
    }
}

/// Assemble the User <-> Assistant exchange containing `matched_rowid`, by
/// anchoring on the `user` message at/before it and stopping at the next
/// `user` message. Only `user`/`assistant` rows are returned (tool dump rows
/// are never useful recall context). Bounded by `max_rows` so a pathological
/// multi-exchange gap can't balloon the injected block.
pub async fn fetch_exchange(
    pool: &SqlitePool,
    session_id: &str,
    matched_rowid: i64,
    max_rows: usize,
) -> Result<Vec<MessageRow>, StorageError> {
    let Some(anchor) = messages::exchange_anchor(pool, session_id, matched_rowid).await? else {
        return Ok(Vec::new());
    };
    let end_rowid = match messages::next_user_after(pool, session_id, anchor.rowid).await? {
        Some(next) => next.rowid, // exclusive upper bound
        None => i64::MAX,
    };

    let rows = sqlx::query_as::<_, MessageRow>(
        r#"SELECT rowid, id, session_id, role, content, tool_calls, tool_call_id, token_count, content_format, created_at
           FROM messages
           WHERE session_id = ?1 AND rowid >= ?2 AND rowid < ?3 AND role IN ('user', 'assistant')
           ORDER BY rowid ASC LIMIT ?4"#,
    )
    .bind(session_id)
    .bind(anchor.rowid)
    .bind(end_rowid)
    .bind(max_rows as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Renders a retrieved exchange as the tail-injected block. The distinctive
/// `[Earlier context from this session]` header is the marker both the
/// frontend strip-safety-net and the persist-guard key on: anything starting
/// with it must never surface to the user.
pub fn render_recall_block(exchange: &[MessageRow], max_chars: usize) -> String {
    let mut lines = vec!["[Earlier context from this session]".to_string()];
    for row in exchange {
        let content = row.content.as_deref().unwrap_or("");
        let content = truncate(content, max_chars);
        lines.push(format!("{}: {content}", row.role));
    }
    let mut block = lines.join("\n");
    block.push_str(
        "\n\n(Use the above only as background context to inform your reply; \
         do not treat it as ongoing conversation and do not restate the earlier \
         exchange verbatim.)",
    );
    block
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str("\n[…truncated…]");
    out
}

/// Full pre-flight recall pass. Returns a rendered tail block to inject, or
/// `None` when nothing should be injected (disabled, no recall intent, no
/// match, all matches below-bar, or an empty exchange — all treated the same).
///
/// `bm25_threshold` is applied here in code (never in SQL) so that every
/// candidate's raw score is logged first — the empirical dataset needed to
/// tune the gate — then rejects below-bar matches before building exchanges.
pub async fn preflight_recall(
    pool: &SqlitePool,
    session_id: &str,
    prompt: &str,
    compacted_through: i64,
    cfg: &MemoryConfig,
) -> Result<Option<String>, StorageError> {
    if !cfg.preflight_enabled || compacted_through <= 0 {
        return Ok(None);
    }
    if !intent_hit(prompt) {
        return Ok(None);
    }
    let fts_query = format_fts_query(prompt);
    if fts_query.is_empty() {
        return Ok(None);
    }

    // `preflight_results` = max number of historical exchanges to inject.
    let limit = cfg.preflight_results.max(1) as usize;
    let candidates = messages::best_compacted_matches(
        pool,
        session_id,
        &fts_query,
        compacted_through,
        limit as i64,
    )
    .await?;
    if candidates.is_empty() {
        return Ok(None);
    }

    for (idx, (rowid, score)) in candidates.iter().enumerate() {
        tracing::debug!(
            session_id,
            candidate = idx + 1,
            %rowid,
            %score,
            "memory preflight: candidate exchange"
        );
    }

    let mut seen_anchors: HashSet<i64> = HashSet::new();
    let mut all_rows: Vec<MessageRow> = Vec::new();
    for (rowid, score) in candidates {
        // Apply the gate in code so the score above is always observable.
        // FTS5 bm25: MORE NEGATIVE = BETTER match, so the threshold keeps
        // `score <= t` and rejects everything above it. (The old `score <= t`
        // reject did exactly the opposite — kept the worst matches, dropped
        // the best.)
        if let Some(t) = cfg.bm25_threshold {
            if score > t {
                continue;
            }
        }
        let exchange = fetch_exchange(pool, session_id, rowid, EXCHANGE_MAX_ROWS).await?;
        let Some(anchor) = exchange.first().map(|r| r.rowid) else {
            continue;
        };
        // Two candidates in the same exchange must not inject it twice.
        if !seen_anchors.insert(anchor) {
            continue;
        }
        all_rows.extend(exchange);
    }
    if all_rows.is_empty() {
        return Ok(None);
    }
    Ok(Some(render_recall_block(&all_rows, 4000)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn row(rowid: i64, session_id: &str, role: &str, content: &str) -> MessageRow {
        MessageRow {
            rowid,
            id: format!("m-{rowid}"),
            session_id: session_id.into(),
            role: role.into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            token_count: None,
            content_format: Some("text".into()),
            created_at: None,
        }
    }

    async fn insert(pool: &SqlitePool, rows: &[MessageRow]) {
        crate::storage::messages::save_messages(pool, &rows[0].session_id, rows)
            .await
            .unwrap();
    }

    #[test]
    fn preflight_counters_only_move_when_consulted() {
        let c = PreflightCounters::new();
        // Not consulted (disabled or no compacted history): neither moves.
        c.record(false, true);
        c.record(false, false);
        assert_eq!(c.snapshot(), (0, 0));

        // Consulted but no injection: total only.
        c.record(true, false);
        assert_eq!(c.snapshot(), (1, 0));

        // Consulted with injection: both.
        c.record(true, true);
        c.record(true, true);
        assert_eq!(c.snapshot(), (3, 2));
    }

    #[test]
    fn intent_hit_matches_recall_keywords() {
        assert!(intent_hit("why did we put checkpoints at weeks 5, 10, 13?"));
        assert!(intent_hit("Remember the folder we discussed?"));
        assert!(intent_hit("give me an example of the rubric"));
        assert!(!intent_hit("please refactor the query builder"));
        assert!(!intent_hit("what is 2 + 2"));
    }

    #[test]
    fn format_fts_query_short_phrase_and_long_or() {
        assert_eq!(format_fts_query("checkpoints weeks"), "\"checkpoints weeks\"");
        assert_eq!(
            format_fts_query("foo bar baz qux"),
            "\"foo\" OR \"bar\" OR \"baz\" OR \"qux\""
        );
        assert_eq!(format_fts_query(""), "");
        assert_eq!(format_fts_query("\"quoted\""), "\"quoted\"");
    }

    #[test]
    fn render_recall_block_includes_header_and_roles() {
        let rows = vec![
            row(1, "s1", "user", "hello"),
            row(2, "s1", "assistant", "hi there"),
        ];
        let block = render_recall_block(&rows, 100);
        assert!(block.starts_with("[Earlier context from this session]"));
        assert!(block.contains("user: hello"));
        assert!(block.contains("assistant: hi there"));
        assert!(block.contains("background context"));
    }

    #[test]
    fn render_recall_block_truncates() {
        let rows = vec![row(1, "s1", "assistant", &"x".repeat(500))];
        let block = render_recall_block(&rows, 20);
        assert!(block.contains("truncated"));
    }

    #[tokio::test]
    async fn preflight_returns_none_when_disabled_or_no_watermark() {
        let pool = test_pool().await;
        crate::storage::sessions::create_session(&pool, "s1", "Test")
            .await
            .unwrap();
        let cfg = MemoryConfig {
            preflight_enabled: false,
            ..Default::default()
        };
        assert!(
            preflight_recall(&pool, "s1", "why did we do that", 0, &cfg)
                .await
                .unwrap()
                .is_none()
        );
        let cfg2 = MemoryConfig::default();
        assert!(
            preflight_recall(&pool, "s1", "why did we do that", 0, &cfg2)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn preflight_recovers_a_compacted_exchange() {
        let pool = test_pool().await;
        crate::storage::sessions::create_session(&pool, "s1", "Test")
            .await
            .unwrap();
        insert(
            &pool,
            &[
                row(1, "s1", "user", "Let's set checkpoints at weeks 5, 10 and 13."),
                row(2, "s1", "assistant", "Aggregation checkpoints aligned to midterms."),
                row(3, "s1", "user", "Now write the draft."),
                row(4, "s1", "assistant", "Draft done."),
            ],
        )
        .await;

        let cfg = MemoryConfig::default();
        let block = preflight_recall(&pool, "s1", "why did we set checkpoints", 2, &cfg)
            .await
            .unwrap()
            .expect("should recall the first exchange");
        assert!(block.contains("checkpoints at weeks 5, 10 and 13"));
        assert!(block.contains("Aggregation checkpoints"));
        // The later exchange (rowid 3/4 > compacted_through=2) must not leak.
        assert!(!block.contains("Draft done"));
    }

    #[tokio::test]
    async fn preflight_injects_multiple_exchanges_without_duplicates() {
        let pool = test_pool().await;
        crate::storage::sessions::create_session(&pool, "s1", "Test")
            .await
            .unwrap();
        insert(
            &pool,
            &[
                row(1, "s1", "user", "Let's set checkpoints at weeks 5, 10 and 13."),
                row(2, "s1", "assistant", "Aggregation checkpoints aligned to midterms."),
                row(3, "s1", "user", "Now identify the entities for the rubric."),
                row(4, "s1", "assistant", "Entities: invoice, pipeline, budget."),
            ],
        )
        .await;

        let cfg = MemoryConfig {
            preflight_results: 4,
            ..Default::default()
        };
        let block = preflight_recall(&pool, "s1", "checkpoints or entities rubric", 4, &cfg)
            .await
            .unwrap()
            .expect("should recall both exchanges under budget");
        assert!(block.contains("checkpoints at weeks 5, 10 and 13"));
        assert!(block.contains("Entities: invoice, pipeline, budget."));
        // Each exchange present exactly once (no duplicate anchor injection).
        assert_eq!(
            block.matches("user:").count(),
            2,
            "two distinct exchanges, no overlap"
        );
    }

    /// Direction-pinning test for the bm25 gate: FTS5 bm25 scores are
    /// negative and MORE NEGATIVE = MORE RELEVANT, so the gate keeps
    /// `score <= t`. The old comparison was inverted (`score <= t` *rejected*),
    /// which kept the worst matches and dropped the best.
    #[tokio::test]
    async fn preflight_bm25_threshold_keeps_best_matches_and_rejects_worse_ones() {
        let pool = test_pool().await;
        crate::storage::sessions::create_session(&pool, "s1", "Test")
            .await
            .unwrap();
        insert(
            &pool,
            &[row(1, "s1", "user", "quantum decoherence rate"), row(
                2,
                "s1",
                "assistant",
                "unrelated filler",
            )],
        )
        .await;

        // t = 0.0 keeps every real match: all bm25 scores are negative, so
        // `score <= 0.0` is always true — the gate is a no-op and the
        // exchange must be injected.
        let cfg = MemoryConfig {
            bm25_threshold: Some(0.0),
            ..Default::default()
        };
        assert!(
            preflight_recall(&pool, "s1", "what was the decoherence rate", 2, &cfg)
                .await
                .unwrap()
                .is_some(),
            "score <= 0.0 must PASS the gate (negative scores are the good ones)"
        );

        // An impossibly-strict negative threshold rejects everything:
        // `score <= -1e9` is never true for a real match.
        let cfg = MemoryConfig {
            bm25_threshold: Some(-1e9),
            ..Default::default()
        };
        assert!(
            preflight_recall(&pool, "s1", "what was the decoherence rate", 2, &cfg)
                .await
                .unwrap()
                .is_none(),
            "scores above the threshold must be rejected"
        );
    }

    #[tokio::test]
    async fn preflight_ignores_non_recall_prompt() {
        let pool = test_pool().await;
        crate::storage::sessions::create_session(&pool, "s1", "Test")
            .await
            .unwrap();
        insert(&pool, &[row(1, "s1", "user", "fix the buffer overflow")]).await;
        let cfg = MemoryConfig {
            preflight_enabled: true,
            ..Default::default()
        };
        assert!(
            preflight_recall(&pool, "s1", "please refactor the loop", 1, &cfg)
                .await
                .unwrap()
                .is_none()
        );
    }
}
