//! Onwards - A flexible LLM proxy library
//!
//! This library provides the core functionality for proxying requests to various LLM endpoints
//! with support for authentication, model routing, and dynamic configuration.

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
pub mod errors;
pub mod handlers;
pub mod models;
pub mod target;

use client::{HttpClient, HyperClient};
use handlers::{models as models_handler, target_message_handler};
use models::ExtractedModel;

/// Type alias for body transformation function
/// Takes (path, headers, body_bytes) and returns transformed body_bytes or None if no transformation
pub type BodyTransformFn =
    Arc<dyn Fn(&str, &HeaderMap, &[u8]) -> Option<axum::body::Bytes> + Send + Sync>;

/// The main application state containing the HTTP client and targets configuration
#[derive(Clone)]
pub struct AppState<T: HttpClient> {
    pub http_client: T,
    pub targets: target::Targets,
    pub body_transform_fn: Option<BodyTransformFn>,
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
            .finish()
    }
}

impl AppState<HyperClient> {
    /// Create a new AppState with the default Hyper client
    pub fn new(targets: target::Targets) -> Self {
        let http_client = client::create_hyper_client();
        Self {
            http_client,
            targets,
            body_transform_fn: None,
        }
    }

    /// Create a new AppState with the default Hyper client and a body transformation function
    pub fn with_transform(targets: target::Targets, body_transform_fn: BodyTransformFn) -> Self {
        let http_client = client::create_hyper_client();
        Self {
            http_client,
            targets,
            body_transform_fn: Some(body_transform_fn),
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
        }
    }
}

/// Extract the model name from a request
///
/// This function checks for a model override header first, then extracts the model from the JSON body.
/// This is the same logic used by the proxy handler, extracted for reuse.
///
/// # Arguments
/// * `headers` - The request headers to check for model override
/// * `body_bytes` - The request body as bytes to parse for model field
///
/// # Returns
/// * `Ok(String)` - The extracted model name
/// * `Err(())` - If no model could be extracted or parsing failed
pub fn extract_model_from_request(headers: &HeaderMap, body_bytes: &[u8]) -> Result<String, ()> {
    const MODEL_OVERRIDE_HEADER: &str = "model-override";

    // Order of precedence for the model:
    // 1. supplied as a header (model-override)
    // 2. Available in the request body as JSON
    match headers.get(MODEL_OVERRIDE_HEADER) {
        Some(header_value) => {
            let model_str = header_value.to_str().map_err(|_| ())?;
            Ok(model_str.to_string())
        }
        None => {
            let extracted: ExtractedModel = serde_json::from_slice(body_bytes).map_err(|_| ())?;
            Ok(extracted.model.to_string())
        }
    }
}

/// Build the main router for the proxy
/// This creates routes for:
/// - `/models` - Returns available models
/// - `/v1/models` - Returns available models
/// - `/{*path}` - Forwards all other requests to the appropriate target
#[instrument(skip(state))]
pub fn build_router<T: HttpClient + Clone + Send + Sync + 'static>(state: AppState<T>) -> Router {
    info!("Building router");
    Router::new()
        .route("/models", get(models_handler))
        .route("/v1/models", get(models_handler))
        .route("/{*path}", any(target_message_handler))
        .with_state(state)
}

/// Builds a router for the metrics endpoint.
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

/// Builds a layer and handle for prometheus metrics collection.
///
/// # Parameters
/// - `prefix`: A string prefix for the metrics, which can be either a string literal or an owned string.
///   This parameter uses `impl Into<Cow<'static, str>>` to allow flexibility in passing either borrowed
///   or owned strings. The `'static` lifetime ensures that the prefix is valid for the entire duration
///   of the program, as required by the Prometheus metrics layer.
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

