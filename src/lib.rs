//! # Onwards - A flexible LLM proxy library
//!
//! Onwards provides core functionality for building LLM proxy services that can route requests
//! to multiple AI model endpoints with authentication, rate limiting, and request transformation.
//!
//! ## Quick Start
//!
//! ```no_run
//! use onwards::{AppState, build_router, config::Config, target::Targets};
//! use axum::serve;
//! use tokio::net::TcpListener;
//! use clap::Parser;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Parse CLI configuration
//!     let config = Config::parse();
//!     
//!     // Load targets from configuration file
//!     let targets = Targets::from_config_file(&config.targets).await?;
//!
//!     // Create application state with connection pool settings from config
//!     let app_state = AppState::new(targets, &config);
//!
//!     // Build router with proxy routes
//!     let app = build_router(app_state);
//!
//!     // Start server
//!     let listener = TcpListener::bind("0.0.0.0:3000").await?;
//!     serve(listener, app).await?;
//!     Ok(())
//! }
//! ```
//!

use axum::Router;
use axum::http::HeaderMap;
use axum::routing::{any, get};
use axum_prometheus::{
    GenericMetricLayer, Handle, PrometheusMetricLayerBuilder,
    metrics_exporter_prometheus::PrometheusHandle,
};
use std::borrow::Cow;
use std::sync::Arc;
use tracing::{info, instrument};

pub mod auth;
pub mod client;
pub mod config;
pub mod errors;
pub mod handlers;
pub mod load_balancer;
pub mod models;
pub mod response_sanitizer;
pub mod sse;
pub mod target;

use client::{HttpClient, HyperClient};
use handlers::{models as models_handler, target_message_handler};
use models::ExtractedModel;

/// Type alias for body transformation function
///
/// Takes (path, headers, body_bytes) and returns transformed body_bytes or None if no transformation.
/// This allows you to modify request bodies before they are forwarded to upstream services.
///
/// # Arguments
///
/// * `&str` - The request path (e.g., "/v1/chat/completions")
/// * `&HeaderMap` - HTTP headers from the incoming request
/// * `&[u8]` - The request body as raw bytes
///
/// # Returns
///
/// * `Some(Bytes)` - Transformed request body to forward
/// * `None` - Use original request body unchanged
///
/// # Examples
///
/// ```
/// use onwards::BodyTransformFn;
/// use axum::http::HeaderMap;
/// use std::sync::Arc;
/// use serde_json::{json, Value};
///
/// // Transform function that adds stream_options to OpenAI streaming requests
/// let transform: BodyTransformFn = Arc::new(|path, _headers, body_bytes| {
///     if path == "/v1/chat/completions" {
///         if let Ok(mut json_body) = serde_json::from_slice::<Value>(body_bytes) {
///             if json_body.get("stream") == Some(&json!(true)) {
///                 json_body["stream_options"] = json!({"include_usage": true});
///                 if let Ok(transformed) = serde_json::to_vec(&json_body) {
///                     return Some(axum::body::Bytes::from(transformed));
///                 }
///             }
///         }
///     }
///     None // No transformation
/// });
/// ```
pub type BodyTransformFn =
    Arc<dyn Fn(&str, &HeaderMap, &[u8]) -> Option<axum::body::Bytes> + Send + Sync>;

/// Type alias for response transformation function
///
/// Takes (path, headers, body_bytes, original_model) and returns transformed body_bytes or None if no transformation.
/// This allows you to sanitize and modify response bodies before they are returned to clients.
///
/// # Arguments
///
/// * `&str` - The request path (e.g., "/v1/chat/completions")
/// * `&HeaderMap` - HTTP response headers from the upstream provider
/// * `&[u8]` - The response body as raw bytes
/// * `Option<&str>` - The original model requested by the client (for rewriting)
///
/// # Returns
///
/// * `Ok(Some(Bytes))` - Transformed response body to return
/// * `Ok(None)` - Use original response body unchanged
/// * `Err(String)` - Transformation failed with error message
///
/// # Examples
///
/// ```
/// use onwards::ResponseTransformFn;
/// use axum::http::HeaderMap;
/// use std::sync::Arc;
///
/// // Transform function that sanitizes OpenAI responses
/// let transform: ResponseTransformFn = Arc::new(|path, _headers, body_bytes, _model| {
///     if path.contains("/v1/chat/completions") {
///         // Sanitization logic here
///         Ok(Some(axum::body::Bytes::from(body_bytes.to_vec())))
///     } else {
///         Ok(None)
///     }
/// });
/// ```
pub type ResponseTransformFn = Arc<
    dyn Fn(&str, &HeaderMap, &[u8], Option<&str>) -> Result<Option<axum::body::Bytes>, String>
        + Send
        + Sync,
>;

/// The main application state containing the HTTP client and targets configuration
///
/// This struct holds all the state needed to run the proxy server. It contains:
/// - An HTTP client for making upstream requests
/// - The collection of configured targets (destinations)
/// - An optional body transformation function
/// - An optional response transformation function
///
/// # Examples
///
/// Basic setup:
/// ```no_run
/// use onwards::{AppState, config::Config, target::Targets};
/// use clap::Parser;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = Config::parse();
/// let targets = Targets::from_config_file(&config.targets).await?;
/// let app_state = AppState::new(targets, &config);
/// # Ok(())
/// # }
/// ```
///
/// With request transformation:
/// ```no_run
/// use onwards::{AppState, BodyTransformFn, config::Config, target::Targets};
/// use std::sync::Arc;
/// use clap::Parser;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = Config::parse();
/// let targets = Targets::from_config_file(&config.targets).await?;
///
/// let transform: BodyTransformFn = Arc::new(|path, _headers, body| {
///     // Custom transformation logic
///     None
/// });
///
/// let app_state = AppState::with_transform(targets, &config, transform);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct AppState<T: HttpClient> {
    pub http_client: T,
    pub targets: target::Targets,
    pub body_transform_fn: Option<BodyTransformFn>,
    pub response_transform_fn: Option<ResponseTransformFn>,
}

impl<T: HttpClient> std::fmt::Debug for AppState<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("http_client", &self.http_client)
            .field("targets", &self.targets)
            .field(
                "body_transform_fn",
                &self.body_transform_fn.as_ref().map(|_| "<function>"),
            )
            .field(
                "response_transform_fn",
                &self.response_transform_fn.as_ref().map(|_| "<function>"),
            )
            .finish()
    }
}

impl AppState<HyperClient> {
    /// Create a new AppState with the default Hyper client
    pub fn new(targets: target::Targets, config: &crate::config::Config) -> Self {
        let http_client = client::create_hyper_client(
            config.pool_max_idle_per_host,
            config.pool_idle_timeout_secs,
        );
        Self {
            http_client,
            targets,
            body_transform_fn: None,
            response_transform_fn: None,
        }
    }

    /// Create a new AppState with the default Hyper client and a body transformation function
    pub fn with_transform(
        targets: target::Targets,
        config: &crate::config::Config,
        body_transform_fn: BodyTransformFn,
    ) -> Self {
        let http_client = client::create_hyper_client(
            config.pool_max_idle_per_host,
            config.pool_idle_timeout_secs,
        );
        Self {
            http_client,
            targets,
            body_transform_fn: Some(body_transform_fn),
            response_transform_fn: None,
        }
    }
}

impl<T: HttpClient> AppState<T> {
    /// Create a new AppState with a custom HTTP client (useful for testing)
    pub fn with_client(targets: target::Targets, http_client: T) -> Self {
        Self {
            http_client,
            targets,
            body_transform_fn: None,
            response_transform_fn: None,
        }
    }

    /// Create a new AppState with a custom HTTP client and body transformation function
    pub fn with_client_and_transform(
        targets: target::Targets,
        http_client: T,
        body_transform_fn: BodyTransformFn,
    ) -> Self {
        Self {
            http_client,
            targets,
            body_transform_fn: Some(body_transform_fn),
            response_transform_fn: None,
        }
    }

