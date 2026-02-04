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

### Proposed: Strict Mode as Tower Middleware

Rather than a parallel code path, strict mode is implemented as a **Tower middleware** that wraps the existing handler:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Request Flow (strict_mode: true)             │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Request                                                        │
│     │                                                           │
│     ▼                                                           │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │              StrictLayer (Tower Middleware)              │   │
│  │  ┌─────────────────────────────────────────────────┐    │   │
│  │  │ 1. Validate path (reject unknown)               │    │   │
│  │  │ 2. Validate request schema                      │    │   │
│  │  │ 3. Transform request if needed                  │    │   │
│  │  │    (e.g., /v1/responses → /v1/chat/completions) │    │   │
│  │  └─────────────────────────────────────────────────┘    │   │
│  │                         │                                │   │
│  │                         ▼                                │   │
│  │  ┌─────────────────────────────────────────────────┐    │   │
│  │  │         Inner Service (existing handler)         │    │   │
│  │  │  • Extract model                                 │    │   │
│  │  │  • Route to target                               │    │   │
│  │  │  • Passthrough to upstream                       │    │   │
│  │  │  • SSE buffering                                 │    │   │
│  │  └─────────────────────────────────────────────────┘    │   │
│  │                         │                                │   │
│  │                         ▼                                │   │
│  │  ┌─────────────────────────────────────────────────┐    │   │
│  │  │ 4. Transform response if needed                  │    │   │
│  │  │    (e.g., chat completions → responses format)  │    │   │
│  │  │ 5. Validate response schema                      │    │   │
│  │  └─────────────────────────────────────────────────┘    │   │
│  └─────────────────────────────────────────────────────────┘   │
│     │                                                           │
│     ▼                                                           │
│  Response                                                       │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

**Benefits of middleware approach:**
- ✅ No code duplication - inner handler unchanged
- ✅ Composable with other Tower layers
- ✅ Easy to enable/disable
- ✅ Clear separation of concerns
- ✅ Can be conditionally applied per-route if needed

**Tower implementation sketch:**

```rust
pub struct StrictLayer {
    config: StrictConfig,
}

impl<S> Layer<S> for StrictLayer {
    type Service = StrictService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        StrictService {
            inner,
            config: self.config.clone(),
        }
    }
}

impl<S> Service<Request> for StrictService<S>
where
    S: Service<Request, Response = Response>,
{
    type Response = Response;
    type Error = S::Error;

    async fn call(&mut self, req: Request) -> Result<Response, Self::Error> {
        // 1. Validate path
        let endpoint = validate_path(req.uri().path())?;

        // 2. Validate request schema
        let (parts, body) = req.into_parts();
        let validated = validate_request(endpoint, &body)?;

        // 3. Transform if needed (responses → chat completions)
        let (transformed_body, needs_response_transform) =
            self.maybe_transform_request(endpoint, validated, &parts)?;

        let req = Request::from_parts(parts, transformed_body);

        // 4. Call inner service (existing handler)
        let response = self.inner.call(req).await?;

        // 5. Transform response if needed (chat completions → responses)
        let response = if needs_response_transform {
            self.transform_response(response).await?
        } else {
            response
        };

        // 6. Validate response schema
        validate_response(endpoint, &response)?;

        Ok(response)
    }
}
```

**Router construction:**

```rust
pub fn build_router<T: HttpClient>(state: AppState<T>) -> Router {
    let base_router = Router::new()
        .route("/models", get(models_handler))
        .route("/v1/models", get(models_handler))
        .route("/{*path}", any(target_message_handler))
        .with_state(state.clone());

    if state.strict_mode {
        // Wrap with strict validation middleware
        base_router.layer(StrictLayer::new(state.strict_config))
    } else {
        base_router
    }
}
```

---

## Implementation Plan

### Phase 0: Passthrough (Already Complete)

**Status**: ✅ Already works today

The wildcard route `/{*path}` already forwards `/v1/responses` to upstreams. If your upstream supports Open Responses, it works. No code changes needed.

### Phase 1: Strict Mode Middleware Foundation

**Goal**: Create Tower middleware infrastructure for strict mode.

**New module structure**:

```
src/
├── strict/
│   ├── mod.rs              # StrictLayer, StrictService
│   ├── validator.rs        # Path and schema validation
│   ├── schemas/
│   │   ├── mod.rs
│   │   ├── chat_completions.rs
│   │   ├── responses.rs
│   │   └── embeddings.rs
│   └── translator.rs       # Protocol translation
```

**Middleware skeleton**:

```rust
// src/strict/mod.rs
use tower::{Layer, Service};

pub struct StrictLayer {
    config: StrictConfig,
}

pub struct StrictService<S> {
    inner: S,
    config: StrictConfig,
}

impl<S> Service<Request<Body>> for StrictService<S>
where
    S: Service<Request<Body>, Response = Response<Body>>,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = StrictFuture<S::Future>;

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // Middleware logic here
    }
}
```

**Router integration**:

```rust
pub fn build_router<T: HttpClient>(state: AppState<T>) -> Router {
    let router = Router::new()
        .route("/models", get(models_handler))
        .route("/v1/models", get(models_handler))
        .route("/{*path}", any(target_message_handler))
        .with_state(state.clone());

    if state.strict_mode {
        router.layer(StrictLayer::new(state.strict_config.clone()))
    } else {
        router
    }
}
```

### Phase 2: Path Validation

**Goal**: Middleware rejects unknown paths in strict mode.

