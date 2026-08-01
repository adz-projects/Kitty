use rmcp::ServiceExt;
use kitty_tools::server::KittyToolsServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server = KittyToolsServer::new()
        .serve(rmcp::transport::stdio())
        .await?;
    server.waiting().await?;
    Ok(())
}
