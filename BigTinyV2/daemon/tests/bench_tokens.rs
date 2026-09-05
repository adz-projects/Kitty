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
