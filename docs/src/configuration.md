# Configuration

Onwards is configured through a JSON file. Each key in the `targets` object defines a model alias that clients can request.

## Target options

| Option | Type | Required | Description |
|--------|------|----------|-------------|
| `url` | string | Yes | Base URL of the AI provider |
| `onwards_key` | string | No | API key to include in requests to the target |
| `onwards_model` | string | No | Model name to use when forwarding requests |
| `keys` | string[] | No | API keys required for authentication to this target |
| `rate_limit` | object | No | Per-target rate limiting (see [Rate Limiting](rate-limiting.md)) |
| `concurrency_limit` | object | No | Per-target concurrency limiting (see [Concurrency Limiting](concurrency.md)) |
| `upstream_auth_header_name` | string | No | Custom header name for upstream auth (default: `Authorization`) |
| `upstream_auth_header_prefix` | string | No | Custom prefix for upstream auth header value (default: `Bearer `) |
| `response_headers` | object | No | Key-value pairs to add or override in the response headers |
| `sanitize_response` | bool | No | Enforce strict OpenAI schema compliance (see [Sanitization](sanitization.md)) |
| `strategy` | string | No | Load balancing strategy: `weighted_random` or `priority` |
| `fallback` | object | No | Retry configuration (see [Load Balancing](load-balancing.md)) |
| `providers` | array | No | Array of provider configurations for load balancing |

## Rate limit object

| Field | Type | Description |
|-------|------|-------------|
| `requests_per_second` | float | Number of requests allowed per second |
| `burst_size` | integer | Maximum burst size of requests |

## Concurrency limit object

| Field | Type | Description |
|-------|------|-------------|
| `max_concurrent_requests` | integer | Maximum number of concurrent requests |

## Auth configuration

The top-level `auth` object configures global authentication:

| Field | Type | Description |
|-------|------|-------------|
| `global_keys` | string[] | Keys that grant access to all authenticated targets |
| `key_definitions` | object | Named key definitions with per-key rate/concurrency limits |

See [Authentication](authentication.md) for details.

## Minimal example

```json
{
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key"
    }
  }
}
```

## Full example

```json
{
  "auth": {
    "global_keys": ["global-api-key-1"],
    "key_definitions": {
      "premium_user": {
        "key": "sk-premium-67890",
        "rate_limit": {
          "requests_per_second": 100,
          "burst_size": 200
        },
        "concurrency_limit": {
          "max_concurrent_requests": 10
        }
      }
    }
  },
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key",
      "onwards_model": "gpt-4-turbo",
      "keys": ["premium_user"],
      "rate_limit": {
        "requests_per_second": 50,
        "burst_size": 100
      },
      "concurrency_limit": {
        "max_concurrent_requests": 20
      },
      "response_headers": {
        "Input-Price-Per-Token": "0.0001",
        "Output-Price-Per-Token": "0.0002"
      },
      "sanitize_response": true
    }
  }
}
```
