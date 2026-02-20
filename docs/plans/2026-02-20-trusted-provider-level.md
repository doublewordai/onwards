# Per-Provider `trusted` Flag Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Allow `trusted` to be set per-provider in `ProviderSpec`, overriding the pool-level default, so different providers within the same pool can have different trust levels for strict-mode error sanitization.

**Architecture:** Add `trusted: Option<bool>` to both `ProviderSpec` (config) and `Target` (runtime). Placing it on `Target` (not `Provider`) follows the existing pattern where `sanitize_response` lives on `Target`. When a provider handles a request, resolve the effective trust as `target.trusted.unwrap_or(pool.is_trusted())` and attach it to the response via a `ResolvedTrust` extension. The strict-mode `forward_request` wrapper reads that extension and returns a `ForwardResult { response, trusted }`, eliminating the current pre-request pool lookup.

**Tech Stack:** Rust, axum, serde, derive_builder, tokio

---

### Task 1: Add `trusted: Option<bool>` to `ProviderSpec`

**Files:**
- Modify: `src/target.rs:46-79` (ProviderSpec struct)
- Modify: `src/target.rs:295-310` (legacy list format literal construction)
- Modify: `src/target.rs:328-340` (single provider format literal construction)
- Test: `src/target.rs` (tests module at bottom)

**Step 1: Write the failing test**

Add inside the `#[cfg(test)]` mod at the bottom of `src/target.rs`:

```rust
#[test]
fn test_provider_spec_trusted_defaults_to_none() {
    let json = r#"{"url": "https://example.com"}"#;
    let spec: ProviderSpec = serde_json::from_str(json).unwrap();
    assert_eq!(spec.trusted, None);
}

#[test]
fn test_provider_spec_trusted_can_be_set() {
    let json = r#"{"url": "https://example.com", "trusted": true}"#;
    let spec: ProviderSpec = serde_json::from_str(json).unwrap();
    assert_eq!(spec.trusted, Some(true));
}

#[test]
fn test_provider_spec_trusted_explicit_false() {
    let json = r#"{"url": "https://example.com", "trusted": false}"#;
    let spec: ProviderSpec = serde_json::from_str(json).unwrap();
    assert_eq!(spec.trusted, Some(false));
}
```

**Step 2: Run tests to verify they fail**

```bash
cargo test test_provider_spec_trusted 2>&1 | tail -20
```

Expected: compilation error — `trusted` field not found on `ProviderSpec`.

**Step 3: Add the field to `ProviderSpec`**

In `src/target.rs`, after the `request_timeout_secs` field (line ~78), add:

```rust
/// Per-provider override for strict mode error sanitization trust.
/// When Some(true), error responses from this provider bypass sanitization.
/// When Some(false), error responses are sanitized even if pool is trusted.
/// When None (default), inherits the pool-level `trusted` setting.
#[serde(default)]
#[builder(default)]
pub trusted: Option<bool>,
```

Then fix the two struct literal constructions that will fail to compile:

**Legacy list format** (`src/target.rs` ~line 297-309, inside `TargetSpecOrList::List` arm):
```rust
.map(|t| ProviderSpec {
    url: t.url,
    onwards_key: t.onwards_key,
    onwards_model: t.onwards_model,
    rate_limit: t.rate_limit,
    concurrency_limit: t.concurrency_limit,
    upstream_auth_header_name: t.upstream_auth_header_name,
    upstream_auth_header_prefix: t.upstream_auth_header_prefix,
    response_headers: t.response_headers,
    weight: t.weight,
    sanitize_response: t.sanitize_response,
    request_timeout_secs: t.request_timeout_secs,
    trusted: None, // pool-level trusted handles this for legacy format
})
```

**Single provider format** (`src/target.rs` ~line 328-340, inside `TargetSpecOrList::Single` arm):
```rust
let provider = ProviderSpec {
    url: spec.url,
    onwards_key: spec.onwards_key,
    onwards_model: spec.onwards_model,
    rate_limit: spec.rate_limit,
    concurrency_limit: spec.concurrency_limit,
    upstream_auth_header_name: spec.upstream_auth_header_name,
    upstream_auth_header_prefix: spec.upstream_auth_header_prefix,
    response_headers: spec.response_headers,
    weight: spec.weight,
    sanitize_response: false,
    request_timeout_secs: spec.request_timeout_secs,
    trusted: None, // pool-level trusted handles this for single-provider format
};
```

**Step 4: Run tests to verify they pass**

```bash
cargo test test_provider_spec_trusted 2>&1 | tail -20
```

