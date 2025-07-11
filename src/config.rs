/// Configuration for the proxy server
use anyhow::anyhow;
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Clone, Parser)]
#[command(version, about, long_about = None)]
pub(crate) struct Config {
    /// The port on which the proxy server will listen.
    #[arg(short = 'p', default_value_t = 3000)]
    pub(crate) port: u16,

    /// The file from which to read the targets.
    #[arg(short = 'f')]
    pub(crate) targets: PathBuf,

    /// Whether we should continue watching the targets file for changes
    #[arg(short = 'w', default_value_t = true)]
    pub(crate) watch: bool,
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

