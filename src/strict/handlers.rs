//! Typed request handlers for strict mode
//!
//! These handlers validate requests using Axum's Json extractor (which uses serde)
//! before forwarding to the upstream provider.
//!
//! For responses, strict mode provides security by:
//! - Deserializing third-party responses through our strict schemas (drops extra fields)
//! - Rewriting model field to match the originally requested model
//! - Re-serializing with only our defined fields (prevents info leakage)
//! - Sanitizing error responses to prevent third-party info leakage

use super::schemas::chat_completions::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
};
use super::schemas::embeddings::{EmbeddingsRequest, EmbeddingsResponse};
use super::schemas::responses::ResponsesRequest;
use crate::AppState;
use crate::client::HttpClient;
use crate::handlers::target_message_handler;
use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::json;
use tracing::{debug, error, warn};

/// Handler for GET /v1/models
///
/// Returns the list of available models from the configured targets.
pub async fn models_handler<T: HttpClient + Clone + Send + Sync + 'static>(
    State(state): State<AppState<T>>,
    req: Request<Body>,
) -> impl IntoResponse {
    crate::handlers::models(State(state), req).await
}

/// Handler for POST /v1/chat/completions
///
/// Validates the request against the Chat Completions schema, then forwards
/// to the upstream provider via the standard passthrough handler.
pub async fn chat_completions_handler<T: HttpClient + Clone + Send + Sync + 'static>(
    State(state): State<AppState<T>>,
    headers: HeaderMap,
    Json(request): Json<ChatCompletionRequest>,
) -> Response {
    let original_model = request.model.clone();
    let is_streaming = request.stream.unwrap_or(false);

    debug!(
        model = %original_model,
        messages_count = request.messages.len(),
        stream = is_streaming,
        "Chat completions request validated"
    );

    // Re-serialize the validated request and forward it
    let body_bytes = match serde_json::to_vec(&request) {
        Ok(bytes) => bytes,
        Err(e) => {
            error!(error = %e, "Failed to serialize chat completions request");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Failed to process request",
            );
        }
    };

    let response = forward_request(state, headers, "/v1/chat/completions", body_bytes).await;

    // Sanitize response to ensure model field matches and extra fields are dropped
    if response.status().is_success() {
        if is_streaming {
            sanitize_streaming_chat_response(response, original_model).await
        } else {
            sanitize_chat_response(response, original_model).await
        }
    } else {
        // Sanitize error responses to prevent third-party info leakage
        sanitize_error_response(response).await
    }
}

/// Handler for POST /v1/responses
///
/// Validates the request against the Open Responses schema, then forwards
/// to the upstream provider as-is.
pub async fn responses_handler<T: HttpClient + Clone + Send + Sync + 'static>(
    State(state): State<AppState<T>>,
    headers: HeaderMap,
    Json(request): Json<ResponsesRequest>,
) -> Response {
    debug!(
        model = %request.model,
        has_previous_response_id = request.previous_response_id.is_some(),
        stream = ?request.stream,
        "Responses request validated"
    );

    // Re-serialize the validated request and forward it
    let body_bytes = match serde_json::to_vec(&request) {
        Ok(bytes) => bytes,
        Err(e) => {
            error!(error = %e, "Failed to serialize responses request");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Failed to process request",
            );
        }
    };

    forward_request(state, headers, "/v1/responses", body_bytes).await
}

/// Handler for POST /v1/embeddings
///
/// Validates the request against the Embeddings schema, then forwards
/// to the upstream provider via the standard passthrough handler.
pub async fn embeddings_handler<T: HttpClient + Clone + Send + Sync + 'static>(
    State(state): State<AppState<T>>,
    headers: HeaderMap,
    Json(request): Json<EmbeddingsRequest>,
) -> Response {
    let original_model = request.model.clone();

    debug!(
        model = %original_model,
        "Embeddings request validated"
    );

    // Re-serialize the validated request and forward it
    let body_bytes = match serde_json::to_vec(&request) {
        Ok(bytes) => bytes,
        Err(e) => {
            error!(error = %e, "Failed to serialize embeddings request");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Failed to process request",
            );
        }
    };

    let response = forward_request(state, headers, "/v1/embeddings", body_bytes).await;

    // Sanitize response
    if response.status().is_success() {
        sanitize_embeddings_response(response, original_model).await
    } else {
        // Sanitize error responses to prevent third-party info leakage
        sanitize_error_response(response).await
    }
}

