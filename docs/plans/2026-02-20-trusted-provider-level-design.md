# Design: Per-Provider `trusted` Flag

**Date:** 2026-02-20
**Status:** Approved

## Background

`trusted` is a pool-level flag that controls strict mode error response sanitization. When `true`, error responses from that pool are passed through as-is (not sanitized), which can leak provider metadata. When `false`, error responses are sanitized to prevent info leakage. Success responses are always sanitized regardless.

Currently all providers in a pool share the same trust level. This design adds the ability to set `trusted` per-provider, with pool-level acting as the default.

## Design

### Data Model

**`ProviderSpec`** (`src/target.rs`) — new optional field:
```rust
#[serde(default)]
#[builder(default)]
pub trusted: Option<bool>,
```
`None` means inherit from pool. Existing configs are fully backwards-compatible.

**`Provider`** (`src/load_balancer.rs`) — mirrors this:
```rust
pub struct Provider {
    pub target: Target,
    pub weight: u32,
    pub trusted: Option<bool>,
}
```

Pool-level `trusted: bool` is unchanged — it becomes the default for providers that don't specify their own value.

### Trust Resolution

When the load balancer selects a provider, it resolves the effective trust level:
```rust
let resolved_trust = provider.trusted.unwrap_or(pool.trusted);
```

- For **fallback retries**, the trust level of whichever provider ultimately serves the response is used.
- For **pre-provider-selection failures** (e.g. pool-level rate limit hit before any provider is chosen), `pool.trusted` is used as the fallback since no provider was involved.

### `ForwardResult` Struct

New struct in `src/handlers.rs`:
```rust
pub struct ForwardResult {
    pub response: Response<Body>,
    pub trusted: bool,
}
```

`forward_request` changes its return type from `Response<Body>` to `ForwardResult`.

### Strict Mode Handler Changes

The three strict mode endpoints (chat completions, responses, embeddings) currently do a pre-check of `pool.is_trusted()` before forwarding. This pre-check is removed. Instead, the resolved trust is read from `ForwardResult` after the request:

```rust
let ForwardResult { response, trusted } = forward_request(...).await;

if response.status().is_success() {
    sanitize_chat_response(response, resolved_model).await
} else if trusted {
    response
} else {
    sanitize_error_response(response).await
}
```

## Files Changed

| File | Change |
|------|--------|
| `src/target.rs` | Add `trusted: Option<bool>` to `ProviderSpec`; propagate through config conversion |
| `src/load_balancer.rs` | Add `trusted: Option<bool>` to `Provider`; resolve trust at selection time |
| `src/handlers.rs` | Add `ForwardResult` struct; change `forward_request` return type; return resolved trust |
| `src/strict/handlers.rs` | Remove pre-check; destructure `ForwardResult`; use `trusted` from result |

## Example Configuration

```json
{
  "targets": {
    "gpt-4": {
      "trusted": false,
      "providers": [
        {
          "url": "https://internal.example.com",
          "trusted": true,
          "weight": 3
        },
        {
          "url": "https://external.example.com",
          "weight": 1
        }
      ]
    }
  }
}
```

In this example, the internal provider is trusted (error responses pass through), while the external provider inherits the pool default of `false` (error responses sanitized).
