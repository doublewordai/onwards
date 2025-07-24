//! Onwards - A flexible LLM proxy library
//! 
//! This library provides the core functionality for proxying requests to various LLM endpoints
//! with support for authentication, model routing, and dynamic configuration.

use axum::Router;
use axum::routing::{any, get};

pub mod auth;
pub mod client;
pub mod handlers;
pub mod models;
pub mod target;

use client::{HttpClient, HyperClient};
use handlers::{models as models_handler, target_message_handler};

/// The main application state containing the HTTP client and targets configuration
#[derive(Clone, Debug)]
pub struct AppState<T: HttpClient> {
    pub http_client: T,
    pub targets: target::Targets,
}

impl AppState<HyperClient> {
    /// Create a new AppState with the default Hyper client
    pub fn new(targets: target::Targets) -> Self {
        let http_client = client::create_hyper_client();
        Self {
            http_client,
            targets,
        }
    }
}

impl<T: HttpClient> AppState<T> {
    /// Create a new AppState with a custom HTTP client (useful for testing)
    #[cfg(test)]
    pub fn with_client(targets: target::Targets, http_client: T) -> Self {
        Self {
            http_client,
            targets,
        }
    }
}

/// Build the main router for the proxy
/// 
/// This creates routes for:
/// - `/v1/models` - Returns available models
/// - `/{*path}` - Forwards all other requests to the appropriate target
pub async fn build_router<T: HttpClient + Clone + Send + Sync + 'static>(
    state: AppState<T>,
) -> Router {
    Router::new()
        .route("/v1/models", get(models_handler))
        .route("/{*path}", any(target_message_handler))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{Target, Targets};
    use async_trait::async_trait;
    use axum::http::StatusCode;
    use axum_test::TestServer;
    use dashmap::DashMap;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

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
        let targets = target::Targets {
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
}