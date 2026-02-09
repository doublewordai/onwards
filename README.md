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
- Response sanitization for strict OpenAI schema compliance
- **HTTP connection pooling** for high-performance request handling
- Prometheus metrics
- Custom response headers

## Performance Tuning

### Connection Pooling

Onwards uses HTTP connection pooling to dramatically improve performance under load by reusing connections instead of creating new ones for each request. This eliminates the 1:1 request-to-file-descriptor ratio and prevents TIME_WAIT connection accumulation.

**Default settings (suitable for most deployments):**
```bash
onwards -f config.json \
  --pool-max-idle-per-host 100 \
  --pool-idle-timeout-secs 90
```

**When to increase `pool-max-idle-per-host`:**

The pool limit is applied **per upstream host**. Choose based on your deployment pattern:

#### Scenario 1: Fan-out (Multiple Upstreams)
*Example: Main server routes to 10+ different model providers*

- **Recommendation:** `--pool-max-idle-per-host 200-300`
- **Why:** Traffic spreads across many upstreams, each gets moderate volume
- **Math:** 10 providers × 200 connections = 2,000 total pooled connections

```bash
# Fan-out configuration
onwards -f config.json \
  --pool-max-idle-per-host 300 \
  --pool-idle-timeout-secs 90
```

#### Scenario 2: Single Upstream (High Concurrency)
*Example: Gateway in front of a single vLLM server handling all traffic*

- **Recommendation:** `--pool-max-idle-per-host 1000-2000`
- **Why:** ALL traffic goes to one host - needs high capacity to avoid creating new connections
- **Math:** Peak 2000 concurrent requests → 2000 pooled connections reused across all requests

```bash
# Single upstream configuration
onwards -f config.json \
  --pool-max-idle-per-host 2000 \
  --pool-idle-timeout-secs 120
```

**Rule of thumb:** Set `pool-max-idle-per-host` >= your expected peak concurrent requests per upstream host. If the pool is too small, new connections will be created beyond the pool limit, reducing the performance benefit.

### Idle Timeout

**Why 90 seconds is optimal:**

The `pool-idle-timeout-secs` setting (default: 90s) controls how long idle connections stay in the pool:

- **HTTP/2 standard:** Recommends keeping connections alive for 2+ minutes
- **Cloud load balancers:** Typically timeout after 60-120 seconds  
- **90s balances:**
  - ✅ Long enough to reuse connections between request bursts
  - ✅ Short enough to avoid holding stale connections upstream LBs have closed
  - ✅ Prevents connection leak from forgotten idle connections

**When to adjust:**
- Increase to **120s** for more aggressive connection reuse (bursty traffic with gaps)
- Decrease to **60s** if you see connection errors (upstream closing connections sooner)

### Monitoring Connection Usage

```bash
# Monitor file descriptor usage (Linux/macOS)
lsof -p $(pgrep onwards) | wc -l

# Check connection states
ss -s  # Linux
netstat -an | grep TIME_WAIT | wc -l  # macOS/Linux
```

**Expected improvements with connection pooling:**
- **Before:** 1000+ file descriptors for 200 concurrent requests (1:1 ratio)
- **After:** 150-300 file descriptors for 200 concurrent requests (10:1 reuse ratio)
- TIME_WAIT connections drop from thousands to near-zero

## Documentation

Full documentation is available at **[doublewordai.github.io/onwards](https://doublewordai.github.io/onwards/)**, covering:

- [Configuration reference](https://doublewordai.github.io/onwards/configuration.html)
- [Authentication](https://doublewordai.github.io/onwards/authentication.html)
- [Rate limiting](https://doublewordai.github.io/onwards/rate-limiting.html)
- [Load balancing](https://doublewordai.github.io/onwards/load-balancing.html)
- [Response sanitization](https://doublewordai.github.io/onwards/sanitization.html)
- [Contributing](https://doublewordai.github.io/onwards/contributing.html)
