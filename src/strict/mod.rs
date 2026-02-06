//! Strict mode router with typed handlers and schema validation
//!
//! This module provides an alternative router that validates requests against
//! OpenAI API schemas before forwarding them. Unlike the default passthrough
//! router, strict mode:
//!
//! - Only accepts known OpenAI API paths
//! - Validates request bodies against typed schemas via serde
//! - Rejects unknown paths with 404
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

pub mod handlers;
pub mod schemas;

use crate::AppState;
use crate::client::HttpClient;
use axum::Router;
use axum::routing::{get, post};
use tracing::info;

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
/// - `POST /v1/responses` - Open Responses API (validated, passthrough)
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
    use crate::target::{Target, Targets};
    use crate::test_utils::MockHttpClient;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use dashmap::DashMap;
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
    async fn test_passthrough_mode_for_responses() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
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

        // In passthrough mode, request should go to /v1/responses
        assert!(requests[0].uri.contains("/responses"));
    }
}
