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
use super::schemas::responses::{ResponsesRequest, ResponsesResponse};
use crate::AppState;
use crate::client::HttpClient;
use crate::handlers::target_message_handler;
use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Request, StatusCode};
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

    // Check if this target is trusted - if so, bypass all sanitization
    let is_trusted = state.targets.targets.get(&original_model)
        .map(|pool| {
            pool.providers().first()
                .map(|p| p.target.trusted)
                .unwrap_or(false)
        })
        .unwrap_or(false);

    if is_trusted {
        debug!(model = %original_model, "Bypassing sanitization for trusted target");
        return forward_request(state, headers, "/v1/chat/completions", body_bytes).await;
    }

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
    let original_model = request.model.clone();
    let is_streaming = request.stream.unwrap_or(false);

    debug!(
        model = %original_model,
        has_previous_response_id = request.previous_response_id.is_some(),
        stream = is_streaming,
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

    // Check if this target is trusted - if so, bypass all sanitization
    let is_trusted = state.targets.targets.get(&original_model)
        .map(|pool| {
            pool.providers().first()
                .map(|p| p.target.trusted)
                .unwrap_or(false)
        })
        .unwrap_or(false);

    if is_trusted {
        debug!(model = %original_model, "Bypassing sanitization for trusted target");
        return forward_request(state, headers, "/v1/responses", body_bytes).await;
    }

    let response = forward_request(state, headers, "/v1/responses", body_bytes).await;

    // Sanitize response to ensure model field matches and extra fields are dropped
    if response.status().is_success() {
        if is_streaming {
            // TODO: Add streaming sanitization when Open Responses streaming format is defined
            warn!("Streaming responses not yet sanitized");
            response
        } else {
            sanitize_responses_response(response, original_model).await
        }
    } else {
        // Sanitize error responses to prevent third-party info leakage
        sanitize_error_response(response).await
    }
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

    // Check if this target is trusted - if so, bypass all sanitization
    let is_trusted = state.targets.targets.get(&original_model)
        .map(|pool| {
            pool.providers().first()
                .map(|p| p.target.trusted)
                .unwrap_or(false)
        })
        .unwrap_or(false);

    if is_trusted {
        debug!(model = %original_model, "Bypassing sanitization for trusted target");
        return forward_request(state, headers, "/v1/embeddings", body_bytes).await;
    }

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
///
/// Format matches OpenAI's error structure:
/// https://platform.openai.com/docs/guides/error-codes
fn error_response(status: StatusCode, error_type: &str, message: &str) -> Response {
    let body = json!({
        "error": {
            "message": message,
            "type": error_type,
            "param": null,
            "code": null
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
            // Failed to deserialize - provider returned malformed response
            // Log the error but return standard error instead of passing through
            error!(
                error = %e,
                body_sample = ?String::from_utf8_lossy(&body_bytes).chars().take(200).collect::<String>(),
                "Failed to deserialize chat response from provider, returning standard error"
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "Bad gateway",
            );
        }
    };

    // Rewrite model field to match original request
    chat_response.model = original_model;

    // Re-serialize with only our defined fields
    match serde_json::to_vec(&chat_response) {
        Ok(sanitized_bytes) => {
            // Set Content-Length to match the new sanitized body size
            let content_length = sanitized_bytes.len();
            *response.body_mut() = Body::from(sanitized_bytes);
            response.headers_mut().insert(
                header::CONTENT_LENGTH,
                header::HeaderValue::from(content_length),
            );
            debug!("Sanitized non-streaming chat completion response");
            response
        }
        Err(e) => {
            error!(
                error = %e,
                "Failed to serialize sanitized chat response, returning standard error"
            );
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "Internal server error",
            )
        }
    }
}

/// Sanitize streaming chat completion response
///
/// Processes each SSE chunk line-by-line, deserializes through our strict schema,
/// rewrites the model field, and re-serializes. Ignores comment lines and other
/// non-data SSE fields to prevent metadata leakage.
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
                let mut sanitized_lines = Vec::new();

                // Process SSE line-by-line for streaming chunks,
                // preserving empty lines and ignoring comment/event-type lines
                for line in chunk_str.lines() {
                    if let Some(data_part) = line.strip_prefix("data: ") {
                        // This is a data line

                        // Handle [DONE] marker
                        if data_part.trim() == "[DONE]" {
                            sanitized_lines.push(line.to_string());
                            continue;
                        }

                        // Deserialize the chunk through our strict schema
                        match serde_json::from_str::<ChatCompletionChunk>(data_part) {
                            Ok(mut chunk_data) => {
                                // Rewrite model field
                                chunk_data.model = original_model.clone();

                                // Re-serialize
                                match serde_json::to_string(&chunk_data) {
                                    Ok(sanitized_json) => {
                                        sanitized_lines.push(format!("data: {}", sanitized_json));
                                    }
                                    Err(e) => {
                                        error!(error = %e, "Failed to serialize chunk, terminating stream");
                                        return Err(std::io::Error::other(
                                            "Failed to serialize chunk",
                                        ));
                                    }
                                }
                            }
                            Err(e) => {
                                error!(
                                    error = %e,
                                    data_sample = ?data_part.chars().take(200).collect::<String>(),
                                    "Failed to parse SSE data line from provider, terminating stream"
                                );
                                return Err(std::io::Error::other(
                                    "Malformed SSE data from provider",
                                ));
                            }
                        }
                    } else if line.is_empty() {
                        // Preserve empty lines (SSE event delimiters)
                        sanitized_lines.push(String::new());
                    }
                    // Ignore all other lines (comments, event types, etc.) - don't leak metadata
                }

                // Reconstruct the chunk with sanitized content
                // Preserve trailing newlines from original chunk
                let mut sanitized_chunk = sanitized_lines.join("\n");

                // Count trailing newlines in input and restore them
                let input_trailing = chunk_str.chars().rev().take_while(|&c| c == '\n').count();
                let output_trailing = sanitized_chunk
                    .chars()
                    .rev()
                    .take_while(|&c| c == '\n')
                    .count();

                for _ in output_trailing..input_trailing {
                    sanitized_chunk.push('\n');
                }

                Ok::<_, std::io::Error>(axum::body::Bytes::from(sanitized_chunk))
            }
            Err(e) => {
                error!(error = %e, "Stream error");
                Err(std::io::Error::other(e))
            }
        }
    });

    *response.body_mut() = Body::from_stream(sanitized_stream);
    // Remove Content-Length header - streaming responses should use chunked encoding
    response.headers_mut().remove(header::CONTENT_LENGTH);
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
            error!(
                error = %e,
                body_sample = ?String::from_utf8_lossy(&body_bytes).chars().take(200).collect::<String>(),
                "Failed to deserialize embeddings response from provider, returning standard error"
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "Bad gateway",
            );
        }
    };

    // Rewrite model field
    embeddings_response.model = original_model;

    match serde_json::to_vec(&embeddings_response) {
        Ok(sanitized_bytes) => {
            // Set Content-Length to match the new sanitized body size
            let content_length = sanitized_bytes.len();
            *response.body_mut() = Body::from(sanitized_bytes);
            response.headers_mut().insert(
                header::CONTENT_LENGTH,
                header::HeaderValue::from(content_length),
            );
            debug!("Sanitized embeddings response");
            response
        }
        Err(e) => {
            error!(
                error = %e,
                "Failed to serialize sanitized embeddings response, returning standard error"
            );
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "Internal server error",
            )
        }
    }
}

