//! HTTP client abstraction for forwarding requests to upstream services
//!
//! This module provides a unified interface for making HTTP requests, allowing
//! different client implementations (hyper, mock clients for testing, etc.) to
//! be used interchangeably throughout the proxy.
use std::time::Duration;

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

/// Create a hyper HTTP client with connection pooling configured
///
/// Connection pooling reuses HTTP connections instead of creating new ones for each request,
/// dramatically reducing file descriptor usage and eliminating TIME_WAIT connection accumulation.
///
/// See README for performance tuning guidance based on deployment scenarios.
pub fn create_hyper_client(
    pool_max_idle_per_host: usize,
    pool_idle_timeout_secs: u64,
) -> HyperClient {
    let mut http_connector = hyper_util::client::legacy::connect::HttpConnector::new();

    // fix enforce all Uris have the http scheme.
    http_connector.enforce_http(false);
    
    // Send TCP keepalive probes to detect dead connections.
    // After 60s idle, send a probe every 15s (Linux default); give up after 3
    // failures (Linux default). This keeps conntrack entries alive and detects
    // zombies within ~105s.
    http_connector.set_keepalive(Some(Duration::from_secs(60)));

    let https = hyper_tls::HttpsConnector::new_with_connector(http_connector);

    tracing::info!(
        "Creating HTTP client with connection pool: max_idle_per_host={}, idle_timeout={}s, tcp_keepalive=60s",
        pool_max_idle_per_host,
        pool_idle_timeout_secs
    );

    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(pool_idle_timeout_secs))
        .pool_max_idle_per_host(pool_max_idle_per_host)
        .pool_timer(hyper_util::rt::TokioTimer::new())
        .build(https)
}
