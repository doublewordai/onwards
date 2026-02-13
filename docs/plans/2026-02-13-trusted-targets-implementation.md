# Trusted Targets Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add `trusted` flag to targets to bypass strict mode sanitization for trusted providers.

**Architecture:** Add boolean field to ProviderSpec config, check in strict handlers before sanitization, skip all sanitization when trusted=true.

**Tech Stack:** Rust, Axum, Serde, Tokio

---

## Task 1: Add `trusted` field to ProviderSpec

**Files:**
- Modify: `src/target.rs:45-71` (ProviderSpec struct)

**Step 1: Add trusted field to ProviderSpec**

Add the field after `sanitize_response`:

```rust
/// Enable response sanitization to enforce strict OpenAI schema compliance.
/// Removes provider-specific fields and rewrites the model field.
/// Defaults to false.
#[serde(default)]
pub sanitize_response: bool,

/// Mark this provider as trusted to bypass strict mode sanitization.
/// When strict_mode is enabled globally AND trusted is true for a target,
/// all sanitization (both success and error responses) is skipped.
/// WARNING: Trusted providers can leak metadata and non-standard responses.
/// Only use for providers you fully control or trust.
/// Defaults to false.
#[serde(default)]
pub trusted: bool,
```

**Step 2: Add trusted to ProviderSpec builder**

The `#[derive(Builder)]` will automatically include it, but add it to the builder default:

```rust
/// Weight for load balancing (higher = more traffic). Defaults to 1.
#[serde(default = "default_weight")]
#[builder(default = default_weight())]
pub weight: u32,

/// Enable response sanitization to enforce strict OpenAI schema compliance.
/// Removes provider-specific fields and rewrites the model field.
/// Defaults to false.
#[serde(default)]
pub sanitize_response: bool,

/// Mark this provider as trusted to bypass strict mode sanitization.
/// Defaults to false.
#[serde(default)]
#[builder(default)]
pub trusted: bool,
```

**Step 3: Verify compilation**

Run: `cargo check`
Expected: Success (no errors)

**Step 4: Commit configuration changes**

```bash
git add src/target.rs
git commit -m "feat: add trusted field to ProviderSpec config"
```

---

## Task 2: Test trusted target bypasses success response sanitization

**Files:**
- Modify: `src/strict/handlers.rs` (add test at end of tests module)

**Step 1: Write failing test for trusted success responses**

Add after existing tests (around line 2090):

```rust
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
```

**Step 2: Run test to verify it fails**

Run: `cargo test test_trusted_target_bypasses_sanitization -- --nocapture`
Expected: FAIL - trusted field doesn't exist yet, or sanitization not bypassed

**Step 3: Implement trusted check in chat_completions_handler**

In `src/strict/handlers.rs`, modify `chat_completions_handler` (around line 43-84):

```rust
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

    let response = forward_request(state.clone(), headers, "/v1/chat/completions", body_bytes).await;

    // Check if this target is trusted - if so, bypass all sanitization
    if let Some(pool) = state.targets.targets.get(&original_model) {
        if pool.providers.first().map(|p| p.config.trusted).unwrap_or(false) {
            debug!(model = %original_model, "Bypassing sanitization for trusted target");
            return response;
        }
    }

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
```

**Step 4: Run test to verify it passes**

Run: `cargo test test_trusted_target_bypasses_sanitization -- --nocapture`
Expected: PASS

**Step 5: Commit**

```bash
git add src/strict/handlers.rs
git commit -m "feat: bypass sanitization for trusted targets in chat completions"
```

---

## Task 3: Test trusted target bypasses error sanitization

**Files:**
- Modify: `src/strict/handlers.rs` (add test after previous test)

**Step 1: Write failing test for trusted error responses**

```rust
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
    assert_eq!(response_json["error"]["metadata"]["trace_id"], "trace-abc-123");
}
```

**Step 2: Run test to verify it passes**