/// Sanitize responses API response
///
/// Deserializes the response through our strict schema (drops extra fields),
/// rewrites the model field, and re-serializes.
async fn sanitize_responses_response(mut response: Response, original_model: String) -> Response {
    let body_bytes =
        match axum::body::to_bytes(std::mem::take(response.body_mut()), usize::MAX).await {
            Ok(bytes) => bytes,
            Err(e) => {
                error!(error = %e, "Failed to read responses API response body");
                return response;
            }
        };

    let mut responses_response: ResponsesResponse = match serde_json::from_slice(&body_bytes) {
        Ok(resp) => resp,
        Err(e) => {
            error!(
                error = %e,
                body_sample = ?String::from_utf8_lossy(&body_bytes).chars().take(200).collect::<String>(),
                "Failed to deserialize responses API response from provider, returning standard error"
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "Bad gateway",
            );
        }
    };

    // Rewrite model field
    responses_response.model = original_model;

    match serde_json::to_vec(&responses_response) {
        Ok(sanitized_bytes) => {
            // Set Content-Length to match the new sanitized body size
            let content_length = sanitized_bytes.len();
            *response.body_mut() = Body::from(sanitized_bytes);
            response.headers_mut().insert(
                header::CONTENT_LENGTH,
                header::HeaderValue::from(content_length),
            );
            debug!("Sanitized responses API response");
            response
        }
        Err(e) => {
            error!(
                error = %e,
                "Failed to serialize sanitized responses API response, returning standard error"
            );
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "Internal server error",
            )
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

    /// Test that error responses match OpenAI's exact format
    #[tokio::test]
    async fn test_error_response_matches_openai_format() {
        // Our error response
        let response = error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Invalid request",
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let our_error: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // OpenAI's error format (from their API docs)
        // https://platform.openai.com/docs/guides/error-codes
        let openai_format = json!({
            "error": {
                "message": "Invalid request",
                "type": "invalid_request_error",
                "param": null,
                "code": null
            }
        });

        // Verify structure matches exactly
        assert_eq!(our_error, openai_format);

        // Verify all required fields are present
        assert!(our_error["error"]["message"].is_string());
        assert!(our_error["error"]["type"].is_string());
        assert!(our_error["error"]["param"].is_null());
        assert!(our_error["error"]["code"].is_null());
    }

    /// Test that different error types use correct OpenAI error type names
    #[tokio::test]
    async fn test_error_types_match_openai_conventions() {
        // Test various status codes map to correct OpenAI error types
        let test_cases = vec![
            (StatusCode::BAD_REQUEST, "invalid_request_error"),
            (StatusCode::UNAUTHORIZED, "authentication_error"),
            (StatusCode::FORBIDDEN, "permission_error"),
            (StatusCode::NOT_FOUND, "not_found_error"),
            (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
            (StatusCode::INTERNAL_SERVER_ERROR, "api_error"),
            (StatusCode::BAD_GATEWAY, "api_error"),
            (StatusCode::SERVICE_UNAVAILABLE, "api_error"),
        ];

        for (status, expected_type) in test_cases {
            let response = standard_error_response(status);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let error: serde_json::Value = serde_json::from_slice(&body).unwrap();

            assert_eq!(
                error["error"]["type"].as_str().unwrap(),
                expected_type,
                "Status {} should map to error type {}",
                status,
                expected_type
            );
        }
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

    /// Test that responses API drops extra fields from third parties
    #[tokio::test]
    async fn test_strict_sanitize_responses_removes_unknown_fields() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
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
            "id": "resp_abc123",
            "object": "response",
            "created_at": 1677652288,
            "completed_at": 1677652290,
            "status": "completed",
            "incomplete_details": null,
            "model": "gpt-4o",
            "previous_response_id": null,
            "instructions": null,
            "output": [],
            "error": null,
            "tools": [],
            "tool_choice": "auto",
            "truncation": "auto",
            "parallel_tool_calls": true,
            "text": {},
            "top_p": 1.0,
            "presence_penalty": 0.0,
            "frequency_penalty": 0.0,
            "top_logprobs": 0,
            "temperature": 1.0,
            "reasoning": null,
            "usage": null,
            "max_output_tokens": null,
            "max_tool_calls": null,
            "store": false,
            "background": false,
            "service_tier": "default",
            "metadata": null,
            "safety_identifier": null,
            "prompt_cache_key": null,
            "provider": "custom-provider",
            "cost": 0.0123,
            "internal_trace_id": "xyz-789"
        }"#;

        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{"model":"gpt-4o","input":"Hello"}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        // Verify standard fields are present
        assert!(body_str.contains("\"id\":\"resp_abc123\""));
        assert!(body_str.contains("\"model\":\"gpt-4o\""));
        assert!(body_str.contains("\"status\":\"completed\""));

        // Verify extra fields are NOT present
        assert!(!body_str.contains("provider"));
        assert!(!body_str.contains("cost"));
        assert!(!body_str.contains("internal_trace_id"));
    }

    /// Test that responses API rewrites the model field
    #[tokio::test]
    async fn test_strict_sanitize_responses_rewrites_model() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .onwards_key("sk-test".to_string())
                .onwards_model("gpt-4o-2024-05-13".to_string())
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
            "id": "resp_abc123",
            "object": "response",
            "created_at": 1677652288,
            "completed_at": 1677652290,
            "status": "completed",
            "incomplete_details": null,
            "model": "gpt-4o-2024-05-13",
            "previous_response_id": null,
            "instructions": null,
            "output": [],
            "error": null,
            "tools": [],
            "tool_choice": "auto",
            "truncation": "auto",
            "parallel_tool_calls": true,
            "text": {},
            "top_p": 1.0,
            "presence_penalty": 0.0,
            "frequency_penalty": 0.0,
            "top_logprobs": 0,
            "temperature": 1.0,
            "reasoning": null,
            "usage": null,
            "max_output_tokens": null,
            "max_tool_calls": null,
            "store": false,
            "background": false,
            "service_tier": "default",
            "metadata": null,
            "safety_identifier": null,
            "prompt_cache_key": null
        }"#;

        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{"model":"gpt-4o","input":"Hello"}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        // Model should be rewritten to client-requested model
        assert!(body_str.contains("\"model\":\"gpt-4o\""));
        assert!(!body_str.contains("gpt-4o-2024-05-13"));
    }

    /// Test that responses API errors are sanitized
    #[tokio::test]
    async fn test_strict_sanitize_responses_error() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
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

        // Third-party error with internal details
        let mock_error_response = r#"{
            "error": {
                "message": "Internal error in provider backend at server xyz-123.internal.com",
                "type": "server_error",
                "provider": "custom-provider",
                "trace_id": "abc-def-ghi"
            }
        }"#;

        let mock_client = MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, mock_error_response);
        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{"model":"gpt-4o","input":"Hello"}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        // Should return standard error message
        assert!(body_str.contains("\"type\":\"api_error\""));
        assert!(body_str.contains("\"message\":\"Internal server error\""));

        // Should NOT contain third-party details
        assert!(!body_str.contains("Internal error in provider backend"));
        assert!(!body_str.contains("xyz-123.internal.com"));
        assert!(!body_str.contains("provider"));
        assert!(!body_str.contains("trace_id"));
    }

    /// Test that Content-Length header is updated correctly after chat completion sanitization
    #[tokio::test]
    async fn test_chat_sanitization_updates_content_length_header() {
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

        // Response with extra fields that will be removed during sanitization
        // This changes the body size, making the original Content-Length incorrect
        let mock_response = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "provider-model-name",
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
            "provider": "custom-provider",
            "cost": 0.00123,
            "trace_id": "abc-xyz-123",
            "custom_metadata": "will be dropped"
        }"#;

        // Set up mock with incorrect Content-Length that would be wrong after sanitization
        let mut mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        // The mock response is larger than what will remain after sanitization
        mock_client.set_header("content-length", mock_response.len().to_string());

        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{"model":"gpt-4","messages":[{"role":"user","content":"test"}]}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Verify the body can be read completely with correct Content-Length
        // If Content-Length wasn't updated, the body would be truncated or hang
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        // Verify body is valid JSON and doesn't contain removed fields
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["model"], "gpt-4"); // Model rewritten
        assert!(body_json.get("provider").is_none()); // Extra field removed
        assert!(body_json.get("cost").is_none()); // Extra field removed
        assert!(body_json.get("trace_id").is_none()); // Extra field removed
    }

    /// Test that Content-Length header is updated correctly after embeddings sanitization
    #[tokio::test]
    async fn test_embeddings_sanitization_updates_content_length_header() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "text-embedding-ada-002".to_string(),
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

        // Response with extra provider-specific fields
        let mock_response = r#"{
            "object": "list",
            "data": [{
                "object": "embedding",
                "index": 0,
                "embedding": [0.1, 0.2, 0.3]
            }],
            "model": "provider-embedding-model",
            "usage": {
                "prompt_tokens": 5,
                "total_tokens": 5
            },
            "provider_metadata": "extra_data",
            "cost": 0.0001,
            "processing_time_ms": 123
        }"#;

        let mut mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        mock_client.set_header("content-length", mock_response.len().to_string());

        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{"model":"text-embedding-ada-002","input":"test"}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/embeddings")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Verify body is readable and sanitized
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["model"], "text-embedding-ada-002"); // Model rewritten
        assert!(body_json.get("provider_metadata").is_none()); // Extra field removed
        assert!(body_json.get("cost").is_none()); // Extra field removed
        assert!(body_json.get("processing_time_ms").is_none()); // Extra field removed
    }

    #[tokio::test]
    async fn test_trusted_target_bypasses_sanitization() {
        use crate::target::{Target, Targets};
        use crate::test_utils::MockHttpClient;
        use axum::body::Body;
        use axum::http::Request;
        use dashmap::DashMap;
        use std::sync::Arc;
        use tower::ServiceExt;

        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com".parse().unwrap())
                .onwards_key("sk-test".to_string())
                .trusted(true) // Mark as trusted
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        // Mock upstream response with provider-specific fields
        let mock_response = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "gpt-4-actual-provider-model",
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
            "provider_metadata": {
                "cost": 0.001,
                "trace_id": "trace-123"
            }
        }"#;

        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        let state = crate::AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hello"}]
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        // Verify ORIGINAL response is passed through for trusted target:
        // 1. Model field NOT rewritten (still provider's model)
        assert_eq!(
            response_json["model"], "gpt-4-actual-provider-model",
            "Trusted target should preserve original model field"
        );

        // 2. Provider-specific fields NOT removed
        assert!(
            response_json.get("provider_metadata").is_some(),
            "Trusted target should preserve provider metadata"
        );
        assert_eq!(response_json["provider_metadata"]["cost"], 0.001);
        assert_eq!(response_json["provider_metadata"]["trace_id"], "trace-123");

        // 3. Standard fields still present
        assert_eq!(response_json["object"], "chat.completion");
        assert_eq!(response_json["choices"][0]["message"]["content"], "Hello!");
    }

    #[tokio::test]
    async fn test_trusted_target_bypasses_error_sanitization() {
        use crate::target::{Target, Targets};
        use crate::test_utils::MockHttpClient;
        use axum::body::Body;
        use axum::http::Request;
        use dashmap::DashMap;
        use std::sync::Arc;
        use tower::ServiceExt;

        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com".parse().unwrap())
                .onwards_key("sk-test".to_string())
                .trusted(true)
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        // Mock upstream error with provider-specific details
        let mock_error = r#"{
            "error": {
                "message": "Internal provider error: GPU cluster unavailable in eu-west-3",
                "type": "provider_error",
                "code": "internal_error",
                "metadata": {
                    "provider": "openai",
                    "region": "eu-west-3",
                    "trace_id": "trace-abc-123"
                }
            }
        }"#;

        let mock_client = MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, mock_error);
        let state = crate::AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hello"}]
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        // Verify ORIGINAL error is passed through for trusted target:
        // 1. Original error message preserved
        assert_eq!(
            response_json["error"]["message"],
            "Internal provider error: GPU cluster unavailable in eu-west-3",
            "Trusted target should preserve original error message"
        );

        // 2. Provider-specific error code preserved
        assert_eq!(response_json["error"]["code"], "internal_error");

        // 3. Provider metadata preserved
        assert!(response_json["error"]["metadata"].is_object());
        assert_eq!(response_json["error"]["metadata"]["provider"], "openai");
        assert_eq!(response_json["error"]["metadata"]["region"], "eu-west-3");
        assert_eq!(response_json["error"]["metadata"]["trace_id"], "trace-abc-123");
    }

    /// Test that Content-Length header is updated correctly after Responses API sanitization
    #[tokio::test]
    async fn test_responses_sanitization_updates_content_length_header() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
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

        // Response with extra provider-specific fields
        let mock_response = r#"{
            "id": "resp_123",
            "object": "response",
            "created_at": 1234567890,
            "completed_at": 1234567900,
            "status": "completed",
            "incomplete_details": null,
            "model": "provider-model",
            "previous_response_id": null,
            "instructions": null,
            "output": [],
            "error": null,
            "tools": [],
            "tool_choice": "auto",
            "truncation": "disabled",
            "parallel_tool_calls": true,
            "text": {
                "format": {
                    "type": "text"
                }
            },
            "top_p": 1.0,
            "presence_penalty": 0.0,
            "frequency_penalty": 0.0,
            "top_logprobs": 0,
            "temperature": 1.0,
            "reasoning": null,
            "usage": null,
            "max_output_tokens": null,
            "max_tool_calls": null,
            "store": false,
            "background": false,
            "service_tier": "default",
            "metadata": null,
            "safety_identifier": null,
            "prompt_cache_key": null,
            "provider_trace_id": "xyz-123",
            "internal_cost": 0.456,
            "custom_field": "should_be_removed"
        }"#;

        let mut mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        mock_client.set_header("content-length", mock_response.len().to_string());

        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{"model":"gpt-4o","input":"test"}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Verify body is readable and sanitized
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["model"], "gpt-4o"); // Model rewritten
        assert!(body_json.get("provider_trace_id").is_none()); // Extra field removed
        assert!(body_json.get("internal_cost").is_none()); // Extra field removed
        assert!(body_json.get("custom_field").is_none()); // Extra field removed
    }

    /// Test that multi-line SSE data events are properly sanitized
    /// Multi-line data events have multiple "data: " lines that should be concatenated
    #[tokio::test]
    async fn test_streaming_multiline_sse_events_are_sanitized() {
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

        // Multi-line SSE event with provider-specific fields in the JSON
        // This is a valid SSE format where the data is split across multiple data: lines
        let chunk1 = r#"data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1234567890,"model":"provider-model","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}],"provider":"leaked-provider","cost":0.001}

"#;

        let mock_client = MockHttpClient::new_streaming(StatusCode::OK, vec![chunk1.to_string()]);
        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body =
            r#"{"model":"gpt-4","messages":[{"role":"user","content":"test"}],"stream":true}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Read the streaming response
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        // The response should have the model rewritten
        assert!(body_str.contains("\"model\":\"gpt-4\""));

        // Provider-specific fields should be removed
        assert!(!body_str.contains("provider"), "Provider field should be removed");
        assert!(!body_str.contains("cost"), "Cost field should be removed");
        assert!(!body_str.contains("leaked-provider"), "Provider name should not leak");
    }

    /// Test that SSE events with comment lines don't leak provider metadata
    #[tokio::test]
    async fn test_streaming_sse_comments_dont_leak_metadata() {
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

        // SSE stream with comment line containing provider-specific info
        // Comment lines start with : and should be stripped in strict mode
        let chunks = vec![
            ": provider=custom-llm cost=0.001 trace_id=xyz-123\n".to_string(),
            r#"data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1234567890,"model":"provider-model","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}

"#
            .to_string(),
        ];

        let mock_client = MockHttpClient::new_streaming(StatusCode::OK, chunks);
        let state = AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body =
            r#"{"model":"gpt-4","messages":[{"role":"user","content":"test"}],"stream":true}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Read the streaming response
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        // Comment lines with provider metadata should NOT appear in output
        assert!(
            !body_str.contains("provider=custom-llm"),
            "Provider metadata in comments should be stripped"
        );
        assert!(!body_str.contains("trace_id=xyz-123"), "Trace ID should be stripped");
        assert!(!body_str.contains("cost=0.001"), "Cost should be stripped");

        // But the actual data should be sanitized and present
        assert!(body_str.contains("\"model\":\"gpt-4\""));
    }

    /// Test that optional request fields are not injected as explicit nulls
    /// When a field is omitted in the request, it should remain omitted when forwarded
    #[tokio::test]
    async fn test_responses_api_omitted_fields_not_injected_as_null() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
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

        // Mock client that captures the forwarded request
        let mock_response = r#"{
            "id": "resp_123",
            "object": "response",
            "created_at": 1234567890,
            "completed_at": 1234567900,
            "status": "completed",
            "incomplete_details": null,
            "model": "gpt-4o",
            "previous_response_id": null,
            "instructions": null,
            "output": [],
            "error": null,
            "tools": [],
            "tool_choice": "auto",
            "truncation": "disabled",
            "parallel_tool_calls": true,
            "text": { "format": { "type": "text" } },
            "top_p": 1.0,
            "presence_penalty": 0.0,
            "frequency_penalty": 0.0,
            "top_logprobs": 0,
            "temperature": 1.0,
            "reasoning": null,
            "usage": null,
            "max_output_tokens": null,
            "max_tool_calls": null,
            "store": false,
            "background": false,
            "service_tier": "default",
            "metadata": null,
            "safety_identifier": null,
            "prompt_cache_key": null
        }"#;

        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        let state = AppState::with_client(targets, mock_client.clone());
        let router = crate::strict::build_strict_router(state);

        // Request with MessageItem that omits optional id and status fields
        let request_body = r#"{
            "model": "gpt-4o",
            "input": [{
                "type": "message",
                "role": "user",
                "content": "Hello"
            }]
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let _response = router.oneshot(request).await.unwrap();

        // Check what was forwarded to the upstream
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);
        let forwarded_body = String::from_utf8(requests[0].body.clone()).unwrap();
        let forwarded_json: serde_json::Value = serde_json::from_str(&forwarded_body).unwrap();

        // The input array should have the message item
        let input_items = forwarded_json["input"].as_array().unwrap();
        assert_eq!(input_items.len(), 1);

        let message_item = &input_items[0];

        // CRITICAL: id and status fields should NOT be present (not even as null)
        // If they are serialized as "id": null or "status": null, this test will fail
        assert!(
            !message_item.as_object().unwrap().contains_key("id"),
            "Optional 'id' field should not be present when omitted in request, found: {:?}",
            message_item
        );
        assert!(
            !message_item.as_object().unwrap().contains_key("status"),
            "Optional 'status' field should not be present when omitted in request, found: {:?}",
            message_item
        );
    }

    /// Test that unknown item types preserve their original payload
    /// When an unknown item type is encountered, it should be forwarded unchanged
    #[tokio::test]
    async fn test_responses_api_unknown_item_types_preserved() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
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

        let mock_response = r#"{
            "id": "resp_123",
            "object": "response",
            "created_at": 1234567890,
            "completed_at": 1234567900,
            "status": "completed",
            "incomplete_details": null,
            "model": "gpt-4o",
            "previous_response_id": null,
            "instructions": null,
            "output": [],
            "error": null,
            "tools": [],
            "tool_choice": "auto",
            "truncation": "disabled",
            "parallel_tool_calls": true,
            "text": { "format": { "type": "text" } },
            "top_p": 1.0,
            "presence_penalty": 0.0,
            "frequency_penalty": 0.0,
            "top_logprobs": 0,
            "temperature": 1.0,
            "reasoning": null,
            "usage": null,
            "max_output_tokens": null,
            "max_tool_calls": null,
            "store": false,
            "background": false,
            "service_tier": "default",
            "metadata": null,
            "safety_identifier": null,
            "prompt_cache_key": null
        }"#;

        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        let state = AppState::with_client(targets, mock_client.clone());
        let router = crate::strict::build_strict_router(state);

        // Request with a future unknown item type (e.g., "web_search")
        let request_body = r#"{
            "model": "gpt-4o",
            "input": [{
                "type": "web_search",
                "query": "latest news",
                "max_results": 10,
                "custom_field": "should be preserved"
            }]
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let _response = router.oneshot(request).await.unwrap();

        // Check what was forwarded
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);
        let forwarded_body = String::from_utf8(requests[0].body.clone()).unwrap();
        let forwarded_json: serde_json::Value = serde_json::from_str(&forwarded_body).unwrap();

        let input_items = forwarded_json["input"].as_array().unwrap();
        assert_eq!(input_items.len(), 1);

        let unknown_item = &input_items[0];

        // CRITICAL: All fields from the unknown item should be preserved
        assert_eq!(unknown_item["type"], "web_search");
        assert_eq!(unknown_item["query"], "latest news");
        assert_eq!(unknown_item["max_results"], 10);
        assert_eq!(
            unknown_item["custom_field"], "should be preserved",
            "Unknown item fields should be preserved, but got: {:?}",
            unknown_item
        );
    }

    #[tokio::test]
    async fn test_strict_mode_ignores_target_sanitize_response_flag() {
        // This test verifies that when strict mode is enabled globally,
        // individual target sanitize_response flags are ignored to prevent
        // double sanitization. Strict mode handlers already perform complete
        // sanitization, so response_transform_fn should be skipped.

        use crate::target::{Target, Targets};
        use crate::test_utils::MockHttpClient;
        use axum::body::Body;
        use axum::http::Request;
        use dashmap::DashMap;
        use std::sync::Arc;
        use tower::ServiceExt;

        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com".parse().unwrap())
                .onwards_key("sk-test".to_string())
                // This flag should be IGNORED in strict mode
                .sanitize_response(true)
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true, // Strict mode enabled
        };

        // Mock upstream response with provider-specific fields
        let mock_response = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "gpt-4-actual-provider-model",
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
            "provider_metadata": {
                "cost": 0.001,
                "trace_id": "trace-123"
            }
        }"#;

        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        let state = crate::AppState::with_client(targets, mock_client)
            .with_response_transform(crate::create_openai_sanitizer());

        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hello"}]
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        // Verify strict mode sanitization worked:
        // 1. Model field rewritten to match request
        assert_eq!(response_json["model"], "gpt-4");

        // 2. Provider-specific fields removed
        assert!(
            response_json.get("provider_metadata").is_none(),
            "Provider metadata should be removed by strict mode sanitization"
        );

        // 3. Standard fields preserved
        assert_eq!(response_json["object"], "chat.completion");
        assert_eq!(response_json["choices"][0]["message"]["content"], "Hello!");

        // This test passing confirms that:
        // - Strict mode sanitization works even when sanitize_response: true
        // - No double sanitization occurs (which could corrupt responses)
        // - response_transform_fn is properly skipped in strict mode
    }

    #[tokio::test]
    async fn test_untrusted_target_still_sanitized() {
        use crate::target::{Target, Targets};
        use crate::test_utils::MockHttpClient;
        use axum::body::Body;
        use axum::http::Request;
        use dashmap::DashMap;
        use std::sync::Arc;
        use tower::ServiceExt;

        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "third-party".to_string(),
            Target::builder()
                .url("https://third-party.com".parse().unwrap())
                .onwards_key("sk-test".to_string())
                // NO trusted flag - defaults to false
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        // Mock upstream response with provider-specific fields
        let mock_response = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "provider-internal-model",
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
            "provider_metadata": {
                "should": "be removed"
            }
        }"#;

        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        let state = crate::AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{
            "model": "third-party",
            "messages": [{"role": "user", "content": "Hello"}]
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        // Verify strict mode sanitization STILL APPLIES for untrusted:
        // 1. Model field rewritten to match request
        assert_eq!(
            response_json["model"], "third-party",
            "Untrusted target should have model field rewritten"
        );

        // 2. Provider metadata removed
        assert!(
            response_json.get("provider_metadata").is_none(),
            "Untrusted target should have provider metadata removed"
        );

        // 3. Standard fields preserved
        assert_eq!(response_json["choices"][0]["message"]["content"], "Hello!");
    }

    #[tokio::test]
    async fn test_trusted_streaming_responses_passthrough() {
        use crate::target::{Target, Targets};
        use crate::test_utils::MockHttpClient;
        use axum::body::Body;
        use axum::http::Request;
        use dashmap::DashMap;
        use std::sync::Arc;
        use tower::ServiceExt;

        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com".parse().unwrap())
                .onwards_key("sk-test".to_string())
                .trusted(true)
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            strict_mode: true,
        };

        // Mock SSE streaming response with provider metadata in chunks
        let mock_stream = "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4-provider\",\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"index\":0}],\"provider_cost\":0.001}\n\n";

        let mock_client = MockHttpClient::new(StatusCode::OK, mock_stream);
        let state = crate::AppState::with_client(targets, mock_client);
        let router = crate::strict::build_strict_router(state);

        let request_body = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": true
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response_str = String::from_utf8(body_bytes.to_vec()).unwrap();

        // Verify stream is passed through without sanitization
        assert!(
            response_str.contains("\"model\":\"gpt-4-provider\""),
            "Trusted streaming should preserve original model"
        );
        assert!(
            response_str.contains("\"provider_cost\":0.001"),
            "Trusted streaming should preserve provider metadata"
        );
    }
}
