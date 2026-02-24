use anyhow::Result;
use deragabu_agent::start_all_subsystems;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Server bind address
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:9000".to_string());

    start_all_subsystems(bind_addr).await;

    Ok(())
}

