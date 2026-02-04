//! model-switcher - Zero-reload model switching for vLLM
//!
//! This binary manages multiple vLLM models on shared GPU(s), starting them
//! lazily and coordinating wake/sleep to allow seamless model switching.

use anyhow::{Context, Result};
use clap::Parser;
use model_switcher::Config;
use std::path::PathBuf;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "model-switcher")]
#[command(about = "Zero-reload model switching for vLLM")]
struct Args {
    /// Path to configuration file
    #[arg(short, long, default_value = "config.json")]
    config: PathBuf,

    /// Port to listen on (overrides config)
    #[arg(short, long)]
    port: Option<u16>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    let filter = if args.verbose {
        EnvFilter::new("model_switcher=debug,onwards=debug,tower_http=debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();

    info!("Starting model-switcher");

    // Load configuration
    let mut config = Config::from_file(&args.config)
        .await
        .with_context(|| format!("Failed to load config from {}", args.config.display()))?;

    // Override port if specified
    if let Some(port) = args.port {
        config.port = port;
    }

    info!(
        models = ?config.models.keys().collect::<Vec<_>>(),
        port = config.port,
        "Configuration loaded"
    );

    // Build the application
    let app = model_switcher::build_app(config.clone())
        .await
        .context("Failed to build application")?;

    // Start server
    let addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind to {}", addr))?;

    info!(addr = %addr, "Listening for requests");

    axum::serve(listener, app)
        .await
        .context("Server error")?;

    Ok(())
}