/// Forward a validated request to the upstream provider
async fn forward_request<T: HttpClient + Clone + Send + Sync + 'static>(
    state: AppState<T>,
    mut headers: HeaderMap,
    path: &str,
    body_bytes: Vec<u8>,
) -> Response {
    // Ensure content-type is set
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );

    // Build the request to forward
    let mut request_builder = Request::builder().method("POST").uri(path);

    // Copy headers to the request
    for (name, value) in headers.iter() {
        request_builder = request_builder.header(name, value);
    }

    let request = match request_builder.body(Body::from(body_bytes)) {
        Ok(req) => req,
        Err(e) => {
            error!(error = %e, "Failed to build request");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Failed to build request",
            );
        }
    };

    // Use the existing target message handler
    match target_message_handler(State(state), request).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Create an OpenAI-compatible error response
fn error_response(status: StatusCode, error_type: &str, message: &str) -> Response {
    let body = json!({
        "error": {
            "type": error_type,
            "message": message
        }
    });

    (status, Json(body)).into_response()
}

/// Sanitize non-streaming chat completion response
///
/// Deserializes the response through our strict schema (drops extra fields),
/// rewrites the model field, and re-serializes.
async fn sanitize_chat_response(mut response: Response, original_model: String) -> Response {
    // Read the response body
    let body_bytes =
        match axum::body::to_bytes(std::mem::take(response.body_mut()), usize::MAX).await {
            Ok(bytes) => bytes,
            Err(e) => {
                error!(error = %e, "Failed to read response body for sanitization");
                return response;
            }
        };

    // Deserialize through our strict schema (automatically drops extra fields)
    let mut chat_response: ChatCompletionResponse = match serde_json::from_slice(&body_bytes) {
        Ok(resp) => resp,
        Err(e) => {
            warn!(error = %e, "Failed to deserialize chat response, passing through");
            *response.body_mut() = Body::from(body_bytes);
            return response;
        }
    };

    // Rewrite model field to match original request
    chat_response.model = original_model;

    // Re-serialize with only our defined fields
    match serde_json::to_vec(&chat_response) {
        Ok(sanitized_bytes) => {
            *response.body_mut() = Body::from(sanitized_bytes);
            debug!("Sanitized non-streaming chat completion response");
            response
        }
        Err(e) => {
            error!(error = %e, "Failed to serialize sanitized response");
            *response.body_mut() = Body::from(body_bytes);
            response
        }
    }
}

