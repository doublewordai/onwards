//! HTTP client abstraction for forwarding requests to upstream services
//!
//! This module provides a unified interface for making HTTP requests, allowing
//! different client implementations (hyper, mock clients for testing, etc.) to
//! be used interchangeably throughout the proxy.
use async_trait::async_trait;
use axum::response::IntoResponse;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};

pub type HyperClient = Client<
    hyper_tls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    axum::body::Body,
>;

#[async_trait]
pub trait HttpClient: std::fmt::Debug {
    async fn request(
        &self,
        req: axum::extract::Request,
    ) -> Result<axum::response::Response, Box<dyn std::error::Error + Send + Sync>>;
}

#[async_trait]
impl HttpClient for HyperClient {
    async fn request(
        &self,
        req: axum::extract::Request,
    ) -> Result<axum::response::Response, Box<dyn std::error::Error + Send + Sync>> {
        self.request(req)
            .await
            .map(|res| res.into_response())
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }
}

pub fn create_hyper_client() -> HyperClient {
    let https = hyper_tls::HttpsConnector::new();

    // Connection pool configuration via environment variables
    // Defaults are conservative, increase for high-volume deployments
    let pool_idle_timeout_secs = std::env::var("ONWARDS_POOL_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(90);

    let pool_max_idle_per_host = std::env::var("ONWARDS_POOL_MAX_IDLE_PER_HOST")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100);

    tracing::debug!(
        "HTTP client pool config: idle_timeout={}s, max_idle_per_host={}",
        pool_idle_timeout_secs,
        pool_max_idle_per_host
    );

    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(std::time::Duration::from_secs(pool_idle_timeout_secs))
        .pool_max_idle_per_host(pool_max_idle_per_host)
        .pool_timer(hyper_util::rt::TokioTimer::new())
        .build(https)
}