Expected: 3 tests PASS.

**Step 5: Commit**

```bash
git add src/target.rs
git commit -m "feat: add trusted field to ProviderSpec"
```

---

### Task 2: Add `trusted: Option<bool>` to `Target` and propagate from `ProviderSpec`

**Files:**
- Modify: `src/target.rs:487-502` (Target struct)
- Modify: `src/target.rs:397-421` (From<ProviderSpec> for Target)
- Test: `src/target.rs` (tests module)

**Step 1: Write the failing test**

Add inside the `#[cfg(test)]` mod in `src/target.rs`:

```rust
#[test]
fn test_target_from_provider_spec_propagates_trusted_some_true() {
    let json = r#"{"url": "https://example.com", "trusted": true}"#;
    let spec: ProviderSpec = serde_json::from_str(json).unwrap();
    let target: Target = spec.into();
    assert_eq!(target.trusted, Some(true));
}

#[test]
fn test_target_from_provider_spec_propagates_trusted_some_false() {
    let json = r#"{"url": "https://example.com", "trusted": false}"#;
    let spec: ProviderSpec = serde_json::from_str(json).unwrap();
    let target: Target = spec.into();
    assert_eq!(target.trusted, Some(false));
}

#[test]
fn test_target_from_provider_spec_propagates_trusted_none() {
    let json = r#"{"url": "https://example.com"}"#;
    let spec: ProviderSpec = serde_json::from_str(json).unwrap();
    let target: Target = spec.into();
    assert_eq!(target.trusted, None);
}
```

**Step 2: Run tests to verify they fail**

```bash
cargo test test_target_from_provider_spec_propagates_trusted 2>&1 | tail -20
```

Expected: compilation error — `trusted` not on `Target`.

**Step 3: Add `trusted` to `Target` struct**

In `src/target.rs`, after `sanitize_response: bool` in the `Target` struct (~line 500):

```rust
/// Per-provider override for strict mode error sanitization trust.
/// None means inherit from pool-level trusted setting.
#[builder(default)]
pub trusted: Option<bool>,
```

**Step 4: Update `From<ProviderSpec> for Target`**

In `src/target.rs` in the `impl From<ProviderSpec> for Target` block (~line 397-421), add `trusted: value.trusted` to the `Target { ... }` construction:

```rust
impl From<ProviderSpec> for Target {
    fn from(value: ProviderSpec) -> Self {
        Target {
            url: normalize_url(value.url),
            keys: None,
            onwards_key: value.onwards_key,
            onwards_model: value.onwards_model,
            limiter: value.rate_limit.map(|rl| {
                Arc::new(governor::RateLimiter::direct(
                    Quota::per_second(rl.requests_per_second)
                        .allow_burst(rl.burst_size.unwrap_or(rl.requests_per_second)),
                )) as Arc<dyn RateLimiter>
            }),
            concurrency_limiter: value.concurrency_limit.map(|cl| {
                SemaphoreConcurrencyLimiter::new(cl.max_concurrent_requests)
                    as Arc<dyn ConcurrencyLimiter>
            }),
            upstream_auth_header_name: value.upstream_auth_header_name,
            upstream_auth_header_prefix: value.upstream_auth_header_prefix,
            response_headers: value.response_headers,
            sanitize_response: value.sanitize_response,
            request_timeout_secs: value.request_timeout_secs,
            trusted: value.trusted,
        }
    }
}
```

**Step 5: Fix any remaining compilation errors**

The `Target::into_pool` method at ~line 508 constructs `Provider { target: self, weight: 1 }` directly. That doesn't need a change since `Target` carries `trusted` now (it's already inside `self`).

Run `cargo build` to surface any struct literal exhaustiveness errors:

```bash
cargo build 2>&1 | grep "error"
```

Fix any exhaustive struct literal errors (add `trusted: None` to any `Target { ... }` constructions in tests or elsewhere).

**Step 6: Run tests**

```bash
cargo test test_target_from_provider_spec_propagates_trusted 2>&1 | tail -20
```

Expected: 3 tests PASS.

**Step 7: Run all tests to check nothing regressed**

```bash
cargo test 2>&1 | tail -30
```

Expected: all existing tests pass.

**Step 8: Commit**

```bash
git add src/target.rs
git commit -m "feat: add trusted field to Target, propagate from ProviderSpec"
```

---

### Task 3: Attach `ResolvedTrust` extension to responses in `target_message_handler`

**Files:**
- Modify: `src/handlers.rs` (add type + attach in for loop)