The implementation from Task 2 already handles errors (the bypass happens before the success/error check).

Run: `cargo test test_trusted_target_bypasses_error_sanitization -- --nocapture`
Expected: PASS

**Step 3: Commit**

```bash
git add src/strict/handlers.rs
git commit -m "test: verify trusted targets bypass error sanitization"
```

---

## Task 4: Test untrusted targets still get sanitization

**Files:**
- Modify: `src/strict/handlers.rs` (add test after previous test)

**Step 1: Write test for untrusted target sanitization**

```rust
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
```

**Step 2: Run test to verify it passes**

Run: `cargo test test_untrusted_target_still_sanitized -- --nocapture`
Expected: PASS

**Step 3: Commit**

```bash
git add src/strict/handlers.rs
git commit -m "test: verify untrusted targets still get sanitization"
```

---

## Task 5: Add trusted support to responses_handler

**Files:**
- Modify: `src/strict/handlers.rs:90-133` (responses_handler)

**Step 1: Add trusted check to responses_handler**

Modify `responses_handler` to match the pattern from `chat_completions_handler`:

```rust
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

    let response = forward_request(state.clone(), headers, "/v1/responses", body_bytes).await;

    // Check if this target is trusted - if so, bypass all sanitization
    if let Some(pool) = state.targets.targets.get(&original_model) {
        if pool.providers.first().map(|p| p.config.trusted).unwrap_or(false) {
            debug!(model = %original_model, "Bypassing sanitization for trusted target");
            return response;
        }
    }

    // Sanitize response to ensure model field matches and extra fields are dropped
    if response.status().is_success() {
        if is_streaming {
            sanitize_streaming_responses_response(response, original_model).await
        } else {
            sanitize_responses_response(response, original_model).await
        }
    } else {
        // Sanitize error responses to prevent third-party info leakage
        sanitize_error_response(response).await
    }
}
```

**Step 2: Verify compilation**

Run: `cargo check`
Expected: Success

**Step 3: Commit**

```bash
git add src/strict/handlers.rs
git commit -m "feat: bypass sanitization for trusted targets in responses endpoint"
```

---

## Task 6: Add trusted support to embeddings_handler

**Files:**
- Modify: `src/strict/handlers.rs:135-173` (embeddings_handler)

**Step 1: Add trusted check to embeddings_handler**

Modify `embeddings_handler`:

```rust
pub async fn embeddings_handler<T: HttpClient + Clone + Send + Sync + 'static>(
    State(state): State<AppState<T>>,
    headers: HeaderMap,
    Json(request): Json<EmbeddingsRequest>,
) -> Response {
    let original_model = request.model.clone();

    debug!(
        model = %original_model,
        input_count = request.input.len(),
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

    let response = forward_request(state.clone(), headers, "/v1/embeddings", body_bytes).await;

    // Check if this target is trusted - if so, bypass all sanitization
    if let Some(pool) = state.targets.targets.get(&original_model) {
        if pool.providers.first().map(|p| p.config.trusted).unwrap_or(false) {
            debug!(model = %original_model, "Bypassing sanitization for trusted target");
            return response;
        }
    }

    // Sanitize response to ensure model field matches and extra fields are dropped
    if response.status().is_success() {
        sanitize_embeddings_response(response, original_model).await
    } else {
        // Sanitize error responses to prevent third-party info leakage
        sanitize_error_response(response).await
    }
}
```

**Step 2: Verify compilation**

Run: `cargo check`
Expected: Success

**Step 3: Commit**

```bash
git add src/strict/handlers.rs
git commit -m "feat: bypass sanitization for trusted targets in embeddings endpoint"
```

---

## Task 7: Test trusted streaming responses

**Files:**
- Modify: `src/strict/handlers.rs` (add test)

**Step 1: Write test for trusted streaming**

```rust
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
```

**Step 2: Run test to verify it passes**

Run: `cargo test test_trusted_streaming_responses_passthrough -- --nocapture`
Expected: PASS

