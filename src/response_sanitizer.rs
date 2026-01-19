//! Response sanitization module for OpenAI-compatible API responses
//!
//! This module provides functionality to sanitize responses from various LLM providers,
//! ensuring they conform to the strict OpenAI API schema. It supports multiple endpoints
//! and handles both streaming (Server-Sent Events) and non-streaming (JSON) responses.
//!
//! ## Supported Endpoints
//!
//! - **`/v1/chat/completions`**: Chat completions (streaming and non-streaming)
//! - **`/v1/embeddings`**: Text embeddings generation
//! - **`/v1/audio/speech`**: Text-to-speech (binary passthrough)
//! - **`/v1/moderations`**: Content moderation
//! - **`/v1/responses`**: Generic responses endpoint
//!
//! ## Features
//!
//! - **Field filtering**: Removes provider-specific fields (e.g., `provider`, `cost`, `custom_metadata`)
//! - **Model rewriting**: Replaces the upstream model name with the client's requested model
//! - **Lenient parsing**: Handles missing fields with sensible defaults
//! - **Streaming support**: Processes SSE chunks in real-time without buffering
//! - **Binary passthrough**: Audio responses pass through unchanged
//!
//! ## Example
//!
//! ```rust
//! use onwards::response_sanitizer::ResponseSanitizer;
//!
//! let sanitizer = ResponseSanitizer {
//!     original_model: Some("gpt-4".to_string()),
//! };
//!
//! // Sanitize a chat completion response (removes extra fields, rewrites model to "gpt-4")
//! // let sanitized = sanitizer.sanitize("/v1/chat/completions", headers, body)?;
//!
//! // Sanitize an embeddings response
//! // let sanitized = sanitizer.sanitize("/v1/embeddings", headers, body)?;
//!
//! // Audio responses pass through unchanged
//! // let sanitized = sanitizer.sanitize("/v1/audio/speech", headers, body)?; // Returns None
//! ```

use axum::body::Bytes;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Represents OpenAI API endpoints that support response sanitization
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SanitizableEndpoint {
    /// `/v1/chat/completions` - Chat completion endpoint
    ChatCompletions,
    /// `/v1/embeddings` - Text embeddings endpoint
    Embeddings,
    /// `/v1/audio/speech` - Text-to-speech endpoint (binary audio)
    AudioSpeech,
    /// `/v1/moderations` - Content moderation endpoint
    Moderations,
    /// `/v1/responses` - Responses endpoint
    Responses,
}

impl SanitizableEndpoint {
    /// Attempts to match a request path to a sanitizable endpoint
    ///
    /// # Arguments
    ///
    /// * `path` - The request path (e.g., "/v1/chat/completions")
    ///
    /// # Returns
    ///
    /// * `Some(endpoint)` if the path matches a known sanitizable endpoint
    /// * `None` if the path is not applicable for sanitization
    pub fn from_path(path: &str) -> Option<Self> {
        if path.contains("/chat/completions") {
            Some(Self::ChatCompletions)
        } else if path.contains("/embeddings") {
            Some(Self::Embeddings)
        } else if path.contains("/audio/speech") {
            Some(Self::AudioSpeech)
        } else if path.contains("/moderations") {
            Some(Self::Moderations)
        } else if path.contains("/responses") {
            Some(Self::Responses)
        } else {
            None
        }
    }

    /// Check if this endpoint supports streaming responses
    pub fn supports_streaming(&self) -> bool {
        matches!(
            self,
            Self::ChatCompletions | Self::AudioSpeech | Self::Responses
        )
    }
}

