//! Differential verification of token-count reuse.
//!
//! This is the phase that must not ship on unit tests alone. The code under
//! test decides whether a request fits the context window; a subtle error does
//! not surface as a failed assertion but as an opaque provider 400, arriving
//! much later and pointing nowhere useful.
//!
//! So the test is differential rather than exemplary: build real context from a
//! real database, once with reuse on and once with it forced off, and require
//! that **every** intermediate number agrees. A discrepancy anywhere means the
//! stored count and a live recount disagree, which is the only way this feature
//! can be wrong.

use bigtiny2::agent::context::builder::ContextBuilder;
use bigtiny2::agent::tokens::{
    count_messages_tokens, set_token_reuse, token_reuse_enabled, TOKEN_HINT_KEY,
};
use bigtiny2::config::BigTinyConfig;
use serde_json::{json, Value};
use sqlx::SqlitePool;

async fn seeded_pool(exchanges: usize) -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    bigtiny2::storage::apps::register_app(&pool, "app", "App", "key")
        .await
        .unwrap();
    bigtiny2::storage::sessions::create_session_for_app(&pool, "s1", "session", "app")
        .await
        .unwrap();

    let builder = ContextBuilder::new(
        pool.clone(),
        BigTinyConfig::default().token_management,
        BigTinyConfig::default().summarizer.reserve_exchanges,
    );

    // Deliberately varied: plain prose, CJK, dense JSON, a fenced code block,
    // and a tool result. The byte-based estimate this replaced was
    // systematically wrong for exactly these, so they are where a mismatch
    // between stored and recomputed would show up first.
    let bodies = [
        "Plain English prose about a fairly ordinary topic.",
        "日本語のテキストと、いくつかの漢字を含む長めの文章です。",
        r#"{"deeply":{"nested":[1,2,3,{"json":"payload","with":"punctuation!"}]}}"#,
        "```rust\nfn main() { println!(\"a fenced code block\"); }\n```",
    ];

    for i in 0..exchanges {
        let mut msgs = vec![
            json!({"role": "user", "content": format!("{} (turn {i})", bodies[i % bodies.len()])}),
            json!({"role": "assistant", "content": format!("Reply {i}: {}", bodies[(i + 1) % bodies.len()])}),
            json!({
                "role": "tool",
                "tool_call_id": format!("call_{i}"),
                "content": format!("{{\"rows\": {i}, \"detail\": \"{}\"}}", bodies[(i + 2) % bodies.len()]),
            }),
        ];
        builder.save_messages("s1", &mut msgs).await.unwrap();
    }
    pool
}

async fn build_context(pool: &SqlitePool, new_message: &str) -> Vec<Value> {
    let builder = ContextBuilder::new(
        pool.clone(),
        BigTinyConfig::default().token_management,
        BigTinyConfig::default().summarizer.reserve_exchanges,
    );
    builder
        .build_messages("s1", new_message, None, None, None, None, None, None, None, None, None)
        .await
        .expect("context should build")
}

/// Run `f` with reuse forced off, restoring the previous setting afterwards.
async fn without_reuse<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let previous = token_reuse_enabled();
    set_token_reuse(false);
    let out = f().await;
    set_token_reuse(previous);
    out
}

#[tokio::test]
async fn every_message_counts_identically_with_and_without_reuse() {
    // The core claim: a stored count is what a recount would produce, not an
    // approximation of it. Compared per message rather than in aggregate, so a
    // failure names the message rather than just the total.
    let pool = seeded_pool(12).await;

    set_token_reuse(true);
    let with = build_context(&pool, "the new message").await;
    let without = without_reuse(|| build_context(&pool, "the new message")).await;

    assert_eq!(
        with.len(),
        without.len(),
        "reuse must not change how many messages the context contains"
    );

    for (i, (a, b)) in with.iter().zip(without.iter()).enumerate() {
        // Same message, ignoring the hint itself.
        let strip = |v: &Value| {
            let mut v = v.clone();
            if let Some(o) = v.as_object_mut() {
                o.remove(TOKEN_HINT_KEY);
            }
            v
        };
        assert_eq!(strip(a), strip(b), "message {i} differs in content");

        let counted = count_messages_tokens(std::slice::from_ref(a));
        let recounted = without_reuse(|| async { count_messages_tokens(std::slice::from_ref(b)) }).await;
        assert_eq!(
            counted, recounted,
            "message {i} counted {counted} with reuse and {recounted} without"
        );
    }
}

#[tokio::test]
async fn the_total_budget_is_identical() {
    // What actually gates the request. If this ever diverges, the daemon's
    // idea of "does this fit" is wrong, which is the opaque-400 failure.
    let pool = seeded_pool(20).await;

    set_token_reuse(true);
    let with = build_context(&pool, "another message").await;
    let with_total = count_messages_tokens(&with);

    let without_total = without_reuse(|| async {
        let msgs = build_context(&pool, "another message").await;
        count_messages_tokens(&msgs)
    })
    .await;

    assert_eq!(
        with_total, without_total,
        "the context budget must not depend on whether counts were reused"
    );
}

