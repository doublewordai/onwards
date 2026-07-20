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

pub mod handlers;
pub mod schemas;

use crate::AppState;
use crate::client::HttpClient;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use tracing::info;

pub use schemas::chat_completions::{ChatCompletionRequest, ChatCompletionResponse};

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
/// - `POST /v1/completions` - Legacy text completions (proxied to upstream /v1/completions)
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
        // Chat completions
        .route(
            "/chat/completions",
            post(handlers::chat_completions_handler::<T>),
        )
        // Anthropic Messages ingress alias. Foreign-protocol translation happens
        // at the dwctl edge (the request body is already Chat Completions and the
        // path is normalised to `/chat/completions` by the time it reaches here);
        // this alias only exists so strict-mode routing matches `/messages` and
        // dispatches to the chat-completions handler. No Anthropic logic lives in
        // onwards. Non-strict mode needs no alias (its catch-all already matches).
        .route("/messages", post(handlers::chat_completions_handler::<T>))
        // Legacy text completions
        .route("/completions", post(handlers::completions_handler::<T>))
        // Open Responses ingress alias. The Responses->Chat Completions
        // translation happens at the dwctl edge (the request body is already
        // Chat Completions and the path is normalised to `/chat/completions`
        // by the time it reaches here); this alias only exists so strict-mode
        // routing matches `/responses` and dispatches to the chat-completions
        // handler. No Responses logic lives in onwards. Mirrors `/messages`.
        .route("/responses", post(handlers::chat_completions_handler::<T>))
        // Embeddings
        .route("/embeddings", post(handlers::embeddings_handler::<T>))
        // Without this layer the `Json` extractors above fall back to Axum's
        // 2 MB default and reject larger payloads with a 413.
        .layer(DefaultBodyLimit::max(state.body_limit))
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
            key_labels: Arc::new(DashMap::new()),
            strict_mode: true,
            http_pool_config: None,
        };

        let mock_response = r#"{"id":"chatcmpl-123","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"Hello!"}}]}"#;
        AppState::with_client(targets, MockHttpClient::new(StatusCode::OK, mock_response))
    }

    #[tokio::test]
    async fn test_strict_router_rejects_unknown_paths() {
        let state = create_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .uri("/unknown/endpoint")
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
            .uri("/models")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Build a valid chat completions body padded to at least `size` bytes.
    fn padded_chat_completions_body(size: usize) -> String {
        let padding = "x".repeat(size);
        format!(r#"{{"model": "gpt-4", "messages": [{{"role": "user", "content": "{padding}"}}]}}"#)
    }

    #[tokio::test]
    async fn test_strict_router_accepts_body_over_axum_2mb_default() {
        // Regression test: without an explicit DefaultBodyLimit layer the Json
        // extractors fall back to Axum's 2 MB default and 413 anything larger.
        let state = create_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(padded_chat_completions_body(3 * 1024 * 1024)))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_strict_router_messages_alias_routes_to_chat_completions() {
        // The Anthropic ingress alias: a (already edge-translated) Chat
        // Completions body posted to `/messages` must route to the
        // chat-completions handler, exactly like `/chat/completions`. Foreign
        // translation and path normalisation happen upstream at the dwctl edge;
        // onwards just needs the route to match.
        let state = create_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/messages")
            .header("content-type", "application/json")
            .body(Body::from(padded_chat_completions_body(16)))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_strict_router_responses_alias_routes_to_chat_completions() {
        // `/responses` is an unconditional alias to the chat-completions handler:
        // the Responses->Chat translation happens upstream at the dwctl edge (the
        // body posted here is already Chat Completions), so onwards just matches
        // the route, exactly like `/messages`.
        let state = create_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/responses")
            .header("content-type", "application/json")
            .body(Body::from(padded_chat_completions_body(16)))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_strict_router_accepts_base64_image_over_axum_2mb_default() {
        // The prod failure mode for COR-440: a vision request whose base64
        // data URL pushes the body past Axum's old 2 MB extractor default.
        // Also exercises the image_url content-part schema with a large URL.
        let state = create_test_app_state();
        let router = build_strict_router(state);

        // ~3 MB of valid base64 (must be a multiple of 4 chars).
        let base64_data = "QUJD".repeat(3 * 1024 * 1024 / 4);
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "What is in this image?"},
                    {"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{base64_data}")}}
                ]
            }]
        });
        assert!(body.to_string().len() > 2 * 1024 * 1024);

        let request = Request::builder()
            .method("POST")
            .uri("/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_strict_router_rejects_body_over_configured_limit() {
        let state = create_test_app_state().with_body_limit(1024);
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(padded_chat_completions_body(2048)))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    fn create_completions_test_app_state() -> (AppState<MockHttpClient>, MockHttpClient) {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-3.5-turbo-instruct".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: true,
            http_pool_config: None,
        };

        let mock_response = r#"{
            "id": "cmpl-abc123",
            "object": "text_completion",
            "created": 1677652288,
            "model": "gpt-3.5-turbo-instruct",
            "choices": [{"text": "Hello!", "index": 0, "logprobs": null, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12}
        }"#;
        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        (
            AppState::with_client(targets, mock_client.clone()),
            mock_client,
        )
    }

    /// The strict router accepts POST /completions with a valid request body
    #[tokio::test]
    async fn test_strict_router_accepts_completions_endpoint() {
        let (state, _) = create_completions_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Say hello"}"#,
            ))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Response is in legacy completions format
        assert_eq!(body_json["object"], "text_completion");
        assert!(body_json["choices"].is_array());
        assert!(body_json["choices"][0]["text"].is_string());
    }

    #[tokio::test]
    async fn test_strict_router_rejects_reasoning_on_completions_endpoint() {
        let (state, mock_client) = create_completions_test_app_state();
        let router = build_strict_router(state);
        let request = Request::builder()
            .method("POST")
            .uri("/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Say hello","reasoning_effort":"low"}"#,
            ))
            .unwrap();

        let response = router.clone().oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["param"], "reasoning_effort");
        assert_eq!(body["error"]["code"], "unsupported_parameter");

        let request = Request::builder()
            .method("POST")
            .uri("/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Say hello","thinking":false}"#,
            ))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["param"], "thinking");
        assert_eq!(body["error"]["code"], "unsupported_parameter");

        let request = Request::builder()
            .method("POST")
            .uri("/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Say hello","thinking_token_budget":1024}"#,
            ))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["param"], "thinking_token_budget");
        assert_eq!(body["error"]["code"], "unsupported_parameter");
        assert!(mock_client.get_requests().is_empty());
    }

    /// The strict router forwards to the upstream /completions endpoint (not /chat/completions)
    #[tokio::test]
    async fn test_completions_proxied_to_upstream_completions() {
        let (state, mock_client) = create_completions_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Hello"}"#,
            ))
            .unwrap();

        router.oneshot(request).await.unwrap();

        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].uri.contains("completions"),
            "Should proxy to /completions, got: {}",
            requests[0].uri
        );
        assert!(
            !requests[0].uri.contains("chat"),
            "Must NOT proxy to /chat/completions"
        );
    }

    /// The strict router accepts POST /completions without a prompt — prompt is optional per the
    /// OpenAI spec (defaults to `<|endoftext|>` server-side)
    #[tokio::test]
    async fn test_completions_accepts_missing_prompt() {
        let (state, _) = create_completions_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/completions")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"gpt-3.5-turbo-instruct"}"#))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Streaming completions: SSE chunks contain text_completion objects
    #[tokio::test]
    async fn test_completions_streaming_response_format() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-3.5-turbo-instruct".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .build()
                .into_pool(),
        );
        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: true,
            http_pool_config: None,
        };

        let chunks = vec![
            "data: {\"id\":\"cmpl-abc\",\"object\":\"text_completion\",\"created\":1677652288,\"model\":\"gpt-3.5-turbo-instruct\",\"choices\":[{\"text\":\"Hello\",\"index\":0,\"logprobs\":null,\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"cmpl-abc\",\"object\":\"text_completion\",\"created\":1677652288,\"model\":\"gpt-3.5-turbo-instruct\",\"choices\":[{\"text\":\" world\",\"index\":0,\"logprobs\":null,\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"cmpl-abc\",\"object\":\"text_completion\",\"created\":1677652288,\"model\":\"gpt-3.5-turbo-instruct\",\"choices\":[{\"text\":\"\",\"index\":0,\"logprobs\":null,\"finish_reason\":\"stop\"}]}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ];
        let mock_client = MockHttpClient::new_streaming(StatusCode::OK, chunks);
        let state = AppState::with_client(targets, mock_client);
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Hello","stream":true}"#,
            ))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(content_type.contains("text/event-stream"));

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = std::str::from_utf8(&body).unwrap();

        // Each SSE chunk is a text_completion object
        assert!(body_str.contains("\"object\":\"text_completion\""));
        assert!(body_str.contains("\"text\":\"Hello\""));
        assert!(body_str.contains("\"text\":\" world\""));
        assert!(body_str.contains("[DONE]"));
    }
}