/// Lenient wrapper for OpenAI chat completion responses.
///
/// Deserializes with defaults for missing fields and ignores extra provider-specific fields.
/// Uses `#[serde(flatten)]` to capture unknown fields and `skip_serializing` to drop them.
#[derive(Debug, Deserialize, Serialize)]
struct LenientChatCompletion {
    #[serde(default = "default_id")]
    id: String,
    #[serde(default = "default_object")]
    object: String,
    #[serde(default = "default_created")]
    created: u32,
    model: String,
    choices: Vec<LenientChoice>,
    #[serde(default)]
    usage: Option<LenientUsage>,
    #[serde(default)]
    system_fingerprint: Option<String>,
    #[serde(default)]
    service_tier: Option<String>,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LenientChoice {
    #[serde(default)]
    index: u64,
    message: serde_json::Value,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    logprobs: Option<serde_json::Value>,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LenientUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    audio_tokens: u64,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: u64,
    #[serde(default)]
    audio_tokens: u64,
    #[serde(default)]
    accepted_prediction_tokens: u64,
    #[serde(default)]
    rejected_prediction_tokens: u64,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LenientStreamChunk {
    #[serde(default = "default_id")]
    id: String,
    #[serde(default = "default_stream_object")]
    object: String,
    #[serde(default = "default_created")]
    created: u32,
    model: String,
    choices: Vec<LenientStreamChoice>,
    #[serde(default)]
    usage: Option<LenientUsage>,
    #[serde(default)]
    system_fingerprint: Option<String>,
    #[serde(default)]
    service_tier: Option<String>,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LenientStreamChoice {
    #[serde(default)]
    index: u64,
    #[serde(default)]
    delta: Option<serde_json::Value>,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    logprobs: Option<serde_json::Value>,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

fn default_id() -> String {
    "chatcmpl-unknown".to_string()
}

fn default_object() -> String {
    "chat.completion".to_string()
}

fn default_stream_object() -> String {
    "chat.completion.chunk".to_string()
}

fn default_created() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32
}

/// Lenient wrapper for OpenAI embeddings response
///
/// Deserializes with defaults for missing fields and ignores extra provider-specific fields.
#[derive(Debug, Deserialize, Serialize)]
struct LenientEmbeddingsResponse {
    #[serde(default = "default_list_object")]
    object: String,
    data: Vec<LenientEmbedding>,
    model: String,
    #[serde(default)]
    usage: Option<LenientEmbeddingsUsage>,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LenientEmbedding {
    #[serde(default = "default_embedding_object")]
    object: String,
    /// Embedding vector - can be Vec<f64> or base64-encoded string
    embedding: serde_json::Value,
    #[serde(default)]
    index: u64,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LenientEmbeddingsUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

fn default_list_object() -> String {
    "list".to_string()
}

fn default_embedding_object() -> String {
    "embedding".to_string()
}

/// Lenient wrapper for OpenAI responses endpoint
///
/// Generic response structure that handles various response formats.
#[derive(Debug, Deserialize, Serialize)]
struct LenientResponsesResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    object: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    results: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    choices: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usage: Option<serde_json::Value>,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

/// Lenient wrapper for OpenAI moderations response
///
/// Deserializes with defaults for missing fields and ignores extra provider-specific fields.
#[derive(Debug, Deserialize, Serialize)]
struct LenientModerationsResponse {
    id: String,
    model: String,
    results: Vec<LenientModerationResult>,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LenientModerationResult {
    flagged: bool,
    /// Categories as generic Value to handle different provider schemas
    categories: serde_json::Value,
    /// Category scores as generic Value to handle different provider schemas
    category_scores: serde_json::Value,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

/// Response sanitizer for OpenAI chat completion responses
///
/// Enforces strict OpenAI API schema compliance by:
/// - Removing provider-specific fields
/// - Rewriting the model field to match the client's original request
/// - Supporting both streaming (SSE) and non-streaming responses
pub struct ResponseSanitizer {
    /// The model name originally requested by the client
    pub original_model: Option<String>,
}

impl ResponseSanitizer {
    /// Sanitizes an OpenAI API response
    ///
    /// # Arguments
    ///
    /// * `path` - The request path (e.g., "/v1/chat/completions", "/v1/embeddings")
    /// * `headers` - HTTP response headers from the upstream provider
    /// * `body` - Raw response body bytes
    ///
    /// # Returns
    ///
    /// * `Ok(Some(Bytes))` - Sanitized response body
    /// * `Ok(None)` - Path not applicable for sanitization
    /// * `Err(String)` - Sanitization failed with error message
    pub fn sanitize(
        &self,
        path: &str,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<Option<Bytes>, String> {
        // Determine which endpoint this is
        let endpoint = match SanitizableEndpoint::from_path(path) {
            Some(ep) => ep,
            None => return Ok(None), // Not a sanitizable endpoint
        };

        // Detect streaming via Content-Type header
        let is_streaming = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("text/event-stream"))
            .unwrap_or(false);

        // Route to appropriate handler based on endpoint and streaming
        match (endpoint, is_streaming) {
            (SanitizableEndpoint::ChatCompletions, false) => self.sanitize_chat_completion(body),
            (SanitizableEndpoint::ChatCompletions, true) => {
                self.sanitize_chat_completion_stream(body)
            }
            (SanitizableEndpoint::Embeddings, _) => self.sanitize_embeddings(body),
            (SanitizableEndpoint::AudioSpeech, _) => self.sanitize_audio_speech(body),
            (SanitizableEndpoint::Moderations, _) => self.sanitize_moderations(body),
            (SanitizableEndpoint::Responses, false) => self.sanitize_responses(body),
            (SanitizableEndpoint::Responses, true) => self.sanitize_responses_stream(body),
        }
    }

    /// Sanitizes a chat completion JSON response (non-streaming)
    fn sanitize_chat_completion(&self, body: &[u8]) -> Result<Option<Bytes>, String> {
        // Deserialize using lenient types that ignore unknown fields and provide defaults
        let mut completion: LenientChatCompletion = serde_json::from_slice(body)
            .map_err(|e| format!("Failed to parse response as JSON: {}", e))?;

        // Rewrite model field if original model provided
        if let Some(ref original) = self.original_model {
            completion.model = original.clone();
        }

        // Serialize back to clean JSON (unknown fields are automatically dropped)
        let sanitized_bytes = serde_json::to_vec(&completion)
            .map_err(|e| format!("Failed to serialize sanitized response: {}", e))?;

        Ok(Some(Bytes::from(sanitized_bytes)))
    }

    /// Sanitizes a chat completion streaming Server-Sent Events (SSE) response
    ///
    /// SSE format: `data: {...}\n\ndata: {...}\n\ndata: [DONE]\n\n`
    pub fn sanitize_chat_completion_stream(&self, body: &[u8]) -> Result<Option<Bytes>, String> {
        let body_str = std::str::from_utf8(body)
            .map_err(|e| format!("Invalid UTF-8 in streaming response: {}", e))?;

        let mut sanitized_lines = Vec::new();

        for line in body_str.lines() {
            if let Some(data_part) = line.strip_prefix("data: ") {
                // Skip "data: " prefix

                if data_part.trim() == "[DONE]" {
                    // Preserve [DONE] marker as-is
                    sanitized_lines.push(line.to_string());
                } else {
                    // Deserialize using lenient types that ignore unknown fields and provide defaults
                    let mut chunk: LenientStreamChunk = serde_json::from_str(data_part)
                        .map_err(|e| format!("Failed to parse stream chunk: {}", e))?;

                    // Rewrite model field if original model provided
                    if let Some(ref original) = self.original_model {
                        chunk.model = original.clone();
                    }

                    // Serialize the sanitized chunk (unknown fields are automatically dropped)
                    let sanitized_json = serde_json::to_string(&chunk)
                        .map_err(|e| format!("Failed to serialize stream chunk: {}", e))?;

                    sanitized_lines.push(format!("data: {}", sanitized_json));
                }
            } else if line.is_empty() {
                // Preserve empty lines (SSE delimiter)
                sanitized_lines.push(String::new());
            }
            // Ignore other lines (comments, etc.)
        }

        // Join with \n and ensure the output preserves trailing newlines from input
        // SSE format requires messages to end with \n\n
        let mut sanitized_body = sanitized_lines.join("\n");

        // .lines() strips trailing newlines, so we need to restore them
        // Count trailing newlines in both input and current output
        let input_trailing = body_str.chars().rev().take_while(|&c| c == '\n').count();
        let output_trailing = sanitized_body
            .chars()
            .rev()
            .take_while(|&c| c == '\n')
            .count();

        // Add the difference to match input's trailing newlines
        for _ in output_trailing..input_trailing {
            sanitized_body.push('\n');
        }

        Ok(Some(Bytes::from(sanitized_body)))
    }

    /// Sanitizes an embeddings API response
    ///
    /// Removes provider-specific fields and rewrites the model field if needed.
    fn sanitize_embeddings(&self, body: &[u8]) -> Result<Option<Bytes>, String> {
        // Deserialize using lenient types that ignore unknown fields and provide defaults
        let mut response: LenientEmbeddingsResponse = serde_json::from_slice(body)
            .map_err(|e| format!("Failed to parse embeddings response as JSON: {}", e))?;

        // Rewrite model field if original model provided
        if let Some(ref original) = self.original_model {
            response.model = original.clone();
        }

        // Serialize back to clean JSON (unknown fields are automatically dropped)
        let sanitized_bytes = serde_json::to_vec(&response)
            .map_err(|e| format!("Failed to serialize sanitized embeddings response: {}", e))?;

        Ok(Some(Bytes::from(sanitized_bytes)))
    }

    /// Handles audio/speech API responses (binary passthrough)
    ///
    /// Audio responses contain binary data (mp3, opus, aac, flac, wav, pcm) and should
    /// not be parsed or modified. This method returns `None` to pass through the original
    /// response unchanged.
    fn sanitize_audio_speech(&self, _body: &[u8]) -> Result<Option<Bytes>, String> {
        // Binary audio data - pass through unchanged
        Ok(None)
    }

    /// Sanitizes a moderations API response
    ///
    /// Removes provider-specific fields and rewrites the model field if needed.
    fn sanitize_moderations(&self, body: &[u8]) -> Result<Option<Bytes>, String> {
        // Deserialize using lenient types that ignore unknown fields
        let mut response: LenientModerationsResponse = serde_json::from_slice(body)
            .map_err(|e| format!("Failed to parse moderations response as JSON: {}", e))?;

        // Rewrite model field if original model provided
        if let Some(ref original) = self.original_model {
            response.model = original.clone();
        }

        // Serialize back to clean JSON (unknown fields are automatically dropped)
        let sanitized_bytes = serde_json::to_vec(&response)
            .map_err(|e| format!("Failed to serialize sanitized moderations response: {}", e))?;

        Ok(Some(Bytes::from(sanitized_bytes)))
    }

    /// Sanitizes a responses API response
    ///
    /// Removes provider-specific fields and rewrites the model field if needed.
    /// Handles generic response formats with flexible structure.
    fn sanitize_responses(&self, body: &[u8]) -> Result<Option<Bytes>, String> {
        // Deserialize using lenient types that ignore unknown fields
        let mut response: LenientResponsesResponse = serde_json::from_slice(body)
            .map_err(|e| format!("Failed to parse responses endpoint response as JSON: {}", e))?;

        // Rewrite model field if original model provided
        if let Some(ref original) = self.original_model {
            response.model = original.clone();
        }

        // Serialize back to clean JSON (unknown fields are automatically dropped)
        let sanitized_bytes = serde_json::to_vec(&response)
            .map_err(|e| format!("Failed to serialize sanitized responses response: {}", e))?;

        Ok(Some(Bytes::from(sanitized_bytes)))
    }

    /// Sanitizes a streaming responses API response (SSE format)
    ///
    /// Similar to chat completion streaming, but for generic responses endpoint.
    /// SSE format: `data: {...}\n\ndata: {...}\n\ndata: [DONE]\n\n`
    fn sanitize_responses_stream(&self, body: &[u8]) -> Result<Option<Bytes>, String> {
        let body_str = std::str::from_utf8(body)
            .map_err(|e| format!("Invalid UTF-8 in streaming response: {}", e))?;

        let mut sanitized_lines = Vec::new();

        for line in body_str.lines() {
            if let Some(data_part) = line.strip_prefix("data: ") {
                if data_part.trim() == "[DONE]" {
                    // Preserve [DONE] marker as-is
                    sanitized_lines.push(line.to_string());
                } else {
                    // Deserialize using lenient types
                    let mut chunk: LenientResponsesResponse = serde_json::from_str(data_part)
                        .map_err(|e| format!("Failed to parse stream chunk: {}", e))?;

                    // Rewrite model field if original model provided
                    if let Some(ref original) = self.original_model {
                        chunk.model = original.clone();
                    }

                    // Serialize the sanitized chunk
                    let sanitized_json = serde_json::to_string(&chunk)
                        .map_err(|e| format!("Failed to serialize stream chunk: {}", e))?;

                    sanitized_lines.push(format!("data: {}", sanitized_json));
                }
            } else if line.is_empty() {
                // Preserve empty lines (SSE delimiter)
                sanitized_lines.push(String::new());
            }
        }

        // Join with \n and restore trailing newlines
        let mut sanitized_body = sanitized_lines.join("\n");

        // Restore trailing newlines
        let input_trailing = body_str.chars().rev().take_while(|&c| c == '\n').count();
        let output_trailing = sanitized_body
            .chars()
            .rev()
            .take_while(|&c| c == '\n')
            .count();

        for _ in output_trailing..input_trailing {
            sanitized_body.push('\n');
        }

        Ok(Some(Bytes::from(sanitized_body)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn test_non_streaming_sanitization() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("gpt-4".to_string()),
        };

        let response_json = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "gpt-4-turbo",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 9,
                "completion_tokens": 2,
                "total_tokens": 11
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/chat/completions", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(sanitized["model"], "gpt-4");
    }

    #[test]
    fn test_streaming_sanitization() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("gpt-4".to_string()),
        };

        let sse_response = "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1677652288,\"model\":\"gpt-4-turbo\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";

        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );

        let result = sanitizer
            .sanitize("/v1/chat/completions", &headers, sse_response.as_bytes())
            .unwrap()
            .unwrap();

        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(result_str.contains("\"model\":\"gpt-4\""));
        assert!(result_str.contains("data: [DONE]"));

        // Verify SSE format: should end with \n\n for proper message termination
        assert!(
            result_str.ends_with("\n\n"),
            "SSE response should end with \\n\\n, got: {:?}",
            &result_str[result_str.len().saturating_sub(10)..]
        );

        // Verify messages are separated by \n\n
        assert!(
            result_str.contains("}\n\ndata:"),
            "SSE messages should be separated by \\n\\n"
        );

        // Verify exactly 2 trailing newlines (not 3 or more)
        let trailing_count = result_str.chars().rev().take_while(|&c| c == '\n').count();
        assert_eq!(
            trailing_count, 2,
            "Should have exactly 2 trailing newlines, got {}",
            trailing_count
        );
    }

    #[test]
    fn test_streaming_multiple_chunks() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("gpt-4".to_string()),
        };

        // Multiple chunks with proper SSE format
        let sse_response = "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1677652288,\"model\":\"gpt-4-turbo\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1677652289,\"model\":\"gpt-4-turbo\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" World\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";

        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );

        let result = sanitizer
            .sanitize("/v1/chat/completions", &headers, sse_response.as_bytes())
            .unwrap()
            .unwrap();

        let result_str = std::str::from_utf8(&result).unwrap();

        // All chunks should have model rewritten
        let chunk_count = result_str.matches("\"model\":\"gpt-4\"").count();
        assert_eq!(chunk_count, 2, "Both chunks should have model rewritten");

        // Should end with \n\n
        assert!(result_str.ends_with("\n\n"));

        // Should have three messages (2 chunks + [DONE])
        let message_count = result_str.matches("data:").count();
        assert_eq!(message_count, 3);
    }

    #[test]
    fn test_path_filtering() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let headers = HeaderMap::new();
        let result = sanitizer.sanitize("/v1/models", &headers, b"test").unwrap();

        assert!(result.is_none());
    }

    #[test]
    fn test_no_model_rewrite() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let response_json = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "gpt-4-turbo",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 9,
                "completion_tokens": 2,
                "total_tokens": 11
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/chat/completions", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(sanitized["model"], "gpt-4-turbo");
    }

