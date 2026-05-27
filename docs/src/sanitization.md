# Response Sanitization

Onwards can enforce strict OpenAI API schema compliance for `/v1/chat/completions` responses. This feature:

- **Removes provider-specific fields** from responses
- **Rewrites the model field** to match what the client originally requested
- **Supports both streaming and non-streaming** responses
- **Validates responses** against OpenAI's official API schema
- **Sanitizes error responses** to prevent upstream provider details from leaking to clients

This is useful when proxying to non-OpenAI providers that add custom fields, or when using `onwards_model` to rewrite model names upstream.

> **Note:** For production deployments requiring additional security (request validation, error standardization), consider using [Strict Mode](strict-mode.md) instead, which includes response sanitization plus comprehensive security features.

## Enabling response sanitization

Add `sanitize_response: true` to any target or provider in your configuration.

**Single provider:**

```json
{
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-key",
      "onwards_model": "gpt-4-turbo-2024-04-09",
      "sanitize_response": true
    }
  }
}
```

**Pool with multiple providers:**

```json
{
  "targets": {
    "gpt-4": {
      "sanitize_response": true,
      "providers": [
        {
          "url": "https://api1.example.com",
          "onwards_key": "sk-key-1"
        },
        {
          "url": "https://api2.example.com",
          "onwards_key": "sk-key-2"
        }
      ]
    }
  }
}
```

## How it works

When `sanitize_response: true` and a client requests `model: gpt-4`:

1. **Request sent upstream** with `model: gpt-4`
2. **Upstream responds** with custom fields and `model: gpt-4-turbo-2024-04-09`
3. **Onwards sanitizes**:
   - Parses response using OpenAI schema (removes unknown fields)
   - Rewrites `model` field to `gpt-4` (matches original request)
   - Reserializes clean response
4. **Client receives** standard OpenAI response with `model: gpt-4`

## Common use cases

**Third-party providers** (e.g., OpenRouter, Together AI) often add extra fields like `provider`, `native_finish_reason`, `cost`, etc. Sanitization strips these.

**Provider comparison** -- normalize responses from different providers for consistent handling.

**Debugging** -- reduce noise by filtering to only standard OpenAI fields.

## Error sanitization

When `sanitize_response: true`, error responses from upstream providers are also sanitized. This prevents information leakage -- upstream error bodies can contain provider names, internal URLs, and model identifiers that you may not want exposed to clients.

### How it works

Onwards replaces the upstream error body with a generic OpenAI-compatible error, while preserving the original HTTP status code:

- **4xx errors** are replaced with:

```json
{
  "error": {
    "message": "The upstream provider rejected the request.",
    "type": "invalid_request_error",
    "param": null,
    "code": "upstream_error"
  }
}
```

- **5xx errors** (and any other non-2xx status) are replaced with:

```json
{
  "error": {
    "message": "An internal error occurred. Please try again later.",
    "type": "internal_error",
    "param": null,
    "code": "internal_error"
  }
}
```

The original error body is logged at `ERROR` level (up to 64 KB) for debugging, so operators can still investigate upstream failures without exposing details to clients.

### Errors embedded in 2xx SSE streams

Some providers — most notably OpenRouter — return `HTTP 200 OK` and start an SSE stream even when the *upstream* of the upstream has failed. The failure surfaces as a chunk with shape:

```json
data: {"id":"...","object":"chat.completion.chunk","choices":[],"error":{"code":429,"message":"..."}}
```

The naive strict deserializer would parse this as a valid (empty) chunk and silently drop the `error` field, leaving a downstream stream reassembler with no completion content and no signal that anything went wrong.

Onwards detects the embedded `error` envelope *before* strict deserialization and forwards it as a stand-alone event:

```text
data: {"error":{"message":"Rate limit exceeded","type":"rate_limit_error","param":null,"code":429}}
```

The chunk wrapper is stripped so the emitted SSE data line begins with `{"error"` — that prefix is what the [`fusillade`](https://crates.io/crates/fusillade) reassembler matches on to reclassify the response from HTTP 200 to the embedded `code`. End-to-end, an upstream-of-upstream 429 becomes a real HTTP 429 to the client, with retry semantics intact.

This applies to both bare error chunks and chunks that carry completion fields alongside the error.

### Account-class status code masking

Untrusted providers can return status codes that describe the *operator's* relationship with the provider — not the *caller's* relationship with onwards. Surfacing those would let callers probe the operator's account state. Onwards rewrites:

| Upstream status | Surfaced as | Rationale |
|---|---|---|
| `401 Unauthorized` | `502 Bad Gateway` | The caller's auth is fine; ours isn't — don't expose that |
| `402 Payment Required` | `502 Bad Gateway` | Provider billing state is internal |
| `403 Forbidden` | `502 Bad Gateway` | Permission failures are operator-scoped |
| `451 Unavailable For Legal Reasons` | `502 Bad Gateway` | Jurisdictional restrictions are operator-scoped |
| `408 Request Timeout` | `504 Gateway Timeout` | Upstream timeouts are gateway timeouts from the caller's view |

User-facing codes (`400`, `404`, `413`, `422`, `429`) pass through unchanged — they're real signal about the caller's request and downstream retry logic depends on seeing them.

Masking is applied to both non-streaming error responses (where the upstream status is the outer HTTP status) and to embedded `error.code` values in SSE streams (so a downstream reassembler that reclassifies on the embedded code surfaces the masked code as the HTTP status). Trusted targets bypass masking entirely.

### Error format

All Onwards error responses (both sanitized upstream errors and errors generated by Onwards itself) use the OpenAI-compatible `{"error": {...}}` envelope:

```json
{
  "error": {
    "message": "...",
    "type": "...",
    "param": null,
    "code": "..."
  }
}
```

| Field | Description |
|-------|-------------|
| `message` | Human-readable error description |
| `type` | Error category (`invalid_request_error`, `rate_limit_error`, `internal_error`) |
| `param` | The request parameter that caused the error, if applicable |
| `code` | Machine-readable error code |

## Supported endpoints

Currently supports:

- `/v1/chat/completions` (streaming and non-streaming)
