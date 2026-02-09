//! Configuration parsing and validation for the proxy server
//!
//! This module handles command-line argument parsing and validation using clap.
//! It defines the main configuration structure used throughout the application.
use anyhow::anyhow;
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Clone, Parser)]
#[command(version, about, long_about = None)]
pub struct Config {
    /// The port on which the proxy server will listen.
    #[arg(short = 'p', long, default_value_t = 3000)]
    pub port: u16,

    /// The port on which the metrics server will listen.
    #[arg(long, default_value_t = 9090)]
    pub metrics_port: u16,

    /// Whether to enable the metrics endpoint.
    #[arg(short = 'm', long, default_value_t = true)]
    pub metrics: bool,

    /// The file from which to read the targets.
    #[arg(short = 'f', long)]
    pub targets: PathBuf,

    /// Whether we should continue watching the targets file for changes
    #[arg(short = 'w', long, default_value_t = true)]
    pub watch: bool,

    /// The prefix to use for metrics.
    #[arg(long, default_value = "onwards")]
    pub metrics_prefix: String,

    /// Maximum number of idle HTTP connections to keep alive per upstream host.
    /// Higher values improve performance under load by reusing connections.
    /// - Fan-out scenarios (many upstreams): 100-300
    /// - Single upstream scenarios: 1000-2000
    #[arg(long, default_value_t = 100)]
    pub pool_max_idle_per_host: usize,

    /// How long (in seconds) to keep idle HTTP connections alive.
    /// 90s balances connection reuse with avoiding stale connections.
    #[arg(long, default_value_t = 90)]
    pub pool_idle_timeout_secs: u64,
}

impl Config {
    pub fn validate(self) -> Result<Self, anyhow::Error> {
        if !self.targets.exists() {
            return Err(anyhow!(
                "Config file '{}' does not exist",
                self.targets.display()
            ));
        }
        Ok(self)
    }
}
