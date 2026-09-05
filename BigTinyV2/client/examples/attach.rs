//! Prove `attach_or_spawn` attaches to a daemon it did not start.
//!
//! Run with a daemon already up:
//!   BIGTINYV2_DATA_DIR=<dir> cargo run --example attach
use bigtiny2_client::discovery::{attach_or_spawn, DiscoveryConfig};

#[tokio::main]
async fn main() {
    let config = DiscoveryConfig {
        daemon_args: Vec::new(),
        daemon_binary: "bigtiny2-daemon.exe".into(),
        min_api_version: bigtiny2_client::MIN_API_VERSION,
        env: vec![],
    };
    match attach_or_spawn(&config).await {
        Ok(found) => {
            println!("base_url      = {}", found.base_url);
            println!("spawned_by_us = {}", found.spawned_by_us);
            println!("instance_id   = {}", found.handshake.instance_id);
            println!("api_version   = {}", found.handshake.api_version);
        }
        Err(e) => {
            println!("ERROR: {e}");
            std::process::exit(1);
        }
    }
}
