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

### Option A: Protocol Translation Layer (Recommended)

Add a bidirectional translation layer that converts between Open Responses and Chat Completions formats, allowing Onwards to:

1. Accept Open Responses requests from clients
2. Route to existing Chat Completions providers
3. Translate responses back to Open Responses format

**Pros**: Leverages existing provider ecosystem, minimal provider changes
**Cons**: Some features (sub-agent loops, reasoning) require provider support

### Option B: Native Open Responses Passthrough

Add native routing for Open Responses requests to providers that support the spec natively.

**Pros**: Full feature support with compliant providers
**Cons**: Limited to providers implementing Open Responses

### Option C: Hybrid Approach (Recommended for Production)

Combine both options:
- Native passthrough for Open Responses-compliant providers
- Translation layer for Chat Completions-only providers
- Automatic detection/routing based on provider capabilities

---

## Implementation Plan

### Phase 1: Core Data Models

**Goal**: Define Rust types for Open Responses request/response structures.

**New files**:
```
src/
├── open_responses/
│   ├── mod.rs              # Module root
│   ├── models.rs           # Request/response types
│   ├── items.rs            # Item types (message, function_call, reasoning)
│   └── streaming.rs        # Streaming event types
```

**Key types to implement**:

```rust
// Request structure
pub struct CreateResponseRequest {
    pub model: String,
    pub input: Input,                           // String or Vec<InputItem>
    pub previous_response_id: Option<String>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<ToolChoice>,
    pub max_tool_calls: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub stream: Option<bool>,
    pub reasoning: Option<ReasoningConfig>,
    pub metadata: Option<HashMap<String, String>>,
}

// Response structure
pub struct ResponseResource {
    pub id: String,
    pub object: String,                         // "response"
    pub created_at: u64,
    pub completed_at: Option<u64>,
    pub status: ResponseStatus,                 // completed, in_progress, incomplete, failed
    pub model: String,
    pub output: Vec<OutputItem>,
    pub usage: Option<Usage>,
    pub reasoning: Option<ReasoningOutput>,
    pub error: Option<ResponseError>,
}

// Item types
pub enum InputItem {
    Message(InputMessage),
    FunctionCallOutput(FunctionCallOutput),
}

pub enum OutputItem {
    Message(OutputMessage),
    FunctionCall(FunctionCall),
    Reasoning(ReasoningItem),
}
```

### Phase 2: Endpoint Registration

**Goal**: Add `/v1/responses` endpoint to the Axum router.

**Changes to `lib.rs`**:

```rust
pub fn build_router<T: HttpClient>(state: AppState<T>) -> Router {
    Router::new()
        .route("/v1/models", get(handlers::models))
        .route("/v1/chat/completions", any(handlers::target_message_handler))
        // NEW: Open Responses endpoint
        .route("/v1/responses", post(handlers::open_responses_handler))
        .fallback(any(handlers::target_message_handler))
        .with_state(state)
}
```

### Phase 3: Request Translation (Open Responses → Chat Completions)

**Goal**: Convert incoming Open Responses requests to Chat Completions format for routing to existing providers.

**Translation logic**:

```rust
impl From<CreateResponseRequest> for ChatCompletionRequest {
    fn from(req: CreateResponseRequest) -> Self {
        ChatCompletionRequest {
            model: req.model,
            messages: items_to_messages(req.input),
            tools: req.tools.map(convert_tools),
            tool_choice: req.tool_choice,
            temperature: req.temperature,
            top_p: req.top_p,
            max_tokens: req.max_output_tokens,
            stream: req.stream,
            // Note: reasoning config not supported by most providers
        }
    }
}

fn items_to_messages(input: Input) -> Vec<Message> {
    match input {
        Input::Text(s) => vec![Message::user(s)],
        Input::Items(items) => items.into_iter().map(item_to_message).collect(),
    }
}
```

### Phase 4: Response Translation (Chat Completions → Open Responses)

**Goal**: Convert Chat Completions responses back to Open Responses format.

```rust
impl From<ChatCompletionResponse> for ResponseResource {
    fn from(resp: ChatCompletionResponse) -> Self {
        ResponseResource {
            id: format!("resp_{}", resp.id),
            object: "response".to_string(),
            created_at: resp.created,
            completed_at: Some(resp.created),
            status: ResponseStatus::Completed,
            model: resp.model,
            output: choices_to_items(resp.choices),
            usage: resp.usage.map(convert_usage),
            reasoning: None,
            error: None,
        }
    }
}
```

### Phase 5: Semantic Streaming Events

**Goal**: Transform raw SSE deltas into Open Responses semantic events.

**Event types to support**:

```rust
pub enum StreamingEvent {
    ResponseCreated(ResponseResource),
    ResponseInProgress,
    OutputItemAdded { item_id: String, item: OutputItem },
    OutputTextDelta { item_id: String, delta: String },
    OutputItemDone { item_id: String, item: OutputItem },
    ResponseCompleted(ResponseResource),
    ResponseFailed { error: ResponseError },
}
```

**Streaming transformer**:

```rust
pub struct OpenResponsesStreamTransformer {
    response_id: String,
    item_counter: u32,
    current_item: Option<PartialItem>,
}

impl OpenResponsesStreamTransformer {
    pub fn transform_chunk(&mut self, chunk: ChatCompletionChunk) -> Vec<StreamingEvent> {
        let mut events = Vec::new();

        for choice in chunk.choices {
            if let Some(delta) = choice.delta {
                // Convert delta to semantic events
                if let Some(content) = delta.content {
                    events.push(StreamingEvent::OutputTextDelta {
                        item_id: self.current_item_id(),
                        delta: content,
                    });
                }
                if let Some(tool_calls) = delta.tool_calls {
                    // Handle tool call deltas
                }
            }
            if choice.finish_reason.is_some() {
                events.push(StreamingEvent::OutputItemDone { ... });
            }
        }

        events
    }
}
```

