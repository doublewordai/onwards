# Response Sanitization

Onwards can enforce strict OpenAI API schema compliance for `/v1/chat/completions` responses. This feature:

- **Removes provider-specific fields** from responses
- **Rewrites the model field** to match what the client originally requested
- **Supports both streaming and non-streaming** responses
- **Validates responses** against OpenAI's official API schema

This is useful when proxying to non-OpenAI providers that add custom fields, or when using `onwards_model` to rewrite model names upstream.

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

## Supported endpoints

Currently supports:

- `/v1/chat/completions` (streaming and non-streaming)
