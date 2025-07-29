/// Configuration for the proxy server
use anyhow::anyhow;
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Clone, Parser)]
#[command(version, about, long_about = None)]
pub(crate) struct Config {
    /// The port on which the proxy server will listen.
    #[arg(short = 'p', long, default_value_t = 3000)]
    pub(crate) port: u16,

    /// The port on which the metrics server will listen.
    #[arg(long, default_value_t = 9090)]
    pub(crate) metrics_port: u16,

    /// Whether to enable the metrics endpoint.
    #[arg(short = 'm', long, default_value_t = false)]
    pub(crate) metrics: bool,

    /// The file from which to read the targets.
    #[arg(short = 'f', long)]
    pub(crate) targets: PathBuf,

    /// Whether we should continue watching the targets file for changes
    #[arg(short = 'w', long, default_value_t = true)]
    pub(crate) watch: bool,

    /// The prefix to use for metrics.
    #[arg(long, default_value = "onwards")]
    pub(crate) metrics_prefix: String,
}

impl Config {
    pub(crate) fn validate(self) -> Result<Self, anyhow::Error> {
        if !self.targets.exists() {
            return Err(anyhow!(
                "Config file '{}' does not exist",
                self.targets.display()
            ));
        }
        Ok(self)
    }
}