**Step 3: Commit**

```bash
git add src/strict/handlers.rs
git commit -m "test: verify trusted targets bypass streaming sanitization"
```

---

## Task 8: Update documentation

**Files:**
- Modify: `docs/src/strict-mode.md` (add section after "Comparison with response sanitization")

**Step 1: Add Trusted Targets section to documentation**

Find the "Comparison with response sanitization" section (around line 110) and add after it:

```markdown
## Trusted Targets

In strict mode, you can mark specific targets as trusted to bypass all sanitization. This is useful when you have providers you fully control (e.g., your own OpenAI account) and want their exact responses and error messages passed through.

### Configuration

Add `trusted: true` to any target configuration:

\`\`\`json
{
  "strict_mode": true,
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-...",
      "trusted": true
    },
    "third-party": {
      "url": "https://some-provider.com",
      "onwards_key": "sk-..."
    }
  }
}
\`\`\`

In this example, `gpt-4` responses bypass all sanitization, while `third-party` responses receive full strict mode sanitization.

### Behavior

When a target is marked as `trusted: true`:

- **Success responses**: Passed through completely with all provider metadata intact
- **Error responses**: Original error messages and metadata forwarded to clients
- **No model field rewriting**: Response model field matches provider's response exactly
- **No Content-Length updates**: Headers passed through as-is
- **No SSE sanitization**: Streaming responses passed through without line-by-line parsing

### Security Warning

⚠️ **Use trusted targets carefully.** Marking a target as trusted bypasses all strict mode security guarantees:

- Provider-specific metadata may leak to clients (costs, trace IDs, internal identifiers)
- Third-party error details and stack traces will be exposed
- Responses may not match OpenAI schema exactly
- Non-standard fields may confuse client applications

**Only mark targets as trusted when you fully control or trust the provider.** This typically means:
- Your own OpenAI/Anthropic accounts
- Self-hosted models you operate
- Internal services you maintain

Do **not** mark third-party providers as trusted unless you understand the security and compatibility implications.
```

**Step 2: Verify documentation renders correctly**

Run: `mdbook build docs` (if mdbook is available)
Or just visually inspect the markdown.

**Step 3: Commit**

```bash
git add docs/src/strict-mode.md
git commit -m "docs: add trusted targets section to strict mode documentation"
```

---

## Task 9: Run full test suite and verify

**Files:**
- N/A (verification only)

**Step 1: Run all tests**

Run: `cargo test`
Expected: All tests pass (should be 172 tests now - 167 + 5 new)

**Step 2: Run specific trusted tests**

Run: `cargo test trusted -- --nocapture`
Expected: All 4 trusted-related tests pass

**Step 3: Check for any warnings**

Run: `cargo clippy`
Expected: No warnings

**Step 4: Final commit if needed**

If any clippy fixes were needed:
```bash
git add -A
git commit -m "fix: address clippy warnings"
```

---

## Task 10: Push and verify CI

**Files:**
- N/A (git operations only)

**Step 1: Push to remote**

Run: `git push origin feat/strict-mode`
Expected: Success

**Step 2: Wait for CI and check status**

Run: `sleep 90 && gh pr checks`
Expected: All checks pass (test, docker, check-title)

**Step 3: Verify test count in CI logs**

Run: `gh run view --log | grep "test result"`
Expected: Should show 172 tests passed

---

## Completion Checklist

- [ ] `trusted` field added to ProviderSpec config
- [ ] Chat completions handler checks trusted flag
- [ ] Responses handler checks trusted flag
- [ ] Embeddings handler checks trusted flag
- [ ] Test: trusted target bypasses success sanitization
- [ ] Test: trusted target bypasses error sanitization
- [ ] Test: untrusted target still sanitized
- [ ] Test: trusted streaming passthrough
- [ ] Documentation updated with security warnings
- [ ] All 172 tests pass
- [ ] CI passes
