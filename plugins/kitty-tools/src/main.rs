use kitty_tools::server::KittyToolsServer;
use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = kitty_tools::InProcessConfig::from_env();
    let server = KittyToolsServer::with_config(config)
        .serve(rmcp::transport::stdio())
        .await?;
    server.waiting().await?;
    Ok(())
}
