use anyhow::Result;
use wisp_server::{serve, ServerConfig};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("wisp_server=info".parse()?),
        )
        .with_target(false)
        .init();
    serve(ServerConfig::from_env()?).await
}