**Step 1: Add `ResolvedTrust` newtype**

Near the top of `src/handlers.rs`, after the existing `struct OriginalModel` definition, add:

```rust
/// Response extension that carries the resolved trust level from the provider that handled
/// the request. Set by `target_message_handler` so strict-mode handlers can read it without
/// a second pool lookup.
pub(crate) struct ResolvedTrust(pub(crate) bool);
```

**Step 2: Attach extension before returning `Ok(response)`**

In `target_message_handler`, find the line `return Ok(response);` at ~line 740 (the successful return inside the for loop). Add two lines immediately before it:

```rust
let resolved_trust = target.trusted.unwrap_or_else(|| pool.is_trusted());
response.extensions_mut().insert(ResolvedTrust(resolved_trust));
return Ok(response);
```

**Step 3: Run all tests**

```bash
cargo test 2>&1 | tail -30
```

Expected: all tests pass (no behavior change yet — extension is attached but nothing reads it).

**Step 4: Commit**

```bash
git add src/handlers.rs
git commit -m "feat: attach ResolvedTrust extension in target_message_handler"
```

---

### Task 4: Define `ForwardResult` and update `forward_request` in `strict/handlers.rs`

**Files:**
- Modify: `src/strict/handlers.rs` (ForwardResult struct + forward_request return type)
- Test: `src/strict/handlers.rs` (tests module)

**Step 1: Write failing tests for per-provider trust override**

Add these tests inside the `#[cfg(test)]` mod in `src/strict/handlers.rs`:

```rust
#[tokio::test]
async fn test_provider_trusted_overrides_untrusted_pool() {
    // Pool is NOT trusted, but the single provider IS trusted.
    // Error response should pass through unchanged.
    use crate::load_balancer::{Provider, ProviderPool};
    use crate::target::{LoadBalanceStrategy, Target, Targets};
    use crate::test_utils::MockHttpClient;
    use axum::body::Body;
    use axum::http::Request;
    use dashmap::DashMap;
    use std::sync::Arc;
    use tower::ServiceExt;

    let targets_map = Arc::new(DashMap::new());
    let pool = ProviderPool::with_config(
        vec![Provider {
            target: Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .trusted(Some(true)) // provider is trusted
                .build(),
            weight: 1,
        }],
        None, None, None, None,
        LoadBalanceStrategy::default(),
        false, // pool is NOT trusted
    );
    targets_map.insert("gpt-4".to_string(), pool);

    let targets = Targets {
        targets: targets_map,
        key_rate_limiters: Arc::new(DashMap::new()),
        key_concurrency_limiters: Arc::new(DashMap::new()),
        strict_mode: true,
        http_pool_config: None,
    };

    let error_body = r#"{"error": {"message": "provider specific error", "type": "rate_limit_error", "provider_trace": "trace-xyz"}}"#;
    let mock_client = MockHttpClient::new(StatusCode::TOO_MANY_REQUESTS, error_body);
    let state = crate::AppState::with_client(targets, mock_client);
    let router = crate::strict::build_strict_router(state);

    let request_body = r#"{"model": "gpt-4", "messages": [{"role": "user", "content": "Hi"}]}"#;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(request_body))
        .unwrap();

    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body_str = std::str::from_utf8(&body_bytes).unwrap();
    // Provider-specific fields should be present (not sanitized)
    assert!(body_str.contains("provider_trace"), "Provider trace should pass through for trusted provider");
    assert!(body_str.contains("rate_limit_error"), "Provider error type should pass through for trusted provider");
}

#[tokio::test]
async fn test_provider_untrusted_overrides_trusted_pool() {
    // Pool IS trusted, but the single provider is explicitly NOT trusted.
    // Error response should be sanitized.
    use crate::load_balancer::{Provider, ProviderPool};
    use crate::target::{LoadBalanceStrategy, Target, Targets};
    use crate::test_utils::MockHttpClient;
    use axum::body::Body;
    use axum::http::Request;
    use dashmap::DashMap;
    use std::sync::Arc;
    use tower::ServiceExt;

    let targets_map = Arc::new(DashMap::new());
    let pool = ProviderPool::with_config(
        vec![Provider {
            target: Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .trusted(Some(false)) // provider explicitly NOT trusted
                .build(),
            weight: 1,
        }],
        None, None, None, None,
        LoadBalanceStrategy::default(),
        true, // pool IS trusted
    );
    targets_map.insert("gpt-4".to_string(), pool);

    let targets = Targets {
        targets: targets_map,
        key_rate_limiters: Arc::new(DashMap::new()),
        key_concurrency_limiters: Arc::new(DashMap::new()),
        strict_mode: true,
        http_pool_config: None,
    };

    let error_body = r#"{"error": {"message": "provider specific error", "type": "rate_limit_error", "provider_trace": "trace-xyz"}}"#;
    let mock_client = MockHttpClient::new(StatusCode::TOO_MANY_REQUESTS, error_body);
    let state = crate::AppState::with_client(targets, mock_client);
    let router = crate::strict::build_strict_router(state);

    let request_body = r#"{"model": "gpt-4", "messages": [{"role": "user", "content": "Hi"}]}"#;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(request_body))
        .unwrap();

    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body_str = std::str::from_utf8(&body_bytes).unwrap();
    // Provider-specific fields should be ABSENT (sanitized)
    assert!(!body_str.contains("provider_trace"), "Provider trace should be stripped for untrusted provider");
    assert!(!body_str.contains("rate_limit_error"), "Provider error type should be stripped for untrusted provider");
}
```

