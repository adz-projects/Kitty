use kitty_web::server::KittyWebServer;
use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = kitty_web::InProcessConfig::from_env();
    let server = KittyWebServer::with_config(config)
        .serve(rmcp::transport::stdio())
        .await?;
    server.waiting().await?;
    Ok(())
}
