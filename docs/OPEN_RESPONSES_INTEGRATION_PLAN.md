# Open Responses Specification Integration Plan

## Executive Summary

This document outlines a plan for enhanced [Open Responses specification](https://github.com/openresponses/openresponses) support in the Onwards LLM router.

**Current state**: Open Responses passthrough already works today via the wildcard route. If your upstream supports `/v1/responses`, it works with no changes.

**This plan adds**: A `strict_mode` option with a separate router that provides schema validation, and an adapter layer for upstreams that only support Chat Completions.

---

## Background

### Current Onwards Architecture

Onwards is a Rust-based **transparent LLM proxy** that routes requests to upstream providers based on the `model` field:

- **Transparent passthrough**: Requests forwarded as-is via wildcard route `/{*path}`
- **Minimal parsing**: Only the `model` field is extracted for routing decisions
- **Protocol agnostic**: Works with any endpoint the upstream supports, including `/v1/responses`
- **Optional transformations**: Model rewriting, response sanitization, body transforms (all opt-in)
- **Load balancing**: Weighted random / priority selection across provider pools
- **SSE buffering**: Handles incomplete JSON chunks in streaming responses

**Current state**: `/v1/responses` already works today if the upstream provider supports it.

### Open Responses Specification

[Open Responses](https://openresponses.org) is an open-source specification for multi-provider, interoperable LLM interfaces. Initiated by OpenAI (March 2025) and developed with the open-source community (backed by Hugging Face), it extends OpenAI's Responses API as an open standard designed for **agentic workloads**.

#### Why Open Responses?

The Chat Completions API was designed for turn-based conversations. As LLM applications evolved toward autonomous agents, limitations emerged:

- **Tool loops**: Chat Completions requires clients to manage tool execution loops manually
- **Reasoning visibility**: No standard way to expose model reasoning/thinking
- **Streaming semantics**: Raw text deltas don't convey structured meaning
- **State management**: Clients must resend full conversation history each turn

Open Responses addresses these with purpose-built primitives for agentic systems.

#### Core Concepts

**Items as the context unit**: Typed Items with explicit state machines replace flat messages:

```json
{
  "input": [
    { "type": "message", "role": "user", "content": "Book me a flight to Paris" },
    { "type": "function_call_output", "call_id": "call_123", "output": "{...}" }
  ]
}
```

**Semantic streaming events**: Typed events instead of raw text deltas:

```
event: response.output_item.added
data: {"type": "message", "id": "item_0", ...}

event: response.output_text.delta
data: {"item_id": "item_0", "delta": "Hello", "sequence_number": 1}

event: response.output_item.done
data: {"type": "message", "id": "item_0", "status": "completed", ...}
```

**Provider-managed tool loops**: Server-side tool execution:

```json
{
  "model": "gpt-4o",
  "input": "What's the weather in Paris?",
  "tools": [{"type": "function", "name": "get_weather", ...}],
  "tool_choice": "auto"
}
// Provider executes get_weather internally, returns final response
```

**Reasoning traces**: Structured access to model thinking:

```json
{
  "output": [
    {
      "type": "reasoning",
      "content": "User wants weather info...",
      "encrypted_content": "...",
      "summary": "Determined user location..."
    }
  ]
}
```

**Stateful conversations**: Reference previous responses instead of resending context:

```json
{
  "previous_response_id": "resp_abc123",
  "input": "What about tomorrow?"
}
```

#### Comparison with Chat Completions

| Aspect | Chat Completions | Open Responses |
|--------|------------------|----------------|
| **Endpoint** | `/v1/chat/completions` | `/v1/responses` |
| **Context unit** | Messages array | Items array (typed, with state) |
| **Streaming** | Raw text deltas | Semantic events (`response.*`) |
| **State** | Stateless | Optional via `previous_response_id` |
| **Tool loops** | Client-managed | Provider-managed |
| **Reasoning** | Not exposed | `content`, `encrypted_content`, `summary` |
| **Request mapping** | 1:1 | 1:N (tool loops require multiple LLM calls) |

---

## Integration Strategy

### Current State: Passthrough Already Works

The existing wildcard route `/{*path}` already forwards `/v1/responses` requests to upstreams. **No changes needed for native passthrough.**

```
Client (any format) → Onwards (extract model, route) → Provider (handles format)
```

### Proposed: Strict Mode with Separate Router

Strict mode uses a **separate router** with typed handlers that leverage Axum's extractor system:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Router Selection                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  strict_mode: false (default)                                   │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │  Router::new()                                           │   │
│  │    .route("/{*path}", any(target_message_handler))       │   │
│  │  // Transparent passthrough, any path accepted           │   │
│  └─────────────────────────────────────────────────────────┘   │
│                                                                 │
│  strict_mode: true                                              │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │  Router::new()                                           │   │
│  │    .route("/v1/chat/completions", post(chat_handler))    │   │
│  │    .route("/v1/responses", post(responses_handler))      │   │
│  │    .route("/v1/embeddings", post(embeddings_handler))    │   │
│  │    .route("/v1/models", get(models_handler))             │   │
│  │  // Unknown paths → 404 automatically                    │   │
│  │  // Axum extractors validate request schemas             │   │
│  └─────────────────────────────────────────────────────────┘   │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

**Benefits:**
- Leverages Axum's built-in validation via `Json<T>` extractors (serde deserialization)
- Unknown paths rejected automatically (no wildcard)
- Type-safe request handling
- Upstream validates further - we just ensure well-formed requests

### Open Responses Adapter

The adapter implements full Open Responses semantics over **any Chat Completions backend**. It's not just for "legacy" upstreams - it adds capabilities that even partial Responses API implementations (like vLLM) lack:

- **State management** (`previous_response_id`) - most upstreams don't implement this
- **Tool loop orchestration** - execute tools at the proxy, regardless of upstream support
- **Streaming event synthesis** - generate semantic events from raw deltas
- **Hybrid tool handling** - some tools local, some passed through

**The adapter always uses Chat Completions internally**, even if the upstream claims Responses API support. This is simpler and more reliable - partial implementations (like vLLM's stateless Responses API) don't offer meaningful advantages when the adapter is managing state and tool loops anyway.

```
┌─────────────────────────────────────────────────────────────────┐
│                    Open Responses Adapter                        │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Client Request (/v1/responses)                                 │
│       │                                                         │
│       ▼                                                         │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │ 1. Expand previous_response_id (if present)             │   │
│  │    └── ResponseStore.get_context()                       │   │
│  └─────────────────────────────────────────────────────────┘   │
│       │                                                         │
│       ▼                                                         │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │ 2. Convert Items → Messages                             │   │
│  └─────────────────────────────────────────────────────────┘   │
│       │                                                         │
│       ▼                                                         │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │ 3. Forward to upstream (/v1/chat/completions)           │   │
│  │    └── Always Chat Completions, regardless of upstream   │   │
│  └─────────────────────────────────────────────────────────┘   │
│       │                                                         │
│       ▼                                                         │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │ 4. Tool call in response?                               │   │
│  │    ├── ToolExecutor.can_handle()? Execute locally       │   │
│  │    │   └── Loop back to step 3 with result              │   │
│  │    └── Return to client (requires_action)               │   │
│  └─────────────────────────────────────────────────────────┘   │
│       │                                                         │
│       ▼                                                         │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │ 5. Convert response + synthesize streaming events       │   │
│  └─────────────────────────────────────────────────────────┘   │
│       │                                                         │
│       ▼                                                         │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │ 6. Store response for future previous_response_id       │   │
│  │    └── ResponseStore.store()                            │   │
│  └─────────────────────────────────────────────────────────┘   │
│       │                                                         │
│       ▼                                                         │
│  Client Response (Open Responses format)                        │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Extensibility Traits

The system is **stateless by default**, but library users can implement traits for advanced features:

```rust
/// Trait for storing and retrieving response context.
/// Implement this to enable `previous_response_id` support.
#[async_trait]
pub trait ResponseStore: Send + Sync {
    async fn store(&self, response: &ResponsesResponse) -> Result<String, StoreError>;
    async fn get_context(&self, response_id: &str) -> Result<Option<Vec<Item>>, StoreError>;
}

/// Trait for executing tools server-side during sub-agent loops.
/// Implement this to enable automatic tool execution.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, tool_call: &FunctionCall) -> Result<FunctionCallOutput, ToolError>;
    fn can_handle(&self, tool_name: &str) -> bool;
}
```

Default `NoOp` implementations preserve stateless behavior. Tools not handled by `ToolExecutor` can either:
- Be returned to the client (`requires_action` status)
- Be passed through to the upstream (for hosted tools)

This allows hybrid tool handling where some tools are executed locally and others are delegated.

---

## Implementation Steps

### Step 1: Strict Router Foundation

Create separate strict router with typed Axum handlers:

```
src/
├── strict/
│   ├── mod.rs              # build_strict_router, re-exports
│   ├── handlers.rs         # Typed request handlers
│   ├── schemas/
│   │   ├── mod.rs
│   │   ├── chat_completions.rs
│   │   ├── responses.rs
│   │   └── embeddings.rs
│   └── adapter.rs          # Open Responses adapter logic
├── traits/
│   ├── mod.rs
│   ├── response_store.rs   # ResponseStore trait
│   └── tool_executor.rs    # ToolExecutor trait
```

### Step 2: Request/Response Schemas

Define request types that Axum's `Json<T>` extractor validates via serde deserialization. The upstream validates further - we just ensure well-formed JSON that matches the expected schema.

### Step 3: Response Sanitization

Validate and sanitize responses using typed schemas. Extend the existing `ResponseSanitizer` pattern for Open Responses format.

### Step 4: Open Responses Adapter

Implement the adapter that bridges Open Responses to Chat Completions:
- Items ↔ Messages conversion
- Streaming event synthesis
- Response format conversion

### Step 5: Streaming Event Synthesis

Transform `chat.completion.chunk` events into Open Responses semantic events:
- `response.output_item.added`
- `response.content_part.added`
- `response.output_text.delta`
- `response.content_part.done`
- `response.output_item.done`

### Step 6: Extensibility Traits

Define `ResponseStore` and `ToolExecutor` traits with `NoOp` defaults. Update `AppState` to be generic over these traits.

### Step 7: Tool Loop Orchestration

Implement the sub-agent loop:
- Detect tool calls in responses
- Execute via `ToolExecutor` if handled
- Pass through to upstream for hosted tools
- Return to client if unhandled
- Loop until completion or `max_tool_calls`

---

## Configuration

### Global Strict Mode

```json
{
  "strict_mode": false,  // Default: transparent passthrough

  "targets": {
    // ... unchanged when strict_mode is false
  }
}
```

### Per-Target Configuration

When `strict_mode: true`, Open Responses requests are **validated then passed through** by default. The adapter is opt-in:

```json
{
  "strict_mode": true,

  "targets": {
    "gpt-4o": {
      "url": "https://api.openai.com/v1",
      "onwards_key": "sk-..."
      // Default: validate schema, then passthrough to upstream /v1/responses
    },
    "vllm-local": {
      "url": "http://localhost:8000/v1",
      "open_responses": {
        "adapter": true  // Use adapter for full Open Responses semantics
      }
      // Even though vLLM has partial /v1/responses support, the adapter
      // provides state management and tool loops that vLLM lacks
    },
    "claude-3-opus": {
      "url": "https://api.anthropic.com",
      "onwards_key": "sk-...",
      "open_responses": {
        "adapter": true  // Use adapter (Anthropic only supports Chat Completions)
      }
    }
  }
}
```

**Strict mode behavior for `/v1/responses`:**
1. Validate request schema (fail fast with good errors)
2. If `adapter: true` → use adapter (always calls upstream via Chat Completions)
3. If `adapter: false` (default) → passthrough to upstream `/v1/responses` as-is

### Mode Comparison

| Feature | `strict_mode: false` | `strict_mode: true` |
|---------|---------------------|---------------------|
| Path validation | Any path forwarded | Only known OpenAI paths |
| Request validation | None (passthrough) | Schema validation via serde |
| Response validation | Optional (`sanitize_response`) | Always validated |
| `/v1/responses` handling | Passthrough | Validate → passthrough (default) or adapter |
| Unknown fields | Preserved | Stripped during validation |
| `body_transform_fn` | Available | Applied after validation |

---

## Testing Strategy

### Unit Tests

- Schema validation for all request/response types
- Items ↔ Messages conversion accuracy
- Streaming event synthesis correctness
- Error handling edge cases

### Integration Tests

- End-to-end with mock providers
- Real provider testing (passthrough and adapter modes)
- Tool loop scenarios with mock `ToolExecutor`

### Compliance Testing

The Open Responses repository includes a compliance test framework:

```bash
# Clone the compliance tests
git clone https://github.com/openresponses/openresponses
cd openresponses

# Run against Onwards
npx ts-node bin/compliance-test.ts --endpoint http://localhost:8080/v1/responses
```

The compliance suite validates:
- Request/response schema conformance
- Streaming event ordering and format
- Error response format
- Item state machine transitions

### Test Matrix

| Scenario | Passthrough | Strict (native) | Strict (adapter) |
|----------|-------------|-----------------|------------------|
| Basic text | ✅ | ✅ | ✅ |
| Streaming | ✅ | ✅ | ✅ (synthesized) |
| Tool calls | ✅ | ✅ | ✅ (orchestrated) |
| `previous_response_id` | ✅ | ✅ | ✅ (via trait) |
| Reasoning traces | ✅ | ✅ | ❌ (requires upstream) |

---

## Adapter Capabilities

The adapter provides full Open Responses semantics over any Chat Completions backend:

| Feature | Supported | Notes |
|---------|-----------|-------|
| `previous_response_id` | ✅ | Via `ResponseStore` trait |
| Semantic streaming events | ✅ | Synthesized from `chat.completion.chunk` |
| Item state machine | ✅ | Tracked by adapter |
| `allowed_tools` vs `tools` | ✅ | Filter/reject disallowed calls |
| Tool loops | ✅ | Via `ToolExecutor` trait |
| Proxy-level tool execution | ✅ | Execute tools locally, feed results back |
| Reasoning traces | ❌ | Requires upstream model support |
| `truncation` | ❌ | Passed through to upstream |
| `service_tier` | ❌ | Passed through to upstream |

**Why always use Chat Completions internally?**

Even upstreams with "Responses API support" (like vLLM) often have partial implementations:
- No `previous_response_id` support
- No `store` parameter
- Stateless design

Since the adapter manages state and tool loops anyway, using Chat Completions is simpler and more reliable. The adapter provides the full Open Responses semantics regardless of upstream capabilities.

---

## Additional Spec Considerations

### Error Schema

The spec defines structured errors:

```json
{
  "error": {
    "type": "invalid_request_error",
    "code": "model_not_found",
    "param": "model",
    "message": "The requested model 'fake-model' does not exist."
  }
}
```

| Error Type | HTTP Status |
|------------|-------------|
| `server_error` | 500 |
| `invalid_request_error` | 400 |
| `not_found` | 404 |
| `model_error` | 500 |
| `too_many_requests` | 429 |

### Streaming Event Ordering

The spec mandates this sequence for output items:

1. `response.output_item.added` - Item created
2. `response.content_part.added` - Content part started
3. `response.output_text.delta` - Text deltas (repeating)
4. `response.content_part.done` - Content part finished
5. `response.output_item.done` - Item finished

### Item State Machine

Items have explicit lifecycle states:

| State | Description |
|-------|-------------|
| `in_progress` | Model is currently emitting tokens |
| `incomplete` | Token budget exhausted (terminal) |
| `completed` | Fully sampled (terminal) |

### Extension Guidelines

If Onwards adds custom items/events:

- **Custom items MUST**: Be prefixed (`onwards:trace_item`), include `id`, `type`, `status`
- **Custom events MUST**: Be prefixed (`onwards:latency_event`), include `type`, `sequence_number`

---

## Open Questions

1. **Strict mode naming**: Alternatives include `validated_mode`, `schema_validation`. Current preference: `strict_mode`.

2. **Which endpoints?**: Start with chat completions, responses, embeddings, models. Expand based on demand.

3. **Error format**: Use existing OpenAI-compatible format for consistency.

4. **Upstream detection**: Explicit `open_responses.adapter` config rather than auto-detection.

5. **Schema versioning**: Start without versioning, add `OpenResponses-Version` header support if needed.

---

## References

- [Open Responses Specification](https://www.openresponses.org/specification)
- [Open Responses GitHub](https://github.com/openresponses/openresponses)
- [OpenAI Responses API Reference](https://platform.openai.com/docs/api-reference/responses)
- [Hugging Face Open Responses Blog](https://huggingface.co/blog/open-responses)