**Step 2: Run tests to confirm they fail**

```bash
cargo test test_provider_trusted_overrides_untrusted_pool test_provider_untrusted_overrides_trusted_pool 2>&1 | tail -20
```

Expected: compilation errors (`Target::builder()` has no `trusted` method yet, or if Task 2 is done, the tests fail because behavior not yet implemented).

**Step 3: Define `ForwardResult` and update `forward_request`**

In `src/strict/handlers.rs`, after the imports block (around line 28), add:

```rust
/// Result of forwarding a request, carrying the response and the trust level
/// of the provider that handled it.
pub(crate) struct ForwardResult {
    pub(crate) response: Response,
    pub(crate) trusted: bool,
}
```

Then change the return type and body of `forward_request` (~line 220-257):

```rust
async fn forward_request<T: HttpClient + Clone + Send + Sync + 'static>(
    state: AppState<T>,
    mut headers: HeaderMap,
    path: &str,
    body_bytes: Vec<u8>,
) -> ForwardResult {
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );

    let mut request_builder = Request::builder().method("POST").uri(path);
    for (name, value) in headers.iter() {
        request_builder = request_builder.header(name, value);
    }

    let request = match request_builder.body(Body::from(body_bytes)) {
        Ok(req) => req,
        Err(e) => {
            error!(error = %e, "Failed to build request");
            let response = error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Failed to build request",
            );
            return ForwardResult { response, trusted: false };
        }
    };

    let response = match target_message_handler(State(state), request).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    };

    let trusted = response
        .extensions()
        .get::<crate::handlers::ResolvedTrust>()
        .map(|t| t.0)
        .unwrap_or(false);

    ForwardResult { response, trusted }
}
```

**Step 4: Run tests to verify they still fail (behavior not wired yet)**

```bash
cargo test test_provider_trusted_overrides_untrusted_pool test_provider_untrusted_overrides_trusted_pool 2>&1 | tail -20
```

Expected: compilation errors — the three calling handlers still expect `Response`, not `ForwardResult`.

**Step 5: Commit the ForwardResult struct and forward_request change**

```bash
git add src/strict/handlers.rs
git commit -m "feat: define ForwardResult, update forward_request to return trusted level"
```

---

### Task 5: Update the three strict-mode handlers to use `ForwardResult`

**Files:**
- Modify: `src/strict/handlers.rs` (chat_completions_handler, responses_handler, embeddings_handler)

**Step 1: Update `chat_completions_handler` (~lines 72-99)**

Replace the pre-check block and response handling:

```rust
// BEFORE (lines 72-99):
let resolved_model =
    extract_model_from_request(&headers, &body_bytes).unwrap_or(original_model.clone());
let is_trusted = state
    .targets
    .targets
    .get(&resolved_model)
    .map(|pool| pool.is_trusted())
    .unwrap_or(false);

let response = forward_request(state, headers, "/v1/chat/completions", body_bytes).await;

if response.status().is_success() {
    if is_streaming {
        sanitize_streaming_chat_response(response, resolved_model).await
    } else {
        sanitize_chat_response(response, resolved_model).await
    }
} else if is_trusted {
    debug!(model = %resolved_model, "Bypassing error sanitization for trusted pool");
    response
} else {
    sanitize_error_response(response).await
}

// AFTER:
let resolved_model =
    extract_model_from_request(&headers, &body_bytes).unwrap_or(original_model.clone());
let ForwardResult { response, trusted } =
    forward_request(state, headers, "/v1/chat/completions", body_bytes).await;

if response.status().is_success() {
    if is_streaming {
        sanitize_streaming_chat_response(response, resolved_model).await
    } else {
        sanitize_chat_response(response, resolved_model).await
    }
} else if trusted {
    debug!(model = %resolved_model, "Bypassing error sanitization for trusted provider");
    response
} else {
    sanitize_error_response(response).await
}
```

