//! Typed request handlers for strict mode
//!
//! These handlers validate requests using Axum's Json extractor (which uses serde)
//! before forwarding to the upstream provider.

use super::adapter::OpenResponsesAdapter;
use super::schemas::chat_completions::{ChatCompletionRequest, ChatCompletionResponse};
use super::schemas::embeddings::EmbeddingsRequest;
use super::schemas::responses::ResponsesRequest;
use crate::client::HttpClient;
use crate::handlers::target_message_handler;
use crate::traits::{NoOpResponseStore, NoOpToolExecutor};
use crate::AppState;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use http_body_util::BodyExt;
use serde_json::json;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

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
    debug!(
        model = %request.model,
        messages_count = request.messages.len(),
        stream = ?request.stream,
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

    forward_request(state, headers, "/v1/chat/completions", body_bytes).await
}

/// Handler for POST /v1/responses
///
/// Validates the request against the Open Responses schema. If the target has
/// `open_responses.adapter: true`, the request is processed through the adapter.
/// Otherwise, it's forwarded to the upstream as-is.
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

    // Check if we should use the adapter for this target
    let use_adapter = should_use_adapter(&state, &request.model);

    if use_adapter {
        // Adapter mode: convert to Chat Completions, forward, convert back
        debug!(model = %request.model, "Using Open Responses adapter");
        return handle_adapter_request(state, headers, request).await;
    }

    // Passthrough mode: forward request as-is
    debug!(model = %request.model, "Passthrough mode for responses request");

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

/// Check if the adapter should be used for this model
fn should_use_adapter<T: HttpClient + Clone + Send + Sync + 'static>(
    state: &AppState<T>,
    model: &str,
) -> bool {
    // Get the pool for this model
    let pool = match state.targets.targets.get(model) {
        Some(pool) => pool,
        None => {
            debug!(model = %model, "No target found, cannot determine adapter setting");
            return false;
        }
    };

    // Get the first target to check its config
    let target = match pool.first_target() {
        Some(target) => target,
        None => {
            debug!(model = %model, "Pool is empty, cannot determine adapter setting");
            return false;
        }
    };

    // Check if open_responses.adapter is true
    target
        .open_responses
        .as_ref()
        .map(|config| config.adapter)
        .unwrap_or(false)
}

/// Handle a request using the Open Responses adapter
async fn handle_adapter_request<T: HttpClient + Clone + Send + Sync + 'static>(
    state: AppState<T>,
    headers: HeaderMap,
    request: ResponsesRequest,
) -> Response {
    // Create adapter with no-op implementations for now
    // In production, these would be configurable
    let adapter = OpenResponsesAdapter::new(
        Arc::new(NoOpResponseStore),
        Arc::new(NoOpToolExecutor),
    );

    // Convert the Responses request to a Chat Completions request
    let chat_request = match adapter.to_chat_request(&request).await {
        Ok(req) => req,
        Err(e) => {
            error!(error = %e, "Failed to convert responses request to chat completions");
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("Failed to process request: {}", e),
            );
        }
    };

    // Check if streaming is requested
    if request.stream == Some(true) {
        // For streaming, we need different handling
        // TODO: Implement streaming adapter using StreamingState
        warn!("Streaming adapter mode not yet implemented, falling back to non-streaming");
    }

    // Serialize the chat request
    let body_bytes = match serde_json::to_vec(&chat_request) {
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

    // Forward to Chat Completions endpoint
    let response = forward_request_raw(state, headers, "/v1/chat/completions", body_bytes).await;

    // Check if the response is successful
    if !response.status().is_success() {
        // Pass through error responses
        return response;
    }

    // Parse the response body as ChatCompletionResponse
    let (parts, body) = response.into_parts();
    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!(error = %e, "Failed to read response body");
            return error_response(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "Failed to read upstream response",
            );
        }
    };

    let chat_response: ChatCompletionResponse = match serde_json::from_slice(&body_bytes) {
        Ok(resp) => resp,
        Err(e) => {
            error!(error = %e, "Failed to parse chat completions response");
            // Log some of the response for debugging
            if let Ok(text) = std::str::from_utf8(&body_bytes) {
                debug!(response_preview = &text[..text.len().min(500)], "Response body preview");
            }
            return error_response(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "Failed to parse upstream response",
            );
        }
    };

    // Convert to Responses format
    let responses_response = adapter.to_responses_response(&chat_response, &request.model);

    info!(
        response_id = %responses_response.id,
        status = ?responses_response.status,
        output_items = responses_response.output.len(),
        "Adapter conversion complete"
    );

    // Return the converted response
    let response_bytes = match serde_json::to_vec(&responses_response) {
        Ok(bytes) => bytes,
        Err(e) => {
            error!(error = %e, "Failed to serialize responses response");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Failed to serialize response",
            );
        }
    };

    Response::builder()
        .status(parts.status)
        .header("content-type", "application/json")
        .body(Body::from(response_bytes))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Failed to build response",
            )
        })
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
    debug!(
        model = %request.model,
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

    forward_request(state, headers, "/v1/embeddings", body_bytes).await
}

/// Forward a validated request to the upstream provider
async fn forward_request<T: HttpClient + Clone + Send + Sync + 'static>(
    state: AppState<T>,
    headers: HeaderMap,
    path: &str,
    body_bytes: Vec<u8>,
) -> Response {
    forward_request_raw(state, headers, path, body_bytes).await
}

/// Forward a validated request to the upstream provider, returning the raw response
async fn forward_request_raw<T: HttpClient + Clone + Send + Sync + 'static>(
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
    let mut request_builder = Request::builder()
        .method("POST")
        .uri(path);

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

/// Custom rejection handler for JSON parsing errors
pub async fn handle_json_rejection(
    err: axum::extract::rejection::JsonRejection,
) -> impl IntoResponse {
    let (status, error_type, message) = match err {
        axum::extract::rejection::JsonRejection::JsonDataError(e) => {
            warn!(error = %e, "Invalid JSON data");
            (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Invalid JSON: {}", e),
            )
        }
        axum::extract::rejection::JsonRejection::JsonSyntaxError(e) => {
            warn!(error = %e, "JSON syntax error");
            (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("JSON syntax error: {}", e),
            )
        }
        axum::extract::rejection::JsonRejection::MissingJsonContentType(e) => {
            warn!(error = %e, "Missing content type");
            (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "invalid_request_error",
                "Content-Type must be application/json".to_string(),
            )
        }
        axum::extract::rejection::JsonRejection::BytesRejection(e) => {
            warn!(error = %e, "Failed to read request body");
            (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Failed to read request body".to_string(),
            )
        }
        _ => {
            error!("Unknown JSON rejection");
            (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Invalid request".to_string(),
            )
        }
    };

    let body = json!({
        "error": {
            "type": error_type,
            "message": message
        }
    });

    (status, Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_response_format() {
        let response = error_response(StatusCode::BAD_REQUEST, "invalid_request_error", "Test error");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
