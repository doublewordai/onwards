//! Typed request handlers for strict mode
//!
//! These handlers validate requests using Axum's Json extractor (which uses serde)
//! before forwarding to the upstream provider.

use super::adapter::{OpenResponsesAdapter, PendingToolCall};
use super::schemas::chat_completions::{
    ChatCompletionRequest, ChatCompletionResponse, ChatMessage, FunctionCall, ToolCall,
};
use super::schemas::embeddings::EmbeddingsRequest;
use super::schemas::responses::ResponsesRequest;
use super::streaming::{StreamingState, parse_chat_chunk};
use crate::AppState;
use crate::client::HttpClient;
use crate::handlers::target_message_handler;
use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::json;
use tracing::{debug, error, info, trace, warn};

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
///
/// This function implements the full Open Responses adapter flow including tool loop orchestration:
/// 1. Convert Responses request to Chat Completions request
/// 2. Forward to upstream
/// 3. If response requires tool action:
///    a. Execute tools via ToolExecutor if handled
///    b. If any tools are unhandled, return to client with requires_action status
///    c. Add tool results to messages and loop back to step 2
/// 4. Continue until completion or max iterations reached
async fn handle_adapter_request<T: HttpClient + Clone + Send + Sync + 'static>(
    state: AppState<T>,
    headers: HeaderMap,
    request: ResponsesRequest,
) -> Response {
    let adapter =
        OpenResponsesAdapter::new(state.response_store.clone(), state.tool_executor.clone());

    // Convert the Responses request to a Chat Completions request
    let mut chat_request = match adapter.to_chat_request(&request).await {
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
        debug!("Using streaming adapter mode");
        return handle_streaming_adapter_request(state, headers, request, chat_request).await;
    }

    // Tool loop orchestration for non-streaming requests
    let max_iterations = adapter.max_iterations();
    let mut iteration = 0;

    loop {
        iteration += 1;
        debug!(
            iteration = iteration,
            max = max_iterations,
            "Tool loop iteration"
        );

        // Serialize the chat request for non-streaming
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
        let response = forward_request_raw(
            state.clone(),
            headers.clone(),
            "/v1/chat/completions",
            body_bytes,
        )
        .await;

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
                    debug!(
                        response_preview = &text[..text.len().min(500)],
                        "Response body preview"
                    );
                }
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "Failed to parse upstream response",
                );
            }
        };

        // Check if the response requires tool action
        if adapter.requires_tool_action(&chat_response) && iteration < max_iterations {
            // Extract tool calls
            let tool_calls = adapter.extract_tool_calls(&chat_response);
            let tool_names: Vec<&str> = tool_calls.iter().map(|tc| tc.name.as_str()).collect();
            info!(
                iteration,
                tools = ?tool_names,
                "Model requested tool calls"
            );

            // Execute tool calls
            let results = adapter.execute_tool_calls(&tool_calls).await;

            // Check if there are unhandled tools
            if adapter.has_unhandled_tools(&results) {
                info!(
                    iteration,
                    tools = ?tool_names,
                    "Returning unhandled tools to client"
                );
                // Return to client with requires_action status
                let responses_response = adapter.to_responses_response(&chat_response, &request);

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

                return Response::builder()
                    .status(parts.status)
                    .header("content-type", "application/json")
                    .body(Body::from(response_bytes))
                    .unwrap_or_else(|_| {
                        error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "server_error",
                            "Failed to build response",
                        )
                    });
            }

            // All tools handled - add results to messages and continue loop
            debug!(iteration, "All tools handled, continuing loop");

            // Get the assistant message from the response
            if let Some(choice) = chat_response.choices.first() {
                adapter.add_tool_results_to_messages(
                    &mut chat_request.messages,
                    &choice.message,
                    &results,
                );
            }

            // Continue to next iteration
            continue;
        }

        // No tool action required or max iterations reached - return final response
        if adapter.requires_tool_action(&chat_response) && iteration >= max_iterations {
            warn!(
                iteration,
                max = max_iterations,
                "Tool loop reached max iterations, returning incomplete response"
            );
        }

        let responses_response = adapter.to_responses_response(&chat_response, &request);

        info!(
            response_id = %responses_response.id,
            status = ?responses_response.status,
            output_items = responses_response.output.len(),
            iterations = iteration,
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

        return Response::builder()
            .status(parts.status)
            .header("content-type", "application/json")
            .body(Body::from(response_bytes))
            .unwrap_or_else(|_| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    "Failed to build response",
                )
            });
    }
}

