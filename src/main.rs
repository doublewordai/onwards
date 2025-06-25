mod config;
mod errors;
mod models;
mod routes;
mod state;

use axum::routing::post;
use clap::Parser as _;
use config::Config;
use routes::{chat_completions, embeddings};

pub async fn build_router(config: Config) -> axum::Router {
    axum::Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/embeddings", post(embeddings))
        .with_state(state::AppState {})
}

#[tokio::main]
pub async fn main() {
    let args = Config::parse();
    let router = build_router(args.clone()).await;
}
