use axum::body::Bytes;
use axum::http::HeaderMap;

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
        // First parse as generic JSON to handle provider-specific fields
        let raw_json: serde_json::Value = serde_json::from_slice(body)
            .map_err(|e| format!("Failed to parse response as JSON: {}", e))?;

        // Extract fields, providing defaults for missing required fields
        let id = raw_json
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("chatcmpl-unknown")
            .to_string();

        let object = raw_json
            .get("object")
            .and_then(|v| v.as_str())
            .unwrap_or("chat.completion")
            .to_string();

        let created = raw_json
            .get("created")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
            }) as u32;

        // Use original model if provided, otherwise use model from response
        let model = if let Some(ref original) = self.original_model {
            original.clone()
        } else {
            raw_json
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string()
        };

        // Extract choices - this is the most complex part
        let choices = raw_json
            .get("choices")
            .and_then(|v| v.as_array())
            .ok_or("Response missing 'choices' array")?;

        // Convert to OpenAI format, keeping only standard fields
        let sanitized_choices: Result<Vec<serde_json::Value>, String> = choices
            .iter()
            .map(|choice| {
                let index = choice.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let message = choice.get("message").cloned().unwrap_or(serde_json::json!({}));
                let finish_reason = choice
                    .get("finish_reason")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                Ok(serde_json::json!({
                    "index": index,
                    "message": message,
                    "finish_reason": finish_reason
                }))
            })
            .collect();

        let sanitized_choices = sanitized_choices?;

        // Extract and sanitize optional usage field (only standard OpenAI fields)
        let usage = raw_json.get("usage").map(|u| {
            serde_json::json!({
                "prompt_tokens": u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                "completion_tokens": u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                "total_tokens": u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
            })
        });

        // Extract optional system_fingerprint
        let system_fingerprint = raw_json
            .get("system_fingerprint")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Construct sanitized response with only OpenAI fields
        let mut sanitized = serde_json::json!({
            "id": id,
            "object": object,
            "created": created,
            "model": model,
            "choices": sanitized_choices
        });

        // Add optional fields if present
        if let Some(usage) = usage {
            sanitized["usage"] = usage;
        }
        if let Some(fp) = system_fingerprint {
            sanitized["system_fingerprint"] = serde_json::Value::String(fp);
        }

        // Serialize the sanitized response
        let sanitized_bytes = serde_json::to_vec(&sanitized)
            .map_err(|e| format!("Failed to serialize sanitized response: {}", e))?;

        Ok(Some(Bytes::from(sanitized_bytes)))
    }

    /// Sanitizes a streaming Server-Sent Events (SSE) response
    ///
    /// SSE format: `data: {...}\n\ndata: {...}\n\ndata: [DONE]\n\n`
    fn sanitize_streaming(&self, body: &[u8]) -> Result<Option<Bytes>, String> {
        let body_str = std::str::from_utf8(body)
            .map_err(|e| format!("Invalid UTF-8 in streaming response: {}", e))?;

        let mut sanitized_lines = Vec::new();

        for line in body_str.lines() {
            if line.starts_with("data: ") {
                let data_part = &line[6..]; // Skip "data: " prefix

                if data_part.trim() == "[DONE]" {
                    // Preserve [DONE] marker as-is
                    sanitized_lines.push(line.to_string());
                } else {
                    // Parse as generic JSON first
                    let raw_chunk: serde_json::Value = serde_json::from_str(data_part)
                        .map_err(|e| format!("Failed to parse stream chunk: {}", e))?;

                    // Extract and sanitize chunk fields
                    let id = raw_chunk
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("chatcmpl-unknown")
                        .to_string();

                    let object = raw_chunk
                        .get("object")
                        .and_then(|v| v.as_str())
                        .unwrap_or("chat.completion.chunk")
                        .to_string();

                    let created = raw_chunk
                        .get("created")
                        .and_then(|v| v.as_u64())
                        .unwrap_or_else(|| {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_secs()
                        }) as u32;

                    // Use original model if provided
                    let model = if let Some(ref original) = self.original_model {
                        original.clone()
                    } else {
                        raw_chunk
                            .get("model")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string()
                    };

                    // Extract choices (keep only standard fields)
                    let choices = raw_chunk
                        .get("choices")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .map(|choice| {
                                    let index =
                                        choice.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                                    let delta = choice.get("delta").cloned();
                                    let finish_reason = choice
                                        .get("finish_reason")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string());

                                    let mut sanitized_choice = serde_json::json!({
                                        "index": index
                                    });

                                    if let Some(delta) = delta {
                                        sanitized_choice["delta"] = delta;
                                    }
                                    if let Some(fr) = finish_reason {
                                        sanitized_choice["finish_reason"] =
                                            serde_json::Value::String(fr);
                                    }

                                    sanitized_choice
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();

                    // Build sanitized chunk
                    let mut sanitized_chunk = serde_json::json!({
                        "id": id,
                        "object": object,
                        "created": created,
                        "model": model,
                        "choices": choices
                    });

                    // Include optional fields if present (sanitize usage to only standard fields)
                    if let Some(usage) = raw_chunk.get("usage") {
                        sanitized_chunk["usage"] = serde_json::json!({
                            "prompt_tokens": usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                            "completion_tokens": usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                            "total_tokens": usage.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
                        });
                    }
                    if let Some(fp) = raw_chunk.get("system_fingerprint") {
                        sanitized_chunk["system_fingerprint"] = fp.clone();
                    }

                    // Serialize the sanitized chunk
                    let sanitized_json = serde_json::to_string(&sanitized_chunk)
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
        headers.insert("content-type", HeaderValue::from_static("text/event-stream"));

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
        let result = sanitizer
            .sanitize("/v1/models", &headers, b"test")
            .unwrap();

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
            .sanitize("/v1/chat/completions", &headers, openrouter_response.as_bytes())
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
        assert!(choices[0]["message"]["content"].as_str().unwrap().starts_with("Hello!"));

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
