# Onwards

[![Crates.io](https://img.shields.io/crates/v/onwards)](https://crates.io/crates/onwards)
[![Documentation](https://docs.rs/onwards/badge.svg)](https://docs.rs/onwards)
[![GitHub](https://img.shields.io/badge/GitHub-doublewordai%2Fonwards-blue)](https://github.com/doublewordai/onwards)

A Rust-based AI Gateway that provides a unified interface for routing requests
to openAI compatible targets. The goal is to be as 'transparent' as possible.

## Quickstart

Create a `config.json` file with your target configurations:

```json
{
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key",
      "onwards_model": "gpt-4"
    },
    "claude-3": {
      "url": "https://api.anthropic.com",
      "onwards_key": "sk-ant-your-anthropic-key"
    },
    "local-model": {
      "url": "http://localhost:8080"
    }
  }
}
```

Start the gateway:

```bash
cargo run -- -f config.json
```

Modifying the file will automatically & atomically reload the configuration (to
disable, set the `--watch` flag to false).

### Configuration Options

- `url`: The base URL of the AI provider
- `onwards_key`: API key to include in requests to the target (optional)
- `onwards_model`: Model name to use when forwarding requests (optional)
- `keys`: Array of API keys required for authentication to this target (optional)

## Usage

### Command Line Options

- `--targets <file>`: Path to configuration file (required)
- `--port <port>`: Port to listen on (default: 3000)
- `--watch`: Enable configuration file watching for hot-reloading (default: true)
- `--metrics`: Enable Prometheus metrics endpoint (default: true)
- `--metrics-port <port>`: Port for Prometheus metrics (default: 9090)
- `--metrics-prefix <prefix>`: Prefix for metrics (default: "onwards")

### API Usage

### List Available Models

Get a list of all configured targets, in the openAI models format:

```bash
curl http://localhost:3000/v1/models
```

### Sending requests

Send requests to the gateway using the standard OpenAI API format:

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

### Model Override Header

Override the target using the `model-override` header:

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "model-override: claude-3" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

This is also used for routing requests without bodies - for example, to get the
embeddings usage for your organization:

```bash
curl -X GET http://localhost:3000/v1/organization/usage/embeddings \
  -H "model-override: claude-3"
```

### Metrics

To enable Prometheus metrics, start the gateway with the `--metrics` flag, then access the metrics endpoint by:

```bash
curl http://localhost:9090/metrics
```

## Authentication

Onwards supports bearer token authentication to control access to your AI targets. You can configure authentication keys both globally and per-target.

### Global Authentication Keys

Global keys apply to all targets that have authentication enabled:

```json
{
  "auth": {
    "keys": [
      { "key": "global-api-key-1" },
      { "key": "global-api-key-2" }
    ]
  },
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key",
      "keys": [
        { "key": "target-specific-key" }
      ]
    }
  }
}
```

### Per-Target Authentication

You can also specify authentication keys for individual targets:

```json
{
  "targets": {
    "secure-gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key",
      "keys": [
        { "key": "secure-key-1" },
        { "key": "secure-key-2" }
      ]
    },
    "open-local": {
      "url": "http://localhost:8080"
    }
  }
}
```

In this example:

- `secure-gpt-4` requires a valid bearer token from the `keys` array
- `open-local` has no authentication requirements

If both global and local keys are supplied, either global or local keys will be valid for accessing models with local keys.

### How Authentication Works

When a target has `keys` configured, requests must include a valid `Authorization: Bearer <token>` header where `<token>` matches one of the configured keys. If global keys are configured, they are automatically added to each target's key set.

**Successful authenticated request:**

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer secure-key-1" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "secure-gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

**Failed authentication (invalid key):**

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer wrong-key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "secure-gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
# Returns: 401 Unauthorized
```

**Failed authentication (missing header):**

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "secure-gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
# Returns: 401 Unauthorized
```

**No authentication required:**

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "open-local",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
# Success - no authentication required for this target
```

## Rate Limiting

Onwards supports per-target rate limiting using a token bucket algorithm. This
allows you to control the request rate to each AI provider independently.

### Configuration

Add rate limiting to any target in your `config.json`:

```json
{
  "targets": {
    "rate-limited-model": {
      "url": "https://api.provider.com",
      "onwards_key": "your-api-key",
      "rate_limit": {
        "requests_per_second": 5.0,
        "burst_size": 10
      }
    }
  }
}
```

### How It Works

We use a "Token Bucket Algorithm": Each target gets its own token bucket.Tokens
are refilled at a rate determined by the "requests_per_second" parameter. The
maximum number of tokens in the bucket is determined by the "burst_size"
parameter. When the bucket is empty, requests to that target will be rejected
with a `429 Too Many Requests` response.

### Examples

```json
// Allow 1 request per second with burst of 5
"rate_limit": {
  "requests_per_second": 1.0,
  "burst_size": 5
}

// Allow 100 requests per second with burst of 200  
"rate_limit": {
  "requests_per_second": 100.0,
  "burst_size": 200
}
```

Rate limiting is optional - targets without `rate_limit` configuration have no
rate limiting applied.

## Per-API-Key Rate Limiting

In addition to per-target rate limiting, Onwards supports individual rate limits for different API keys. This allows you to provide different service tiers to your users - for example, basic users might have lower limits while premium users get higher limits.

### Configuration

Per-key rate limiting is configured directly on each key object:

```json
{
  "auth": {
    "keys": [
      {
        "key": "sk-user-12345",
        "rate_limit": {
          "requests_per_second": 10,
          "burst_size": 20
        }
      },
      {
        "key": "sk-premium-67890",
        "rate_limit": {
          "requests_per_second": 100,
          "burst_size": 200
        }
      },
      {
        "key": "sk-enterprise-abcdef",
        "rate_limit": {
          "requests_per_second": 500,
          "burst_size": 1000
        }
      },
      { "key": "fallback-key" }
    ]
  },
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key"
    }
  }
}
```

### Priority Order

Rate limits are checked in this order:

1. **Per-key rate limits** (if the API key has limits configured)
2. **Per-target rate limits** (if the target has limits configured)

If either limit is exceeded, the request returns `429 Too Many Requests`.

### Usage Examples

**Basic user request (10/sec limit):**

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer sk-user-12345" \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4", "messages": [{"role": "user", "content": "Hello!"}]}'
```

**Premium user request (100/sec limit):**

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer sk-premium-67890" \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4", "messages": [{"role": "user", "content": "Hello!"}]}'
```

**Key without rate limits:**

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer fallback-key" \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4", "messages": [{"role": "user", "content": "Hello!"}]}'
```

## Testing

Run the test suite:

```bash
cargo test
```