#[tokio::test]
async fn adversarial_content_counts_identically() {
    // CJK, dense JSON and code are where the byte-based estimate this replaced
    // was systematically wrong, so they are where a stored/recomputed mismatch
    // would surface first.
    let pool = seeded_pool(8).await;
    let messages = build_context(&pool, "多言語のテスト").await;

    for (i, msg) in messages.iter().enumerate() {
        let reused = count_messages_tokens(std::slice::from_ref(msg));
        let fresh =
            without_reuse(|| async { count_messages_tokens(std::slice::from_ref(msg)) }).await;
        assert_eq!(reused, fresh, "message {i} disagreed on adversarial content");
    }
}

#[tokio::test]
async fn an_image_message_counts_identically() {
    // Images are charged a flat per-block cost rather than tokenized, so a
    // stamped count covering one is a distinct path worth pinning.
    let pool = seeded_pool(2).await;
    let builder = ContextBuilder::new(
        pool.clone(),
        BigTinyConfig::default().token_management,
        BigTinyConfig::default().summarizer.reserve_exchanges,
    );
    let mut msgs = vec![json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "what is in this picture?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
        ],
    })];
    builder.save_messages("s1", &mut msgs).await.unwrap();

    let messages = build_context(&pool, "follow-up").await;
    let reused = count_messages_tokens(&messages);
    let fresh = without_reuse(|| async { count_messages_tokens(&messages) }).await;
    assert_eq!(reused, fresh);
}

#[tokio::test]
async fn clearing_a_hint_forces_a_real_recount() {
    // The invariant the design rests on: any transform that changes content
    // clears the hint, and a cleared message is counted for real.
    //
    // Note this stamps a *truthful* hint. Stamping a false one is untestable
    // in a debug build by design -- the assertion below catches it, which is
    // the point of having it.
    let mut msg = json!({"role": "user", "content": "some content here"});
    let truth = count_messages_tokens(std::slice::from_ref(&msg));

    bigtiny2::agent::tokens::stamp_token_hint(&mut msg, truth);
    assert_eq!(count_messages_tokens(std::slice::from_ref(&msg)), truth);

    // Mutate the content the way a transform would, without clearing.
    msg["content"] = json!("a much longer body of content than before, by some margin");
    bigtiny2::agent::tokens::clear_token_hint(&mut msg);

    let recounted = count_messages_tokens(std::slice::from_ref(&msg));
    assert!(
        recounted > truth,
        "a cleared message must be counted afresh (got {recounted}, was {truth})"
    );
}

/// A stale hint is a bug, and a debug build must not let it pass silently.
///
/// This is the safety net for the one way this feature can go wrong: a
/// transform that mutates content and forgets to clear. In release builds the
/// assertion compiles out and the stale count is used, which is why the
/// clearing discipline — not this test — is the actual guarantee.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "disagreed with a live recount")]
fn a_stale_hint_trips_the_debug_assertion() {
    let mut msg = json!({"role": "user", "content": "short"});
    bigtiny2::agent::tokens::stamp_token_hint(&mut msg, 9_999);
    let _ = count_messages_tokens(std::slice::from_ref(&msg));
}

#[tokio::test]
async fn the_kill_switch_ignores_hints_entirely() {
    // Kept so reuse can be switched off in the field if a budget anomaly ever
    // appears, without a rebuild being the only remedy. With it off, even a
    // wrong hint is ignored rather than trusted -- which is exactly the
    // property that makes it a usable escape hatch.
    let mut msg = json!({"role": "user", "content": "short"});
    bigtiny2::agent::tokens::stamp_token_hint(&mut msg, 9_999);

    let ignored =
        without_reuse(|| async { count_messages_tokens(std::slice::from_ref(&msg)) }).await;
    assert_ne!(ignored, 9_999, "with reuse off a hint must be ignored");
    assert!(ignored > 0 && ignored < 100, "and the real count used instead");
}

#[tokio::test]
async fn nothing_internal_reaches_a_provider() {
    // The hint is a private key on the message, so it must be stripped before
    // the request leaves the daemon -- along with `id`, which V1 had been
    // sending to providers all along.
    let pool = seeded_pool(4).await;
    let messages = build_context(&pool, "hello").await;

    let outgoing = bigtiny2::provider::wire::sanitize_for_wire(&messages);
    let serialized = serde_json::to_string(&outgoing).unwrap();

    assert!(!serialized.contains(TOKEN_HINT_KEY), "token hint leaked");
    for msg in &outgoing {
        let obj = msg.as_object().expect("messages are objects");
        assert!(obj.get("id").is_none(), "row id leaked: {msg}");
        assert!(obj.get("rowid").is_none(), "rowid leaked: {msg}");
        assert!(
            !obj.keys().any(|k| k.starts_with('_')),
            "an internal key leaked: {msg}"
        );
    }
}
