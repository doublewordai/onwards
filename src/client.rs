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

    // Allow HTTPS URIs (HttpConnector enforces HTTP-only by default)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_hyper_client_accepts_https_uris() {
        // Test that create_hyper_client() produces a client that accepts HTTPS URIs
        // Bug: HttpConnector with enforce_http=true (default) rejects HTTPS schemes
        // This causes real HTTPS requests to fail with connection errors

        let client = create_hyper_client(10, 60);

        // Create a request with an HTTPS URI to a reliable test endpoint
        let uri: hyper::Uri = "https://httpbin.org/status/200".parse().unwrap();
        let request = axum::extract::Request::builder()
            .uri(uri)
            .method("GET")
            .body(axum::body::Body::empty())
            .unwrap();

        // Make the request
        let result = client.request(request).await;

        // This should succeed (or fail for legitimate network reasons)
        // Without enforce_http(false), it fails with a connection error
        match result {
            Ok(response) => {
                // Success - HTTPS is working
                assert!(response.status().is_success() || response.status().is_redirection(),
                    "Expected successful response");
            },
            Err(e) => {
                // If there's an error, it should NOT be about invalid scheme
                // Common legitimate errors: DNS, timeout, network unreachable
                let error_string = e.to_string();
                panic!(
                    "HTTPS request failed. This might indicate enforce_http is blocking HTTPS. Error: {}",
                    error_string
                );
            }
        }
    }
}
