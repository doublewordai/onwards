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

## Authentication

Onwards supports bearer token authentication to control access to your AI targets. You can configure authentication keys both globally and per-target.

### Global Authentication Keys

Global keys apply to all targets that have authentication enabled:

```json
{
  "auth": {
    "global_keys": ["global-api-key-1", "global-api-key-2"]
  },
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key",
      "keys": ["target-specific-key"]
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
      "keys": ["secure-key-1", "secure-key-2"]
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

## Testing

Run the test suite:

```bash
cargo test
```
