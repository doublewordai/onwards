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

### Proposed: Strict Mode (New Opt-in Feature)

Add a new `strict_mode` flag that enables comprehensive request/response validation and translation. This **subsumes and extends** existing `sanitize_response` and `body_transform_fn` functionality.

```
strict_mode: false (default)     strict_mode: true
─────────────────────────────    ─────────────────────────────
• Transparent passthrough        • Path validation
• Minimal model extraction       • Request schema validation
• No body parsing               • Response schema validation
• Provider determines format    • Protocol translation (when needed)
```

**Strict mode provides:**

1. **Path validation**: Only accept known OpenAI-compatible paths
   - `/v1/chat/completions`
   - `/v1/responses`
   - `/v1/embeddings`
   - `/v1/models`
   - (reject unknown paths with 404)

2. **Request schema validation**: Validate request bodies against known schemas
   - Chat Completions: `messages` array, `model`, etc.
   - Responses: `input` (string or items), `model`, etc.
   - Embeddings: `input`, `model`, etc.

3. **Response schema validation**: Enforce strict OpenAI schema compliance
   - Subsumes existing `sanitize_response` functionality
   - Remove provider-specific fields
   - Rewrite model names

4. **Protocol translation** (for `/v1/responses` only):
   - If upstream supports Open Responses → passthrough
   - If upstream only supports Chat Completions → translate request/response

### Architecture: Strict Mode Sits Alongside Transparent Mode

```
┌─────────────────────────────────────────────────────────────┐
│                        Onwards Router                        │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  strict_mode: false (default)                               │
│  ┌─────────────────────────────────────────────────────┐   │
│  │ • Extract model from body                            │   │
│  │ • Route to target                                    │   │
│  │ • Passthrough request/response as bytes              │   │
│  │ • SSE buffering for streaming                        │   │
│  │ • (optional) body_transform_fn                       │   │
│  │ • (optional) sanitize_response per target            │   │
│  └─────────────────────────────────────────────────────┘   │
│                                                             │
│  strict_mode: true (opt-in)                                 │
│  ┌─────────────────────────────────────────────────────┐   │
│  │ • Validate path against allowed endpoints            │   │
│  │ • Parse and validate request schema                  │   │
│  │ • Route to target                                    │   │
│  │ • Translate if needed (responses → chat completions) │   │
│  │ • Parse and validate response schema                 │   │
│  │ • Translate response if needed                       │   │
│  └─────────────────────────────────────────────────────┘   │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

**Key principle**: Strict mode doesn't change the existing transparent behavior. It's an entirely separate code path that users opt into.

---

## Implementation Plan

### Phase 0: Passthrough (Already Complete)

**Status**: ✅ Already works today

The wildcard route `/{*path}` already forwards `/v1/responses` to upstreams. If your upstream supports Open Responses, it works. No code changes needed.

### Phase 1: Strict Mode Foundation

**Goal**: Add `strict_mode` configuration flag and routing infrastructure.

**New module structure**:

```
src/
├── strict/
│   ├── mod.rs              # Strict mode entry point
│   ├── validator.rs        # Path and schema validation
│   ├── schemas/
│   │   ├── mod.rs
│   │   ├── chat_completions.rs
│   │   ├── responses.rs
│   │   └── embeddings.rs
│   └── translator.rs       # Protocol translation (Phase 2)
```

**Configuration**:

```json
{
  "strict_mode": true,  // Global flag, default false
  "targets": {
    "gpt-4o": {
      "url": "https://api.openai.com/v1",
      "supports_responses_api": true  // Passthrough for /v1/responses
    },
    "claude-3": {
      "url": "https://api.anthropic.com",
      "supports_responses_api": false  // Translate /v1/responses → /v1/chat/completions
    }
  }
}
```

**Handler modification**:

```rust
pub async fn target_message_handler<T: HttpClient>(
    State(state): State<AppState<T>>,
    req: Request,
) -> Result<Response, OnwardsErrorResponse> {
    // Check if strict mode is enabled globally
    if state.strict_mode {
        return strict::handle_request(state, req).await;
    }

    // Existing transparent passthrough logic (unchanged)
    // ...
}
```

### Phase 2: Path and Request Validation

**Goal**: In strict mode, validate paths and request schemas.

**Allowed paths**:

```rust
const STRICT_MODE_PATHS: &[&str] = &[
    "/v1/chat/completions",
    "/v1/responses",
    "/v1/embeddings",
    "/v1/models",
    "/models",
];

fn validate_path(path: &str) -> Result<EndpointType, OnwardsErrorResponse> {
    match path {
        p if p.ends_with("/chat/completions") => Ok(EndpointType::ChatCompletions),
        p if p.ends_with("/responses") => Ok(EndpointType::Responses),
        p if p.ends_with("/embeddings") => Ok(EndpointType::Embeddings),
        p if p.ends_with("/models") => Ok(EndpointType::Models),
        _ => Err(OnwardsErrorResponse::not_found("Unknown endpoint")),
    }
}
```

**Request validation**:

```rust
fn validate_request(endpoint: EndpointType, body: &[u8]) -> Result<ValidatedRequest, OnwardsErrorResponse> {
    match endpoint {
        EndpointType::ChatCompletions => {
            let req: ChatCompletionRequest = serde_json::from_slice(body)?;
            // Validate required fields, types, etc.
            Ok(ValidatedRequest::ChatCompletion(req))
        }
        EndpointType::Responses => {
            let req: ResponsesRequest = serde_json::from_slice(body)?;
            Ok(ValidatedRequest::Responses(req))
        }
        // ...
    }
}
```

### Phase 3: Response Validation (Subsumes `sanitize_response`)

**Goal**: In strict mode, validate and sanitize all responses.

This replaces the per-target `sanitize_response` flag with automatic validation:

```rust
fn validate_response(
    endpoint: EndpointType,
    body: &[u8],
    original_model: Option<&str>,
) -> Result<Bytes, OnwardsErrorResponse> {
    match endpoint {
        EndpointType::ChatCompletions => {
            // Reuse existing ResponseSanitizer logic
            let sanitizer = ResponseSanitizer { original_model };
            sanitizer.sanitize_non_streaming(body)
        }
        EndpointType::Responses => {
            // New: Open Responses sanitizer
            let sanitizer = OpenResponsesSanitizer { original_model };
            sanitizer.sanitize(body)
        }
        // ...
    }
}
```

### Phase 4: Protocol Translation (Responses ↔ Chat Completions)

**Goal**: In strict mode, translate `/v1/responses` requests for upstreams that don't support it.

**Per-target configuration**:

```rust
pub struct Target {
    // ... existing fields ...

