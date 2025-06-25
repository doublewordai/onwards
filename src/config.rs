use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(version, about, long_about = None)]
pub(crate) struct Config {
    #[clap(short = 'k', long, env = "OPENAI_API_KEY", hide_env_values = true)]
    pub(crate) openai_api_key: String,
    #[clap(short = 't', long, env = "OPENAI_BASE")]
    pub(crate) openai_api_base: String,
}
