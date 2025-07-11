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
- `key`: API key for authentication (optional)
- `onwards_model`: Model name to use when forwarding requests (optional)

## Usage

### Command Line Options

- `--targets <file>`: Path to configuration file (required)
- `--port <port>`: Port to listen on (default: 3000)
- `--watch`: Enable configuration file watching for hot-reloading (default: true)

### API Usage

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

This is also used for routing requests without bodies - for example, to list
models available at a specific provider:

```bash
curl -X GET http://localhost:3000/v1/models \
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

### List Available Models

Get a list of all configured models:

```bash
curl http://localhost:3000/v1/models
```

## Testing

Run the test suite:

```bash
cargo test
```