    /// Does this upstream support /v1/responses natively?
    /// Only relevant when strict_mode is enabled.
    #[serde(default)]
    pub supports_responses_api: bool,
}
```

**Translation logic**:

```rust
async fn handle_responses_request(
    state: &AppState,
    target: &Target,
    request: ResponsesRequest,
) -> Result<Response, OnwardsErrorResponse> {
    if target.supports_responses_api {
        // Passthrough - serialize back to bytes and forward
        forward_as_responses(state, target, request).await
    } else {
        // Translate to chat completions
        let chat_request = translate_to_chat_completions(&request)?;
        let chat_response = forward_as_chat_completions(state, target, chat_request).await?;
        let responses_response = translate_from_chat_completions(&chat_response)?;
        Ok(responses_response)
    }
}
```

### Phase 5: Streaming Translation

**Goal**: Handle streaming responses in translation mode.

```rust
pub struct StreamingTranslator {
    direction: TranslationDirection,
    response_id: String,
    sequence: u64,
}

enum TranslationDirection {
    ChatToResponses,  // Upstream sent chat.completion.chunk, client expects response.*
    PassThrough,      // No translation needed
}

impl StreamingTranslator {
    fn translate_chunk(&mut self, chunk: &[u8]) -> Result<Bytes, String> {
        match self.direction {
            TranslationDirection::PassThrough => Ok(Bytes::copy_from_slice(chunk)),
            TranslationDirection::ChatToResponses => {
                // Parse chat.completion.chunk
                // Emit response.output_text.delta events
                self.translate_chat_to_responses(chunk)
            }
        }
    }
}
```

### Phase 6: Advanced Features (Future)

- Sub-agent loop orchestration
- Reasoning trace normalization
- `previous_response_id` state management

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
      "onwards_key": "sk-...",
      "supports_responses_api": true   // Passthrough /v1/responses
    },
    "claude-3-opus": {
      "url": "https://api.anthropic.com",
      "onwards_key": "sk-...",
      "supports_responses_api": false  // Translate /v1/responses → chat completions
    },
    "local-llama": {
      "url": "http://localhost:8000",
      "supports_responses_api": false
    }
  }
}
```

### Mode Comparison

| Feature | `strict_mode: false` | `strict_mode: true` |
|---------|---------------------|---------------------|
| Path validation | ❌ Any path forwarded | ✅ Only known OpenAI paths |
| Request validation | ❌ Passthrough | ✅ Schema validation |
| Response validation | ⚠️ Optional (`sanitize_response`) | ✅ Always validated |
| Protocol translation | ❌ Not available | ✅ Based on `supports_responses_api` |
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

| Feature | Transparent (default) | Strict + Native | Strict + Translate |
|---------|----------------------|-----------------|-------------------|
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

### Sprint 1: Strict Mode Infrastructure (1 week)
- [ ] Add `strict_mode` flag to config
- [ ] Create `src/strict/` module structure
- [ ] Add routing branch in `target_message_handler`
- [ ] Implement path validation (reject unknown paths)

**Deliverable**: `strict_mode: true` rejects unknown paths.

### Sprint 2: Request/Response Schemas (1-2 weeks)
- [ ] Define schemas for chat completions, responses, embeddings
- [ ] Implement request validation
- [ ] Implement response validation (migrate from `ResponseSanitizer`)
- [ ] Add `supports_responses_api` target option

**Deliverable**: Full request/response validation in strict mode.

### Sprint 3: Protocol Translation (1-2 weeks)
- [ ] Implement Responses → Chat Completions request translation
- [ ] Implement Chat Completions → Responses response translation
- [ ] Non-streaming end-to-end working

**Deliverable**: `/v1/responses` works with Chat Completions-only upstreams.

### Sprint 4: Streaming Translation (1 week)
- [ ] Implement streaming chunk translation
- [ ] Handle semantic event generation
- [ ] Test with various providers

**Deliverable**: Streaming works in translation mode.

### Sprint 5: Advanced Features (Future)
- [ ] Sub-agent loop orchestration
- [ ] Reasoning trace normalization
- [ ] Compliance test suite integration

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

### 2. Strict Mode is Separate, Not Incremental

Strict mode is an entirely different code path, not a modification of transparent mode:

```
strict_mode: false → existing target_message_handler (unchanged)
strict_mode: true  → new strict::handle_request
```

This ensures:
- ✅ Existing deployments unaffected
- ✅ Clear separation of concerns
- ✅ Strict mode can evolve independently
- ✅ Easy to disable if issues arise

### 3. Strict Mode Subsumes Existing Validation Features

When `strict_mode: true`:
- `sanitize_response` → always on (subsumed)
- `body_transform_fn` → replaced by schema validation
- Path validation → always on (new)

This avoids configuration complexity and ensures consistent behavior.

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