/// Sanitize streaming chat completion response
///
/// Processes each SSE chunk, deserializes through our strict schema,
/// rewrites the model field, and re-serializes.
async fn sanitize_streaming_chat_response(
    mut response: Response,
    original_model: String,
) -> Response {
    let body_stream =
        http_body_util::BodyExt::into_data_stream(std::mem::take(response.body_mut()));

    let sanitized_stream = body_stream.map(move |chunk_result| {
        match chunk_result {
            Ok(chunk) => {
                let chunk_str = String::from_utf8_lossy(&chunk);

                // Process SSE format: "data: {...}\n\n"
                if chunk_str.starts_with("data: ") {
                    let json_part = chunk_str.trim_start_matches("data: ").trim();

                    // Handle [DONE] marker
                    if json_part == "[DONE]" {
                        return Ok(chunk);
                    }

                    // Deserialize the chunk through our strict schema
                    match serde_json::from_str::<ChatCompletionChunk>(json_part) {
                        Ok(mut chunk_data) => {
                            // Rewrite model field
                            chunk_data.model = original_model.clone();

                            // Re-serialize
                            match serde_json::to_string(&chunk_data) {
                                Ok(sanitized_json) => {
                                    let sanitized_chunk = format!("data: {}\n\n", sanitized_json);
                                    Ok::<_, std::io::Error>(axum::body::Bytes::from(
                                        sanitized_chunk,
                                    ))
                                }
                                Err(e) => {
                                    error!(error = %e, "Failed to serialize chunk");
                                    Ok(chunk)
                                }
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to parse SSE chunk, passing through");
                            Ok(chunk)
                        }
                    }
                } else {
                    // Non-data line (e.g., event: type), pass through
                    Ok(chunk)
                }
            }
            Err(e) => {
                error!(error = %e, "Stream error");
                Err(std::io::Error::other(e))
            }
        }
    });

    *response.body_mut() = Body::from_stream(sanitized_stream);
    debug!("Set up streaming chat completion response sanitization");
    response
}

/// Sanitize embeddings response
///
/// Deserializes the response through our strict schema (drops extra fields),
/// rewrites the model field, and re-serializes.
async fn sanitize_embeddings_response(mut response: Response, original_model: String) -> Response {
    let body_bytes =
        match axum::body::to_bytes(std::mem::take(response.body_mut()), usize::MAX).await {
            Ok(bytes) => bytes,
            Err(e) => {
                error!(error = %e, "Failed to read embeddings response body");
                return response;
            }
        };

    let mut embeddings_response: EmbeddingsResponse = match serde_json::from_slice(&body_bytes) {
        Ok(resp) => resp,
        Err(e) => {
            warn!(error = %e, "Failed to deserialize embeddings response, passing through");
            *response.body_mut() = Body::from(body_bytes);
            return response;
        }
    };

    // Rewrite model field
    embeddings_response.model = original_model;

    match serde_json::to_vec(&embeddings_response) {
        Ok(sanitized_bytes) => {
            *response.body_mut() = Body::from(sanitized_bytes);
            debug!("Sanitized embeddings response");
            response
        }
        Err(e) => {
            error!(error = %e, "Failed to serialize sanitized embeddings response");
            *response.body_mut() = Body::from(body_bytes);
            response
        }
    }
}

/// Sanitize error responses from third parties
///
/// In strict mode, we never pass through third-party error messages.
/// Instead, we return standard HTTP error responses based on status code.
/// The actual error is logged for debugging but never sent to the client.
async fn sanitize_error_response(mut response: Response) -> Response {
    let status = response.status();

    // Read and log the actual error for debugging
    let body_bytes =
        match axum::body::to_bytes(std::mem::take(response.body_mut()), usize::MAX).await {
            Ok(bytes) => bytes,
            Err(e) => {
                error!(error = %e, "Failed to read error response body");
                return standard_error_response(status);
            }
        };

    // Log the actual third-party error (never sent to client)
    error!(
        status = %status,
        third_party_error = ?String::from_utf8_lossy(&body_bytes),
        "Third-party error response (logged, not forwarded)"
    );

    // Return standard error based on status code
    standard_error_response(status)
}

/// Generate standard error response based on HTTP status code
/// Never includes third-party error details
fn standard_error_response(status: StatusCode) -> Response {
    let (error_type, message) = match status.as_u16() {
        400 => ("invalid_request_error", "Invalid request"),
        401 => ("authentication_error", "Authentication failed"),
        403 => ("permission_error", "Permission denied"),
        404 => ("not_found_error", "Not found"),
        429 => ("rate_limit_error", "Rate limit exceeded"),
        500 => ("api_error", "Internal server error"),
        502 => ("api_error", "Bad gateway"),
        503 => ("api_error", "Service unavailable"),
        _ => ("api_error", "An error occurred"),
    };

    error_response(status, error_type, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{Target, Targets};
    use crate::test_utils::MockHttpClient;
    use axum::body::Body;
    use axum::http::Request;
    use dashmap::DashMap;
    use std::sync::Arc;
    use tower::ServiceExt;

    #[test]
    fn test_error_response_format() {
        let response = error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Test error",
        );
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// Test that non-streaming chat responses drop extra fields from third parties
    #[tokio::test]
    async fn test_strict_sanitize_non_streaming_removes_unknown_fields() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .onwards_key("sk-test".to_string())
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        // Third-party response with extra fields that should be stripped
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
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            },
            "provider": "custom-llm-provider",
            "cost": 0.00123,
            "internal_id": "xyz-123",
            "custom_field": "should_be_removed"
        }"#;

        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        // Verify standard fields are present
        assert!(body_str.contains("\"id\":\"chatcmpl-123\""));
        assert!(body_str.contains("\"model\":\"gpt-4\""));
        assert!(body_str.contains("\"choices\""));

        // Verify extra fields are NOT present
        assert!(!body_str.contains("provider"));
        assert!(!body_str.contains("cost"));
        assert!(!body_str.contains("internal_id"));
        assert!(!body_str.contains("custom_field"));
    }

    /// Test that strict mode rewrites the model field to match the requested model
    #[tokio::test]
    async fn test_strict_sanitize_rewrites_model_field() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .onwards_key("sk-test".to_string())
                .onwards_model("gpt-4-turbo-2024-04-09".to_string()) // Maps to internal model
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        // Third-party returns internal model name
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
            }]
        }"#;

        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        // Model should be rewritten to the client-requested model
        assert!(body_str.contains("\"model\":\"gpt-4\""));
        assert!(!body_str.contains("gpt-4-turbo-2024-04-09"));
    }

    /// Test that streaming responses drop extra fields from third parties
    #[tokio::test]
    async fn test_strict_sanitize_streaming_removes_unknown_fields() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .onwards_key("sk-test".to_string())
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        let streaming_chunks = vec![
            r#"data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1677652288,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}],"provider":"custom-provider","cost":0.001}

"#.to_string(),
            "data: [DONE]\n\n".to_string(),
        ];

        let mock_client = MockHttpClient::new_streaming(StatusCode::OK, streaming_chunks);
        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body =
            r#"{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}],"stream":true}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        // Verify extra fields are NOT present
        assert!(!body_str.contains("provider"));
        assert!(!body_str.contains("cost"));
        // Verify [DONE] is preserved
        assert!(body_str.contains("[DONE]"));
    }

    /// Test that streaming responses rewrite the model field
    #[tokio::test]
    async fn test_strict_sanitize_streaming_rewrites_model() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .onwards_key("sk-test".to_string())
                .onwards_model("gpt-4-turbo".to_string())
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        let streaming_chunks = vec![
            r#"data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1677652288,"model":"gpt-4-turbo","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}

"#.to_string(),
            "data: [DONE]\n\n".to_string(),
        ];

        let mock_client = MockHttpClient::new_streaming(StatusCode::OK, streaming_chunks);
        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body =
            r#"{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}],"stream":true}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        // Model should be rewritten to client-requested model
        assert!(body_str.contains("\"model\":\"gpt-4\""));
        assert!(!body_str.contains("gpt-4-turbo"));
    }

    /// Test that embeddings responses drop extra fields from third parties
    #[tokio::test]
    async fn test_strict_sanitize_embeddings_removes_unknown_fields() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "text-embedding-3-small".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .onwards_key("sk-test".to_string())
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        // Third-party response with extra fields
        let mock_response = r#"{
            "object": "list",
            "data": [{
                "object": "embedding",
                "embedding": [0.1, 0.2, 0.3],
                "index": 0
            }],
            "model": "text-embedding-3-small",
            "usage": {
                "prompt_tokens": 5,
                "total_tokens": 5
            },
            "provider": "custom-embeddings",
            "cost": 0.0001,
            "cache_hit": true
        }"#;

        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{"model":"text-embedding-3-small","input":"Hello world"}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/embeddings")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        // Verify standard fields are present
        assert!(body_str.contains("\"object\":\"list\""));
        assert!(body_str.contains("\"model\":\"text-embedding-3-small\""));
        assert!(body_str.contains("\"data\""));

        // Verify extra fields are NOT present
        assert!(!body_str.contains("provider"));
        assert!(!body_str.contains("cost"));
        assert!(!body_str.contains("cache_hit"));
    }

    /// Test that embeddings responses rewrite the model field
    #[tokio::test]
    async fn test_strict_sanitize_embeddings_rewrites_model() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "text-embedding-3-small".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .onwards_key("sk-test".to_string())
                .onwards_model("text-embedding-3-small-internal".to_string())
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        let mock_response = r#"{
            "object": "list",
            "data": [{
                "object": "embedding",
                "embedding": [0.1, 0.2],
                "index": 0
            }],
            "model": "text-embedding-3-small-internal",
            "usage": {
                "prompt_tokens": 5,
                "total_tokens": 5
            }
        }"#;

        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{"model":"text-embedding-3-small","input":"Hello"}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/embeddings")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        // Model should be rewritten
        assert!(body_str.contains("\"model\":\"text-embedding-3-small\""));
        assert!(!body_str.contains("text-embedding-3-small-internal"));
    }

    /// Test that error responses return standard messages (no third-party details)
    #[tokio::test]
    async fn test_strict_error_returns_standard_message() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .onwards_key("sk-test".to_string())
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        // Third-party error with internal fields (should be completely replaced)
        let mock_error_response = r#"{
            "error": {
                "message": "Database connection failed at postgresql://internal-db:5432",
                "type": "internal_error",
                "provider": "custom-llm-backend",
                "internal_trace_id": "xyz-123-abc",
                "debug_info": "Stack trace: error at line 42"
            }
        }"#;

        let mock_client = MockHttpClient::new(StatusCode::BAD_REQUEST, mock_error_response);
        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        // Should return standard error message
        assert!(body_str.contains("\"error\""));
        assert!(body_str.contains("\"type\":\"invalid_request_error\""));
        assert!(body_str.contains("\"message\":\"Invalid request\""));

        // Should NOT contain ANY third-party content
        assert!(!body_str.contains("Database"));
        assert!(!body_str.contains("postgresql"));
        assert!(!body_str.contains("provider"));
        assert!(!body_str.contains("internal_trace_id"));
        assert!(!body_str.contains("debug_info"));
    }

    /// Test that 500 errors return standard "Internal server error" message
    #[tokio::test]
    async fn test_strict_error_500_returns_standard_message() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .onwards_key("sk-test".to_string())
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        // Third-party error with leaky message
        let mock_error_response = r#"{
            "error": {
                "message": "Contact support@provider.com with trace ID abc-123",
                "type": "server_error"
            }
        }"#;

        let mock_client =
            MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, mock_error_response);
        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        // Should return standard 500 error message
        assert!(body_str.contains("\"type\":\"api_error\""));
        assert!(body_str.contains("\"message\":\"Internal server error\""));

        // Should NOT contain third-party contact info
        assert!(!body_str.contains("support@provider.com"));
        assert!(!body_str.contains("trace ID abc-123"));
    }

    /// Test that malformed error responses are handled gracefully
    #[tokio::test]
    async fn test_strict_handle_malformed_error_response() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .onwards_key("sk-test".to_string())
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        // Non-JSON error response (e.g., plain text or HTML)
        let mock_error_response = "<html><body>Internal Server Error</body></html>";

        let mock_client =
            MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, mock_error_response);
        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        // Should return clean JSON error, not HTML
        assert!(!body_str.contains("<html>"));
        assert!(!body_str.contains("<body>"));
        assert!(body_str.contains("\"error\""));

        // Should parse as valid JSON
        let _: serde_json::Value = serde_json::from_str(&body_str).expect("Should be valid JSON");
    }
}
