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

### Architectural Constraint: No Body Serialization by Default

Onwards is designed as a **transparent proxy** that avoids deserializing request/response bodies. The current approach:

1. **Minimal extraction**: Only the `model` field is extracted for routing (via lightweight `ExtractedModel`)
2. **Model rewriting via `serde_json::Value`**: Preserves all fields without full schema definitions
3. **Opt-in sanitization**: Response sanitization (`sanitize_response: true`) is the only feature that does full deserialization, and it's per-target opt-in

This philosophy prioritizes:
- Performance (no unnecessary parsing)
- Flexibility (unknown fields pass through)
- Provider compatibility (doesn't impose schema requirements)

### Option A: Native Passthrough (Recommended Default)

Add `/v1/responses` endpoint that routes directly to providers supporting Open Responses natively:

```
Client (Open Responses) → Onwards → Provider (Open Responses)
```

**Behavior**:
- Extract `model` field from request body (same minimal extraction as chat completions)
- Route to target based on model name
- Pass request/response through unchanged as bytes
- SSE buffering for streaming (already implemented)

**Pros**: Maintains transparent proxy philosophy, zero schema maintenance
**Cons**: Only works with Open Responses-compliant providers

### Option B: Strict Mode Translation (Opt-in)

Add opt-in `strict_mode` or `translate_responses` flag that enables protocol translation:

```
Client (Open Responses) → Onwards (translate) → Provider (Chat Completions)
```

**Behavior**:
- When enabled: Full deserialization, protocol translation, re-serialization
- When disabled: Passthrough (Option A behavior)
- Follows same pattern as existing `sanitize_response` opt-in

**Pros**: Enables Open Responses clients with legacy providers
**Cons**: Requires schema maintenance, loses unknown fields

### Option C: Hybrid Routing (Recommended for Production)

Combine both approaches with automatic detection:

```json
{
  "targets": {
    "gpt-4o": {
      "url": "https://api.openai.com/v1",
      "open_responses": {
        "mode": "native"           // passthrough, provider supports it
      }
    },
    "claude-3": {
      "url": "https://api.anthropic.com",
      "open_responses": {
        "mode": "translate"        // opt-in translation
      }
    },
    "local-model": {
      "url": "http://localhost:8000",
      "open_responses": {
        "mode": "disabled"         // no /v1/responses support
      }
    }
  }
}
```

**Routing logic**:
1. `/v1/responses` request arrives
2. Extract model, look up target
3. If `mode: native` → passthrough
4. If `mode: translate` → deserialize, convert to chat completions, forward, convert response back
5. If `mode: disabled` → return 404 or error

---

## Implementation Plan

### Phase 1: Native Passthrough (Minimal Change)

**Goal**: Add `/v1/responses` endpoint that routes to providers with zero body parsing beyond model extraction.

**Changes to `lib.rs`**:

```rust
pub fn build_router<T: HttpClient>(state: AppState<T>) -> Router {
    Router::new()
        .route("/v1/models", get(handlers::models))
        .route("/v1/chat/completions", any(handlers::target_message_handler))
        // NEW: Open Responses endpoint - reuses existing handler
        .route("/v1/responses", any(handlers::target_message_handler))
        .fallback(any(handlers::target_message_handler))
        .with_state(state)
}
```

That's it for Phase 1. The existing `target_message_handler` already:
- Extracts `model` from JSON body (works for Open Responses format too)
- Routes to the correct target
- Handles SSE buffering
- Applies rate limiting and auth

**No new types needed** - bodies pass through as bytes.

### Phase 2: Configuration for Open Responses Mode

**Goal**: Allow per-target configuration for how to handle `/v1/responses` requests.

**New config options in `target.rs`**:

```rust
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpenResponsesMode {
    #[default]
    Native,      // Passthrough to provider
    Translate,   // Convert to/from chat completions (strict mode)
    Disabled,    // Return 404 for /v1/responses
}

pub struct Target {
    // ... existing fields ...

    /// How to handle /v1/responses requests for this target
    #[serde(default)]
    pub open_responses_mode: OpenResponsesMode,
}
```

**Handler modification**:

```rust
// In target_message_handler, after routing:
if path.contains("/v1/responses") {
    match target.open_responses_mode {
        OpenResponsesMode::Native => { /* passthrough, current behavior */ }
        OpenResponsesMode::Translate => { /* Phase 3 */ }
        OpenResponsesMode::Disabled => {
            return Err(OnwardsErrorResponse::not_found("Open Responses not supported for this target"));
        }
    }
}
```

### Phase 3: Strict Mode Translation (Opt-in Only)

**Goal**: When `open_responses_mode: translate` is set, convert between formats.

**New module** (only loaded when translation is needed):

```
src/
├── open_responses/
│   ├── mod.rs              # Feature-gated module
│   ├── translator.rs       # Bidirectional translation
│   └── types.rs            # Minimal types for translation
```

**Key design principles for translation**:

1. **Lenient parsing** (like response sanitizer):
   ```rust
   #[derive(Deserialize)]
   struct LenientOpenResponsesRequest {
       model: String,
       input: serde_json::Value,  // Don't fully parse, just transform
       #[serde(flatten)]
       other: HashMap<String, serde_json::Value>,  // Preserve unknown fields
   }
   ```

2. **Minimal transformation**:
   ```rust
   fn translate_request(body: &[u8]) -> Result<Bytes, String> {
       let req: LenientOpenResponsesRequest = serde_json::from_slice(body)?;

       // Convert input to messages array
       let messages = input_to_messages(&req.input)?;

       // Build chat completion request, preserving other fields
       let mut chat_req = req.other;
       chat_req.insert("messages".into(), messages);
       chat_req.insert("model".into(), req.model.into());

       Ok(Bytes::from(serde_json::to_vec(&chat_req)?))
   }
   ```

3. **Response transformation follows existing pattern**:
   - Use `ResponseTransformFn` hook
   - Apply only when `open_responses_mode: translate`
   - Streaming: transform SSE chunks
   - Non-streaming: buffer and transform

### Phase 4: Streaming Event Translation (Strict Mode Only)

**Goal**: Transform Chat Completions SSE deltas into Open Responses semantic events.

**Only applies when `translate` mode is active**:

```rust
pub struct StreamingTranslator {
    response_id: String,
    sequence: u64,
}

impl StreamingTranslator {
    /// Transform a chat.completion.chunk into response.* events
    pub fn translate_chunk(&mut self, chunk: &[u8]) -> Result<Bytes, String> {
        // Parse minimally
        let parsed: serde_json::Value = serde_json::from_slice(chunk)?;

        // Generate semantic events
        let events = vec![
            self.make_delta_event(&parsed)?,
        ];

        // Format as SSE
        Ok(self.format_sse_events(&events))
    }
}
```

### Phase 5: Sub-Agent Loop Orchestration (Advanced, Strict Mode)

**Goal**: For providers without native tool loops, orchestrate client-side.

**Only relevant for `translate` mode with tool-calling requests**:

```rust
/// Orchestrates tool loops for providers that don't support them natively
pub async fn orchestrate_tool_loop<T: HttpClient>(
    client: &T,
    target: &Target,
    original_request: Bytes,
    max_iterations: u32,
) -> Result<Response, Error> {
    // This is complex and requires full deserialization
    // Only enabled when:
    // 1. open_responses_mode: translate
    // 2. Request contains tools
    // 3. Provider doesn't support native loops

    // Implementation deferred to future phase
    unimplemented!("Tool loop orchestration")
}
```

---

## Configuration Changes

### New Target Options

```json
{
  "targets": {
    "gpt-4o": {
      "url": "https://api.openai.com/v1",
      "onwards_key": "sk-...",

      // NEW: Open Responses mode (default: "native")
      "open_responses_mode": "native"
    },
    "claude-3-opus": {
      "url": "https://api.anthropic.com",
      "onwards_key": "sk-...",

      // Opt-in translation (strict mode)
      "open_responses_mode": "translate"
    },
    "local-llama": {
      "url": "http://localhost:8000",

      // Disable /v1/responses for this target
      "open_responses_mode": "disabled"
    }
  }
}
```

### Mode Descriptions

| Mode | Body Parsing | Use Case |
|------|--------------|----------|
| `native` (default) | Minimal (model only) | Provider supports Open Responses |
| `translate` | Full deserialization | Provider only supports Chat Completions |
| `disabled` | None | Explicitly disable /v1/responses |

---

## API Compatibility Matrix

| Feature | Native Mode | Translate Mode | Notes |
|---------|-------------|----------------|-------|
| Basic text generation | ✅ Passthrough | ✅ | |
| Tool/function calling | ✅ Passthrough | ⚠️ Partial | External tools only |
| Streaming | ✅ Passthrough | ⚠️ Complex | Requires event translation |
| Reasoning traces | ✅ Passthrough | ❌ | No Chat Completions equivalent |
| Encrypted reasoning | ✅ Passthrough | ❌ | Provider-specific |
| Sub-agent loops | ✅ Passthrough | ❌ | Would require orchestration |
| `previous_response_id` | ✅ Passthrough | ❌ | No state maintained |
| Unknown fields | ✅ Preserved | ❌ Lost | Translation requires schema |
| Performance | ✅ Optimal | ⚠️ Overhead | Deserialization cost |

---

## Implementation Timeline

### Sprint 1: Native Passthrough (1-2 days)
- [ ] Add `/v1/responses` route to `build_router()` (single line change)
- [ ] Verify model extraction works for Open Responses request format
- [ ] Test with Open Responses-compliant provider (e.g., OpenAI)
- [ ] Document the new endpoint

**Deliverable**: `/v1/responses` works as transparent passthrough.

### Sprint 2: Configuration & Mode Selection (3-5 days)
- [ ] Add `open_responses_mode` field to `Target` struct
- [ ] Add mode-based routing in handler
- [ ] Implement `disabled` mode (return 404)
- [ ] Update configuration documentation

**Deliverable**: Per-target control over Open Responses behavior.

### Sprint 3: Strict Mode Translation (1-2 weeks, optional)
- [ ] Create `open_responses/` module
- [ ] Implement lenient request translation (Items → Messages)
- [ ] Implement lenient response translation (Choices → Output Items)
- [ ] Add request/response transform hooks

**Deliverable**: `translate` mode works for non-streaming requests.

### Sprint 4: Streaming Translation (1 week, optional)
- [ ] Implement SSE chunk translation
- [ ] Handle semantic event generation
- [ ] Test with various providers

**Deliverable**: `translate` mode works for streaming requests.

### Sprint 5: Advanced Features (Future)
- [ ] Sub-agent loop orchestration
- [ ] Reasoning trace handling
- [ ] Compliance test suite integration

**Note**: Sprints 3-5 are only needed if `translate` mode is required. Most deployments can use native passthrough.

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
| Provider doesn't support Open Responses | High | Low | Return provider's error, document limitations |
| Model extraction fails on Open Responses format | Low | High | Test extraction with spec examples |
| Translation mode loses fidelity | Medium | Medium | Document limitations, prefer native mode |
| Spec changes after implementation | Medium | Low | Native mode unaffected, version pin for translate |

## Key Architectural Decision

**The native passthrough approach was chosen to maintain Onwards' transparent proxy philosophy.**

Previous versions of this plan assumed full request/response deserialization. After reviewing the codebase, this was revised to:

1. **Phase 1 (Native)**: Single line change - add route, reuse existing handler
2. **Phase 2 (Config)**: Add mode selection per target
3. **Phase 3+ (Translate)**: Only if needed, follows existing `sanitize_response` pattern

This means:
- ✅ No new types required for basic support
- ✅ Unknown fields pass through unchanged
- ✅ No schema maintenance burden
- ✅ Performance identical to current passthrough
- ⚠️ Translation mode (if needed) is opt-in and isolated

---

## Open Questions

1. **Default mode**: Should the default `open_responses_mode` be `native` (passthrough) or `disabled` (explicit opt-in)?
   - **Recommendation**: Default to `native` for simplicity - if a provider doesn't support it, they'll return an error anyway.

2. **Model extraction**: The current `ExtractedModel` struct expects `{"model": "..."}`. Open Responses uses the same field name, but should we validate the overall request structure?
   - **Recommendation**: No - maintain minimal extraction philosophy.

3. **Translate mode priority**: Is `translate` mode actually needed? If most providers will support Open Responses natively (OpenAI, Hugging Face Inference), translation may be low priority.
   - **Recommendation**: Implement passthrough first, add translation only if there's demand.

4. **Response sanitization for Open Responses**: Should we create an Open Responses-specific sanitizer (like `create_openai_sanitizer()`) for the `native` mode?
   - **Recommendation**: Only if providers return non-compliant responses. Start without it.

5. **Streaming format differences**: Open Responses uses semantic events (`response.output_text.delta`) while Chat Completions uses `chat.completion.chunk`. In `native` mode, is SSE buffering sufficient?
   - **Recommendation**: Yes - existing `SseBufferedStream` handles incomplete chunks regardless of event format.

---

## References

- [Open Responses Specification](https://www.openresponses.org/specification)
- [Open Responses GitHub](https://github.com/openresponses/openresponses)
- [OpenAI Responses API Reference](https://platform.openai.com/docs/api-reference/responses)
- [Hugging Face Open Responses Blog](https://huggingface.co/blog/open-responses)
- [Migration Guide: Chat Completions → Responses](https://platform.openai.com/docs/guides/migrate-to-responses)
