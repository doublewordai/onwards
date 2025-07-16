mod auth;
mod client;
mod config;
mod handlers;
mod models;
mod target;

use axum::{
    Router,
    routing::{any, get},
};
use clap::Parser as _;
use client::{HttpClient, HyperClient, create_hyper_client};
use config::Config;
use handlers::{models, target_message_handler};
use target::{Targets, WatchedFile};
use tokio::net::TcpListener;
use tracing::{info, instrument};

#[derive(Clone, Debug)]
pub(crate) struct AppState<T: HttpClient> {
    pub(crate) http_client: T,
    pub(crate) targets: Targets,
}

impl AppState<HyperClient> {
    pub(crate) fn new(targets: Targets) -> Self {
        let http_client = create_hyper_client();

        Self {
            http_client,
            targets,
        }
    }
}

impl<T: HttpClient> AppState<T> {
    #[cfg(test)]
    pub(crate) fn with_client(targets: Targets, http_client: T) -> Self {
        Self {
            http_client,
            targets,
        }
    }
}

#[instrument(skip(state))]
pub(crate) async fn build_router<T: HttpClient + Clone + Send + Sync + 'static>(
    state: AppState<T>,
) -> Router {
    info!("Building router");
    Router::new()
        .route("/v1/models", get(models)) // Intercept the /v1/models call at the gateway level.
        .route("/{*path}", any(target_message_handler)) // Anything else is passed straight through to the corresponding target
        .with_state(state)
}

