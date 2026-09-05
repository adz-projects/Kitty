//! A small pipeline consumer, end to end.
//!
//! This is Phase 5's validation: if a realistic client has to reach around the
//! crate — hand-rolling a request, parsing a response the crate should have
//! typed — that is a crate bug, not an example bug.
//!
//! ```text
//! cargo run --example pipeline -- <path-to-bigtiny2-daemon>
//! ```
//!
//! Attaches (spawning a daemon if none is running), registers, reports what
//! the endpoints can serve, submits a batch paced to that, and collects.

use std::time::Duration;

use bigtiny2_client::discovery::{attach_or_spawn, DiscoveryConfig};
use bigtiny2_client::{BigTinyClient, Dispatcher, Job};

const APP_ID: &str = "example-pipeline";

#[tokio::main]
async fn main() {
    let daemon = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: pipeline <path-to-bigtiny2-daemon>");
        std::process::exit(2);
    });

    // 1. Find a daemon, or start one. Never kills a daemon it did not prove
    //    dead, so running this alongside another app is safe.
    let located = match attach_or_spawn(&DiscoveryConfig {
        daemon_args: Vec::new(),
        daemon_binary: daemon.into(),
        min_api_version: bigtiny2_client::MIN_API_VERSION,
        env: vec![],
    })
    .await
    {
        Ok(l) => l,
        Err(e) => {
            eprintln!("could not reach a daemon: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "daemon at {} (api v{}, {})",
        located.base_url,
        located.handshake.api_version,
        if located.spawned_by_us {
            "started by us"
        } else {
            "already running"
        }
    );

    // 2. Register once. A real app persists this key and skips straight to
    //    the client on later launches; re-registering is a 409, not a reissue.
    let key = match BigTinyClient::register(
        &located.base_url,
        &located.handshake.registration_token,
        APP_ID,
        "Example Pipeline",
    )
    .await
    {
        Ok(reg) => reg.api_key,
        Err(bigtiny2_client::ClientError::AlreadyRegistered(_)) => {
            eprintln!(
                "'{APP_ID}' is already registered and this example does not persist its key.\n\
                 Delete it first, or run against a fresh data dir."
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("registration failed: {e}");
            std::process::exit(1);
        }
    };
    let client = BigTinyClient::new(&located.base_url, key);

    // 3. What can the endpoints actually serve? This is what a pipeline paces
    //    against, rather than guessing and discovering the limit as latency.
    match client.providers().await {
        Ok(providers) if providers.is_empty() => {
            println!("\nno providers configured — jobs will fail, which still exercises the path");
        }
        Ok(providers) => {
            println!("\nproviders:");
            for p in &providers {
                println!(
                    "  {:<20} {} slot(s) [{}]  in_flight={} queued={} (mine={})",
                    p.name, p.concurrency, p.slots_source, p.in_flight, p.queue_depth,
                    p.my_queue_depth
                );
            }
        }
        Err(e) => println!("\ncould not read providers: {e}"),
    }

    // 4. A fan-out, grouped under one parent so the children stay findable.
    let parent = match client.create_session("pipeline run").await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("could not create a session: {e}");
            std::process::exit(1);
        }
    };

    let jobs: Vec<Job> = ["summarise A", "summarise B", "summarise C", "summarise D"]
        .iter()
        .map(|p| Job::new(*p).under(&parent))
        .collect();

    // The dispatcher reads each provider's slot count and keeps that many
    // submissions in flight — bounding sockets, not re-implementing the
    // daemon's scheduling, which can see every app rather than just this one.
    let dispatcher = match Dispatcher::new(client.clone()).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("could not build a dispatcher: {e}");
            std::process::exit(1);
        }
    };
    println!("\nsubmitting {} jobs at capacity {}", jobs.len(), dispatcher.capacity());

    let outcomes = dispatcher.run_all(jobs, Duration::from_secs(120)).await;

    println!("\nresults:");
    for (i, outcome) in outcomes.iter().enumerate() {
        match outcome {
            Ok(o) => println!(
                "  [{i}] {} {}",
                o.status,
                o.result
                    .as_deref()
                    .or(o.error.as_deref())
                    .unwrap_or("(no output)")
                    .chars()
                    .take(60)
                    .collect::<String>()
            ),
            Err(e) => println!("  [{i}] client error: {e}"),
        }
    }

    // 5. The children are findable afterwards, which is the point of grouping.
    match client.list_sessions().await {
        Ok(v) => println!(
            "\n{} session(s) belong to this app",
            v["total"].as_i64().unwrap_or(0)
        ),
        Err(e) => println!("\ncould not list sessions: {e}"),
    }
}
