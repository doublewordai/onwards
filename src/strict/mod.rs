//! Strict mode router with typed handlers and schema validation
//!
//! This module provides an alternative router that validates requests against
//! OpenAI API schemas before forwarding them. Unlike the default passthrough
//! router, strict mode:
//!
//! - Only accepts known OpenAI API paths
//! - Validates request bodies against typed schemas via serde
//! - Rejects unknown paths with 404
//! - Supports the Open Responses adapter for backends that only support Chat Completions
//!
//! # Usage
//!
//! ```ignore
//! use onwards::strict::build_strict_router;
//! use onwards::AppState;
//!
//! let app_state = AppState::new(targets);
//! let router = build_strict_router(app_state);
//! ```

pub mod adapter;
pub mod handlers;
pub mod schemas;
pub mod streaming;

use crate::AppState;
use crate::client::HttpClient;
use axum::Router;
use axum::routing::{get, post};
use tracing::info;

pub use adapter::OpenResponsesAdapter;
pub use schemas::chat_completions::{ChatCompletionRequest, ChatCompletionResponse};
pub use schemas::responses::{ResponsesRequest, ResponsesResponse};

/// Build a strict router with typed handlers and schema validation.
///
/// Unlike `build_router()`, this router:
/// - Only accepts known OpenAI API paths
/// - Validates request bodies against typed schemas
/// - Returns 404 for unknown paths (no wildcard)
///
/// # Routes
///
/// - `POST /v1/chat/completions` - Chat completions with schema validation
/// - `POST /v1/responses` - Open Responses API (validated, optional adapter)
/// - `POST /v1/embeddings` - Embeddings API with schema validation
/// - `GET /v1/models` - List available models
/// - `GET /models` - List available models (alias)
///
/// # Example
///
/// ```ignore
/// use onwards::{AppState, target::Targets};
/// use onwards::strict::build_strict_router;
///
/// let targets = Targets::from_config_file(&"config.json".into()).await?;
/// let app_state = AppState::new(targets);
/// let router = build_strict_router(app_state);
/// ```
pub fn build_strict_router<T: HttpClient + Clone + Send + Sync + 'static>(
    state: AppState<T>,
) -> Router {
    info!("Building strict router with schema validation");

    Router::new()
        // Models endpoints
        .route("/models", get(handlers::models_handler::<T>))
        .route("/v1/models", get(handlers::models_handler::<T>))
        // Chat completions
        .route(
            "/v1/chat/completions",
            post(handlers::chat_completions_handler::<T>),
        )
        // Open Responses
        .route("/v1/responses", post(handlers::responses_handler::<T>))
        // Embeddings
        .route("/v1/embeddings", post(handlers::embeddings_handler::<T>))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{OpenResponsesConfig, Target, Targets};
    use crate::test_utils::MockHttpClient;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use dashmap::DashMap;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn create_test_app_state() -> AppState<MockHttpClient> {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        let mock_response = r#"{"id":"chatcmpl-123","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"Hello!"}}]}"#;
        AppState::with_client(targets, MockHttpClient::new(StatusCode::OK, mock_response))
    }

    /// Create test app state with adapter mode enabled
    fn create_adapter_test_app_state() -> (AppState<MockHttpClient>, MockHttpClient) {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .open_responses(OpenResponsesConfig { adapter: true })
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        // Mock response that simulates a Chat Completions response
        let mock_response = r#"{
            "id": "chatcmpl-abc123",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello! How can I help you today?"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 8,
                "total_tokens": 18
            }
        }"#;
        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        (
            AppState::with_client(targets, mock_client.clone()),
            mock_client,
        )
    }

    #[tokio::test]
    async fn test_strict_router_rejects_unknown_paths() {
        let state = create_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .uri("/v1/unknown/endpoint")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_strict_router_accepts_models_endpoint() {
        let state = create_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_adapter_converts_responses_to_chat_completions() {
        let (state, mock_client) = create_adapter_test_app_state();
        let router = build_strict_router(state);

        // Send a Responses API request
        let request_body = r#"{
            "model": "gpt-4o",
            "input": "Hello, how are you?"
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();

        // Check that we got a successful response
        assert_eq!(response.status(), StatusCode::OK);

        // Parse the response body
        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let response_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        // Verify the response is in Open Responses format
        assert_eq!(response_json["object"], "response");
        assert!(response_json["id"].as_str().unwrap().starts_with("resp_"));
        assert_eq!(response_json["status"], "completed");

        // Verify output items exist
        let output = response_json["output"].as_array().unwrap();
        assert!(!output.is_empty());

        // Verify the mock client received a Chat Completions request
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        // Check the request was sent to chat completions endpoint
        assert!(requests[0].uri.contains("chat/completions"));

        // Parse the request body to verify conversion
        let request_json: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(request_json["model"], "gpt-4o");

        // Verify messages array was created from input
        let messages = request_json["messages"].as_array().unwrap();
        assert!(!messages.is_empty());
        assert_eq!(messages[0]["role"], "user");
    }

    #[tokio::test]
    async fn test_adapter_with_instructions() {
        let (state, mock_client) = create_adapter_test_app_state();
        let router = build_strict_router(state);

        // Send a Responses API request with instructions
        let request_body = r#"{
            "model": "gpt-4o",
            "input": "What's 2 + 2?",
            "instructions": "You are a helpful math tutor."
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Verify the mock client received the request
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        // Parse the request body to verify system message was added
        let request_json: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let messages = request_json["messages"].as_array().unwrap();

        // Should have system message first, then user message
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are a helpful math tutor.");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "What's 2 + 2?");
    }

    #[tokio::test]
    async fn test_passthrough_mode_without_adapter() {
        // Create a target WITHOUT adapter enabled
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                // No open_responses config - defaults to passthrough
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        // Mock response in Responses format (as if upstream supports it)
        let mock_response = r#"{
            "id": "resp_abc123",
            "object": "response",
            "status": "completed",
            "output": []
        }"#;
        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        let state = AppState::with_client(targets, mock_client.clone());
        let router = build_strict_router(state);

        // Send a Responses API request
        let request_body = r#"{
            "model": "gpt-4o",
            "input": "Hello"
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Verify the mock client received the request
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        // In passthrough mode, request should go to /v1/responses (not chat/completions)
        assert!(requests[0].uri.contains("/responses"));
    }

    #[tokio::test]
    async fn test_streaming_adapter_mode() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .open_responses(OpenResponsesConfig { adapter: true })
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        // Mock streaming SSE response
        let chunks = vec![
            "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there!\"},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ];
        let mock_client = MockHttpClient::new_streaming(StatusCode::OK, chunks);
        let state = AppState::with_client(targets, mock_client.clone());
        let router = build_strict_router(state);

        // Send a streaming Responses API request
        let request_body = r#"{
            "model": "gpt-4o",
            "input": "Hello",
            "stream": true
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();

        // Check status
        assert_eq!(response.status(), StatusCode::OK);

        // Check content type is SSE
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(content_type.contains("text/event-stream"));

        // Read the streaming response
        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body_text = std::str::from_utf8(&body_bytes).unwrap();

        // Verify we got Open Responses semantic events
        assert!(body_text.contains("response.created"));
        assert!(body_text.contains("response.output_item.added"));
        assert!(body_text.contains("response.output_text.delta"));
        assert!(body_text.contains("response.completed"));

        // Verify the mock client received the request
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        // Check the request was sent to chat completions with stream: true
        assert!(requests[0].uri.contains("chat/completions"));
        let request_json: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(request_json["stream"], true);
    }

    /// Test streaming with tool_calls finish reason.
    ///
    /// With the default NoOpToolExecutor, all tools are unhandled, so the
    /// handler stops after one iteration (correct behavior — unhandled tools
    /// are passed through to the client). Multi-iteration tool loop mechanics
    /// are exercised by the streaming::tests unit tests.
    #[tokio::test]
    async fn test_streaming_adapter_tool_calls_unhandled() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .open_responses(OpenResponsesConfig { adapter: true })
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        // Streaming response that finishes with tool_calls
        let chunks = vec![
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_xyz\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"location\\\":\\\"Paris\\\"}\"}}]},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ];

        let mock_client = MockHttpClient::new_streaming(StatusCode::OK, chunks);
        let state = AppState::with_client(targets, mock_client.clone());
        let router = build_strict_router(state);

        let request_body = r#"{
            "model": "gpt-4o",
            "input": "What's the weather in Paris?",
            "stream": true,
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "parameters": {"type": "object", "properties": {"location": {"type": "string"}}}
            }]
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(content_type.contains("text/event-stream"));

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body_text = std::str::from_utf8(&body_bytes).unwrap();

        // With NoOpToolExecutor, tools are unhandled → single iteration, then complete
        assert!(body_text.contains("response.created"));
        assert!(body_text.contains("response.output_item.added"));
        assert!(body_text.contains("response.completed"));

        // Only one upstream request — no second iteration because tools are unhandled
        let requests = mock_client.get_requests();
        assert_eq!(
            requests.len(),
            1,
            "Should make only 1 request (tools are unhandled by NoOpToolExecutor)"
        );
        assert!(requests[0].uri.contains("chat/completions"));

        // Verify stream_options.include_usage was set
        let request_json: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(request_json["stream"], true);
        assert_eq!(
            request_json["stream_options"]["include_usage"], true,
            "Should set include_usage for streaming"
        );
    }
}
