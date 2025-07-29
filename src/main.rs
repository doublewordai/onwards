mod config;

use clap::Parser as _;
use config::Config;
use onwards::{
    AppState, build_router,
    target::{Targets, WatchedFile},
};
use tokio::net::TcpListener;
use tracing::{info, instrument};

#[tokio::main]
#[instrument]
pub async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::parse().validate()?;
    info!("Starting AI Gateway with config: {:?}", config);

    let targets = Targets::from_config_file(&config.targets)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create targets from config: {}", e))?;

    // Start file watcher if a config file was specified
    if config.watch {
        targets.receive_updates(WatchedFile(config.targets)).await?;
    }

    let app_state = AppState::new(targets);
    let router = build_router(app_state).await;

    let bind_addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&bind_addr).await?;
    info!("AI Gateway listening on {}", bind_addr);

    axum::serve(listener, router).await?;

    Ok(())
}