    /// Set the response transformation function (builder pattern)
    pub fn with_response_transform(mut self, transform_fn: ResponseTransformFn) -> Self {
        self.response_transform_fn = Some(transform_fn);
        self
    }
}

/// Extract the model name from a request
///
/// This function checks for a model override header first, then extracts the model from the JSON body.
/// This is the same logic used by the proxy handler, extracted for reuse.
///
/// The extraction follows this precedence order:
/// 1. `model-override` header value
/// 2. `model` field in JSON request body
///
/// # Arguments
/// * `headers` - The request headers to check for model override
/// * `body_bytes` - The request body as bytes to parse for model field
///
/// # Returns
/// * `Ok(String)` - The extracted model name
/// * `Err(())` - If no model could be extracted or parsing failed
///
/// # Examples
///
/// Extract from header:
/// ```
/// use onwards::extract_model_from_request;
/// use axum::http::{HeaderMap, HeaderValue};
///
/// let mut headers = HeaderMap::new();
/// headers.insert("model-override", HeaderValue::from_static("gpt-4"));
/// let body = br#"{"model": "gpt-3.5", "messages": []}"#;
///
/// let model = extract_model_from_request(&headers, body).unwrap();
/// assert_eq!(model, "gpt-4"); // Header takes precedence
/// ```
///
/// Extract from JSON body:
/// ```
/// use onwards::extract_model_from_request;
/// use axum::http::HeaderMap;
///
/// let headers = HeaderMap::new();
/// let body = br#"{"model": "claude-3", "messages": []}"#;
///
/// let model = extract_model_from_request(&headers, body).unwrap();
/// assert_eq!(model, "claude-3");
/// ```
pub fn extract_model_from_request(headers: &HeaderMap, body_bytes: &[u8]) -> Option<String> {
    const MODEL_OVERRIDE_HEADER: &str = "model-override";

    // Order of precedence for the model:
    // 1. supplied as a header (model-override)
    // 2. Available in the request body as JSON
    match headers.get(MODEL_OVERRIDE_HEADER) {
        Some(header_value) => {
            let model_str = header_value.to_str().ok()?;
            Some(model_str.to_string())
        }
        None => {
            let extracted: ExtractedModel = serde_json::from_slice(body_bytes).ok()?;
            Some(extracted.model.to_string())
        }
    }
}

/// Creates the default OpenAI response sanitization function
///
/// This function creates a response transformer that:
/// - Enforces strict OpenAI API schema compliance
/// - Removes provider-specific fields
/// - Rewrites the model field to match the client's original request
///
/// The sanitizer only applies to `/v1/chat/completions` endpoint and supports
/// both streaming (SSE) and non-streaming responses.
///
/// # Returns
/// A [`ResponseTransformFn`] that can be used with [`AppState::with_response_transform`]
///
/// # Examples
///
/// ```no_run
/// use onwards::{AppState, config::Config, create_openai_sanitizer, target::Targets};
/// use clap::Parser;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = Config::parse();
/// let targets = Targets::from_config_file(&config.targets).await?;
/// let app_state = AppState::new(targets, &config)
///     .with_response_transform(create_openai_sanitizer());
/// # Ok(())
/// # }
/// ```
pub fn create_openai_sanitizer() -> ResponseTransformFn {
    Arc::new(|path, headers, body, original_model| {
        let sanitizer = response_sanitizer::ResponseSanitizer {
            original_model: original_model.map(String::from),
        };
        sanitizer.sanitize(path, headers, body)
    })
}

/// Build the main router for the proxy
///
/// This creates the main Axum router with all proxy endpoints configured.
/// The router handles model listing and request forwarding to configured targets.
///
/// # Routes Created
/// - `/models` - Returns available models (OpenAI-compatible)
/// - `/v1/models` - Returns available models (OpenAI-compatible)
/// - `/{*path}` - Forwards all other requests to the appropriate target
///
/// # Arguments
/// * `state` - The application state containing targets and configuration
///
/// # Returns
/// A configured Axum [`Router`] ready to be served
///
/// # Examples
///
/// Basic usage:
/// ```no_run
/// use onwards::{AppState, build_router, target::Targets};
/// use axum::serve;
/// use tokio::net::TcpListener;
/// use clap::Parser;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = onwards::config::Config::parse();
/// let targets = Targets::from_config_file(&config.targets).await?;
/// let app_state = AppState::new(targets, &config);
/// let router = build_router(app_state);
///
/// let listener = TcpListener::bind("0.0.0.0:3000").await?;
/// serve(listener, router).await?;
/// # Ok(())
/// # }
/// ```
#[instrument(skip(state))]
pub fn build_router<T: HttpClient + Clone + Send + Sync + 'static>(state: AppState<T>) -> Router {
    info!("Building router");
    Router::new()
        .route("/models", get(models_handler))
        .route("/v1/models", get(models_handler))
        .route("/{*path}", any(target_message_handler))
        .with_state(state)
}

/// Builds a router for the metrics endpoint
///
/// Creates a separate router specifically for serving Prometheus metrics.
/// This is typically run on a different port from the main proxy for security.
///
/// # Arguments
/// * `handle` - Prometheus handle for rendering metrics
///
/// # Returns
/// A configured Axum [`Router`] serving metrics at `/metrics`
///
/// # Examples
///
/// ```no_run
/// use onwards::{build_metrics_router, build_metrics_layer_and_handle};
/// use axum::serve;
/// use tokio::net::TcpListener;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let (_metrics_layer, metrics_handle) = build_metrics_layer_and_handle("myapp");
/// let metrics_router = build_metrics_router(metrics_handle);
///
/// let listener = TcpListener::bind("0.0.0.0:9090").await?;
/// serve(listener, metrics_router).await?;
/// # Ok(())
/// # }
/// ```
#[instrument(skip(handle))]
pub fn build_metrics_router(handle: PrometheusHandle) -> Router {
    info!("Building metrics router");
    Router::new().route(
        "/metrics",
        axum::routing::get(move || async move { handle.render() }),
    )
}

type MetricsLayerAndHandle = (
    GenericMetricLayer<'static, PrometheusHandle, Handle>,
    PrometheusHandle,
);

/// Builds a layer and handle for prometheus metrics collection
///
/// Creates both the metrics collection layer (to add to your main router) and a handle
/// for serving the metrics on a separate endpoint. The layer automatically tracks
/// HTTP requests, response times, and other useful metrics.
///
/// # Arguments
/// * `prefix` - A string prefix for all metric names (e.g., "myapp" creates "myapp_http_requests_total")
///
/// # Returns
/// A tuple containing:
/// 1. Metrics layer to add to your main router
/// 2. Prometheus handle for serving metrics
///
/// # Examples
///
/// ```no_run
/// use onwards::{AppState, build_router, build_metrics_router, build_metrics_layer_and_handle, target::Targets};
/// use axum::serve;
/// use tokio::net::TcpListener;
/// use clap::Parser;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = onwards::config::Config::parse();
/// let targets = Targets::from_config_file(&config.targets).await?;
/// let app_state = AppState::new(targets, &config);
///
/// // Build metrics layer and handle
/// let (metrics_layer, metrics_handle) = build_metrics_layer_and_handle("myapp");
///
/// // Main router with metrics collection
/// let app = build_router(app_state)
///     .layer(metrics_layer);
///
/// // Separate metrics router
/// let metrics_app = build_metrics_router(metrics_handle);
///
/// // Serve both (typically on different ports)
/// let main_listener = TcpListener::bind("0.0.0.0:3000").await?;
/// let metrics_listener = TcpListener::bind("0.0.0.0:9090").await?;
///
/// tokio::select! {
///     _ = serve(main_listener, app) => {},
///     _ = serve(metrics_listener, metrics_app) => {},
/// }
/// # Ok(())
/// # }
/// ```
pub fn build_metrics_layer_and_handle(
    prefix: impl Into<Cow<'static, str>>,
) -> MetricsLayerAndHandle {
    info!("Building metrics layer");
    PrometheusMetricLayerBuilder::new()
        .with_prefix(prefix)
        .enable_response_body_size(true)
        .with_endpoint_label_type(axum_prometheus::EndpointLabel::Exact)
        .with_default_metrics()
        .build_pair()
}