/// Handle a streaming request using the Open Responses adapter
///
/// Transforms the upstream Chat Completions SSE stream into Open Responses
/// semantic events. Supports tool loop orchestration: when the upstream
/// finishes with `tool_calls`, tools are executed server-side, results are
/// appended to messages, and a new streaming request is made. This repeats
/// until the upstream finishes without tool calls or max iterations is reached.
async fn handle_streaming_adapter_request<T: HttpClient + Clone + Send + Sync + 'static>(
    state: AppState<T>,
    headers: HeaderMap,
    request: ResponsesRequest,
    mut chat_request: ChatCompletionRequest,
) -> Response {
    // Ensure stream is enabled on the chat request
    chat_request.stream = Some(true);

    // Serialize the chat request
    let body_bytes = match serde_json::to_vec(&chat_request) {
        Ok(bytes) => bytes,
        Err(e) => {
            error!(error = %e, "Failed to serialize streaming chat completions request");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Failed to process request",
            );
        }
    };

    // Make the first request outside the stream block so we can inspect
    // the HTTP status and content-type before committing to SSE.
    let response = forward_request_raw(
        state.clone(),
        headers.clone(),
        "/v1/chat/completions",
        body_bytes,
    )
    .await;

    if !response.status().is_success() {
        return response;
    }

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !content_type.contains("text/event-stream") {
        warn!(
            content_type = content_type,
            "Expected SSE stream but got different content type"
        );
        return response;
    }

    let (parts, first_body) = response.into_parts();
    let request_model = request.model.clone();

    let adapter =
        OpenResponsesAdapter::new(state.response_store.clone(), state.tool_executor.clone());
    let max_iterations = adapter.max_iterations();

    // Build a transformed stream that converts Chat Completions chunks to
    // Open Responses events, with tool loop support.
    let transformed_stream = async_stream::stream! {
        let mut streaming_state = StreamingState::new(&request);
        let mut messages = chat_request.messages.clone();
        let mut current_body = Some(first_body);
        let mut iteration = 0u32;

        while let Some(body) = current_body.take() {
            iteration += 1;
            let byte_stream = body.into_data_stream();
            let mut buffer = String::new();
            let mut last_finish_reason: Option<String> = None;

            let pinned_stream = std::pin::pin!(byte_stream);
            let mut stream = pinned_stream;

            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(bytes) => {
                        if let Ok(text) = std::str::from_utf8(&bytes) {
                            buffer.push_str(text);
                        } else {
                            continue;
                        }

                        while let Some(event_end) = buffer.find("\n\n") {
                            let event_text = buffer[..event_end].to_string();
                            buffer = buffer[event_end + 2..].to_string();

                            for line in event_text.lines() {
                                if let Some(data) = line.strip_prefix("data: ") {
                                    if data.trim() == "[DONE]" {
                                        trace!("Received [DONE] marker");
                                        continue;
                                    }

                                    if let Some(chunk) = parse_chat_chunk(data) {
                                        // Track finish_reason for tool loop decision
                                        for choice in &chunk.choices {
                                            if let Some(ref reason) = choice.finish_reason {
                                                last_finish_reason = Some(reason.clone());
                                            }
                                        }

                                        trace!(chunk_id = %chunk.id, "Processing chat chunk");
                                        let events = streaming_state.process_chunk(&chunk);
                                        for event in events {
                                            yield Ok::<_, std::io::Error>(event.to_sse().into_bytes());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "Error reading stream");
                        break;
                    }
                }
            }

            // Stream for this iteration is exhausted. Check if we need to
            // execute tools and loop.
            if last_finish_reason.as_deref() == Some("tool_calls") && iteration < max_iterations {
                let pending = streaming_state.extract_tool_calls();
                if pending.is_empty() {
                    debug!("finish_reason was tool_calls but no tool calls found");
                    break;
                }

                let tool_calls: Vec<PendingToolCall> = pending
                    .into_iter()
                    .map(|(id, name, args)| PendingToolCall {
                        id,
                        name,
                        arguments: args,
                    })
                    .collect();

                let tool_names: Vec<&str> = tool_calls.iter().map(|tc| tc.name.as_str()).collect();
                info!(
                    iteration,
                    tools = ?tool_names,
                    "Streaming: model requested tool calls"
                );

                let results = adapter.execute_tool_calls(&tool_calls).await;

                // If any tools are unhandled, stop looping — the client
                // already has the tool_call events and can act on them.
                if adapter.has_unhandled_tools(&results) {
                    info!(
                        iteration,
                        tools = ?tool_names,
                        "Streaming: returning unhandled tools to client"
                    );
                    break;
                }

                // Build the assistant message (with tool calls) to append
                // to the conversation for the next iteration.
                let assistant_msg = ChatMessage {
                    role: "assistant".to_string(),
                    content: None,
                    name: None,
                    tool_calls: Some(
                        tool_calls
                            .iter()
                            .map(|tc| ToolCall {
                                id: tc.id.clone(),
                                call_type: "function".to_string(),
                                function: FunctionCall {
                                    name: tc.name.clone(),
                                    arguments: tc.arguments.clone(),
                                },
                            })
                            .collect(),
                    ),
                    tool_call_id: None,
                    extra: None,
                };

                adapter.add_tool_results_to_messages(&mut messages, &assistant_msg, &results);

                // Advance the streaming state so new items get fresh indices.
                streaming_state.prepare_next_iteration();

                // Build and send the next streaming request.
                let mut next_request = chat_request.clone();
                next_request.messages = messages.clone();

                let body_bytes = match serde_json::to_vec(&next_request) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        error!(error = %e, "Failed to serialize next tool-loop iteration");
                        break;
                    }
                };

                let next_response = forward_request_raw(
                    state.clone(),
                    headers.clone(),
                    "/v1/chat/completions",
                    body_bytes,
                )
                .await;

                if !next_response.status().is_success() {
                    error!(
                        status = %next_response.status(),
                        "Upstream error during streaming tool loop"
                    );
                    break;
                }

                let (_, next_body) = next_response.into_parts();
                current_body = Some(next_body);
                continue;
            }

            // No tool calls or max iterations reached — done.
            break;
        }

        // Finalize: emit done events for any remaining items + response.completed
        let final_events = streaming_state.finalize();
        for event in final_events {
            yield Ok::<_, std::io::Error>(event.to_sse().into_bytes());
        }

        yield Ok::<_, std::io::Error>("data: [DONE]\n\n".to_string().into_bytes());
    };

    info!(model = %request_model, "Streaming adapter response started");

    Response::builder()
        .status(parts.status)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Body::from_stream(transformed_stream))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Failed to build streaming response",
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

    // Remove Accept-Encoding so the upstream returns uncompressed responses.
    // The adapter needs to parse response bodies as JSON, and hyper does not
    // automatically decompress gzip/br/zstd content.
    headers.remove(axum::http::header::ACCEPT_ENCODING);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_response_format() {
        let response = error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Test error",
        );
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
