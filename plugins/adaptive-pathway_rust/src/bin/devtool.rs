//! Unshipped dev tool for the behavioral-memory engine. Pay for itself when
//! tuning the extraction prompt. Deliberately NOT in plugins/build.py or
//! externalBin.
//!
//! Subcommands:
//!   adaptive-pathway-devtool <db> dump                 - list all beliefs
//!   adaptive-pathway-devtool <db> recall [domain]      - render the recall block
//!   adaptive-pathway-devtool <db> serve-stdio          - serve the in-process MCP server over stdio

use std::sync::Arc;

use adaptive_pathway::config::Config;
use adaptive_pathway::engine::PathwayEngine;
use adaptive_pathway::mcp::PathwayServer;
use adaptive_pathway::recall::{render_knows, select_beliefs};
use rmcp::ServiceExt;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: adaptive-pathway-devtool <db> <dump|recall [domain]|serve-stdio>");
        std::process::exit(2);
    }
    let db_path = &args[1];
    let sub = &args[2];
    let engine = match PathwayEngine::open(db_path, Config::default()).await {
        Ok(e) => e,
        Err(err) => {
            eprintln!("failed to open {db_path}: {err}");
            std::process::exit(1);
        }
    };

    match sub.as_str() {
        "dump" => dump(&engine).await,
        "recall" => {
            let domain = args.get(3).map(|s| s.as_str());
            recall(&engine, domain).await;
        }
        "serve-stdio" => serve_stdio(engine, "dev-session".to_string()).await,
        _ => {
            eprintln!("unknown subcommand: {sub}");
            std::process::exit(2);
        }
    }
}

async fn dump(engine: &PathwayEngine) {
    let all = match engine.db.list_beliefs(None).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("dump failed: {e}");
            return;
        }
    };
    for b in all {
        println!("[{}] {:>5.2} {}{}  {}",
            b.layer.as_str(),
            b.confidence,
            if b.tested { "✓" } else { "·" },
            if b.pinned { "📌" } else { "" },
            b.text,
        );
    }
}

async fn recall(engine: &PathwayEngine, domain: Option<&str>) {
    let all = match engine.db.list_beliefs(None).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("recall failed: {e}");
            return;
        }
    };
    let mut sel = select_beliefs(&all, domain);
    let block = render_knows(&mut sel);
    println!("{}", if block.is_empty() { "(no beliefs)" } else { &block });
}

async fn serve_stdio(engine: Arc<PathwayEngine>, session_id: String) {
    let server = PathwayServer::new(engine, session_id);
    let served = match server.serve(rmcp::transport::stdio()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("stdio serve failed: {e}");
            return;
        }
    };
    let _ = served.waiting().await;
}