```rust
#[derive(Debug, Clone, Copy)]
pub enum EndpointType {
    ChatCompletions,
    Responses,
    Embeddings,
    Models,
}

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

### Phase 3: Request Schema Validation

**Goal**: Middleware validates request bodies against known schemas.

```rust
pub enum ValidatedRequest {
    ChatCompletion(ChatCompletionRequest),
    Responses(ResponsesRequest),
    Embeddings(EmbeddingsRequest),
    Models, // GET, no body
}

fn validate_request(
    endpoint: EndpointType,
    body: &[u8],
) -> Result<ValidatedRequest, OnwardsErrorResponse> {
    match endpoint {
        EndpointType::ChatCompletions => {
            let req: ChatCompletionRequest = serde_json::from_slice(body)
                .map_err(|e| OnwardsErrorResponse::bad_request(e.to_string()))?;
            Ok(ValidatedRequest::ChatCompletion(req))
        }
        EndpointType::Responses => {
            let req: ResponsesRequest = serde_json::from_slice(body)
                .map_err(|e| OnwardsErrorResponse::bad_request(e.to_string()))?;
            Ok(ValidatedRequest::Responses(req))
        }
        // ...
    }
}
```

### Phase 4: Response Validation

**Goal**: Middleware validates and sanitizes responses.

This reuses and extends the existing `ResponseSanitizer`:

```rust
fn validate_response(
    endpoint: EndpointType,
    headers: &HeaderMap,
    body: &[u8],
    original_model: Option<&str>,
) -> Result<Bytes, OnwardsErrorResponse> {
    match endpoint {
        EndpointType::ChatCompletions => {
            let sanitizer = ResponseSanitizer { original_model };
            sanitizer.sanitize("/v1/chat/completions", headers, body)
        }
        EndpointType::Responses => {
            let sanitizer = OpenResponsesSanitizer { original_model };
            sanitizer.sanitize(headers, body)
        }
        // ...
    }
}
```

### Phase 5: Protocol Translation

**Goal**: When `open_responses.translate: true`, transform requests/responses.

```rust
impl<S> StrictService<S> {
    fn maybe_transform_request(
        &self,
        endpoint: EndpointType,
        validated: ValidatedRequest,
        target: &Target,
    ) -> Result<(Bytes, bool), OnwardsErrorResponse> {
        match (endpoint, &validated) {
            (EndpointType::Responses, ValidatedRequest::Responses(req))
                if target.open_responses.translate =>
            {
                // Transform to chat completions
                let chat_req = translator::responses_to_chat_completions(req)?;
                let body = serde_json::to_vec(&chat_req)?;
                Ok((Bytes::from(body), true)) // true = needs response transform
            }
            _ => {
                // No transformation needed
                let body = serde_json::to_vec(&validated)?;
                Ok((Bytes::from(body), false))
            }
        }
    }

    async fn maybe_transform_response(
        &self,
        response: Response,
        needs_transform: bool,
    ) -> Result<Response, OnwardsErrorResponse> {
        if !needs_transform {
            return Ok(response);
        }

        // Transform chat completions response back to responses format
        translator::chat_completions_to_responses(response).await
    }
}
```

### Phase 6: Streaming Translation

**Goal**: Handle streaming responses in translation mode.

The middleware wraps the response body stream:

```rust
fn wrap_streaming_response(
    response: Response,
    translator: StreamingTranslator,
) -> Response {
    let (parts, body) = response.into_parts();

    let translated_body = body.map(move |chunk| {
        translator.translate_chunk(chunk)
    });

    Response::from_parts(parts, Body::wrap_stream(translated_body))
}
```

### Phase 7: Advanced Features (Future)

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

### Sprint 1: Tower Middleware Foundation (1 week)
- [ ] Add `strict_mode` flag to config
- [ ] Create `src/strict/mod.rs` with `StrictLayer` and `StrictService`
- [ ] Integrate layer conditionally in `build_router()`
- [ ] Implement path validation (reject unknown paths)

**Deliverable**: `strict_mode: true` wraps handler and rejects unknown paths.

### Sprint 2: Request/Response Schemas (1-2 weeks)
- [ ] Define schemas for chat completions, responses, embeddings
- [ ] Implement request validation in middleware
- [ ] Implement response validation (reuse/extend `ResponseSanitizer`)
- [ ] Add `open_responses` target config struct

**Deliverable**: Full request/response validation in strict mode.

### Sprint 3: Protocol Translation (1-2 weeks)
- [ ] Implement Responses → Chat Completions request translation
- [ ] Implement Chat Completions → Responses response translation
- [ ] Wire up `open_responses.translate` config
- [ ] Non-streaming end-to-end working

**Deliverable**: `/v1/responses` works with Chat Completions-only upstreams.

### Sprint 4: Streaming Translation (1 week)
- [ ] Implement streaming body wrapper
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

### 2. Strict Mode as Middleware, Not Parallel Code Path

Strict mode is implemented as a Tower middleware that **wraps** the existing handler:

```
strict_mode: false → request → target_message_handler → response
strict_mode: true  → request → StrictLayer → target_message_handler → StrictLayer → response
```

This ensures:
- ✅ No code duplication
- ✅ Existing handler logic unchanged
- ✅ Composable with other middleware
- ✅ Clear request/response transformation points
- ✅ Easy to disable (just remove the layer)

### 3. Strict Mode Subsumes Existing Validation Features

When `strict_mode: true`:
- `sanitize_response` → always on (subsumed by response validation)
- `body_transform_fn` → replaced by schema validation
- Path validation → always on (new)

This avoids configuration complexity and ensures consistent behavior.

### 4. Translation via `open_responses.translate`

Per-target translation is explicit:

```json
{
  "open_responses": {
    "translate": true  // Transform /v1/responses ↔ /v1/chat/completions
  }
}
```

This makes it clear what's happening - the middleware transforms the request before passing to the inner handler, then transforms the response on the way back.

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
