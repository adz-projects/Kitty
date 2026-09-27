use kitty_wasm::server::KittyWasmServer;
use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = kitty_wasm::InProcessConfig::from_env();
    let server = KittyWasmServer::with_config(config)
        .serve(rmcp::transport::stdio())
        .await?;
    server.waiting().await?;
    Ok(())
}
