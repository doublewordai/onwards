# Onwards

[![Crates.io](https://img.shields.io/crates/v/onwards)](https://crates.io/crates/onwards)
[![Documentation](https://docs.rs/onwards/badge.svg)](https://docs.rs/onwards)
[![GitHub](https://img.shields.io/badge/GitHub-doublewordai%2Fonwards-blue)](https://github.com/doublewordai/onwards)

A Rust-based AI Gateway that provides a unified interface for routing requests to OpenAI-compatible targets. The goal is to be as "transparent" as possible.

**[Read the full documentation](https://doublewordai.github.io/onwards/)**

## Quickstart

Create a `config.json`:

```json
{
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-your-openai-key",
      "onwards_model": "gpt-4"
    }
  }
}
```

Start the gateway:

```bash
cargo run -- -f config.json
```

Send a request:

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

## Features

- Unified routing to any OpenAI-compatible provider
- Hot-reloading configuration with automatic file watching
- Authentication with global and per-target API keys
- Rate limiting and concurrency limiting (per-target and per-key)
- Load balancing with weighted random and priority strategies
- Automatic failover across multiple providers
- Strict mode for request validation and error standardization
- Response sanitization for OpenAI schema compliance
- Prometheus metrics
- Custom response headers

## Documentation

Full documentation is available at **[doublewordai.github.io/onwards](https://doublewordai.github.io/onwards/)**, covering:

- [Configuration reference](https://doublewordai.github.io/onwards/configuration.html)
- [Authentication](https://doublewordai.github.io/onwards/authentication.html)
- [Rate limiting](https://doublewordai.github.io/onwards/rate-limiting.html)
- [Load balancing](https://doublewordai.github.io/onwards/load-balancing.html)
- [Strict mode](https://doublewordai.github.io/onwards/strict-mode.html)
- [Response sanitization](https://doublewordai.github.io/onwards/sanitization.html)
- [Contributing](https://doublewordai.github.io/onwards/contributing.html)