**Step 2: Update `responses_handler` (~lines 134-161)**

Same pattern — replace pre-check + `let response = ...` with:

```rust
let resolved_model =
    extract_model_from_request(&headers, &body_bytes).unwrap_or(original_model.clone());
let ForwardResult { response, trusted } =
    forward_request(state, headers, "/v1/responses", body_bytes).await;

if response.status().is_success() {
    if is_streaming {
        sanitize_streaming_responses_response(response, resolved_model).await
    } else {
        sanitize_responses_response(response, resolved_model).await
    }
} else if trusted {
    debug!(model = %resolved_model, "Bypassing error sanitization for trusted provider");
    response
} else {
    sanitize_error_response(response).await
}
```

**Step 3: Update `embeddings_handler` (~lines 193-216)**

Same pattern:

```rust
let resolved_model =
    extract_model_from_request(&headers, &body_bytes).unwrap_or(original_model.clone());
let ForwardResult { response, trusted } =
    forward_request(state, headers, "/v1/embeddings", body_bytes).await;

if response.status().is_success() {
    sanitize_embeddings_response(response, resolved_model).await
} else if trusted {
    debug!(model = %resolved_model, "Bypassing error sanitization for trusted provider");
    response
} else {
    sanitize_error_response(response).await
}
```

**Step 4: Run the new per-provider tests**

```bash
cargo test test_provider_trusted_overrides_untrusted_pool test_provider_untrusted_overrides_trusted_pool 2>&1 | tail -20
```

Expected: both PASS.

**Step 5: Run all tests to verify no regressions**

```bash
cargo test 2>&1 | tail -30
```

Expected: all tests pass, including existing `test_trusted_target_*` tests.

**Step 6: Commit**

```bash
git add src/strict/handlers.rs
git commit -m "feat: use ForwardResult in strict handlers for per-provider trust"
```

---

### Task 6: Add pool-config-level integration test

**Files:**
- Modify: `src/target.rs` (tests module) — one integration test via JSON config parsing

**Step 1: Write the test**

Add inside the `#[cfg(test)]` mod in `src/target.rs`:

```rust
#[test]
fn test_per_provider_trusted_parsed_from_config() {
    let json = r#"{
        "targets": {
            "gpt-4": {
                "trusted": false,
                "providers": [
                    {"url": "https://internal.example.com", "trusted": true},
                    {"url": "https://external.example.com"}
                ]
            }
        }
    }"#;

    let config: ConfigFile = serde_json::from_str(json).unwrap();
    let targets = Targets::try_from(config).unwrap();
    let pool = targets.targets.get("gpt-4").unwrap();

    // Pool-level trusted is false
    assert!(!pool.is_trusted());

    let providers = pool.providers();
    // First provider explicitly sets trusted=true
    assert_eq!(providers[0].target.trusted, Some(true));
    // Second provider has no override (inherits from pool)
    assert_eq!(providers[1].target.trusted, None);
}
```

**Step 2: Run test to verify it passes**

```bash
cargo test test_per_provider_trusted_parsed_from_config 2>&1 | tail -20
```

Expected: PASS (if Tasks 1-2 are done).

**Step 3: Run full test suite one final time**

```bash
cargo test 2>&1 | tail -30
```

Expected: all tests pass.

**Step 4: Final commit**

```bash
git add src/target.rs
git commit -m "test: add integration test for per-provider trusted config parsing"
```

---

## Summary of Changes

| File | What changes |
|------|-------------|
| `src/target.rs` | `ProviderSpec`: add `trusted: Option<bool>`; `Target`: add `trusted: Option<bool>`; `From<ProviderSpec> for Target`: propagate `trusted`; two struct literal sites: add `trusted: None` |
| `src/handlers.rs` | Add `pub(crate) struct ResolvedTrust(pub(crate) bool)`; before `return Ok(response)` in the for loop, attach `ResolvedTrust(target.trusted.unwrap_or_else(|| pool.is_trusted()))` |
| `src/strict/handlers.rs` | Add `ForwardResult { response, trusted }`; change `forward_request` → `ForwardResult`; update 3 handlers to destructure it and drop the pre-check |