#[tokio::main]
#[instrument]
pub async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::parse().validate()?;
    info!("Starting AI Gateway with config: {:?}", config);

    let targets = Targets::from_config_file(&config.targets)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create targets from config: {}", e))?;
    
    info!("Loaded {} targets:", targets.targets.len());
    for entry in targets.targets.iter() {
        info!("  - Model '{}' -> URL: {}, onwards_model: {:?}", 
            entry.key(), 
            entry.value().url,
            entry.value().onwards_model
        );
    }

    // Start file watcher if a config file was specified
    if config.watch {
        targets.receive_updates(WatchedFile(config.targets)).await?;
    }

    let app_state = AppState::new(targets);
    let router = build_router(app_state).await;

    let bind_addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&bind_addr).await?;
    info!("AI Gateway listening on {}", bind_addr);

    axum::serve(listener, router).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use client::HttpClient;
    use target::Target;

    use async_trait::async_trait;
    use axum::http::StatusCode;
    use axum_test::TestServer;
    use dashmap::DashMap;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use target::Targets;

    pub struct MockHttpClient {
        requests: Arc<Mutex<Vec<MockRequest>>>,
        response_builder: Arc<dyn Fn() -> axum::response::Response + Send + Sync>,
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

    #[tokio::test]
    async fn test_empty_targets_returns_404() {
        // Create empty targets
        let targets = Targets {
            targets: Arc::new(DashMap::new()),
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, "{}");
        let app_state = AppState::with_client(targets, mock_client);
        let router = build_router(app_state).await;
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
            Target::builder()
                .url("https://api.openai.com".parse().unwrap())
                .onwards_key("sk-test-key".to_string())
                .build(),
        );
        targets_map.insert(
            "claude-3".to_string(),
            Target::builder()
                .url("https://api.anthropic.com".parse().unwrap())
                .onwards_key("sk-ant-test-key".to_string())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };

        let mock_client = MockHttpClient::new(
            StatusCode::OK,
            r#"{"choices": [{"message": {"content": "Hello!"}}]}"#,
        );
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
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
        let router = build_router(app_state).await;
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
        let router = build_router(app_state).await;
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
    async fn test_routing_with_header_only_no_json_body() {
        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "header-only-model".to_string(),
            Target::builder()
                .url("https://api.headeronly.com".parse().unwrap())
                .onwards_key("header-only-key".to_string())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"result": "success"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Make request with only model-override header, no JSON body
        let response = server
            .post("/v1/embeddings")
            .add_header("model-override", "header-only-model")
            .await;

        assert_eq!(response.status_code(), 200);

        // Verify request was sent to the correct target
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let request = &requests[0];
        assert!(request.uri.contains("api.headeronly.com"));
        assert!(request.uri.contains("/v1/embeddings"));

        // Verify authorization header is correct
        let auth_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "authorization")
            .map(|(_, value)| value);
        assert_eq!(auth_header, Some(&"Bearer header-only-key".to_string()));

        // Verify body is empty (no JSON was sent)
        assert!(request.body.is_empty());
    }

    #[tokio::test]
    async fn test_invalid_model_override_header_returns_400() {
        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "valid-model".to_string(),
            Target::builder()
                .url("https://api.valid.com".parse().unwrap())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"result": "success"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Non-existent model in header should return 404 (target not found)
        let response = server
            .post("/v1/chat/completions")
            .add_header("model-override", "non-existent-model")
            .await;

        assert_eq!(response.status_code(), 404);
    }

    #[tokio::test]
    async fn test_missing_model_in_header_and_body_returns_400() {
        // Create a target (doesn't matter what it is since we won't reach it)
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "some-model".to_string(),
            Target::builder()
                .url("https://api.some.com".parse().unwrap())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"result": "success"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Test 1: No header, no body
        let response = server.post("/v1/chat/completions").await;

        assert_eq!(response.status_code(), 400);

        // Test 2: No header, JSON body without model field
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "messages": [{"role": "user", "content": "Hello"}],
                "temperature": 0.7
                // Missing "model" field
            }))
            .await;

        assert_eq!(response.status_code(), 400);

        // Test 3: No header, invalid JSON body
        let response = server
            .post("/v1/chat/completions")
            .text("invalid json")
            .await;

        assert_eq!(response.status_code(), 400);

        // Verify no requests were made to the mock client
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 0);
    }

    #[tokio::test]
    async fn test_onward_model_header_rewrites_body() {
        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "input-model".to_string(),
            Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .onwards_key("test-key".to_string())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"response": "success"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Make request with onwards-model header to rewrite the model in the body
        let response = server
            .post("/v1/chat/completions")
            .add_header("onwards-model", "rewritten-model")
            .json(&json!({
                "model": "input-model",
                "messages": [{"role": "user", "content": "Hello"}],
                "temperature": 0.5
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Verify the request was sent and the model was rewritten
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let request = &requests[0];

        // Check the forwarded body has the rewritten model
        let forwarded_body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(forwarded_body["model"], "rewritten-model"); // Should be rewritten
        assert_eq!(forwarded_body["messages"][0]["content"], "Hello"); // Should be preserved
        assert_eq!(forwarded_body["temperature"], 0.5); // Should be preserved

        // Verify Content-Length header was updated
        let content_length_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "content-length")
            .map(|(_, value)| value);
        assert!(content_length_header.is_some());

        // The content length should match the actual body length
        let expected_length = request.body.len().to_string();
        assert_eq!(content_length_header, Some(&expected_length));
    }

    #[tokio::test]
    async fn test_onward_model_header_takes_precedence_over_target_config() {
        // Create a target with model_key configured
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "input-model".to_string(),
            Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .onwards_key("test-key".to_string())
                .onwards_model("config-model".to_string()) // This should be overridden by header
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"response": "success"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Make request with onwards-model header that should override the target's model_key
        let response = server
            .post("/v1/chat/completions")
            .add_header("onwards-model", "header-model") // This should take precedence
            .json(&json!({
                "model": "input-model",
                "messages": [{"role": "user", "content": "Test precedence"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Verify the request was sent and the header value was used, not the config
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let request = &requests[0];

        // Check the forwarded body has the header model, not the config model
        let forwarded_body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(forwarded_body["model"], "header-model"); // From header, not "config-model"
        assert_eq!(forwarded_body["messages"][0]["content"], "Test precedence");
    }

    #[tokio::test]
    async fn test_model_rewriting_skipped_with_empty_body() {
        // Create a target with model_key configured
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .onwards_key("test-key".to_string())
                .onwards_model("should-not-be-used".to_string()) // This should be ignored due to empty body
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"response": "success"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Make request with model-override header and onwards-model header, but empty body
        let response = server
            .post("/v1/embeddings")
            .add_header("model-override", "test-model") // Use this for routing
            .add_header("onwards-model", "rewrite-model") // This should be ignored due to empty body
            .await;

        assert_eq!(response.status_code(), 200);

        // Verify the request was sent but model rewriting was skipped
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let request = &requests[0];

        // Body should be empty - no rewriting should have occurred
        assert!(request.body.is_empty());

        // Verify we routed to the correct target
        assert!(request.uri.contains("api.example.com"));
        assert!(request.uri.contains("/v1/embeddings"));
    }

    #[tokio::test]
    async fn test_different_http_methods_forwarded_correctly() {
        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            Target::builder()
                .url("https://api.methods.com".parse().unwrap())
                .onwards_key("method-key".to_string())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"method": "success"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Test GET request (use a different endpoint since /v1/models is intercepted)
        let response = server
            .get("/v1/files")
            .add_header("model-override", "test-model")
            .await;
        assert_eq!(response.status_code(), 200);

        // Test PUT request
        let response = server
            .put("/v1/fine-tuning/jobs")
            .add_header("model-override", "test-model")
            .json(&json!({"model": "test-model", "training_file": "file-123"}))
            .await;
        assert_eq!(response.status_code(), 200);

        // Test DELETE request
        let response = server
            .delete("/v1/models/test-model")
            .add_header("model-override", "test-model")
            .await;
        assert_eq!(response.status_code(), 200);

        // Test PATCH request
        let response = server
            .patch("/v1/fine-tuning/jobs/job-123")
            .add_header("model-override", "test-model")
            .json(&json!({"model": "test-model", "status": "cancelled"}))
            .await;
        assert_eq!(response.status_code(), 200);

        // Verify all requests were forwarded with correct methods
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 4);

        assert_eq!(requests[0].method, "GET");
        assert!(requests[0].uri.contains("/v1/files"));

        assert_eq!(requests[1].method, "PUT");
        assert!(requests[1].uri.contains("/v1/fine-tuning/jobs"));

        assert_eq!(requests[2].method, "DELETE");
        assert!(requests[2].uri.contains("/v1/models/test-model"));

        assert_eq!(requests[3].method, "PATCH");
        assert!(requests[3].uri.contains("/v1/fine-tuning/jobs/job-123"));

        // All should have the same authorization header
        for request in &requests {
            let auth_header = request
                .headers
                .iter()
                .find(|(key, _)| key == "authorization")
                .map(|(_, value)| value);
            assert_eq!(auth_header, Some(&"Bearer method-key".to_string()));
        }
    }

    #[tokio::test]
    async fn test_query_parameters_preserved() {
        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "query-model".to_string(),
            Target::builder()
                .url("https://api.query.com".parse().unwrap())
                .onwards_key("query-key".to_string())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"query": "preserved"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Test request with various query parameters
        let response = server
            .get("/v1/files?limit=10&after=file-123&purpose=fine-tune")
            .add_header("model-override", "query-model")
            .await;

        assert_eq!(response.status_code(), 200);

        // Verify query parameters were preserved in the forwarded request
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let request = &requests[0];
        assert!(request.uri.contains("/v1/files"));
        assert!(request.uri.contains("limit=10"));
        assert!(request.uri.contains("after=file-123"));
        assert!(request.uri.contains("purpose=fine-tune"));

        // The full URI should be correctly constructed
        assert!(
            request.uri.contains(
                "https://api.query.com/v1/files?limit=10&after=file-123&purpose=fine-tune"
            )
        );
    }

    #[tokio::test]
    async fn test_different_endpoints_forwarded() {
        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "endpoint-model".to_string(),
            Target::builder()
                .url("https://api.endpoints.com".parse().unwrap())
                .onwards_key("endpoint-key".to_string())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"endpoint": "success"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Test different OpenAI API endpoints
        let endpoints = vec![
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/embeddings",
            "/v1/audio/transcriptions",
            "/v1/images/generations",
            "/v1/fine-tuning/jobs",
        ];

        for endpoint in &endpoints {
            let response = server
                .post(endpoint)
                .add_header("model-override", "endpoint-model")
                .json(&json!({"model": "endpoint-model", "input": "test"}))
                .await;

            assert_eq!(
                response.status_code(),
                200,
                "Failed for endpoint: {endpoint}"
            );
        }

        // Verify all endpoints were forwarded correctly
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 6);

        for (i, endpoint) in endpoints.iter().enumerate() {
            assert!(
                requests[i].uri.contains(endpoint),
                "Request {} should contain endpoint {}, but URI was: {}",
                i,
                endpoint,
                requests[i].uri
            );
            assert!(requests[i].uri.contains("api.endpoints.com"));
        }
    }

    #[tokio::test]
    async fn test_custom_headers_forwarded() {
        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "header-test-model".to_string(),
            Target::builder()
                .url("https://api.headers.com".parse().unwrap())
                .onwards_key("header-test-key".to_string())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"headers": "forwarded"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Make request with various custom headers that should be forwarded
        let response = server
            .post("/v1/chat/completions")
            .add_header("model-override", "header-test-model")
            .add_header("x-custom-header", "custom-value")
            .add_header("x-request-id", "req-12345")
            .add_header("user-agent", "test-client/1.0")
            .add_header("accept-encoding", "gzip, deflate")
            .json(&json!({
                "model": "header-test-model",
                "messages": [{"role": "user", "content": "Test headers"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Verify custom headers were forwarded (but not the model-override header)
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let request = &requests[0];

        // Check that custom headers were forwarded
        let custom_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "x-custom-header")
            .map(|(_, value)| value);
        assert_eq!(custom_header, Some(&"custom-value".to_string()));

        let request_id_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "x-request-id")
            .map(|(_, value)| value);
        assert_eq!(request_id_header, Some(&"req-12345".to_string()));

        let user_agent_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "user-agent")
            .map(|(_, value)| value);
        assert_eq!(user_agent_header, Some(&"test-client/1.0".to_string()));

        let accept_encoding_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "accept-encoding")
            .map(|(_, value)| value);
        assert_eq!(accept_encoding_header, Some(&"gzip, deflate".to_string()));

        // Verify that the model-override header WAS forwarded (gateway is transparent)
        let model_override_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "model-override")
            .map(|(_, value)| value);
        assert_eq!(
            model_override_header,
            Some(&"header-test-model".to_string())
        );

        // Note: onwards-model header is also forwarded if present (the test doesn't include it)

        // Verify authorization header was set correctly
        let auth_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "authorization")
            .map(|(_, value)| value);
        assert_eq!(auth_header, Some(&"Bearer header-test-key".to_string()));
    }

    #[tokio::test]
    async fn test_targets_without_api_keys_no_auth_header() {
        // Create a target without an API key
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "no-auth-model".to_string(),
            Target::builder()
                .url("https://api.noauth.com".parse().unwrap())
                .build(), // No API key
        );

        let targets = Targets {
            targets: targets_map,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"no_auth": "success"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Make request to target without API key
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "no-auth-model",
                "messages": [{"role": "user", "content": "Test no auth"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Verify request was forwarded but NO authorization header was added
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let request = &requests[0];

        // Check that NO authorization header was set
        let auth_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "authorization")
            .map(|(_, value)| value);
        assert_eq!(auth_header, None);

        // Verify request went to correct target
        assert!(request.uri.contains("api.noauth.com"));
        assert!(request.uri.contains("/v1/chat/completions"));

        // Verify request body was forwarded correctly
        let forwarded_body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(forwarded_body["model"], "no-auth-model");
        assert_eq!(forwarded_body["messages"][0]["content"], "Test no auth");
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
        let router = build_router(app_state).await;
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
    async fn test_models_endpoint_not_forwarded_to_targets() {
        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            Target::builder()
                .url("https://api.shouldnotreceive.com".parse().unwrap())
                .onwards_key("should-not-be-used".to_string())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"should": "not_be_called"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Test various ways to call the models endpoint
        let get_response = server.get("/v1/models").await;
        assert_eq!(get_response.status_code(), 200);

        // Even with headers that would normally trigger forwarding, /v1/models should be intercepted
        let header_response = server
            .get("/v1/models")
            .add_header("model-override", "test-model")
            .await;
        assert_eq!(header_response.status_code(), 200);

        // Even with query parameters
        let query_response = server.get("/v1/models?limit=10").await;
        assert_eq!(query_response.status_code(), 200);

        // Verify that ALL responses have the same structure (handled locally)
        for response in [&get_response, &header_response, &query_response] {
            let body: serde_json::Value = response.json();
            assert_eq!(body["object"], "list");
            assert!(body["data"].is_array());
        }

        // MOST IMPORTANTLY: verify that NO requests were forwarded to any targets
        let requests = mock_client.get_requests();
        assert_eq!(
            requests.len(),
            0,
            "Models endpoint should never forward to targets"
        );
    }

    #[tokio::test]
    async fn test_error_responses_pass_through_unchanged() {
        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "error-model".to_string(),
            Target::builder()
                .url("https://api.errors.com".parse().unwrap())
                .onwards_key("error-key".to_string())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };

        // Test different error status codes
        let error_codes = vec![
            (
                StatusCode::BAD_REQUEST,
                r#"{"error": {"type": "invalid_request_error", "message": "Bad request"}}"#,
            ),
            (
                StatusCode::UNAUTHORIZED,
                r#"{"error": {"type": "authentication_error", "message": "Invalid API key"}}"#,
            ),
            (
                StatusCode::FORBIDDEN,
                r#"{"error": {"type": "permission_error", "message": "Forbidden"}}"#,
            ),
            (
                StatusCode::NOT_FOUND,
                r#"{"error": {"type": "not_found_error", "message": "Model not found"}}"#,
            ),
            (
                StatusCode::TOO_MANY_REQUESTS,
                r#"{"error": {"type": "rate_limit_error", "message": "Rate limit exceeded"}}"#,
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error": {"type": "server_error", "message": "Internal server error"}}"#,
            ),
            (
                StatusCode::BAD_GATEWAY,
                r#"{"error": {"type": "bad_gateway", "message": "Bad gateway"}}"#,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                r#"{"error": {"type": "service_unavailable", "message": "Service unavailable"}}"#,
            ),
        ];

        for (error_code, error_body) in error_codes {
            // Create a new mock client for each error code test
            let mock_client = MockHttpClient::new(error_code, error_body);
            let app_state = AppState::with_client(targets.clone(), mock_client.clone());
            let router = build_router(app_state).await;
            let server = TestServer::new(router).unwrap();

            // Make a request that should return the error
            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "error-model",
                    "messages": [{"role": "user", "content": "This should error"}]
                }))
                .await;

            // Verify the error status code is passed through unchanged
            assert_eq!(
                response.status_code(),
                error_code,
                "Error code {error_code} should be passed through"
            );

            // Verify the error body is passed through unchanged
            let response_body = response.text();
            assert_eq!(
                response_body, error_body,
                "Error body for {error_code} should be passed through unchanged"
            );

            // Verify the request was actually forwarded
            let requests = mock_client.get_requests();
            assert_eq!(
                requests.len(),
                1,
                "Request should be forwarded even when target returns error {error_code}"
            );

            let request = &requests[0];
            assert!(request.uri.contains("api.errors.com"));
            assert!(request.uri.contains("/v1/chat/completions"));
        }
    }

    #[tokio::test]
    async fn test_streaming_responses_pass_through() {
        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "streaming-model".to_string(),
            Target::builder()
                .url("https://api.streaming.com".parse().unwrap())
                .onwards_key("streaming-key".to_string())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };

        // Create mock streaming response chunks (simulating OpenAI streaming format)
        let streaming_chunks = vec![
            "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1694268190,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1694268190,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1694268190,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there!\"},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1694268190,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ];

        let mock_client = MockHttpClient::new_streaming(StatusCode::OK, streaming_chunks.clone());
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Make a streaming request
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "streaming-model",
                "messages": [{"role": "user", "content": "Hello"}],
                "stream": true
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Verify streaming headers are passed through
        assert_eq!(response.header("content-type"), "text/event-stream");
        assert_eq!(response.header("cache-control"), "no-cache");
        assert_eq!(response.header("connection"), "keep-alive");

        // Verify the streaming content is passed through correctly
        let response_body = response.text();
        let expected_body = streaming_chunks.join("");
        assert_eq!(response_body, expected_body);

        // Verify the request was forwarded correctly
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let request = &requests[0];
        assert!(request.uri.contains("api.streaming.com"));
        assert!(request.uri.contains("/v1/chat/completions"));

        // Verify the request body contains stream=true
        let forwarded_body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(forwarded_body["model"], "streaming-model");
        assert_eq!(forwarded_body["stream"], true);
        assert_eq!(forwarded_body["messages"][0]["content"], "Hello");

        // Verify authorization header
        let auth_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "authorization")
            .map(|(_, value)| value);
        assert_eq!(auth_header, Some(&"Bearer streaming-key".to_string()));
    }

    #[tokio::test]
    async fn test_bearer_token_validation_with_valid_token() {
        let targets_map = Arc::new(DashMap::new());
        let mut keys = std::collections::HashSet::new();
        keys.insert(crate::auth::ConstantTimeString::from(
            "valid-token".to_string(),
        ));

        targets_map.insert(
            "secure-model".to_string(),
            Target::builder()
                .url("https://api.secure.com".parse().unwrap())
                .onwards_key("secure-key".to_string())
                .keys(keys)
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };
        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"response": "success"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Make request with valid Bearer token
        let response = server
            .post("/v1/chat/completions")
            .add_header("authorization", "Bearer valid-token")
            .json(&json!({
                "model": "secure-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Verify request was forwarded
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);
    }

    #[tokio::test]
    async fn test_bearer_token_validation_with_invalid_token() {
        let targets_map = Arc::new(DashMap::new());
        let mut keys = std::collections::HashSet::new();
        keys.insert(crate::auth::ConstantTimeString::from(
            "valid-token".to_string(),
        ));

        targets_map.insert(
            "secure-model".to_string(),
            Target::builder()
                .url("https://api.secure.com".parse().unwrap())
                .onwards_key("secure-key".to_string())
                .keys(keys)
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };
        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"response": "success"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Make request with invalid Bearer token
        let response = server
            .post("/v1/chat/completions")
            .add_header("authorization", "Bearer invalid-token")
            .json(&json!({
                "model": "secure-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 401);

        // Verify no request was forwarded
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 0);
    }

    #[tokio::test]
    async fn test_bearer_token_validation_missing_token() {
        let targets_map = Arc::new(DashMap::new());
        let mut keys = std::collections::HashSet::new();
        keys.insert(crate::auth::ConstantTimeString::from(
            "valid-token".to_string(),
        ));

        targets_map.insert(
            "secure-model".to_string(),
            Target::builder()
                .url("https://api.secure.com".parse().unwrap())
                .onwards_key("secure-key".to_string())
                .keys(keys)
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };
        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"response": "success"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Make request without Bearer token
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "secure-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 401);

        // Verify no request was forwarded
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 0);
    }

    #[tokio::test]
    async fn test_bearer_token_validation_malformed_header() {
        let targets_map = Arc::new(DashMap::new());
        let mut keys = std::collections::HashSet::new();
        keys.insert(crate::auth::ConstantTimeString::from(
            "valid-token".to_string(),
        ));

        targets_map.insert(
            "secure-model".to_string(),
            Target::builder()
                .url("https://api.secure.com".parse().unwrap())
                .onwards_key("secure-key".to_string())
                .keys(keys)
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };
        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"response": "success"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Make request with malformed authorization header
        let response = server
            .post("/v1/chat/completions")
            .add_header("authorization", "Basic valid-token")
            .json(&json!({
                "model": "secure-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 401);

        // Verify no request was forwarded
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 0);
    }

    #[tokio::test]
    async fn test_bearer_token_validation_skipped_for_targets_without_keys() {
        let targets_map = Arc::new(DashMap::new());

        targets_map.insert(
            "open-model".to_string(),
            Target::builder()
                .url("https://api.open.com".parse().unwrap())
                .onwards_key("open-key".to_string())
                .build(),
        );

        let targets = Targets {
            targets: targets_map,
        };
        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"response": "success"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state).await;
        let server = TestServer::new(router).unwrap();

        // Make request without Bearer token to target without keys
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "open-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Verify request was forwarded
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);
    }
}
