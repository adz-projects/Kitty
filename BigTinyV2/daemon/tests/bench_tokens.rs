//! Is the reuse actually worth the invariant it adds?
//!
//! The plan's own condition: if this is not a clear win, the added rule that
//! every content-mutating transform must clear the hint is not worth carrying.

use bigtiny2::agent::context::builder::ContextBuilder;
use bigtiny2::agent::tokens::{count_messages_tokens, set_token_reuse};
use bigtiny2::config::BigTinyConfig;
use serde_json::json;
use sqlx::SqlitePool;

#[tokio::test]
#[ignore = "measurement, not a pass/fail assertion; run with --ignored"]
async fn measure_reuse_saving_on_a_long_session() {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    bigtiny2::storage::apps::register_app(&pool, "app", "App", "key")
        .await
        .unwrap();
    bigtiny2::storage::sessions::create_session_for_app(&pool, "s1", "s", "app")
        .await
        .unwrap();

    let builder = ContextBuilder::new(
        pool.clone(),
        BigTinyConfig::default().token_management,
        BigTinyConfig::default().summarizer.reserve_exchanges,
    );

    // ~100 messages of realistic size, which is where the encode cost bites.
    let body = "A reasonably long assistant reply with several sentences in it, \
                the sort of thing that actually accumulates in a working session \
                and has to be re-encoded on every single tool-loop iteration.";
    for i in 0..50 {
        let mut msgs = vec![
            json!({"role": "user", "content": format!("Question {i}: {body}")}),
            json!({"role": "assistant", "content": format!("Answer {i}: {body} {body}")}),
        ];
        builder.save_messages("s1", &mut msgs).await.unwrap();
    }

    let messages = builder
        .build_messages("s1", "next", None, None, None, None, None, None, None, None, None)
        .await
        .unwrap();

    // The tool loop counts the whole array once per step, so this stands in
    // for a turn that took 20 steps.
    const ITERATIONS: u32 = 20;

    set_token_reuse(false);
    let start = std::time::Instant::now();
    for _ in 0..ITERATIONS {
        std::hint::black_box(count_messages_tokens(&messages));
    }
    let without = start.elapsed();

    set_token_reuse(true);
    let start = std::time::Instant::now();
    for _ in 0..ITERATIONS {
        std::hint::black_box(count_messages_tokens(&messages));
    }
    let with = start.elapsed();

    println!("\n  messages in context: {}", messages.len());
    println!("  {ITERATIONS} full counts (a ~20-step turn):");
    println!("    recount every time: {:?}", without);
    println!("    reusing hints:      {:?}", with);
    println!(
        "    per step: {:?} -> {:?}",
        without / ITERATIONS,
        with / ITERATIONS
    );
    if with < without {
        println!("    speedup: {:.1}x", without.as_secs_f64() / with.as_secs_f64().max(1e-9));
    } else {
        println!("    NO SAVING -- the invariant is not worth carrying");
    }
}

/// Does the `sanitize_for_wire` fast path pay for itself?
///
/// It runs once per provider attempt per step, on the whole transcript. The
/// question is not whether borrowing beats cloning — it obviously does — but
/// whether the scan that decides between them costs enough to matter on the
/// arrays that *do* need rebuilding.
#[test]
#[ignore = "measurement, not a pass/fail assertion; run with --ignored"]
fn measure_wire_sanitation() {
    use bigtiny2::provider::wire::sanitize_for_wire;
    use serde_json::json;

    let body = "A reasonably long assistant reply with several sentences in it, \
                the sort of thing that actually accumulates in a working session.";

    // Clean: what a turn looks like once `_tok` hints have been stripped by a
    // content-mutating transform, or on any message written before hints
    // existed. This is the case the fast path is for.
    let clean: Vec<_> = (0..100)
        .map(|i| json!({"role": "user", "content": format!("{i}: {body}")}))
        .collect();
    // Dirty: every message carries a hint, so every one must be rebuilt. The
    // worst case for the added scan.
    let dirty: Vec<_> = (0..100)
        .map(|i| json!({"_tok": 42, "id": "row", "role": "user",
                        "content": format!("{i}: {body}")}))
        .collect();

    const ITERATIONS: u32 = 200;
    let time = |msgs: &[serde_json::Value]| {
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            std::hint::black_box(sanitize_for_wire(msgs));
        }
        start.elapsed()
    };

    let c = time(&clean);
    let d = time(&dirty);
    println!("\n  {} messages, {ITERATIONS} sanitations:", clean.len());
    println!("    nothing to strip (borrows): {c:?}  -> {:?}/call", c / ITERATIONS);
    println!("    every message dirty (copies): {d:?}  -> {:?}/call", d / ITERATIONS);
    println!(
        "    the fast path is {:.1}x cheaper than a full rebuild",
        d.as_secs_f64() / c.as_secs_f64().max(1e-9)
    );
}
