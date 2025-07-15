# Onwards

A Rust-based AI Gateway that provides a unified interface for routing requests
to openAI compatible targets. The goal is to be as 'transparent' as possible.

## Quickstart

Create a `config.json` file with your target configurations:

```json
{
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "key": "sk-your-openai-key",
      "onwards_model": "gpt-4"
    },
    "claude-3": {
      "url": "https://api.anthropic.com",
      "key": "sk-ant-your-anthropic-key"
    },
    "local-model": {
      "url": "http://localhost:8080",
      "rate_limit": {
        "requests_per_second": 5.0,
        "burst_size": 10
      }
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
- `key`: API key for authentication (optional)
- `onwards_model`: Model name to use when forwarding requests (optional)
- `rate_limit`: Rate limiting configuration (optional)
  - `requests_per_second`: Maximum requests per second (e.g., 5.0)
  - `burst_size`: Maximum burst capacity (e.g., 10)

## Usage

### Command Line Options

- `--targets <file>`: Path to configuration file (required)
- `--port <port>`: Port to listen on (default: 3000)
- `--watch`: Enable configuration file watching for hot-reloading (default: true)

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

### Onwards Model Header

Rewrite the model name in the request body using the `onwards-model` header
(necessary for forwarding to openAI, for example):

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "onwards-model: gpt-4-turbo" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

## Rate Limiting

Onwards supports per-target rate limiting using a token bucket algorithm. This allows you to control the request rate to each AI provider independently.

### Configuration

Add rate limiting to any target in your `config.json`:

```json
{
  "targets": {
    "rate-limited-model": {
      "url": "https://api.provider.com",
      "key": "your-api-key",
      "rate_limit": {
        "requests_per_second": 5.0,
        "burst_size": 10
      }
    }
  }
}
```

### How It Works

- **Token Bucket Algorithm**: Each target gets its own token bucket
- **Requests Per Second**: Tokens are refilled at this rate 
- **Burst Size**: Maximum number of tokens (allows bursts up to this limit)
- **Rate Limit Response**: Returns `429 Too Many Requests` when limit exceeded

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

Rate limiting is optional - targets without `rate_limit` configuration have no rate limiting applied.

## Testing

Run the test suite:

```bash
cargo test
```