    #[test]
    fn test_openrouter_response_sanitization() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("fred".to_string()),
        };

        // Actual OpenRouter response with extra fields
        let openrouter_response = r#"{
            "id":"gen-1768414327-n9nnQrvnhHU8HF0oRVKq",
            "provider":"DeepInfra",
            "model":"google/gemma-3-12b-it",
            "object":"chat.completion",
            "created":1768414327,
            "choices":[{
                "logprobs":null,
                "finish_reason":"stop",
                "native_finish_reason":"stop",
                "index":0,
                "message":{
                    "role":"assistant",
                    "content":"Hello! I'm doing well, thank you for asking! As an AI, I don't experience feelings like humans do, but I'm operating smoothly and ready to assist you. 😊 \n\nHow are *you* doing today?",
                    "refusal":null,
                    "reasoning":null
                }
            }],
            "usage":{
                "prompt_tokens":15,
                "completion_tokens":51,
                "total_tokens":66,
                "cost":0.00000723,
                "is_byok":false,
                "prompt_tokens_details":{
                    "cached_tokens":0,
                    "audio_tokens":0,
                    "video_tokens":0
                },
                "cost_details":{
                    "upstream_inference_cost":null,
                    "upstream_inference_prompt_cost":6e-7,
                    "upstream_inference_completions_cost":0.00000663
                },
                "completion_tokens_details":{
                    "reasoning_tokens":0,
                    "image_tokens":0
                }
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize(
                "/v1/chat/completions",
                &headers,
                openrouter_response.as_bytes(),
            )
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify model rewrite
        assert_eq!(sanitized["model"], "fred");

        // Verify OpenAI fields are preserved
        assert_eq!(sanitized["id"], "gen-1768414327-n9nnQrvnhHU8HF0oRVKq");
        assert_eq!(sanitized["object"], "chat.completion");
        assert_eq!(sanitized["created"], 1768414327);

        // Verify choices structure
        let choices = sanitized["choices"].as_array().unwrap();
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0]["index"], 0);
        assert_eq!(choices[0]["finish_reason"], "stop");
        assert_eq!(choices[0]["message"]["role"], "assistant");
        assert!(
            choices[0]["message"]["content"]
                .as_str()
                .unwrap()
                .starts_with("Hello!")
        );

        // Verify provider-specific fields are removed
        assert!(sanitized.get("provider").is_none());
        assert!(choices[0].get("native_finish_reason").is_none());

        // Verify logprobs is preserved (it's a valid OpenAI field, even when null)
        assert!(choices[0].get("logprobs").is_some());
        assert_eq!(choices[0]["logprobs"], serde_json::Value::Null);

        // Verify usage is preserved (but sanitized)
        assert!(sanitized.get("usage").is_some());
        let usage = &sanitized["usage"];
        assert_eq!(usage["prompt_tokens"], 15);
        assert_eq!(usage["completion_tokens"], 51);
        assert_eq!(usage["total_tokens"], 66);

        // Verify OpenRouter-specific usage fields are removed
        assert!(usage.get("cost").is_none());
        assert!(usage.get("is_byok").is_none());
        assert!(usage.get("cost_details").is_none());

        // Verify OpenAI token detail fields are preserved
        assert!(usage.get("prompt_tokens_details").is_some());
        assert!(usage.get("completion_tokens_details").is_some());

        // Verify the nested details contain valid OpenAI fields (invalid ones like video_tokens, image_tokens are removed)
        let prompt_details = &usage["prompt_tokens_details"];
        assert_eq!(prompt_details["cached_tokens"], 0);
        assert_eq!(prompt_details["audio_tokens"], 0);
        assert!(prompt_details.get("video_tokens").is_none()); // OpenRouter-specific, should be removed

        let completion_details = &usage["completion_tokens_details"];
        assert_eq!(completion_details["reasoning_tokens"], 0);
        assert!(completion_details.get("image_tokens").is_none()); // OpenRouter-specific, should be removed
    }

    #[test]
    fn test_embeddings_sanitization() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("text-embedding-ada-002".to_string()),
        };

        let response_json = r#"{
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": [0.1, 0.2, 0.3],
                    "index": 0
                }
            ],
            "model": "text-embedding-3-small",
            "usage": {
                "prompt_tokens": 10,
                "total_tokens": 10
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/embeddings", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();
        // Verify model rewrite
        assert_eq!(sanitized["model"], "text-embedding-ada-002");
        // Verify data structure preserved
        assert_eq!(sanitized["object"], "list");
        assert_eq!(sanitized["data"].as_array().unwrap().len(), 1);
        assert_eq!(sanitized["data"][0]["object"], "embedding");
        assert_eq!(sanitized["data"][0]["index"], 0);
        // Verify embedding values preserved
        assert_eq!(sanitized["data"][0]["embedding"][0], 0.1);
        assert_eq!(sanitized["data"][0]["embedding"][1], 0.2);
        assert_eq!(sanitized["data"][0]["embedding"][2], 0.3);
    }

    #[test]
    fn test_embeddings_removes_provider_fields() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("text-embedding-ada-002".to_string()),
        };

        // Response with OpenRouter-style provider-specific fields
        let response_json = r#"{
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": [0.1, 0.2],
                    "index": 0,
                    "custom_field": "remove_me"
                }
            ],
            "model": "text-embedding-3-small",
            "provider": "OpenRouter",
            "cost": 0.00001,
            "usage": {
                "prompt_tokens": 10,
                "total_tokens": 10,
                "cost": 0.00001,
                "processing_time_ms": 123
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/embeddings", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify provider-specific fields removed
        assert!(sanitized.get("provider").is_none());
        assert!(sanitized.get("cost").is_none());

        // Verify usage has provider fields removed
        let usage = &sanitized["usage"];
        assert_eq!(usage["prompt_tokens"], 10);
        assert_eq!(usage["total_tokens"], 10);
        assert!(usage.get("cost").is_none());
        assert!(usage.get("processing_time_ms").is_none());

        // Verify embedding custom fields removed
        assert!(sanitized["data"][0].get("custom_field").is_none());
    }

    #[test]
    fn test_embeddings_multiple_embeddings() {
        let sanitizer = ResponseSanitizer {
            original_model: None, // No model rewrite
        };

        let response_json = r#"{
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": [0.1, 0.2],
                    "index": 0
                },
                {
                    "object": "embedding",
                    "embedding": [0.3, 0.4],
                    "index": 1
                },
                {
                    "object": "embedding",
                    "embedding": [0.5, 0.6],
                    "index": 2
                }
            ],
            "model": "text-embedding-3-small",
            "usage": {
                "prompt_tokens": 30,
                "total_tokens": 30
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/embeddings", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify all embeddings preserved
        assert_eq!(sanitized["data"].as_array().unwrap().len(), 3);
        assert_eq!(sanitized["data"][0]["index"], 0);
        assert_eq!(sanitized["data"][1]["index"], 1);
        assert_eq!(sanitized["data"][2]["index"], 2);

        // Verify model not rewritten when original_model is None
        assert_eq!(sanitized["model"], "text-embedding-3-small");
    }

    #[test]
    fn test_embeddings_missing_usage() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("text-embedding-ada-002".to_string()),
        };

        // Response without usage field
        let response_json = r#"{
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": [0.1, 0.2],
                    "index": 0
                }
            ],
            "model": "text-embedding-3-small"
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/embeddings", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify sanitization works even without usage
        assert_eq!(sanitized["model"], "text-embedding-ada-002");
        assert_eq!(sanitized["object"], "list");
        assert!(sanitized.get("usage").is_none() || sanitized["usage"].is_null());
    }

    #[test]
    fn test_embeddings_base64_encoding() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        // Some providers may return base64-encoded embeddings
        let response_json = r#"{
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": "SGVsbG8gV29ybGQ=",
                    "index": 0
                }
            ],
            "model": "text-embedding-3-small",
            "usage": {
                "prompt_tokens": 10,
                "total_tokens": 10
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/embeddings", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify base64 string preserved
        assert_eq!(sanitized["data"][0]["embedding"], "SGVsbG8gV29ybGQ=");
    }

    #[test]
    fn test_audio_speech_binary_passthrough() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("tts-1".to_string()),
        };

        // Simulate binary audio data (not actual audio, just binary bytes)
        let binary_audio: Vec<u8> = vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, // Fake binary data
            0x4A, 0x46, 0x49, 0x46, 0x00, 0x01,
        ];

        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("audio/mpeg"));

        let result = sanitizer
            .sanitize("/v1/audio/speech", &headers, &binary_audio)
            .unwrap();

        // Should return None to pass through unchanged
        assert!(result.is_none());
    }

    #[test]
    fn test_audio_speech_different_formats() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let binary_data: Vec<u8> = vec![0x00, 0x01, 0x02, 0x03];

        // Test various audio content types
        let content_types = vec![
            "audio/mpeg",
            "audio/opus",
            "audio/aac",
            "audio/flac",
            "audio/wav",
            "audio/pcm",
            "application/octet-stream",
        ];

        for content_type in content_types {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static(content_type));

            let result = sanitizer
                .sanitize("/v1/audio/speech", &headers, &binary_data)
                .unwrap();

            // All should pass through
            assert!(
                result.is_none(),
                "Content-Type {} should pass through",
                content_type
            );
        }
    }

    #[test]
    fn test_audio_speech_path_matching() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let binary_data: Vec<u8> = vec![0xFF, 0xD8];
        let headers = HeaderMap::new();

        // Test path variations
        let result1 = sanitizer
            .sanitize("/v1/audio/speech", &headers, &binary_data)
            .unwrap();
        assert!(result1.is_none());

        let result2 = sanitizer
            .sanitize("/audio/speech", &headers, &binary_data)
            .unwrap();
        assert!(result2.is_none());
    }

    #[test]
    fn test_audio_speech_no_corruption() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("tts-1-hd".to_string()),
        };

        // Create a larger binary blob to simulate real audio
        let mut binary_audio: Vec<u8> = Vec::new();
        for i in 0..1000 {
            binary_audio.push((i % 256) as u8);
        }

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/audio/speech", &headers, &binary_audio)
            .unwrap();

        // Should return None (passthrough)
        assert!(result.is_none());
    }

    #[test]
    fn test_moderations_sanitization() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("text-moderation-stable".to_string()),
        };

        let response_json = r#"{
            "id": "modr-123",
            "model": "text-moderation-007",
            "results": [
                {
                    "flagged": false,
                    "categories": {
                        "hate": false,
                        "hate/threatening": false,
                        "harassment": false,
                        "self-harm": false,
                        "sexual": false,
                        "violence": false
                    },
                    "category_scores": {
                        "hate": 0.001,
                        "hate/threatening": 0.0001,
                        "harassment": 0.002,
                        "self-harm": 0.0001,
                        "sexual": 0.003,
                        "violence": 0.001
                    }
                }
            ]
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/moderations", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify model rewrite
        assert_eq!(sanitized["model"], "text-moderation-stable");

        // Verify structure preserved
        assert_eq!(sanitized["id"], "modr-123");
        assert_eq!(sanitized["results"].as_array().unwrap().len(), 1);

        // Verify categories and scores preserved
        let result_obj = &sanitized["results"][0];
        assert_eq!(result_obj["flagged"], false);
        assert!(result_obj.get("categories").is_some());
        assert!(result_obj.get("category_scores").is_some());
        assert_eq!(result_obj["categories"]["hate"], false);
        assert_eq!(result_obj["category_scores"]["hate"], 0.001);
    }

    #[test]
    fn test_moderations_removes_provider_fields() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        // Response with provider-specific fields
        let response_json = r#"{
            "id": "modr-123",
            "model": "text-moderation-007",
            "provider": "CustomProvider",
            "confidence_threshold": 0.95,
            "processing_time_ms": 45,
            "results": [
                {
                    "flagged": true,
                    "categories": {
                        "hate": true
                    },
                    "category_scores": {
                        "hate": 0.95
                    },
                    "model_version": "v2.1",
                    "custom_metadata": "remove_me"
                }
            ]
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/moderations", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify provider-specific fields removed at top level
        assert!(sanitized.get("provider").is_none());
        assert!(sanitized.get("confidence_threshold").is_none());
        assert!(sanitized.get("processing_time_ms").is_none());

        // Verify provider-specific fields removed from results
        let result_obj = &sanitized["results"][0];
        assert!(result_obj.get("model_version").is_none());
        assert!(result_obj.get("custom_metadata").is_none());

        // Verify OpenAI fields preserved
        assert_eq!(result_obj["flagged"], true);
        assert_eq!(result_obj["categories"]["hate"], true);
        assert_eq!(result_obj["category_scores"]["hate"], 0.95);
    }

    #[test]
    fn test_moderations_multiple_results() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("text-moderation-latest".to_string()),
        };

        let response_json = r#"{
            "id": "modr-456",
            "model": "text-moderation-007",
            "results": [
                {
                    "flagged": false,
                    "categories": {"hate": false},
                    "category_scores": {"hate": 0.01}
                },
                {
                    "flagged": true,
                    "categories": {"violence": true},
                    "category_scores": {"violence": 0.98}
                }
            ]
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/moderations", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify both results preserved
        assert_eq!(sanitized["results"].as_array().unwrap().len(), 2);
        assert_eq!(sanitized["results"][0]["flagged"], false);
        assert_eq!(sanitized["results"][1]["flagged"], true);

        // Verify model rewrite
        assert_eq!(sanitized["model"], "text-moderation-latest");
    }

    #[test]
    fn test_moderations_path_matching() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let response_json = r#"{
            "id": "modr-789",
            "model": "text-moderation-007",
            "results": [
                {
                    "flagged": false,
                    "categories": {},
                    "category_scores": {}
                }
            ]
        }"#;

        let headers = HeaderMap::new();

        // Test both path variations
        let result1 = sanitizer
            .sanitize("/v1/moderations", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();
        assert!(result1.len() > 0);

        let result2 = sanitizer
            .sanitize("/moderations", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();
        assert!(result2.len() > 0);
    }

    #[test]
    fn test_responses_sanitization() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("gpt-4".to_string()),
        };

        let response_json = r#"{
            "id": "resp-123",
            "model": "gpt-4-turbo",
            "object": "response",
            "data": {
                "content": "test response"
            },
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/responses", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify model rewrite
        assert_eq!(sanitized["model"], "gpt-4");

        // Verify structure preserved
        assert_eq!(sanitized["id"], "resp-123");
        assert_eq!(sanitized["object"], "response");
        assert!(sanitized.get("data").is_some());
        assert!(sanitized.get("usage").is_some());
    }

    #[test]
    fn test_responses_removes_provider_fields() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        // Response with provider-specific fields
        let response_json = r#"{
            "id": "resp-456",
            "model": "gpt-4-turbo",
            "object": "response",
            "provider": "OpenRouter",
            "cost": 0.001,
            "processing_time_ms": 250,
            "data": {
                "content": "test"
            },
            "custom_metadata": "should_be_removed"
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/responses", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify provider-specific fields removed
        assert!(sanitized.get("provider").is_none());
        assert!(sanitized.get("cost").is_none());
        assert!(sanitized.get("processing_time_ms").is_none());
        assert!(sanitized.get("custom_metadata").is_none());

        // Verify OpenAI fields preserved
        assert_eq!(sanitized["model"], "gpt-4-turbo");
        assert_eq!(sanitized["id"], "resp-456");
        assert!(sanitized.get("data").is_some());
    }

    #[test]
    fn test_responses_with_choices_array() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("custom-model".to_string()),
        };

        let response_json = r#"{
            "model": "provider-model",
            "choices": [
                {
                    "index": 0,
                    "text": "Response 1"
                },
                {
                    "index": 1,
                    "text": "Response 2"
                }
            ]
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/responses", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify model rewrite
        assert_eq!(sanitized["model"], "custom-model");

        // Verify choices array preserved
        assert!(sanitized.get("choices").is_some());
        let choices = sanitized["choices"].as_array().unwrap();
        assert_eq!(choices.len(), 2);
    }

    #[test]
    fn test_responses_with_results_array() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let response_json = r#"{
            "model": "test-model",
            "results": [
                {
                    "output": "result 1"
                },
                {
                    "output": "result 2"
                }
            ]
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/responses", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify results array preserved
        assert!(sanitized.get("results").is_some());
        let results = sanitized["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_responses_path_matching() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let response_json = r#"{
            "model": "test-model",
            "data": {"test": "value"}
        }"#;

        let headers = HeaderMap::new();

        // Test both path variations
        let result1 = sanitizer
            .sanitize("/v1/responses", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();
        assert!(result1.len() > 0);

        let result2 = sanitizer
            .sanitize("/responses", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();
        assert!(result2.len() > 0);
    }

    // Edge case tests for robustness

    #[test]
    fn test_embeddings_missing_model_field() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("backup-model".to_string()),
        };

        // Response without model field - should fail to deserialize
        let response_json = r#"{
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": [0.1, 0.2],
                    "index": 0
                }
            ]
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer.sanitize("/v1/embeddings", &headers, response_json.as_bytes());

        // Should fail because model is required
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing field"));
    }

    #[test]
    fn test_responses_empty_body() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let headers = HeaderMap::new();
        let result = sanitizer.sanitize("/v1/responses", &headers, b"{}");

        // Should fail because model field is required
        assert!(result.is_err());
    }

    #[test]
    fn test_malformed_json() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let malformed_json = b"{ invalid json ][";
        let headers = HeaderMap::new();

        let result = sanitizer.sanitize("/v1/embeddings", &headers, malformed_json);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to parse"));
    }

    #[test]
    fn test_embeddings_with_nested_provider_fields_in_data() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let response_json = r#"{
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": [0.1, 0.2],
                    "index": 0,
                    "provider_metadata": {
                        "cached": true,
                        "processing_time": 50
                    },
                    "custom_score": 0.95
                }
            ],
            "model": "text-embedding-ada-002"
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/embeddings", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify nested provider fields in data array items are removed
        let embedding = &sanitized["data"][0];
        assert!(embedding.get("provider_metadata").is_none());
        assert!(embedding.get("custom_score").is_none());

        // But standard fields should be preserved
        assert_eq!(embedding["index"], 0);
        assert_eq!(embedding["object"], "embedding");
    }

    #[test]
    fn test_responses_with_unicode_content() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("gpt-4".to_string()),
        };

        let response_json = r#"{
            "model": "gpt-4-turbo",
            "data": {
                "content": "Hello 世界 🌍 Émojis and spëcial çhars"
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/responses", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify Unicode content is preserved correctly
        assert!(
            sanitized["data"]["content"]
                .as_str()
                .unwrap()
                .contains("世界")
        );
        assert!(
            sanitized["data"]["content"]
                .as_str()
                .unwrap()
                .contains("🌍")
        );
        assert!(
            sanitized["data"]["content"]
                .as_str()
                .unwrap()
                .contains("Émojis")
        );
    }

    #[test]
    fn test_null_fields_vs_missing_fields() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let response_json = r#"{
            "model": "gpt-4",
            "id": null,
            "object": null,
            "data": {
                "value": null
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/responses", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Null values in optional fields are treated as missing and omitted
        // This is intentional sanitization behavior to clean up responses
        assert!(sanitized.get("id").is_none());
        assert!(sanitized.get("object").is_none());

        // But data field should be present (even though its value.null is preserved)
        assert!(sanitized.get("data").is_some());
        assert!(sanitized["data"]["value"].is_null());
    }

    #[test]
    fn test_very_large_embeddings_array() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        // Create a response with 100 embeddings
        let mut data_items = Vec::new();
        for i in 0..100 {
            data_items.push(format!(
                r#"{{"object": "embedding", "embedding": [0.1, 0.2], "index": {}}}"#,
                i
            ));
        }
        let response_json = format!(
            r#"{{"object": "list", "data": [{}], "model": "test-model"}}"#,
            data_items.join(",")
        );

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/embeddings", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify all 100 embeddings are preserved
        assert_eq!(sanitized["data"].as_array().unwrap().len(), 100);
    }

    #[test]
    fn test_moderations_with_nested_custom_fields() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let response_json = r#"{
            "id": "modr-123",
            "model": "text-moderation-007",
            "results": [
                {
                    "flagged": true,
                    "categories": {
                        "hate": true,
                        "provider_category_v2": false
                    },
                    "category_scores": {
                        "hate": 0.95,
                        "provider_score_v2": 0.1
                    },
                    "custom_analysis": {
                        "confidence": 0.99,
                        "processing_time": 45
                    }
                }
            ]
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/moderations", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        let result_obj = &sanitized["results"][0];

        // Provider fields at result level should be removed
        assert!(result_obj.get("custom_analysis").is_none());

        // But categories and category_scores are preserved as-is (they're serde_json::Value)
        // This means provider-specific fields WITHIN those objects are preserved
        // This is intentional as we don't know the schema of custom categories
        assert!(result_obj["categories"].get("hate").is_some());
        assert!(result_obj["category_scores"].get("hate").is_some());
    }

    #[test]
    fn test_responses_minimal_valid_response() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        // Absolute minimum - just the model field
        let response_json = r#"{"model": "test-model"}"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/responses", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        assert_eq!(sanitized["model"], "test-model");
        // All optional fields should be omitted (not null)
        assert!(sanitized.get("data").is_none());
        assert!(sanitized.get("choices").is_none());
        assert!(sanitized.get("results").is_none());
    }

    #[test]
    fn test_chat_completion_with_extremely_nested_message() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let response_json = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "gpt-4",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "test",
                    "nested": {
                        "level1": {
                            "level2": {
                                "level3": {
                                    "provider_data": "should_be_kept"
                                }
                            }
                        }
                    }
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/chat/completions", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Message field is kept as serde_json::Value, so nested content is preserved
        let message = &sanitized["choices"][0]["message"];
        assert_eq!(message["role"], "assistant");
        // Nested provider data within message is preserved (message is a Value type)
        assert!(
            message["nested"]["level1"]["level2"]["level3"]["provider_data"]
                .as_str()
                .is_some()
        );
    }

    #[test]
    fn test_empty_arrays_in_response() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let response_json = r#"{
            "object": "list",
            "data": [],
            "model": "test-model",
            "usage": {
                "prompt_tokens": 0,
                "total_tokens": 0
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/embeddings", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Empty arrays should be preserved
        assert_eq!(sanitized["data"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_responses_streaming() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("gpt-4".to_string()),
        };

        let sse_response = "data: {\"model\":\"gpt-4-turbo\",\"data\":{\"content\":\"Hello\"}}\n\ndata: {\"model\":\"gpt-4-turbo\",\"data\":{\"content\":\" World\"}}\n\ndata: [DONE]\n\n";

        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );

        let result = sanitizer
            .sanitize("/v1/responses", &headers, sse_response.as_bytes())
            .unwrap()
            .unwrap();

        let result_str = std::str::from_utf8(&result).unwrap();

        // Verify model rewriting in both chunks
        let chunk_count = result_str.matches("\"model\":\"gpt-4\"").count();
        assert_eq!(chunk_count, 2, "Both chunks should have model rewritten");

        // Verify [DONE] marker preserved
        assert!(result_str.contains("data: [DONE]"));

        // Verify SSE format
        assert!(result_str.ends_with("\n\n"));
    }

    #[test]
    fn test_responses_streaming_removes_provider_fields() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let sse_response = "data: {\"model\":\"test-model\",\"data\":{\"value\":\"test\"},\"provider\":\"custom\",\"cost\":0.001}\n\ndata: [DONE]\n\n";

        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );

        let result = sanitizer
            .sanitize("/v1/responses", &headers, sse_response.as_bytes())
            .unwrap()
            .unwrap();

        let result_str = std::str::from_utf8(&result).unwrap();

        // Provider fields should be removed
        assert!(!result_str.contains("\"provider\""));
        assert!(!result_str.contains("\"cost\""));

        // But data should be preserved
        assert!(result_str.contains("\"data\""));
        assert!(result_str.contains("\"value\":\"test\""));
    }

    #[test]
    fn test_responses_streaming_multiple_chunks() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("custom-model".to_string()),
        };

        let sse_response = "data: {\"model\":\"provider-model\",\"choices\":[{\"delta\":{\"content\":\"A\"}}]}\n\ndata: {\"model\":\"provider-model\",\"choices\":[{\"delta\":{\"content\":\"B\"}}]}\n\ndata: {\"model\":\"provider-model\",\"choices\":[{\"delta\":{\"content\":\"C\"}}]}\n\ndata: [DONE]\n\n";

        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );

        let result = sanitizer
            .sanitize("/v1/responses", &headers, sse_response.as_bytes())
            .unwrap()
            .unwrap();

        let result_str = std::str::from_utf8(&result).unwrap();

        // All 3 chunks should have model rewritten
        let chunk_count = result_str.matches("\"model\":\"custom-model\"").count();
        assert_eq!(chunk_count, 3);

        // Should have 4 messages total (3 chunks + [DONE])
        let message_count = result_str.matches("data:").count();
        assert_eq!(message_count, 4);
    }

    #[test]
    fn test_responses_streaming_sse_format_preservation() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let sse_response = "data: {\"model\":\"test\"}\n\ndata: [DONE]\n\n";

        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );

        let result = sanitizer
            .sanitize("/v1/responses", &headers, sse_response.as_bytes())
            .unwrap()
            .unwrap();

        let result_str = std::str::from_utf8(&result).unwrap();

        // Verify double newline termination
        assert!(result_str.ends_with("\n\n"));

        // Verify messages are separated by \n\n
        assert!(result_str.contains("}\n\ndata:"));

        // Verify exactly 2 trailing newlines
        let trailing_count = result_str.chars().rev().take_while(|&c| c == '\n').count();
        assert_eq!(trailing_count, 2);
    }
}
