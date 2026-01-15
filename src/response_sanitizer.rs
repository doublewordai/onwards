use axum::body::Bytes;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Lenient wrapper for OpenAI chat completion responses
/// Deserializes with defaults for missing fields and ignores extra provider-specific fields
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
    /// * `path` - The request path (e.g., "/v1/chat/completions")
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
        // Only apply to /v1/chat/completions endpoint
        if !path.contains("/v1/chat/completions") {
            return Ok(None);
        }

        // Detect streaming via Content-Type header
        let is_streaming = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("text/event-stream"))
            .unwrap_or(false);

        if is_streaming {
            self.sanitize_streaming(body)
        } else {
            self.sanitize_non_streaming(body)
        }
    }

    /// Sanitizes a non-streaming JSON response
    fn sanitize_non_streaming(&self, body: &[u8]) -> Result<Option<Bytes>, String> {
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

    /// Sanitizes a streaming Server-Sent Events (SSE) response
    ///
    /// SSE format: `data: {...}\n\ndata: {...}\n\ndata: [DONE]\n\n`
    pub fn sanitize_streaming(&self, body: &[u8]) -> Result<Option<Bytes>, String> {
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

        let sanitized_body = sanitized_lines.join("\n");
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
        assert!(choices[0].get("logprobs").is_none());

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
    }
}
