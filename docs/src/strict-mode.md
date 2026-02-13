# Strict Mode

Strict mode provides enhanced security and API compliance by using typed request/response handlers instead of the default wildcard passthrough router. This feature:

- **Validates all requests** against OpenAI API schemas before forwarding
- **Sanitizes all responses** by removing third-party provider metadata
- **Standardizes error messages** to prevent information leakage
- **Ensures model field consistency** between requests and responses
- **Supports streaming and non-streaming** for all endpoints

This is useful when you need guaranteed API compatibility, security hardening, or protection against third-party response variations.

## Enabling strict mode

Strict mode is a **global configuration** that applies to all targets in your gateway. Add `strict_mode: true` at the top level of your configuration (not inside individual targets).

```json
{
  "strict_mode": true,
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-openai-key"
    },
    "claude": {
      "url": "https://api.anthropic.com",
      "onwards_key": "sk-ant-key"
    }
  }
}
```

When enabled, all requests to all targets will use strict mode validation and sanitization.

## How it works

When `strict_mode: true` is enabled:

1. **Request validation**: Incoming requests are deserialized through OpenAI schemas. Invalid requests receive immediate `400 Bad Request` errors with clear messages.
2. **Response sanitization**: Third-party responses are deserialized (automatically dropping unknown fields). If deserialization fails, a standard error is returned - malformed responses are never passed through. The model field is rewritten to match the original request, then re-serialized as clean OpenAI responses with correct Content-Length headers.
3. **Error standardization**: Third-party errors are logged internally but never forwarded to clients. Clients receive standardized OpenAI-format errors based only on HTTP status codes.
4. **Streaming support**: SSE streams are parsed line-by-line to handle multi-line data events and strip comment lines, each chunk is sanitized, and re-emitted as clean events.

## Security benefits

**Prevents information leakage:**
- Third-party stack traces, database errors, and debug information are never exposed
- Error responses contain only standard HTTP status codes and generic messages
- No provider-specific metadata (trace IDs, internal IDs, costs) reaches clients
- Malformed provider responses fail closed with standard errors (never leaked)
- SSE comment lines stripped to prevent metadata leakage in streaming responses

**Ensures consistency:**
- Responses always match OpenAI's API format exactly
- The `model` field always reflects what the client requested, not what the provider returned
- Extra fields like `provider`, `cost`, `trace_id` are automatically dropped

**Fast failure:**
- Invalid requests fail immediately with clear, actionable error messages
- No wasted upstream requests for malformed input
- Reduces debugging time for integration issues

## Error standardization

When strict mode is enabled, all error responses follow OpenAI's error format exactly:

```json
{
  "error": {
    "message": "Invalid request",
    "type": "invalid_request_error",
    "param": null,
    "code": null
  }
}
```

**Status code mapping:**

| HTTP Status | Error Type | Message |
|------------|------------|---------|
| 400 | `invalid_request_error` | Invalid request |
| 401 | `authentication_error` | Authentication failed |
| 403 | `permission_error` | Permission denied |
| 404 | `not_found_error` | Not found |
| 429 | `rate_limit_error` | Rate limit exceeded |
| 500 | `api_error` | Internal server error |
| 502 | `api_error` | Bad gateway |
| 503 | `api_error` | Service unavailable |

Third-party error details are always logged server-side but never sent to clients.

## Supported endpoints

Strict mode currently supports:

- `/v1/chat/completions` (streaming and non-streaming) - Full sanitization
- `/v1/embeddings` - Full sanitization
- `/v1/responses` (Open Responses API, non-streaming) - Full sanitization
- `/v1/models` - Model listing (no sanitization needed)

All supported endpoints include:
- **Request validation** - Invalid requests fail immediately with clear error messages
- **Response sanitization** - Third-party metadata automatically removed
- **Model field rewriting** - Ensures consistency with client request
- **Error standardization** - Third-party error details never exposed

Requests to unsupported endpoints will return `404 Not Found` when strict mode is enabled.

## Comparison with response sanitization

| Feature | Response Sanitization | Strict Mode |
|---------|----------------------|-------------|
| Request validation | ✗ No | ✓ Yes |
| Response sanitization | ✓ Yes | ✓ Yes |
| Error standardization | ✗ No | ✓ Yes |
| Endpoint coverage | `/v1/chat/completions` only | Chat, Embeddings, Responses, Models |
| Router type | Wildcard passthrough | Typed handlers |
| Use case | Simple response cleaning | Production security & compliance |