#[cfg(test)]
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{Target, Targets};
    use axum::http::StatusCode;
    use axum_test::TestServer;
    use dashmap::DashMap;
    use serde_json::json;
    use std::sync::Arc;
    use test_utils::MockHttpClient;

    #[tokio::test]
    async fn test_empty_targets_returns_404() {
        // Create empty targets
        let targets = target::Targets {
            targets: Arc::new(DashMap::new()),
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
            target::Target::builder()
                .url("https://api.openai.com".parse().unwrap())
                .onwards_key("sk-test-key".to_string())
                .build(),
        );
        targets_map.insert(
            "claude-3".to_string(),
            target::Target::builder()
                .url("https://api.anthropic.com".parse().unwrap())
                .onwards_key("sk-ant-test-key".to_string())
                .build(),
        );

        let targets = target::Targets {
            targets: targets_map,
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
            Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .onwards_key("test-api-key".to_string())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
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
            Target::builder()
                .url("https://api.header.com".parse().unwrap())
                .onwards_key("header-key".to_string())
                .build(),
        );
        targets_map.insert(
            "body-model".to_string(),
            Target::builder()
                .url("https://api.body.com".parse().unwrap())
                .onwards_key("body-key".to_string())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
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
            Target::builder()
                .url("https://api.openai.com".parse().unwrap())
                .onwards_key("sk-openai-key".to_string())
                .build(),
        );
        targets_map.insert(
            "claude-3".to_string(),
            Target::builder()
                .url("https://api.anthropic.com".parse().unwrap())
                .onwards_key("sk-ant-key".to_string())
                .build(),
        );
        targets_map.insert(
            "gemini-pro".to_string(),
            Target::builder()
                .url("https://api.google.com".parse().unwrap())
                .onwards_model("gemini-1.5-pro".to_string())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
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
            Target::builder()
                .url("https://api.openai.com".parse().unwrap())
                .keys(gpt4_keys)
                .build(),
        );

        // claude-3: requires claude-token
        targets_map.insert(
            "claude-3".to_string(),
            Target::builder()
                .url("https://api.anthropic.com".parse().unwrap())
                .keys(claude_keys)
                .build(),
        );

        // gemini-pro: no keys required (public)
        targets_map.insert(
            "gemini-pro".to_string(),
            Target::builder()
                .url("https://api.google.com".parse().unwrap())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
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
        use crate::target::{RateLimiter, Target, Targets};
        use std::sync::Arc;

        // Create a mock rate limiter that blocks requests
        #[derive(Debug)]
        struct BlockingRateLimiter;

        impl RateLimiter for BlockingRateLimiter {
            fn check(&self) -> Result<(), ()> {
                Err(()) // Always block
            }
        }

        // Create a target with a blocking rate limiter
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "rate-limited-model".to_string(),
            Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .limiter(Arc::new(BlockingRateLimiter) as Arc<dyn RateLimiter>)
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
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
        use crate::target::{RateLimiter, Target, Targets};
        use std::sync::Arc;

        // Create a mock rate limiter that allows requests
        #[derive(Debug)]
        struct AllowingRateLimiter;

        impl RateLimiter for AllowingRateLimiter {
            fn check(&self) -> Result<(), ()> {
                Ok(()) // Always allow
            }
        }

        // Create a target with an allowing rate limiter
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "rate-limited-model".to_string(),
            Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .limiter(Arc::new(AllowingRateLimiter) as Arc<dyn RateLimiter>)
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
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
        use crate::target::{RateLimiter, Target, Targets};
        use std::sync::Arc;

        // Create different rate limiters
        #[derive(Debug)]
        struct BlockingRateLimiter;
        impl RateLimiter for BlockingRateLimiter {
            fn check(&self) -> Result<(), ()> {
                Err(())
            }
        }

        #[derive(Debug)]
        struct AllowingRateLimiter;
        impl RateLimiter for AllowingRateLimiter {
            fn check(&self) -> Result<(), ()> {
                Ok(())
            }
        }

        // Create targets with different rate limiting behavior
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "blocked-model".to_string(),
            Target::builder()
                .url("https://blocked.example.com".parse().unwrap())
                .limiter(Arc::new(BlockingRateLimiter) as Arc<dyn RateLimiter>)
                .build(),
        );
        targets_map.insert(
            "allowed-model".to_string(),
            Target::builder()
                .url("https://allowed.example.com".parse().unwrap())
                .limiter(Arc::new(AllowingRateLimiter) as Arc<dyn RateLimiter>)
                .build(),
        );
        targets_map.insert(
            "unlimited-model".to_string(),
            Target::builder()
                .url("https://unlimited.example.com".parse().unwrap())
                .build(), // No rate limiter
        );

        let targets = Targets {
            targets: targets_map,
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
            #[default(Arc::new(DashMap::new()))] targets: Arc<DashMap<String, Target>>,
        ) -> (TestServer, TestServer) {
            let targets = Targets { targets };

            let (prometheus_layer, handle) = build_metrics_layer_and_handle("onwards");

            let metrics_router = build_metrics_router(handle);
            let metrics_server = TestServer::new(metrics_router).unwrap();

            let app_state = AppState::new(targets);
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
            Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .build(),
        );

        let targets = target::Targets {
            targets: targets_map,
        };

        // Create a body transformation function that adds a "transformed": true field
        let transform_fn: BodyTransformFn = Arc::new(|_path, _headers, body_bytes| {
            if let Ok(mut json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes) {
                if let Some(obj) = json_body.as_object_mut() {
                    obj.insert("transformed".to_string(), json!(true));
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
            Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .build(),
        );

        let targets = target::Targets {
            targets: targets_map,
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
        assert!(!forwarded_body.get("transformed").is_some());
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
            Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .build(),
        );

        let targets = target::Targets {
            targets: targets_map,
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
            Target::builder()
                .url("https://api.openai.com".parse().unwrap())
                .build(),
        );

        let targets = target::Targets {
            targets: targets_map,
        };

        // Create a transformation function that forces include_usage for streaming requests
        let transform_fn: BodyTransformFn = Arc::new(|path, _headers, body_bytes| {
            // Only transform requests to OpenAI chat completions endpoint
            if path == "/v1/chat/completions" {
                if let Ok(mut json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes) {
                    if let Some(obj) = json_body.as_object_mut() {
                        // Check if this is a streaming request
                        if let Some(stream) = obj.get("stream") {
                            if stream.as_bool() == Some(true) {
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
            Target::builder()
                .url("https://api.openai.com".parse().unwrap())
                .build(),
        );

        let targets = target::Targets {
            targets: targets_map,
        };

        // Create the same transformation function
        let transform_fn: BodyTransformFn = Arc::new(|path, _headers, body_bytes| {
            if path == "/v1/chat/completions" {
                if let Ok(mut json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes) {
                    if let Some(obj) = json_body.as_object_mut() {
                        if let Some(stream) = obj.get("stream") {
                            if stream.as_bool() == Some(true) {
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
        assert!(!forwarded_body.get("stream_options").is_some());
    }

    #[tokio::test]
    async fn test_transformation_path_filtering() {
        use serde_json::json;

        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .build(),
        );

        let targets = target::Targets {
            targets: targets_map,
        };

        // Create a transformation function that only transforms specific paths
        let transform_fn: BodyTransformFn = Arc::new(|path, _headers, body_bytes| {
            if path == "/v1/chat/completions" {
                if let Ok(mut json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes) {
                    if let Some(obj) = json_body.as_object_mut() {
                        obj.insert("path_transformed".to_string(), json!(path));
                        if let Ok(transformed_bytes) = serde_json::to_vec(&json_body) {
                            return Some(axum::body::Bytes::from(transformed_bytes));
                        }
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
        assert!(!forwarded_body2.get("path_transformed").is_some());
    }
}
