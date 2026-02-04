# Open Responses Specification Integration Plan

## Executive Summary

This document outlines a plan for integrating the [Open Responses specification](https://github.com/openresponses/openresponses) into the Onwards LLM router. The Open Responses spec is an open-source standard for multi-provider, interoperable LLM interfaces that supports agentic workloads with tool calling, reasoning traces, and semantic streaming.

## Background

### Current Onwards Architecture

Onwards is a Rust-based LLM gateway built around the **OpenAI Chat Completions API** (`/v1/chat/completions`). Key characteristics:

- **Request flow**: Client → Onwards → Provider (OpenAI-compatible)
- **Data model**: Messages array with role/content structure
- **Streaming**: Raw SSE text deltas
- **Transformations**: Model rewriting, response sanitization, body transforms
- **Load balancing**: Weighted random / priority selection across provider pools

### Open Responses Specification

Open Responses extends the OpenAI Responses API as an open standard. Key differences from Chat Completions:

| Aspect | Chat Completions | Open Responses |
|--------|------------------|----------------|
| **Endpoint** | `/v1/chat/completions` | `/v1/responses` |
| **Context unit** | Messages array | Items array |
| **Streaming** | Raw text deltas | Semantic events |
| **State** | Stateless | Optional state via `previous_response_id` |
| **Tool loops** | Client-managed | Provider-managed (sub-agent loops) |
| **Reasoning** | Not exposed | `content`, `encrypted_content`, `summary` |

---

## Integration Strategy

### Current State: Passthrough Already Works

The existing wildcard route `/{*path}` (in `lib.rs:394`) already forwards `/v1/responses` requests to upstreams. **No changes needed for native passthrough.**

```
Client (any format) → Onwards (extract model, route) → Provider (handles format)
```

The upstream provider determines whether Open Responses is supported. If it is, requests work. If not, the provider returns an error. This is the transparent proxy philosophy.

### Proposed: Strict Mode as Separate Router with Axum Extractors

Rather than a middleware, strict mode uses a **separate router** with typed handlers that leverage Axum's extractor system for validation:

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

**Benefits of separate router approach:**
- ✅ Leverages Axum's built-in validation via `Json<T>` extractors
- ✅ Unknown paths rejected automatically (no wildcard)
- ✅ Type-safe request handling
- ✅ Can use `axum-valid` or `validator` crate for additional validation
- ✅ Handlers can still call shared logic (e.g., `forward_to_upstream`)

**Router construction:**

```rust
pub fn build_router<T: HttpClient>(state: AppState<T>) -> Router {
    if state.strict_mode {
        build_strict_router(state)
    } else {
        build_transparent_router(state)
    }
}

fn build_transparent_router<T: HttpClient>(state: AppState<T>) -> Router {
    Router::new()
        .route("/models", get(models_handler))
        .route("/v1/models", get(models_handler))
        .route("/{*path}", any(target_message_handler))
        .with_state(state)
}

fn build_strict_router<T: HttpClient>(state: AppState<T>) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(strict::chat_completions_handler))
        .route("/v1/responses", post(strict::responses_handler))
        .route("/v1/embeddings", post(strict::embeddings_handler))
        .route("/v1/models", get(models_handler))
        .route("/models", get(models_handler))
        .with_state(state)
    // No wildcard = unknown paths get 404
}
```

**Strict handler example using Axum extractors:**

```rust
// src/strict/handlers.rs
use axum::{extract::State, Json};

async fn responses_handler<T: HttpClient>(
    State(state): State<AppState<T>>,
    Json(req): Json<ResponsesRequest>,  // Axum validates + deserializes
) -> Result<Json<ResponsesResponse>, AppError> {
    // Request is already validated by the Json extractor

    let target = state.targets.get(&req.model)?;

    if target.open_responses.translate {
        // Translate and forward to /v1/chat/completions
        let chat_req = translator::to_chat_completions(&req)?;
        let chat_resp = forward_to_upstream(&state, &target, chat_req).await?;
        let resp = translator::from_chat_completions(&chat_resp)?;
        Ok(Json(resp))
    } else {
        // Forward as-is to upstream /v1/responses
        let resp = forward_to_upstream(&state, &target, req).await?;
        Ok(Json(resp))
    }
}
```

### Traits for Extensibility (Library Users)

The system is **stateless by default**, but library users can opt into stateful features by implementing traits:

#### State Management Trait

For `previous_response_id` support:

```rust
/// Trait for storing and retrieving response context.
/// Implement this to enable `previous_response_id` support.
#[async_trait]
pub trait ResponseStore: Send + Sync {
    /// Store a response and return its ID
    async fn store(&self, response: &ResponsesResponse) -> Result<String, StoreError>;

    /// Retrieve the context (messages/items) for a previous response
    async fn get_context(&self, response_id: &str) -> Result<Option<Vec<Item>>, StoreError>;

    /// Optional: Clean up old entries
    async fn cleanup(&self, older_than: Duration) -> Result<(), StoreError>;
}

/// No-op implementation (default) - `previous_response_id` returns error
pub struct NoOpStore;

#[async_trait]
impl ResponseStore for NoOpStore {
    async fn store(&self, _: &ResponsesResponse) -> Result<String, StoreError> {
        Err(StoreError::NotSupported)
    }

    async fn get_context(&self, _: &str) -> Result<Option<Vec<Item>>, StoreError> {
        Err(StoreError::NotSupported)
    }
}
```

#### Tool Executor Trait

For sub-agent loop support (server-side tool execution):

```rust
/// Trait for executing tools server-side during sub-agent loops.
/// Implement this to enable automatic tool execution in /v1/responses.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Execute a tool call and return the result
    async fn execute(
        &self,
        tool_call: &FunctionCall,
    ) -> Result<FunctionCallOutput, ToolError>;

    /// Check if this executor can handle the given tool
    fn can_handle(&self, tool_name: &str) -> bool;
}

/// No-op implementation (default) - all tool calls returned to client
pub struct NoOpExecutor;

#[async_trait]
impl ToolExecutor for NoOpExecutor {
    async fn execute(&self, _: &FunctionCall) -> Result<FunctionCallOutput, ToolError> {
        Err(ToolError::NotSupported)
    }

    fn can_handle(&self, _: &str) -> bool {
        false
    }
}
```

#### AppState with Traits

```rust
pub struct AppState<T: HttpClient, S = NoOpStore, E = NoOpExecutor> {
    pub client: T,
    pub targets: Targets,
    pub strict_mode: bool,

    // Optional trait implementations for library users
    pub response_store: Arc<S>,
    pub tool_executor: Arc<E>,
}

// Default construction (stateless, no tool execution)
impl<T: HttpClient> AppState<T, NoOpStore, NoOpExecutor> {
    pub fn new(client: T, targets: Targets) -> Self {
        Self {
            client,
            targets,
            strict_mode: false,
            response_store: Arc::new(NoOpStore),
            tool_executor: Arc::new(NoOpExecutor),
        }
    }
}

// With custom store/executor
impl<T: HttpClient, S: ResponseStore, E: ToolExecutor> AppState<T, S, E> {
    pub fn with_extensions(
        client: T,
        targets: Targets,
        store: S,
        executor: E,
    ) -> Self {
        Self {
            client,
            targets,
            strict_mode: false,
            response_store: Arc::new(store),
            tool_executor: Arc::new(executor),
        }
    }
}
```

#### Sub-Agent Loop with Traits

```rust
async fn execute_with_tool_loop<T, S, E>(
    state: &AppState<T, S, E>,
    target: &Target,
    mut request: ResponsesRequest,
) -> Result<ResponsesResponse, AppError>
where
    T: HttpClient,
    S: ResponseStore,
    E: ToolExecutor,
{
    let max_iterations = request.max_tool_calls.unwrap_or(10);
    let mut accumulated_output = Vec::new();

    for _ in 0..max_iterations {
        let response = forward_to_upstream(state, target, &request).await?;
        accumulated_output.extend(response.output.clone());

        // Find tool calls in output
        let tool_calls: Vec<_> = response.output.iter()
            .filter_map(|item| match item {
                OutputItem::FunctionCall(fc) => Some(fc),
                _ => None,
            })
            .collect();

        if tool_calls.is_empty() {
            // No more tool calls - done
            return Ok(ResponsesResponse {
                output: accumulated_output,
                ..response
            });
        }

        // Execute tools we can handle
        for tool_call in tool_calls {
            if state.tool_executor.can_handle(&tool_call.name) {
                let result = state.tool_executor.execute(tool_call).await?;
                request.input.push(InputItem::FunctionCallOutput(result));
            } else {
                // Can't execute - return to client
                return Ok(ResponsesResponse {
                    output: accumulated_output,
                    status: ResponseStatus::RequiresAction,
                    ..response
                });
            }
        }
    }

    // Max iterations reached
    Ok(ResponsesResponse {
        output: accumulated_output,
        status: ResponseStatus::Incomplete,
        ..Default::default()
    })
}
```

---

## Implementation Plan

### Phase 0: Passthrough (Already Complete)

**Status**: ✅ Already works today

The wildcard route `/{*path}` already forwards `/v1/responses` to upstreams. If your upstream supports Open Responses, it works. No code changes needed.

### Phase 1: Strict Router Foundation

**Goal**: Create separate strict router with typed Axum handlers.

**New module structure**:

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
│   └── translator.rs       # Protocol translation
├── traits/
│   ├── mod.rs
│   ├── response_store.rs   # ResponseStore trait
│   └── tool_executor.rs    # ToolExecutor trait
```

**Router selection**:

```rust
pub fn build_router<T, S, E>(state: AppState<T, S, E>) -> Router
where
    T: HttpClient,
    S: ResponseStore,
    E: ToolExecutor,
{
    if state.strict_mode {
        strict::build_strict_router(state)
    } else {
        build_transparent_router(state)
    }
}
```

### Phase 2: Request Schemas with Axum Extractors

**Goal**: Define request types that Axum's `Json<T>` extractor validates automatically.

```rust
// src/strict/schemas/responses.rs
use serde::{Deserialize, Serialize};
use validator::Validate;

#[derive(Debug, Deserialize, Validate)]
pub struct ResponsesRequest {
    #[validate(length(min = 1))]
    pub model: String,

    pub input: Input,

    #[serde(default)]
    pub previous_response_id: Option<String>,

    #[serde(default)]
    pub tools: Option<Vec<Tool>>,

    #[serde(default)]
    pub tool_choice: Option<ToolChoice>,

    #[serde(default)]
    pub max_tool_calls: Option<u32>,

    #[serde(default)]
    pub stream: Option<bool>,

    // ... other fields
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Input {
    Text(String),
    Items(Vec<InputItem>),
}
```

**Handler with validation**:

```rust
// Using axum-valid for additional validation
use axum_valid::Valid;

async fn responses_handler<T, S, E>(
    State(state): State<AppState<T, S, E>>,
    Valid(Json(req)): Valid<Json<ResponsesRequest>>,
) -> Result<Response, AppError>
where
    T: HttpClient,
    S: ResponseStore,
    E: ToolExecutor,
{
    // Request already validated by Json extractor + validator
    handle_responses_request(&state, req).await
}
```

### Phase 3: Response Schemas and Sanitization

**Goal**: Validate and sanitize responses using typed schemas.

```rust
// src/strict/schemas/responses.rs
#[derive(Debug, Serialize, Deserialize)]
pub struct ResponsesResponse {
    pub id: String,
    pub object: String,  // "response"
    pub created_at: u64,
    pub status: ResponseStatus,
    pub model: String,
    pub output: Vec<OutputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

// Sanitizer for responses from upstream
pub struct OpenResponsesSanitizer {
    pub original_model: Option<String>,
}

impl OpenResponsesSanitizer {
    pub fn sanitize(&self, body: &[u8]) -> Result<ResponsesResponse, SanitizeError> {
        let mut resp: ResponsesResponse = serde_json::from_slice(body)?;
        if let Some(ref model) = self.original_model {
            resp.model = model.clone();
        }
        Ok(resp)
    }
}
```

### Phase 4: Protocol Translation

**Goal**: Translate between Responses and Chat Completions formats.

```rust
// src/strict/translator.rs

/// Responses API → Chat Completions API
pub fn responses_to_chat_completions(req: &ResponsesRequest) -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: req.model.clone(),
        messages: input_to_messages(&req.input),
        tools: req.tools.clone().map(convert_tools),
        tool_choice: req.tool_choice.clone(),
        stream: req.stream,
        // Map other compatible fields...
    }
}

fn input_to_messages(input: &Input) -> Vec<Message> {
    match input {
        Input::Text(s) => vec![Message::user(s.clone())],
        Input::Items(items) => items.iter().map(item_to_message).collect(),
    }
}

/// Chat Completions API → Responses API
pub fn chat_completions_to_responses(
    resp: &ChatCompletionResponse,
    original_request: &ResponsesRequest,
) -> ResponsesResponse {
    ResponsesResponse {
        id: format!("resp_{}", resp.id),
        object: "response".to_string(),
        created_at: resp.created,
        status: ResponseStatus::Completed,
        model: original_request.model.clone(),
        output: choices_to_output(&resp.choices),
        usage: resp.usage.clone().map(convert_usage),
        error: None,
    }
}
```

### Phase 5: Streaming Translation

**Goal**: Transform streaming responses between formats.

```rust
pub struct StreamingTranslator {
    direction: TranslationDirection,
    response_id: String,
    sequence: u64,
    accumulated_content: String,
}

impl StreamingTranslator {
    /// Transform chat.completion.chunk → response.* events
    pub fn translate_chunk(&mut self, sse_data: &str) -> Result<String, TranslateError> {
        if sse_data == "[DONE]" {
            return Ok(self.emit_done_events());
        }

        let chunk: ChatCompletionChunk = serde_json::from_str(sse_data)?;

        let mut events = Vec::new();

        for choice in &chunk.choices {
            if let Some(delta) = &choice.delta {
                if let Some(content) = &delta.content {
                    events.push(format!(
                        "event: response.output_text.delta\ndata: {}\n\n",
                        serde_json::to_string(&TextDeltaEvent {
                            item_id: self.current_item_id(),
                            delta: content.clone(),
                            sequence_number: self.next_sequence(),
                        })?
                    ));
                }
            }

            if choice.finish_reason.is_some() {
                events.push(self.emit_item_done());
            }
        }

        Ok(events.join(""))
    }
}
```

### Phase 6: Trait Implementations

**Goal**: Implement the extensibility traits.

```rust
// Example: Redis-backed response store
pub struct RedisResponseStore {
    client: redis::Client,
    ttl: Duration,
}

#[async_trait]
impl ResponseStore for RedisResponseStore {
    async fn store(&self, response: &ResponsesResponse) -> Result<String, StoreError> {
        let id = response.id.clone();
        let context = extract_context(response);
        let serialized = serde_json::to_string(&context)?;

        let mut conn = self.client.get_async_connection().await?;
        conn.set_ex(&id, serialized, self.ttl.as_secs()).await?;

        Ok(id)
    }

    async fn get_context(&self, response_id: &str) -> Result<Option<Vec<Item>>, StoreError> {
        let mut conn = self.client.get_async_connection().await?;
        let serialized: Option<String> = conn.get(response_id).await?;

        match serialized {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }
}
```

### Phase 7: Sub-Agent Loop Orchestration

**Goal**: Implement server-side tool execution loop.

This is the M:N mapping where 1 `/v1/responses` request becomes N `/v1/chat/completions` calls:

```rust
async fn handle_responses_with_tools<T, S, E>(
    state: &AppState<T, S, E>,
    target: &Target,
    request: ResponsesRequest,
) -> Result<ResponsesResponse, AppError>
where
    T: HttpClient,
    S: ResponseStore,
    E: ToolExecutor,
{
    // Check if any tools can be executed server-side
    let has_executable_tools = request.tools.as_ref()
        .map(|tools| tools.iter().any(|t| state.tool_executor.can_handle(&t.name)))
        .unwrap_or(false);

    if !has_executable_tools {
        // No server-side tools - single request
        return forward_responses_request(state, target, request).await;
    }

    // Execute tool loop
    execute_with_tool_loop(state, target, request).await
}
```

---

## Configuration Changes

### Global Strict Mode Flag

```json
{
  "strict_mode": false,  // Default: transparent passthrough (existing behavior)

  "targets": {
    // ... targets unchanged when strict_mode is false
  }
}
```

### Strict Mode Configuration

When `strict_mode: true`, additional per-target options become relevant:

```json
{
  "strict_mode": true,

  "targets": {
    "gpt-4o": {
      "url": "https://api.openai.com/v1",
      "onwards_key": "sk-..."
      // No open_responses config needed - upstream supports it natively
    },
    "claude-3-opus": {
      "url": "https://api.anthropic.com",
      "onwards_key": "sk-...",
      "open_responses": {
        "translate": true  // Translate /v1/responses → /v1/chat/completions
      }
    },
    "local-llama": {
      "url": "http://localhost:8000",
      "open_responses": {
        "translate": true
      }
    }
  }
}
```

**Per-target `open_responses` config:**

```rust
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OpenResponsesConfig {
    /// When true, translate /v1/responses requests to /v1/chat/completions
    /// and translate responses back. Only applies when strict_mode is enabled.
    #[serde(default)]
    pub translate: bool,
}
```

### Mode Comparison

| Feature | `strict_mode: false` | `strict_mode: true` |
|---------|---------------------|---------------------|
| Path validation | ❌ Any path forwarded | ✅ Only known OpenAI paths |
| Request validation | ❌ Passthrough | ✅ Schema validation |
| Response validation | ⚠️ Optional (`sanitize_response`) | ✅ Always validated |
| Protocol translation | ❌ Not available | ✅ Based on `open_responses.translate` |
| Unknown fields | ✅ Preserved | ❌ Stripped during validation |
| Performance | ✅ Optimal | ⚠️ Deserialization overhead |

### Migration from Existing Options

When `strict_mode: true`:
- `sanitize_response` is **ignored** (responses always sanitized)
- `body_transform_fn` is **ignored** (strict mode handles transformations)
- `onwards_model` still works (model rewriting)

Existing deployments with `strict_mode: false` (default) are **unchanged**.

---

## API Compatibility Matrix

| Feature | Transparent (default) | Strict (passthrough) | Strict + `translate: true` |
|---------|----------------------|---------------------|---------------------------|
| Basic text generation | ✅ Passthrough | ✅ Validated | ✅ Translated |
| Tool/function calling | ✅ Passthrough | ✅ Validated | ⚠️ External only |
| Streaming | ✅ Passthrough | ✅ Validated | ⚠️ Translated |
| Reasoning traces | ✅ Passthrough | ✅ Validated | ❌ Lost |
| Encrypted reasoning | ✅ Passthrough | ✅ Validated | ❌ Lost |
| Sub-agent loops | ✅ Passthrough | ✅ Validated | ❌ Not supported |
| `previous_response_id` | ✅ Passthrough | ✅ Validated | ❌ Not supported |
| Unknown fields | ✅ Preserved | ❌ Stripped | ❌ Stripped |
| Unknown paths | ✅ Forwarded | ❌ Rejected | ❌ Rejected |
| Performance | ✅ Optimal | ⚠️ Validation | ⚠️ Translation |

---

## Implementation Timeline

### Sprint 0: Passthrough (Done)
- [x] `/v1/responses` already works via wildcard route
- [ ] (Optional) Document that Open Responses passthrough is supported

**Deliverable**: Users can use `/v1/responses` today if their upstream supports it.

### Sprint 1: Strict Router Foundation (1 week)
- [ ] Add `strict_mode` flag to config
- [ ] Create `src/strict/mod.rs` with `build_strict_router()`
- [ ] Add typed handlers for `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings`
- [ ] Route selection based on `strict_mode` in `build_router()`

**Deliverable**: `strict_mode: true` uses separate router with typed handlers.

### Sprint 2: Request/Response Schemas (1-2 weeks)
- [ ] Define request schemas (`ChatCompletionRequest`, `ResponsesRequest`, etc.)
- [ ] Define response schemas (`ChatCompletionResponse`, `ResponsesResponse`, etc.)
- [ ] Use `axum-valid` or `validator` crate for additional validation
- [ ] Implement response sanitizers for each endpoint type

**Deliverable**: Full request/response validation via Axum extractors.

### Sprint 3: Protocol Translation (1-2 weeks)
- [ ] Implement `translator::responses_to_chat_completions()`
- [ ] Implement `translator::chat_completions_to_responses()`
- [ ] Add `open_responses.translate` target config
- [ ] Non-streaming end-to-end working

**Deliverable**: `/v1/responses` works with Chat Completions-only upstreams.

### Sprint 4: Streaming Translation (1 week)
- [ ] Implement `StreamingTranslator` for SSE transformation
- [ ] Handle semantic event generation (`response.output_text.delta`, etc.)
- [ ] Test with various providers

**Deliverable**: Streaming works in translation mode.

### Sprint 5: Extensibility Traits (1 week)
- [ ] Define `ResponseStore` trait for `previous_response_id` support
- [ ] Define `ToolExecutor` trait for server-side tool execution
- [ ] Implement `NoOp` defaults
- [ ] Update `AppState` to be generic over traits

**Deliverable**: Library users can implement traits for stateful features.

### Sprint 6: Sub-Agent Loop Orchestration (1-2 weeks)
- [ ] Implement tool execution loop in responses handler
- [ ] Handle `max_tool_calls` limit
- [ ] Return `RequiresAction` status for unhandled tools
- [ ] Test with mock `ToolExecutor`

**Deliverable**: Server-side tool loops work when `ToolExecutor` provided.

### Sprint 7: Compliance & Polish (Future)
- [ ] Run Open Responses compliance test suite
- [ ] Reasoning trace normalization
- [ ] Documentation and examples

---

## Testing Strategy

### Unit Tests
- Translation accuracy between formats
- Streaming event generation
- Error handling edge cases

### Integration Tests
- End-to-end with mock providers
- Real provider testing (OpenAI, Anthropic via translation)
- Compliance test suite from openresponses/openresponses repo

### Compliance Testing
The Open Responses repo includes a compliance test framework at `bin/compliance-test.ts`. Run this against the Onwards implementation:

```bash
npx ts-node bin/compliance-test.ts --endpoint http://localhost:8080/v1/responses
```

---

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Provider doesn't support Open Responses | High | Low | Transparent mode: provider error. Strict mode: translation. |
| Schema drift as Open Responses evolves | Medium | Medium | Strict mode schemas versioned, transparent unaffected |
| Translation loses fidelity | Medium | Medium | Document limitations, recommend native upstreams |
| Strict mode breaks existing deployments | Low | High | Strict mode is opt-in, default unchanged |

## Key Architectural Decisions

### 1. Passthrough Already Works

The existing wildcard route `/{*path}` forwards `/v1/responses` today. No changes needed for basic Open Responses support with compliant upstreams.

### 2. Strict Mode as Separate Router, Not Middleware

Strict mode uses a **separate router** with typed handlers that leverage Axum extractors:

```
strict_mode: false → build_transparent_router() → wildcard passthrough
strict_mode: true  → build_strict_router()      → typed handlers with validation
```

This ensures:
- ✅ Leverages Axum's built-in validation (`Json<T>`, `axum-valid`)
- ✅ Unknown paths rejected automatically (no wildcard)
- ✅ Type-safe request/response handling
- ✅ Handlers can share logic with transparent mode via shared functions
- ✅ Clean separation - no middleware complexity

### 3. Stateless by Default, Traits for Extension

The system is stateless, but library users can implement traits for advanced features:

```rust
// For `previous_response_id` support
pub trait ResponseStore: Send + Sync { ... }

// For server-side tool execution
pub trait ToolExecutor: Send + Sync { ... }
```

Default implementations (`NoOpStore`, `NoOpExecutor`) preserve stateless behavior.

### 4. Translation via `open_responses.translate`

Per-target translation is explicit:

```json
{
  "open_responses": {
    "translate": true  // Transform /v1/responses ↔ /v1/chat/completions
  }
}
```

When enabled, the handler:
1. Translates Responses request → Chat Completions request
2. Forwards to upstream
3. Translates Chat Completions response → Responses response

### 5. M:N Request Mapping for Tool Loops

A single `/v1/responses` request can result in multiple upstream calls when:
- `tools` are specified
- `ToolExecutor` can handle some tools
- Model emits tool calls

The handler loops until completion or `max_tool_calls` is reached.

### 6. `body_transform_fn` Remains Available

Custom body transforms are still valid in strict mode - applied after schema validation:

```rust
async fn responses_handler(...) {
    let validated: ResponsesRequest = /* from extractor */;

    // Apply custom transform if configured
    let transformed = if let Some(transform) = &state.body_transform_fn {
        transform(validated)?
    } else {
        validated
    };

    // Continue with forwarding...
}
```

---

## Open Questions

1. **Strict mode naming**: Is `strict_mode` the right name? Alternatives:
   - `validated_mode`
   - `openai_compat_mode`
   - `schema_validation`
   - **Current preference**: `strict_mode` - clear and concise

2. **Which endpoints in strict mode?**: The plan lists chat completions, responses, embeddings, models. Should we include:
   - `/v1/audio/*` (transcriptions, speech)
   - `/v1/images/*` (generations, edits)
   - `/v1/files`, `/v1/assistants`, etc.
   - **Recommendation**: Start with core endpoints, expand based on demand

3. **Error format in strict mode**: When validation fails, should errors follow:
   - OpenAI error format (current `OnwardsErrorResponse`)
   - Open Responses error format
   - Both based on request path
   - **Recommendation**: OpenAI format for consistency with existing behavior

4. **`supports_responses_api` detection**: Could we auto-detect by probing the upstream?
   - **Recommendation**: No - explicit configuration is more reliable and predictable

5. **Schema versioning**: Open Responses spec will evolve. How to handle?
   - Pin to specific version
   - `OpenResponses-Version` header support
   - **Recommendation**: Start without versioning, add if needed

---

## References

- [Open Responses Specification](https://www.openresponses.org/specification)
- [Open Responses GitHub](https://github.com/openresponses/openresponses)
- [OpenAI Responses API Reference](https://platform.openai.com/docs/api-reference/responses)
- [Hugging Face Open Responses Blog](https://huggingface.co/blog/open-responses)
- [Migration Guide: Chat Completions → Responses](https://platform.openai.com/docs/guides/migrate-to-responses)