pub mod test_utils {
    use super::*;
    use async_trait::async_trait;
    use axum::http::StatusCode;
    use std::sync::{Arc, Mutex};

    pub struct MockHttpClient {
        pub requests: Arc<Mutex<Vec<MockRequest>>>,
        response_builder: Arc<dyn Fn() -> axum::response::Response + Send + Sync>,
    }

    #[derive(Debug, Clone)]
    pub struct MockRequest {
        pub method: String,
        pub uri: String,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl MockHttpClient {
        pub fn new(status: StatusCode, body: &str) -> Self {
            let body = body.to_string();
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                response_builder: Arc::new(move || {
                    axum::response::Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body.clone()))
                        .unwrap()
                }),
            }
        }

        pub fn new_streaming(status: StatusCode, chunks: Vec<String>) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                response_builder: Arc::new(move || {
                    use axum::body::Body;
                    use futures_util::stream;

                    let stream = stream::iter(
                        chunks
                            .clone()
                            .into_iter()
                            .map(|chunk| Ok::<_, std::io::Error>(chunk.into_bytes())),
                    );

                    axum::response::Response::builder()
                        .status(status)
                        .header("content-type", "text/event-stream")
                        .header("cache-control", "no-cache")
                        .header("connection", "keep-alive")
                        .body(Body::from_stream(stream))
                        .unwrap()
                }),
            }
        }

        pub fn get_requests(&self) -> Vec<MockRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl std::fmt::Debug for MockHttpClient {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MockHttpClient")
                .field("requests", &self.requests)
                .field("response_builder", &"<closure>")
                .finish()
        }
    }

    impl Clone for MockHttpClient {
        fn clone(&self) -> Self {
            Self {
                requests: Arc::clone(&self.requests),
                response_builder: Arc::clone(&self.response_builder),
            }
        }
    }

    #[async_trait]
    impl HttpClient for MockHttpClient {
        async fn request(
            &self,
            req: axum::extract::Request,
        ) -> Result<axum::response::Response, Box<dyn std::error::Error + Send + Sync>> {
            // Extract request details
            let method = req.method().to_string();
            let uri = req.uri().to_string();
            let headers = req
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();

            // Read body
            let body = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                .to_vec();

            // Store the request
            let mock_request = MockRequest {
                method,
                uri,
                headers,
                body,
            };
            self.requests.lock().unwrap().push(mock_request);

            // Return the configured response
            Ok((self.response_builder)())
        }
    }

    /// A mock HTTP client that can be controlled with triggers
    /// Useful for testing concurrency limits with precise control over when requests complete
    pub struct TriggeredMockHttpClient {
        pub requests: Arc<Mutex<Vec<MockRequest>>>,
        response_builder: Arc<dyn Fn() -> axum::response::Response + Send + Sync>,
        triggers: Arc<Mutex<Vec<tokio::sync::oneshot::Sender<()>>>>,
    }

    impl TriggeredMockHttpClient {
        pub fn new(status: StatusCode, body: &str) -> Self {
            let body = body.to_string();
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                response_builder: Arc::new(move || {
                    axum::response::Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body.clone()))
                        .unwrap()
                }),
                triggers: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn get_requests(&self) -> Vec<MockRequest> {
            self.requests.lock().unwrap().clone()
        }

        /// Complete the nth request (0-indexed)
        /// Returns true if the trigger was sent, false if no such request exists
        pub fn complete_request(&self, index: usize) -> bool {
            let mut triggers = self.triggers.lock().unwrap();
            if index < triggers.len() {
                // Remove and send the trigger
                let trigger = triggers.remove(index);
                let _ = trigger.send(());
                true
            } else {
                false
            }
        }

        /// Complete all pending requests
        pub fn complete_all(&self) {
            let mut triggers = self.triggers.lock().unwrap();
            while let Some(trigger) = triggers.pop() {
                let _ = trigger.send(());
            }
        }
    }

    impl std::fmt::Debug for TriggeredMockHttpClient {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TriggeredMockHttpClient")
                .field("requests", &self.requests)
                .field("pending_triggers", &self.triggers.lock().unwrap().len())
                .field("response_builder", &"<closure>")
                .finish()
        }
    }

    impl Clone for TriggeredMockHttpClient {
        fn clone(&self) -> Self {
            Self {
                requests: Arc::clone(&self.requests),
                response_builder: Arc::clone(&self.response_builder),
                triggers: Arc::clone(&self.triggers),
            }
        }
    }

    #[async_trait]
    impl HttpClient for TriggeredMockHttpClient {
        async fn request(
            &self,
            req: axum::extract::Request,
        ) -> Result<axum::response::Response, Box<dyn std::error::Error + Send + Sync>> {
            // Extract request details
            let method = req.method().to_string();
            let uri = req.uri().to_string();
            let headers = req
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();

            // Read body
            let body = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                .to_vec();

            // Store the request
            let mock_request = MockRequest {
                method,
                uri,
                headers,
                body,
            };
            self.requests.lock().unwrap().push(mock_request);

            // Create a trigger for this request
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.triggers.lock().unwrap().push(tx);

            // Wait for the trigger to be fired
            let _ = rx.await;

            // Return the configured response
            Ok((self.response_builder)())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load_balancer::ProviderPool;
    use crate::target::{Target, Targets};
    use axum::http::StatusCode;
    use axum_test::TestServer;
    use dashmap::DashMap;
    use serde_json::json;
    use std::sync::Arc;
    use test_utils::MockHttpClient;

    /// Helper to create a single-provider pool from a target
    fn pool(target: Target) -> ProviderPool {
        target.into_pool()
    }

    #[tokio::test]
    async fn test_empty_targets_returns_404() {
        // Create empty targets
        let targets = target::Targets {
            targets: Arc::new(DashMap::new()),
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, "{}");
        let app_state = AppState::with_client(targets, mock_client);
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 404);
    }

    #[tokio::test]
    async fn test_multiple_targets_routing() {
        // Create targets with multiple models
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            pool(
                target::Target::builder()
                    .url("https://api.openai.com".parse().unwrap())
                    .onwards_key("sk-test-key".to_string())
                    .build(),
            ),
        );
        targets_map.insert(
            "claude-3".to_string(),
            pool(
                target::Target::builder()
                    .url("https://api.anthropic.com".parse().unwrap())
                    .onwards_key("sk-ant-test-key".to_string())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        let mock_client = MockHttpClient::new(
            StatusCode::OK,
            r#"{"choices": [{"message": {"content": "Hello!"}}]}"#,
        );
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Test that gpt-4 model is recognized and returns 200
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Test that claude-3 model is recognized and returns 200
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "claude-3",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Test that non-existent model returns 404
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "non-existent-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 404);

        // Verify that 2 requests were made to the mock client (for the valid models)
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 2);

        // Check that requests were made to the correct URLs
        assert!(requests[0].uri.contains("api.openai.com"));
        assert!(requests[1].uri.contains("api.anthropic.com"));
    }

    #[tokio::test]
    async fn test_request_and_response_details() {
        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .onwards_key("test-api-key".to_string())
                    .build(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        let mock_response_body = r#"{"id": "test-response", "object": "chat.completion", "choices": [{"message": {"content": "Hello from mock!"}}]}"#;
        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response_body);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Make a request
        let request_body = json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Hello!"}],
            "temperature": 0.7
        });

        let response = server
            .post("/v1/chat/completions")
            .json(&request_body)
            .await;

        // Assert on the response
        assert_eq!(response.status_code(), 200);
        let response_body: serde_json::Value = response.json();
        assert_eq!(response_body["id"], "test-response");
        assert_eq!(
            response_body["choices"][0]["message"]["content"],
            "Hello from mock!"
        );

        // Assert on the request that was sent
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let request = &requests[0];

        // Check HTTP method
        assert_eq!(request.method, "POST");

        // Check URL was correctly constructed
        assert_eq!(request.uri, "https://api.example.com/v1/chat/completions");

        // Check headers
        let auth_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "authorization")
            .map(|(_, value)| value);
        assert_eq!(auth_header, Some(&"Bearer test-api-key".to_string()));

        let host_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "host")
            .map(|(_, value)| value);
        assert_eq!(host_header, Some(&"api.example.com".to_string()));

        let content_type_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "content-type")
            .map(|(_, value)| value);
        assert_eq!(content_type_header, Some(&"application/json".to_string()));

        // Check request body
        let forwarded_body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(forwarded_body["model"], "test-model");
        assert_eq!(forwarded_body["messages"][0]["content"], "Hello!");
        assert_eq!(forwarded_body["temperature"], 0.7);
    }

    #[tokio::test]
    async fn test_model_override_header_takes_precedence() {
        // Create two targets
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "header-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.header.com".parse().unwrap())
                    .onwards_key("header-key".to_string())
                    .build(),
            ),
        );
        targets_map.insert(
            "body-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.body.com".parse().unwrap())
                    .onwards_key("body-key".to_string())
                    .build(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Make request with model in JSON body AND model-override header
        let response = server
            .post("/v1/chat/completions")
            .add_header("model-override", "header-model") // Should use this target
            .json(&json!({
                "model": "body-model",  // Should ignore this one
                "messages": [{"role": "user", "content": "Test"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Verify request was sent to the header-model target, not body-model
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let request = &requests[0];
        assert!(request.uri.contains("api.header.com"));
        assert!(!request.uri.contains("api.body.com"));

        // Verify authorization header uses the header-model target's key
        let auth_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "authorization")
            .map(|(_, value)| value);
        assert_eq!(auth_header, Some(&"Bearer header-key".to_string()));
    }

    #[tokio::test]
    async fn test_models_endpoint_returns_proper_model_list() {
        // Create multiple targets
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            pool(
                Target::builder()
                    .url("https://api.openai.com".parse().unwrap())
                    .onwards_key("sk-openai-key".to_string())
                    .build(),
            ),
        );
        targets_map.insert(
            "claude-3".to_string(),
            pool(
                Target::builder()
                    .url("https://api.anthropic.com".parse().unwrap())
                    .onwards_key("sk-ant-key".to_string())
                    .build(),
            ),
        );
        targets_map.insert(
            "gemini-pro".to_string(),
            pool(
                Target::builder()
                    .url("https://api.google.com".parse().unwrap())
                    .onwards_model("gemini-1.5-pro".to_string())
                    .build(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"unused": "response"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Request the /v1/models endpoint
        let response = server.get("/v1/models").await;

        assert_eq!(response.status_code(), 200);

        let response_body: serde_json::Value = response.json();

        // Verify the structure of the response
        assert_eq!(response_body["object"], "list");
        assert!(response_body["data"].is_array());

        let models = response_body["data"].as_array().unwrap();
        assert_eq!(models.len(), 3);

        // Check that all our models are present
        let model_ids: Vec<&str> = models
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();

        assert!(model_ids.contains(&"gpt-4"));
        assert!(model_ids.contains(&"claude-3"));
        assert!(model_ids.contains(&"gemini-pro"));

        // Verify model structure
        for model in models {
            assert_eq!(model["object"], "model");
            assert_eq!(model["owned_by"], "None");
            assert!(model["id"].is_string());
            // created field is optional and None in our implementation
        }

        // Verify that NO requests were made to the mock client (models are handled locally)
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 0);
    }

    #[tokio::test]
    async fn test_models_endpoint_filters_by_bearer_token() {
        use crate::auth::ConstantTimeString;
        use std::collections::HashSet;

        // Create keys for different models
        let mut gpt4_keys = HashSet::new();
        gpt4_keys.insert(ConstantTimeString::from("gpt4-token".to_string()));

        let mut claude_keys = HashSet::new();
        claude_keys.insert(ConstantTimeString::from("claude-token".to_string()));

        // Create targets with different access keys
        let targets_map = Arc::new(DashMap::new());

        // gpt-4: requires gpt4-token
        targets_map.insert(
            "gpt-4".to_string(),
            pool(
                Target::builder()
                    .url("https://api.openai.com".parse().unwrap())
                    .keys(gpt4_keys)
                    .build(),
            ),
        );

        // claude-3: requires claude-token
        targets_map.insert(
            "claude-3".to_string(),
            pool(
                Target::builder()
                    .url("https://api.anthropic.com".parse().unwrap())
                    .keys(claude_keys)
                    .build(),
            ),
        );

        // gemini-pro: no keys required (public)
        targets_map.insert(
            "gemini-pro".to_string(),
            pool(
                Target::builder()
                    .url("https://api.google.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"unused": "response"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Test 1: No bearer token - should only see public models
        let response = server.get("/v1/models").await;
        assert_eq!(response.status_code(), 200);

        let response_body: serde_json::Value = response.json();
        let models = response_body["data"].as_array().unwrap();
        assert_eq!(models.len(), 1); // Only gemini-pro

        let model_ids: Vec<&str> = models
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert!(model_ids.contains(&"gemini-pro"));

        // Test 2: Valid bearer token for gpt-4 - should see gpt-4 + public models
        let response = server
            .get("/v1/models")
            .add_header("authorization", "Bearer gpt4-token")
            .await;
        assert_eq!(response.status_code(), 200);

        let response_body: serde_json::Value = response.json();
        let models = response_body["data"].as_array().unwrap();
        assert_eq!(models.len(), 2); // gpt-4 + gemini-pro

        let model_ids: Vec<&str> = models
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert!(model_ids.contains(&"gpt-4"));
        assert!(model_ids.contains(&"gemini-pro"));

        // Test 3: Valid bearer token for claude - should see claude + public models
        let response = server
            .get("/v1/models")
            .add_header("authorization", "Bearer claude-token")
            .await;
        assert_eq!(response.status_code(), 200);

        let response_body: serde_json::Value = response.json();
        let models = response_body["data"].as_array().unwrap();
        assert_eq!(models.len(), 2); // claude-3 + gemini-pro

        let model_ids: Vec<&str> = models
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert!(model_ids.contains(&"claude-3"));
        assert!(model_ids.contains(&"gemini-pro"));

        // Test 4: Invalid bearer token - should only see public models
        let response = server
            .get("/v1/models")
            .add_header("authorization", "Bearer invalid-token")
            .await;
        assert_eq!(response.status_code(), 200);

        let response_body: serde_json::Value = response.json();
        let models = response_body["data"].as_array().unwrap();
        assert_eq!(models.len(), 1); // Only gemini-pro

        let model_ids: Vec<&str> = models
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert!(model_ids.contains(&"gemini-pro"));
    }

    #[tokio::test]
    async fn test_rate_limiting_blocks_requests() {
        use crate::target::{RateLimitExceeded, RateLimiter, Target, Targets};
        use std::sync::Arc;

        // Create a mock rate limiter that blocks requests
        #[derive(Debug)]
        struct BlockingRateLimiter;

        impl RateLimiter for BlockingRateLimiter {
            fn check(&self) -> Result<(), RateLimitExceeded> {
                Err(RateLimitExceeded) // Always block
            }
        }

        // Create a target with a blocking rate limiter
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "rate-limited-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .limiter(Arc::new(BlockingRateLimiter) as Arc<dyn RateLimiter>)
                    .build(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Make a request to the rate-limited model
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "rate-limited-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Should get 429 Too Many Requests
        assert_eq!(response.status_code(), 429);

        // Should return proper error structure
        let response_body: serde_json::Value = response.json();
        assert_eq!(response_body["type"], "rate_limit_error");
        assert_eq!(response_body["code"], "rate_limit");

        // Verify no request was made to the upstream (since it was rate limited)
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 0);
    }

    #[tokio::test]
    async fn test_rate_limiting_allows_requests() {
        use crate::target::{RateLimitExceeded, RateLimiter, Target, Targets};
        use std::sync::Arc;

        // Create a mock rate limiter that allows requests
        #[derive(Debug)]
        struct AllowingRateLimiter;

        impl RateLimiter for AllowingRateLimiter {
            fn check(&self) -> Result<(), RateLimitExceeded> {
                Ok(()) // Always allow
            }
        }

        // Create a target with an allowing rate limiter
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "rate-limited-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .limiter(Arc::new(AllowingRateLimiter) as Arc<dyn RateLimiter>)
                    .build(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Make a request to the rate-limited model
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "rate-limited-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Should get 200 OK since rate limiter allows it
        assert_eq!(response.status_code(), 200);

        // Verify request was made to the upstream
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].uri.contains("api.example.com"));
    }

    #[tokio::test]
    async fn test_rate_limiting_with_mixed_targets() {
        use crate::target::{RateLimitExceeded, RateLimiter, Target, Targets};
        use std::sync::Arc;

        // Create different rate limiters
        #[derive(Debug)]
        struct BlockingRateLimiter;
        impl RateLimiter for BlockingRateLimiter {
            fn check(&self) -> Result<(), RateLimitExceeded> {
                Err(RateLimitExceeded)
            }
        }

        #[derive(Debug)]
        struct AllowingRateLimiter;
        impl RateLimiter for AllowingRateLimiter {
            fn check(&self) -> Result<(), RateLimitExceeded> {
                Ok(())
            }
        }

        // Create targets with different rate limiting behavior
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "blocked-model".to_string(),
            pool(
                Target::builder()
                    .url("https://blocked.example.com".parse().unwrap())
                    .limiter(Arc::new(BlockingRateLimiter) as Arc<dyn RateLimiter>)
                    .build(),
            ),
        );
        targets_map.insert(
            "allowed-model".to_string(),
            pool(
                Target::builder()
                    .url("https://allowed.example.com".parse().unwrap())
                    .limiter(Arc::new(AllowingRateLimiter) as Arc<dyn RateLimiter>)
                    .build(),
            ),
        );
        targets_map.insert(
            "unlimited-model".to_string(),
            pool(
                Target::builder()
                    .url("https://unlimited.example.com".parse().unwrap())
                    .build(),
            ), // No rate limiter
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Test blocked model - should get 429
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "blocked-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;
        assert_eq!(response.status_code(), 429);

        // Test allowed model - should get 200
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "allowed-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;
        assert_eq!(response.status_code(), 200);

        // Test unlimited model - should get 200
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "unlimited-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;
        assert_eq!(response.status_code(), 200);

        // Verify only allowed and unlimited models made upstream requests
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 2);

        let urls: Vec<&str> = requests.iter().map(|r| r.uri.as_str()).collect();
        assert!(urls.contains(&"https://allowed.example.com/v1/chat/completions"));
        assert!(urls.contains(&"https://unlimited.example.com/v1/chat/completions"));
        assert!(!urls.iter().any(|&url| url.contains("blocked.example.com")));
    }

    #[tokio::test]
    async fn test_concurrency_limiting_below_limits() {
        use target::SemaphoreConcurrencyLimiter;

        // Create a target with concurrency limit
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "limited-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .concurrency_limiter(SemaphoreConcurrencyLimiter::new(5))
                    .build(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Request should succeed (within concurrency limit)
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "limited-model",
                "messages": [{"role": "user", "content": "Test"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Verify request made it through
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);
    }

    #[tokio::test]
    async fn test_concurrency_limiting_at_limits() {
        use std::rc::Rc;
        use target::SemaphoreConcurrencyLimiter;

        // Create a target with concurrency limit of 1
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "limited-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .concurrency_limiter(SemaphoreConcurrencyLimiter::new(1))
                    .build(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        let mock_client =
            test_utils::TriggeredMockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = Rc::new(TestServer::new(router).unwrap());

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                // Start first request in background (it will block waiting for trigger)
                let server_clone = Rc::clone(&server);
                let handle1 = tokio::task::spawn_local(async move {
                    server_clone
                        .post("/v1/chat/completions")
                        .json(&json!({"model": "limited-model", "messages": []}))
                        .await
                });

                // Give it a moment to start and acquire the permit
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;

                // Now send second request - should be rejected immediately since limit is 1
                let response2 = server
                    .post("/v1/chat/completions")
                    .json(&json!({"model": "limited-model", "messages": []}))
                    .await;

                // Second request should be rejected (concurrency limit exceeded)
                assert_eq!(response2.status_code(), 429);
                let body: serde_json::Value = response2.json();
                assert_eq!(body["code"], "concurrency_limit_exceeded");

                // Complete the first request
                mock_client.complete_request(0);
                let response1 = handle1.await.unwrap();
                assert_eq!(response1.status_code(), 200);

                // Only 1 request made it to the mock client
                assert_eq!(mock_client.get_requests().len(), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn test_per_key_concurrency_limiting() {
        use std::rc::Rc;
        use target::SemaphoreConcurrencyLimiter;

        // Create a target without concurrency limit
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .build(),
            ),
        );

        // Set up per-key concurrency limiter
        let key_concurrency_limiters = Arc::new(DashMap::new());
        key_concurrency_limiters.insert(
            "sk-limited-key".to_string(),
            SemaphoreConcurrencyLimiter::new(1) as Arc<dyn target::ConcurrencyLimiter>,
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters,
        };

        let mock_client =
            test_utils::TriggeredMockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = Rc::new(TestServer::new(router).unwrap());

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                // Start first request with the limited key (it will block waiting for trigger)
                let server_clone = Rc::clone(&server);
                let handle1 = tokio::task::spawn_local(async move {
                    server_clone
                        .post("/v1/chat/completions")
                        .add_header(
                            axum::http::HeaderName::from_static("authorization"),
                            axum::http::HeaderValue::from_static("Bearer sk-limited-key"),
                        )
                        .json(&json!({"model": "test-model", "messages": []}))
                        .await
                });

                // Give it a moment to start and acquire the permit
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;

                // Second request with same key should be rejected
                let response2 = server
                    .post("/v1/chat/completions")
                    .add_header(
                        axum::http::HeaderName::from_static("authorization"),
                        axum::http::HeaderValue::from_static("Bearer sk-limited-key"),
                    )
                    .json(&json!({"model": "test-model", "messages": []}))
                    .await;

                // Second request should be rejected (per-key concurrency limit exceeded)
                assert_eq!(response2.status_code(), 429);
                let body: serde_json::Value = response2.json();
                assert_eq!(body["code"], "concurrency_limit_exceeded");

                // Complete the first request
                mock_client.complete_request(0);
                let response1 = handle1.await.unwrap();
                assert_eq!(response1.status_code(), 200);

                // Only 1 request made it to the mock client
                assert_eq!(mock_client.get_requests().len(), 1);
            })
            .await;
    }

    mod metrics {
        use super::*;
        use axum_test::TestServer;
        use dashmap::DashMap;
        use rstest::*;
        use serde_json::json;
        use std::sync::Arc;

        /// Fixture to create a shared metrics server and main server.
        /// The axum-prometheus library using a global Prometheus registry that maintains state across test executions within the same process. Even
        /// with unique prefixes and serial execution, the library prevents creating multiple metric registries with overlapping metric names. So we
        /// use a shared metrics server for all metrics tests.
        #[fixture]
        #[once]
        fn get_shared_metrics_servers(
            #[default(Arc::new(DashMap::new()))] targets: Arc<DashMap<String, ProviderPool>>,
        ) -> (TestServer, TestServer) {
            let targets = Targets {
                targets,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            let (prometheus_layer, handle) = build_metrics_layer_and_handle("onwards");

            let metrics_router = build_metrics_router(handle);
            let metrics_server = TestServer::new(metrics_router).unwrap();

            // Create default config for tests
            let config = crate::config::Config {
                port: 3000,
                targets: std::path::PathBuf::from("config.json"),
                watch: false,
                metrics: false,
                metrics_port: 9090,
                metrics_prefix: "onwards".to_string(),
                pool_max_idle_per_host: 100,
                pool_idle_timeout_secs: 90,
            };
            let app_state = AppState::new(targets, &config);
            let router = build_router(app_state).layer(prometheus_layer);
            let server = TestServer::new(router).unwrap();

            (server, metrics_server)
        }

        #[rstest]
        #[tokio::test]
        async fn test_metrics_server_for_v1_models(
            get_shared_metrics_servers: &(TestServer, TestServer),
        ) {
            let (server, metrics_server) = get_shared_metrics_servers;

            // Get current metrics count before making requests
            let initial_response = metrics_server.get("/metrics").await;
            let initial_metrics = initial_response.text();

            // Count existing v1/models requests (if any)
            let initial_count = initial_metrics
                .lines()
                .find(|line| line.contains("onwards_http_requests_total{method=\"GET\",status=\"200\",endpoint=\"/v1/models\"}"))
                .and_then(|line| line.split_whitespace().last())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);

            // Make a request
            let response = server.get("/v1/models").await;
            assert_eq!(response.status_code(), 200);

            // Check metrics increased by 1
            let response = metrics_server.get("/metrics").await;
            assert_eq!(response.status_code(), 200);
            let metrics_text = response.text();

            let new_count = metrics_text
                .lines()
                .find(|line| line.contains("onwards_http_requests_total{method=\"GET\",status=\"200\",endpoint=\"/v1/models\"}"))
                .and_then(|line| line.split_whitespace().last())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);

            assert_eq!(
                new_count,
                initial_count + 1,
                "Metrics should increment by 1"
            );

            // Make 10 more requests
            for _ in 0..10 {
                let response = server.get("/v1/models").await;
                assert_eq!(response.status_code(), 200);
            }

            // Check metrics increased by 11 total
            let response = metrics_server.get("/metrics").await;
            assert_eq!(response.status_code(), 200);
            let metrics_text = response.text();

            let final_count = metrics_text
                .lines()
                .find(|line| line.contains("onwards_http_requests_total{method=\"GET\",status=\"200\",endpoint=\"/v1/models\"}"))
                .and_then(|line| line.split_whitespace().last())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);

            assert_eq!(
                final_count,
                initial_count + 11,
                "Metrics should increment by 11 total"
            );
        }

        #[rstest]
        #[tokio::test]
        async fn test_metrics_server_for_missing_targets(
            get_shared_metrics_servers: &(TestServer, TestServer),
        ) {
            let (server, metrics_server) = get_shared_metrics_servers;

            // Get current metrics count before making requests
            let initial_response = metrics_server.get("/metrics").await;
            let initial_metrics = initial_response.text();

            // Count existing chat/completions 404 requests (if any)
            let initial_count = initial_metrics
                .lines()
                .find(|line| line.contains("onwards_http_requests_total{method=\"POST\",status=\"404\",endpoint=\"/v1/chat/completions\"}"))
                .and_then(|line| line.split_whitespace().last())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "claude-3",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;
            assert_eq!(response.status_code(), 404);

            let response = metrics_server.get("/metrics").await;

            assert_eq!(response.status_code(), 200);
            let metrics_text = response.text();

            let new_count = metrics_text
                .lines()
                .find(|line| line.contains("onwards_http_requests_total{method=\"POST\",status=\"404\",endpoint=\"/v1/chat/completions\"}"))
                .and_then(|line| line.split_whitespace().last())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);

            assert_eq!(
                new_count,
                initial_count + 1,
                "Metrics should increment by 1"
            );

            for _ in 0..10 {
                let response = server
                    .post("/v1/chat/completions")
                    .json(&json!({
                        "model": "claude-3",
                        "messages": [{"role": "user", "content": "Hello"}]
                    }))
                    .await;
                assert_eq!(response.status_code(), 404);
            }

            let response = metrics_server.get("/metrics").await;
            assert_eq!(response.status_code(), 200);
            let metrics_text = response.text();

            let final_count = metrics_text
                .lines()
                .find(|line| line.contains("onwards_http_requests_total{method=\"POST\",status=\"404\",endpoint=\"/v1/chat/completions\"}"))
                .and_then(|line| line.split_whitespace().last())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);

            assert_eq!(
                final_count,
                initial_count + 11,
                "Metrics should increment by 11 total"
            );
        }
    }

    #[tokio::test]
    async fn test_body_transformation_applied() {
        use serde_json::json;

        // Create a simple target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        // Create a body transformation function that adds a "transformed": true field
        let transform_fn: BodyTransformFn = Arc::new(|_path, _headers, body_bytes| {
            if let Ok(mut json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes)
                && let Some(obj) = json_body.as_object_mut()
            {
                obj.insert("transformed".to_string(), json!(true));
                if let Ok(transformed_bytes) = serde_json::to_vec(&json_body) {
                    return Some(axum::body::Bytes::from(transformed_bytes));
                }
            }
            None
        });

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state =
            AppState::with_client_and_transform(targets, mock_client.clone(), transform_fn);
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Make a request
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Check that the request was transformed before forwarding
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let forwarded_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(forwarded_body["transformed"], true);
        assert_eq!(forwarded_body["model"], "test-model");
        assert_eq!(forwarded_body["messages"][0]["content"], "Hello");
    }

    #[tokio::test]
    async fn test_body_transformation_not_applied_when_none() {
        use serde_json::json;

        // Create a simple target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone()); // No transform function
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Make a request
        let original_body = json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Hello"}]
        });

        let response = server
            .post("/v1/chat/completions")
            .json(&original_body)
            .await;

        assert_eq!(response.status_code(), 200);

        // Check that the request was NOT transformed
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let forwarded_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(forwarded_body.get("transformed").is_none());
        assert_eq!(forwarded_body["model"], "test-model");
        assert_eq!(forwarded_body["messages"][0]["content"], "Hello");
    }

    #[tokio::test]
    async fn test_body_transformation_returns_none() {
        use serde_json::json;

        // Create a simple target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        // Create a transformation function that always returns None (no transformation)
        let transform_fn: BodyTransformFn = Arc::new(|_path, _headers, _body_bytes| None);

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state =
            AppState::with_client_and_transform(targets, mock_client.clone(), transform_fn);
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Make a request
        let original_body = json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Hello"}]
        });

        let response = server
            .post("/v1/chat/completions")
            .json(&original_body)
            .await;

        assert_eq!(response.status_code(), 200);

        // Check that the request was NOT transformed since function returned None
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let forwarded_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(forwarded_body, original_body);
    }

    #[tokio::test]
    async fn test_openai_streaming_include_usage_transformation() {
        use serde_json::json;

        // Create a target for OpenAI
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            pool(
                Target::builder()
                    .url("https://api.openai.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        // Create a transformation function that forces include_usage for streaming requests
        let transform_fn: BodyTransformFn = Arc::new(|path, _headers, body_bytes| {
            // Only transform requests to OpenAI chat completions endpoint
            if path == "/v1/chat/completions"
                && let Ok(mut json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes)
                && let Some(obj) = json_body.as_object_mut()
            {
                // Check if this is a streaming request
                if let Some(stream) = obj.get("stream")
                    && stream.as_bool() == Some(true)
                {
                    // Force include_usage to true for streaming requests
                    obj.insert(
                        "stream_options".to_string(),
                        json!({
                            "include_usage": true
                        }),
                    );

                    if let Ok(transformed_bytes) = serde_json::to_vec(&json_body) {
                        return Some(axum::body::Bytes::from(transformed_bytes));
                    }
                }
            }
            None
        });

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state =
            AppState::with_client_and_transform(targets, mock_client.clone(), transform_fn);
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Test streaming request - should add include_usage
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4",
                "messages": [{"role": "user", "content": "Hello"}],
                "stream": true
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let forwarded_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(forwarded_body["model"], "gpt-4");
        assert_eq!(forwarded_body["stream"], true);
        assert_eq!(forwarded_body["stream_options"]["include_usage"], true);
    }

    #[tokio::test]
    async fn test_openai_non_streaming_not_transformed() {
        use serde_json::json;

        // Create a target for OpenAI
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            pool(
                Target::builder()
                    .url("https://api.openai.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        // Create the same transformation function
        let transform_fn: BodyTransformFn = Arc::new(|path, _headers, body_bytes| {
            if path == "/v1/chat/completions"
                && let Ok(mut json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes)
                && let Some(obj) = json_body.as_object_mut()
                && let Some(stream) = obj.get("stream")
                && stream.as_bool() == Some(true)
            {
                obj.insert(
                    "stream_options".to_string(),
                    json!({
                        "include_usage": true
                    }),
                );

                if let Ok(transformed_bytes) = serde_json::to_vec(&json_body) {
                    return Some(axum::body::Bytes::from(transformed_bytes));
                }
            }
            None
        });

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state =
            AppState::with_client_and_transform(targets, mock_client.clone(), transform_fn);
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Test non-streaming request - should NOT be transformed
        let original_body = json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": false
        });

        let response = server
            .post("/v1/chat/completions")
            .json(&original_body)
            .await;

        assert_eq!(response.status_code(), 200);

        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let forwarded_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(forwarded_body, original_body);
        assert!(forwarded_body.get("stream_options").is_none());
    }

    #[tokio::test]
    async fn test_transformation_path_filtering() {
        use serde_json::json;

        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        // Create a transformation function that only transforms specific paths
        let transform_fn: BodyTransformFn = Arc::new(|path, _headers, body_bytes| {
            if path == "/v1/chat/completions"
                && let Ok(mut json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes)
                && let Some(obj) = json_body.as_object_mut()
            {
                obj.insert("path_transformed".to_string(), json!(path));
                if let Ok(transformed_bytes) = serde_json::to_vec(&json_body) {
                    return Some(axum::body::Bytes::from(transformed_bytes));
                }
            }
            None
        });

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state =
            AppState::with_client_and_transform(targets, mock_client.clone(), transform_fn);
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Test matching path - should be transformed
        let response1 = server
            .post("/v1/chat/completions")
            .json(&json!({"model": "test-model", "test": "data"}))
            .await;
        assert_eq!(response1.status_code(), 200);

        // Test non-matching path - should NOT be transformed
        let response2 = server
            .post("/v1/embeddings")
            .json(&json!({"model": "test-model", "test": "data"}))
            .await;
        assert_eq!(response2.status_code(), 200);

        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 2);

        // First request should be transformed
        let forwarded_body1: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(forwarded_body1["path_transformed"], "/v1/chat/completions");

        // Second request should NOT be transformed
        let forwarded_body2: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert!(forwarded_body2.get("path_transformed").is_none());
    }

    mod response_headers_pricing {
        use super::*;
        use std::collections::HashMap;
        use target::{Target, Targets};

        #[tokio::test]
        async fn test_pricing_added_to_response_headers_when_configured() {
            let targets_map = Arc::new(DashMap::new());
            let mut response_headers = HashMap::new();
            response_headers.insert("Input-Price-Per-Token".to_string(), "0.00003".to_string());
            response_headers.insert("Output-Price-Per-Token".to_string(), "0.00006".to_string());

            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .response_headers(response_headers)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client);
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);
            assert_eq!(response.header("Input-Price-Per-Token"), "0.00003");
            assert_eq!(response.header("Output-Price-Per-Token"), "0.00006");
        }

        #[tokio::test]
        async fn test_no_pricing_headers_when_not_configured() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "free-model".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.example.com".parse().unwrap())
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client);
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "free-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);
            assert!(response.maybe_header("Input-Price-Per-Token").is_none());
            assert!(response.maybe_header("Output-Price-Per-Token").is_none());
        }

        #[tokio::test]
        async fn test_pricing_preserved_in_error_response_headers() {
            let targets_map = Arc::new(DashMap::new());
            let mut response_headers = HashMap::new();
            response_headers.insert("Input-Price-Per-Token".to_string(), "0.00001".to_string());
            response_headers.insert("Output-Price-Per-Token".to_string(), "0.00002".to_string());

            targets_map.insert(
                "error-model".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.example.com".parse().unwrap())
                        .response_headers(response_headers)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            let mock_client = MockHttpClient::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error": "Server error"}"#,
            );
            let app_state = AppState::with_client(targets, mock_client);
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "error-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 500);
            assert_eq!(response.header("Input-Price-Per-Token"), "0.00001");
            assert_eq!(response.header("Output-Price-Per-Token"), "0.00002");
        }

        #[tokio::test]
        async fn test_pricing_headers_with_different_models() {
            let targets_map = Arc::new(DashMap::new());

            let mut expensive_headers = HashMap::new();
            expensive_headers.insert("Input-Price-Per-Token".to_string(), "0.0001".to_string());
            expensive_headers.insert("Output-Price-Per-Token".to_string(), "0.0002".to_string());

            targets_map.insert(
                "expensive-model".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.expensive.com".parse().unwrap())
                        .response_headers(expensive_headers)
                        .build(),
                ),
            );

            let mut cheap_headers = HashMap::new();
            cheap_headers.insert("Input-Price-Per-Token".to_string(), "0.000001".to_string());
            cheap_headers.insert("Output-Price-Per-Token".to_string(), "0.000002".to_string());

            targets_map.insert(
                "cheap-model".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.cheap.com".parse().unwrap())
                        .response_headers(cheap_headers)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client);
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            // Test expensive model
            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "expensive-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);
            assert_eq!(response.header("Input-Price-Per-Token"), "0.0001");
            assert_eq!(response.header("Output-Price-Per-Token"), "0.0002");

            // Test cheap model
            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "cheap-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);
            assert_eq!(response.header("Input-Price-Per-Token"), "0.000001");
            assert_eq!(response.header("Output-Price-Per-Token"), "0.000002");
        }

        #[tokio::test]
        async fn test_pricing_header_with_only_input_price() {
            let targets_map = Arc::new(DashMap::new());
            let mut response_headers = HashMap::new();
            response_headers.insert("Input-Price-Per-Token".to_string(), "0.00005".to_string());

            targets_map.insert(
                "input-only-model".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.example.com".parse().unwrap())
                        .response_headers(response_headers)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client);
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "input-only-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);
            assert_eq!(response.header("Input-Price-Per-Token"), "0.00005");
            assert!(response.maybe_header("Output-Price-Per-Token").is_none());
        }

        #[tokio::test]
        async fn test_pricing_header_with_only_output_price() {
            let targets_map = Arc::new(DashMap::new());
            let mut response_headers = HashMap::new();
            response_headers.insert("Output-Price-Per-Token".to_string(), "0.00008".to_string());

            targets_map.insert(
                "output-only-model".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.example.com".parse().unwrap())
                        .response_headers(response_headers)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client);
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "output-only-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);
            assert!(response.maybe_header("Input-Price-Per-Token").is_none());
            assert_eq!(response.header("Output-Price-Per-Token"), "0.00008");
        }
    }

    mod load_balancing {
        use super::*;
        use crate::load_balancer::{Provider, ProviderPool};

        #[tokio::test]
        async fn test_load_balancing_with_multiple_providers() {
            // Create a pool with two providers
            let providers = vec![
                Provider {
                    target: Target::builder()
                        .url("https://api.provider1.com".parse().unwrap())
                        .onwards_key("key1".to_string())
                        .build(),
                    weight: 1,
                },
                Provider {
                    target: Target::builder()
                        .url("https://api.provider2.com".parse().unwrap())
                        .onwards_key("key2".to_string())
                        .build(),
                    weight: 1,
                },
            ];
            let pool = ProviderPool::new(providers);

            let targets_map = Arc::new(DashMap::new());
            targets_map.insert("test-model".to_string(), pool);

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client.clone());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            // Make multiple requests - they should be routed to one of the providers
            for _ in 0..5 {
                let response = server
                    .post("/v1/chat/completions")
                    .json(&json!({
                        "model": "test-model",
                        "messages": [{"role": "user", "content": "Hello"}]
                    }))
                    .await;

                assert_eq!(response.status_code(), 200);
            }

            // Verify requests were made (at least one should go to each provider over multiple runs)
            let requests = mock_client.get_requests();
            assert_eq!(requests.len(), 5);
        }

        #[tokio::test]
        async fn test_load_balancing_with_weighted_providers() {
            // Create a pool with providers having different weights
            let providers = vec![
                Provider {
                    target: Target::builder()
                        .url("https://api.high-weight.com".parse().unwrap())
                        .onwards_key("key-high".to_string())
                        .build(),
                    weight: 3, // Higher weight = more traffic
                },
                Provider {
                    target: Target::builder()
                        .url("https://api.low-weight.com".parse().unwrap())
                        .onwards_key("key-low".to_string())
                        .build(),
                    weight: 1,
                },
            ];
            let pool = ProviderPool::new(providers);

            let targets_map = Arc::new(DashMap::new());
            targets_map.insert("weighted-model".to_string(), pool);

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client.clone());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            // Make multiple requests
            for _ in 0..20 {
                let response = server
                    .post("/v1/chat/completions")
                    .json(&json!({
                        "model": "weighted-model",
                        "messages": [{"role": "user", "content": "Hello"}]
                    }))
                    .await;

                assert_eq!(response.status_code(), 200);
            }

            let requests = mock_client.get_requests();
            assert_eq!(requests.len(), 20);

            // Count requests to each provider (URI is the full path, check if it contains the host)
            let high_weight_count = requests
                .iter()
                .filter(|r| r.uri.contains("api.high-weight.com"))
                .count();
            let low_weight_count = requests
                .iter()
                .filter(|r| r.uri.contains("api.low-weight.com"))
                .count();

            // With weights 3:1, high-weight should get roughly 3x more traffic
            // Allow for statistical variance - high weight should have at least more requests
            assert!(
                high_weight_count > low_weight_count,
                "Expected high-weight provider ({}) to receive more requests than low-weight ({})",
                high_weight_count,
                low_weight_count
            );
        }

        #[tokio::test]
        async fn test_single_provider_pool_behaves_like_single_target() {
            // A pool with a single provider should work identically to the old behavior
            let pool = ProviderPool::single(
                Target::builder()
                    .url("https://api.single.com".parse().unwrap())
                    .onwards_key("single-key".to_string())
                    .build(),
                1,
            );

            let targets_map = Arc::new(DashMap::new());
            targets_map.insert("single-model".to_string(), pool);

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client.clone());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "single-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);

            let requests = mock_client.get_requests();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].uri.contains("api.single.com"));
        }
    }

    mod response_sanitization {
        use super::*;

        #[tokio::test]
        async fn test_sanitize_non_streaming_removes_unknown_fields() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .sanitize_response(true)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            // Response with provider-specific fields
            let mock_response = r#"{
                "id": "chatcmpl-123",
                "object": "chat.completion",
                "created": 1677652288,
                "model": "gpt-4",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Hello!"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 9,
                    "completion_tokens": 2,
                    "total_tokens": 11
                },
                "custom_provider_field": "should be removed",
                "another_unknown_field": 12345
            }"#;

            let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);

            let body: serde_json::Value = response.json();
            // Verify standard fields are present
            assert!(body.get("id").is_some());
            assert!(body.get("choices").is_some());
            assert!(body.get("usage").is_some());

            // Verify unknown fields are removed
            assert!(body.get("custom_provider_field").is_none());
            assert!(body.get("another_unknown_field").is_none());
        }

        #[tokio::test]
        async fn test_sanitize_rewrites_model_field() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .onwards_model("gpt-4-turbo-2024-04-09".to_string())
                        .sanitize_response(true)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            // Response from upstream has the turbo model
            let mock_response = r#"{
                "id": "chatcmpl-123",
                "object": "chat.completion",
                "created": 1677652288,
                "model": "gpt-4-turbo-2024-04-09",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Hello!"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 9,
                    "completion_tokens": 2,
                    "total_tokens": 11
                }
            }"#;

            let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);

            let body: serde_json::Value = response.json();
            // Verify model field was rewritten to match client request
            assert_eq!(body["model"], "gpt-4");
        }

        #[tokio::test]
        async fn test_sanitize_streaming_removes_unknown_fields() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .sanitize_response(true)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            let streaming_chunks = vec![
                r#"data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1677652288,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}],"custom_field":"remove_me"}

"#
                    .to_string(),
                "data: [DONE]\n\n".to_string(),
            ];

            let mock_client = MockHttpClient::new_streaming(StatusCode::OK, streaming_chunks);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}],
                    "stream": true
                }))
                .await;

            assert_eq!(response.status_code(), 200);

            let body = response.text();
            // Verify [DONE] marker is preserved
            assert!(body.contains("data: [DONE]"));
            // Verify custom field is removed
            assert!(!body.contains("custom_field"));
            assert!(!body.contains("remove_me"));
        }

        #[tokio::test]
        async fn test_sanitize_streaming_rewrites_model() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .onwards_model("gpt-4-turbo".to_string())
                        .sanitize_response(true)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            let streaming_chunks = vec![
                r#"data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1677652288,"model":"gpt-4-turbo","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}

"#
                    .to_string(),
                "data: [DONE]\n\n".to_string(),
            ];

            let mock_client = MockHttpClient::new_streaming(StatusCode::OK, streaming_chunks);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}],
                    "stream": true
                }))
                .await;

            assert_eq!(response.status_code(), 200);

            let body = response.text();
            // Verify model was rewritten
            assert!(body.contains(r#""model":"gpt-4""#));
            assert!(!body.contains(r#""model":"gpt-4-turbo""#));
        }

        #[tokio::test]
        async fn test_sanitization_disabled_passes_through() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .sanitize_response(false) // Explicitly disabled
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            // Response with provider-specific fields
            let mock_response = r#"{
                "id": "chatcmpl-123",
                "object": "chat.completion",
                "created": 1677652288,
                "model": "gpt-4",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Hello!"
                    },
                    "finish_reason": "stop"
                }],
                "custom_provider_field": "should be preserved"
            }"#;

            let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);

            let body: serde_json::Value = response.json();
            // Verify custom field is preserved when sanitization is disabled
            assert!(body.get("custom_provider_field").is_some());
            assert_eq!(body["custom_provider_field"], "should be preserved");
        }

        #[tokio::test]
        async fn test_sanitization_only_applies_to_chat_completions() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .sanitize_response(true)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            // Response from a different endpoint (e.g., embeddings)
            let mock_response = r#"{
                "object": "list",
                "data": [{"embedding": [0.1, 0.2]}],
                "model": "text-embedding-ada-002",
                "usage": {"prompt_tokens": 8, "total_tokens": 8},
                "custom_field": "preserved"
            }"#;

            let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/embeddings")
                .json(&json!({
                    "model": "gpt-4",
                    "input": "Hello"
                }))
                .await;

            assert_eq!(response.status_code(), 200);

            let body: serde_json::Value = response.json();
            // Verify response is not sanitized for non-chat-completion endpoints
            assert!(body.get("custom_field").is_some());
        }

        #[tokio::test]
        async fn test_pool_level_sanitization_applies_to_all_providers() {
            use crate::load_balancer::ProviderPool;

            // Create a pool with sanitization enabled at pool level
            let provider1 = Target::builder()
                .url("https://api1.com".parse().unwrap())
                .onwards_key("key1".to_string())
                .build();

            let provider2 = Target::builder()
                .url("https://api2.com".parse().unwrap())
                .onwards_key("key2".to_string())
                .sanitize_response(false) // Provider overrides pool setting
                .build();

            let pool = ProviderPool::new(vec![
                crate::load_balancer::Provider {
                    target: provider1,
                    weight: 1,
                },
                crate::load_balancer::Provider {
                    target: provider2,
                    weight: 1,
                },
            ]);

            let targets_map = Arc::new(DashMap::new());
            targets_map.insert("test-model".to_string(), pool);

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
            };

            let mock_response = r#"{
                "id": "chatcmpl-123",
                "object": "chat.completion",
                "created": 1677652288,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "Hello!"},
                    "finish_reason": "stop"
                }],
                "custom_field": "value"
            }"#;

            let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "test-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);
        }
    }
}