**Important:** When strict mode is enabled globally, the per-target `sanitize_response` flag is automatically ignored. Strict mode handlers perform complete sanitization themselves, so enabling `sanitize_response: true` on individual targets has no effect and won't cause double sanitization.

**When to use strict mode:**
- Production deployments requiring security hardening
- Compliance requirements around error message content
- Multi-provider setups needing guaranteed response consistency
- Applications that need request validation before forwarding

**When to use response sanitization:**
- Simple use cases where you only need response cleaning
- Non-security-critical deployments
- Maximum flexibility with endpoint coverage

## Trusted Pools

In strict mode, you can mark entire provider pools as trusted to bypass all sanitization. This is useful when you have providers you fully control (e.g., your own OpenAI account) and want their exact responses and error messages passed through.

**Note:** The `trusted` flag is set at the pool level, meaning all providers in a trusted pool bypass sanitization. You cannot mix trusted and untrusted providers within the same pool.

### Configuration

**Single-provider configuration:**

```json
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
```

**Multi-provider pool configuration:**

```json
{
  "strict_mode": true,
  "targets": {
    "gpt-4-pool": {
      "trusted": true,
      "providers": [
        {
          "url": "https://api.openai.com",
          "onwards_key": "sk-primary-..."
        },
        {
          "url": "https://api.openai.com",
          "onwards_key": "sk-backup-..."
        }
      ]
    }
  }
}
```

In these examples, `gpt-4` and `gpt-4-pool` responses bypass all sanitization, while `third-party` responses receive full strict mode sanitization.

### Behavior

When a pool is marked as `trusted: true`:

- **Success responses**: Passed through completely with all provider metadata intact
- **Error responses**: Original error messages and metadata forwarded to clients
- **No model field rewriting**: Response model field matches provider's response exactly
- **No Content-Length updates**: Headers passed through as-is
- **No SSE sanitization**: Streaming responses passed through without line-by-line parsing

### Security Warning

⚠️ **Use trusted pools carefully.** Marking a pool as trusted bypasses all strict mode security guarantees for all providers in that pool:

- Provider-specific metadata may leak to clients (costs, trace IDs, internal identifiers)
- Third-party error details and stack traces will be exposed
- Responses may not match OpenAI schema exactly
- Non-standard fields may confuse client applications

**Only mark pools as trusted when you fully control or trust ALL providers in that pool.** This typically means:
- Your own OpenAI/Anthropic accounts (all providers using your API keys)
- Self-hosted models you operate (all instances)
- Internal services you maintain (all endpoints)

**Do not mix trusted and untrusted providers** - if you have mixed trust levels, create separate pools for trusted and untrusted providers.

## Implementation details

For developers working on the Onwards codebase:

**Router architecture:**
- Strict mode uses typed Axum handlers defined in `src/strict/handlers.rs`
- Each endpoint has dedicated request/response schema types in `src/strict/schemas/`
- Requests are deserialized using serde, which automatically validates structure
- `response_transform_fn` is skipped when strict mode is enabled to prevent double sanitization

**Response sanitization:**
- Responses are deserialized through strict schemas (extra fields automatically dropped by serde)
- Malformed responses fail closed with standard errors - never passed through
- Model field is rewritten to match the original request model
- Re-serialized to ensure only defined fields are present
- Content-Length headers updated to match sanitized response size
- Applies to both non-streaming responses and SSE chunks
- SSE streams processed line-by-line to handle multi-line events and strip comments

**Error handling:**
- Third-party errors are intercepted in `sanitize_error_response()`
- Original error logged with `error!()` macro for server-side debugging
- Standard error generated based only on HTTP status code
- OpenAI-compatible format guaranteed via `error_response()` helper
- Deserialization failures return standard errors, never leak malformed responses

**Testing:**
- Request/response schema tests in each schema module
- Integration tests in `src/strict/handlers.rs` verify sanitization behavior
- Tests verify fail-closed behavior on malformed responses (no passthrough)
- Tests verify SSE multi-line events and comment stripping
- Tests verify Content-Length header correctness after sanitization