### Phase 6: Provider Capability Detection

**Goal**: Detect whether a provider supports Open Responses natively vs requiring translation.

**Configuration extension**:

```json
{
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-...",
      "capabilities": {
        "open_responses": true,
        "reasoning": true,
        "sub_agent_loops": true
      }
    },
    "claude-3": {
      "url": "https://api.anthropic.com",
      "onwards_key": "sk-...",
      "capabilities": {
        "open_responses": false
      }
    }
  }
}
```

### Phase 7: Sub-Agent Loop Support (Advanced)

**Goal**: For providers that don't support sub-agent loops natively, implement client-side loop orchestration.

```rust
pub async fn execute_with_tool_loop(
    client: &impl HttpClient,
    mut request: CreateResponseRequest,
    tools: &[Tool],
    max_iterations: u32,
) -> Result<ResponseResource, Error> {
    let mut iteration = 0;
    let mut accumulated_items = Vec::new();

    loop {
        let response = send_request(client, &request).await?;
        accumulated_items.extend(response.output.clone());

        // Check for tool calls in output
        let tool_calls: Vec<_> = response.output.iter()
            .filter_map(|item| match item {
                OutputItem::FunctionCall(fc) => Some(fc),
                _ => None,
            })
            .collect();

        if tool_calls.is_empty() || iteration >= max_iterations {
            return Ok(ResponseResource {
                output: accumulated_items,
                ..response
            });
        }

        // Execute tools and add results to input
        for tool_call in tool_calls {
            let result = execute_tool(tool_call, tools).await?;
            request.input.push(InputItem::FunctionCallOutput(result));
        }

        iteration += 1;
    }
}
```

---

## Configuration Changes

### New Target Options

```json
{
  "targets": {
    "model-name": {
      "url": "https://provider.example.com",
      "onwards_key": "sk-...",

      // NEW: Open Responses configuration
      "open_responses": {
        "enabled": true,
        "native": false,           // Use translation layer
        "reasoning_mode": "summary", // content | encrypted | summary | none
        "max_tool_loop_iterations": 10
      }
    }
  }
}
```

### Global Settings

```json
{
  "open_responses": {
    "default_enabled": true,
    "translation_mode": "auto",    // auto | always | never
    "preserve_provider_extensions": false
  }
}
```

---

## API Compatibility Matrix

| Feature | Native Support | Translation Support |
|---------|---------------|---------------------|
| Basic text generation | ✅ | ✅ |
| Tool/function calling | ✅ | ✅ |
| Streaming | ✅ | ✅ (semantic events) |
| Reasoning traces | ✅ | ❌ (not available) |
| Encrypted reasoning | ✅ | ❌ |
| Sub-agent loops | ✅ | ⚠️ (client-side) |
| `previous_response_id` | ✅ | ⚠️ (requires state store) |
| Custom provider extensions | ✅ | ❌ |

---

## Implementation Timeline

### Sprint 1: Foundation (Weeks 1-2)
- [ ] Define core data models (`open_responses/models.rs`)
- [ ] Add `/v1/responses` endpoint stub
- [ ] Implement basic request translation
- [ ] Unit tests for translation logic

### Sprint 2: Response Handling (Weeks 3-4)
- [ ] Implement response translation
- [ ] Add response sanitization for Open Responses format
- [ ] Non-streaming end-to-end flow working

### Sprint 3: Streaming (Weeks 5-6)
- [ ] Implement semantic streaming event transformer
- [ ] SSE event formatting for Open Responses
- [ ] Streaming end-to-end flow working

### Sprint 4: Advanced Features (Weeks 7-8)
- [ ] Provider capability detection
- [ ] Native passthrough for compliant providers
- [ ] Client-side tool loop orchestration
- [ ] Configuration schema updates

### Sprint 5: Testing & Documentation (Weeks 9-10)
- [ ] Integration tests with multiple providers
- [ ] Compliance testing against Open Responses test suite
- [ ] Documentation and examples
- [ ] Performance benchmarking

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
| Translation loses fidelity | Medium | Medium | Extensive testing, document limitations |
| Streaming performance overhead | Low | Medium | Benchmark, optimize transformer |
| Provider incompatibilities | Medium | High | Capability detection, fallback modes |
| Spec changes during development | Medium | Medium | Version pinning, abstraction layers |

---

## Open Questions

1. **State storage for `previous_response_id`**: Should we implement a response cache, or require clients to send full context?

2. **Reasoning trace handling**: For providers that expose reasoning (like Claude's extended thinking), should we map to Open Responses reasoning fields?

3. **Extension passthrough**: Should provider-specific extensions be preserved or stripped?

4. **Backwards compatibility**: Should `/v1/chat/completions` automatically translate to Open Responses internally?

---

## References

- [Open Responses Specification](https://www.openresponses.org/specification)
- [Open Responses GitHub](https://github.com/openresponses/openresponses)
- [OpenAI Responses API Reference](https://platform.openai.com/docs/api-reference/responses)
- [Hugging Face Open Responses Blog](https://huggingface.co/blog/open-responses)
- [Migration Guide: Chat Completions → Responses](https://platform.openai.com/docs/guides/migrate-to-responses)
