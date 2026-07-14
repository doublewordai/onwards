# Absolute Reasoning Budget Validation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Emit all configured reasoning fields while rejecting absolute thinking budgets that cannot fit inside an explicit OpenAI-compatible output limit.

**Architecture:** Replace each surface's single translation rule with an explicitly complete set of write rules plus explicit unsupported efforts. Parse canonical reasoning once into a request context, preflight that context against every provider, and carry the original Responses context through the strict Responses-to-Chat adapter while applying writes per upstream attempt.

**Tech Stack:** Rust 2024, serde/serde_json, Axum, Tokio tests, axum-test.

## Global Constraints

- The public OpenAI effort enum is exactly `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`.
- Every configured surface must account for all seven efforts through mapped values or required `unsupported_efforts`.
- Chat Completions uses non-null `max_completion_tokens`, falling back to non-null `max_tokens`; Responses uses non-null `max_output_tokens`.
- Absolute budgets require `thinking_token_budget < effective_max_output_tokens`.
- Missing limits return `422 reasoning_budget_requires_max_tokens`; equal or oversized budgets return `400 reasoning_budget_exceeds_max_tokens`.
- Native-effort and binary mappings do not validate output limits.
- Budgets are never clipped and no relative-budget strategy is added.
- `/v1/models` capability discovery remains out of scope.

---

### Task 1: Explicit multi-write configuration

**Files:**
- Modify: `src/reasoning.rs:68-250`
- Modify: `src/target.rs:963-1060`
- Test: `src/reasoning.rs` inline test module
- Test: `src/target.rs` inline test module

**Interfaces:**
- Produces: `ReasoningWrite { target_path: String, values: BTreeMap<ReasoningEffort, Value> }`.
- Produces: `ReasoningTranslation { unsupported_efforts: BTreeSet<ReasoningEffort>, writes: Vec<ReasoningWrite> }`.
- Produces: `ReasoningTranslationConfig::validate() -> Result<(), ReasoningError>` that validates both request surfaces before targets become live.

- [ ] **Step 1: Replace test fixtures with the new shape and add failing configuration tests**

Add tests that deserialize configurations such as:

```rust
let config: ReasoningTranslationConfig = serde_json::from_value(json!({
    "chat_completions": {
        "unsupported_efforts": ["minimal", "xhigh", "max"],
        "writes": [{
            "target_path": "/reasoning_effort",
            "values": {
                "none": "none",
                "low": "low",
                "medium": "medium",
                "high": "high"
            }
        }]
    }
})).unwrap();
assert!(config.validate().is_ok());
```

Add individual failing tests for a missing `unsupported_efforts` field, mismatched effort keys between writes, duplicate target paths, overlap between mapped and unsupported efforts, incomplete coverage of the seven OpenAI efforts, a non-integer budget, and a budget write without `/reasoning_effort`.

- [ ] **Step 2: Run the focused configuration tests and verify RED**

Run: `cargo test reasoning::tests --all-features`

Expected: compilation or assertion failures because `writes`, `unsupported_efforts`, and `/thinking_token_budget` validation do not exist.

- [ ] **Step 3: Implement the new configuration structs and validation**

Replace the current `ReasoningTranslation` fields with:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningWrite {
    pub target_path: String,
    pub values: BTreeMap<ReasoningEffort, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningTranslation {
    pub unsupported_efforts: BTreeSet<ReasoningEffort>,
    pub writes: Vec<ReasoningWrite>,
}
```

Validate that writes are non-empty, paths are unique, every write has the same non-empty keys, mapped and unsupported efforts are disjoint, and their union equals `ReasoningEffort::ALL`. Allow only the existing reasoning paths plus exact `/thinking_token_budget`. Require every budget value to satisfy `Value::as_u64()` and require a companion exact `/reasoning_effort` write.

Update `validate_effort` to derive the supported list from the common write keys and return `unsupported_value` for every explicitly unsupported effort.

- [ ] **Step 4: Validate configurations during target loading**

In `Targets::from_config`, validate each provider's optional reasoning configuration before converting provider specs into runtime targets:

```rust
for (index, provider) in pool_config.providers.iter().enumerate() {
    if let Some(config) = provider.reasoning_translation.as_ref() {
        config.validate().map_err(|error| {
            anyhow!(
                "Invalid reasoning translation for target '{name}' provider {index}: {}",
                error.message()
            )
        })?;
    }
}
```

Add a `Targets::from_config` test proving an invalid budget mapping returns `Err` rather than creating a target.

- [ ] **Step 5: Run configuration tests and verify GREEN**

Run: `cargo test reasoning::tests --all-features && cargo test target::tests --all-features`

Expected: all reasoning and target tests pass.

- [ ] **Step 6: Commit the configuration change**

```bash
git add src/reasoning.rs src/target.rs
git commit -m "feat: add explicit multi-write reasoning mappings"
```

### Task 2: Normalize and validate absolute budgets

**Files:**
- Modify: `src/reasoning.rs:86-350`
- Modify: `src/errors.rs:120-155`
- Test: `src/reasoning.rs` inline test module

**Interfaces:**
- Produces: `CanonicalReasoningRequest`, a cloneable request-extension value containing the canonical effort and original output-limit context.
- Produces: `ReasoningTranslationConfig::validate_request(path: &str, request: &CanonicalReasoningRequest) -> Result<(), ReasoningError>`.
- Produces: `ReasoningTranslationConfig::apply(path: &str, request: &CanonicalReasoningRequest, body: &mut Value) -> Result<(), ReasoningError>`.
- Produces: a reasoning error classification that preserves `400` versus `422` through `OnwardsErrorResponse`.

- [ ] **Step 1: Add failing normalization and error tests**

Add table-driven unit tests covering:

```rust
#[test]
fn chat_prefers_max_completion_tokens_over_legacy_max_tokens() {
    let request = validate_canonical_reasoning(
        "/chat/completions",
        &json!({
            "reasoning_effort": "high",
            "max_completion_tokens": 9000,
            "max_tokens": 4000
        }),
    ).unwrap().unwrap();
    assert_eq!(request.output_limit_param(), "max_completion_tokens");
    assert_eq!(request.output_limit_value(), Some(&json!(9000)));
}
```

Also test null `max_completion_tokens` falling back to `max_tokens`, Responses selecting `max_output_tokens`, missing Chat and Responses limits, a non-integer limit, budget equal to the limit, budget larger than the limit, budget smaller than the limit, and native/binary mappings ignoring missing or malformed limits.

Assert exact missing-limit status/code/message/param and exact oversized status/code/message/param from the approved design.

- [ ] **Step 2: Run focused budget tests and verify RED**

Run: `cargo test reasoning::tests --all-features`

Expected: failures because canonical validation returns only `ReasoningEffort` and no budget comparison exists.

- [ ] **Step 3: Implement normalized request context**

Introduce cloneable types equivalent to:

```rust
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CanonicalReasoningRequest {
    effort: ReasoningEffort,
    output_limit: OutputLimit,
}

#[derive(Debug, Clone, PartialEq)]
enum OutputLimit {
    Missing { param: &'static str },
    Present { param: &'static str, value: Value },
}
```

Change `validate_canonical_reasoning` to return `Result<Option<CanonicalReasoningRequest>, ReasoningError>`. Normalize the field name and clone the raw JSON value without type-checking it yet so native and binary providers do not gain new output-limit validation.

- [ ] **Step 4: Implement budget-specific request validation**

Find the selected effort's `/thinking_token_budget` value only when the surface mapping contains that write. For budget mappings:

- missing output limit: return `422`, preferred Chat param `max_completion_tokens` or Responses param `max_output_tokens`, code `reasoning_budget_requires_max_tokens`;
- present non-integer limit: return `400 invalid_type` for the selected parameter;
- `budget >= limit`: return `400 reasoning_budget_exceeds_max_tokens`;
- `budget < limit`: return success.

Do not mutate the configured budget.

- [ ] **Step 5: Apply every selected write atomically after validation**

Change `apply` to accept the already-normalized request, call `validate_request`, preserve the canonical effort field when any target path equals the actual upstream surface's canonical path, otherwise prune it, and apply every unique write with `set_json_pointer`.

- [ ] **Step 6: Preserve 422 in the OpenAI error envelope**

Add an error status classification to `ReasoningError` and a single `OnwardsErrorResponse::reasoning(&ReasoningError)` constructor. The constructor must use the existing `ErrorResponseBody` with `invalid_request_error`, the reasoning error's parameter and code, and either `BAD_REQUEST` or `UNPROCESSABLE_ENTITY`.

- [ ] **Step 7: Run budget tests and verify GREEN**

Run: `cargo test reasoning::tests --all-features`

Expected: all reasoning unit tests pass.

- [ ] **Step 8: Commit normalized budget validation**

```bash
git add src/reasoning.rs src/errors.rs
git commit -m "feat: validate absolute reasoning budgets"
```

### Task 3: Preserve canonical context across handlers and the Responses adapter

**Files:**
- Modify: `src/handlers.rs:535-770`
- Modify: `src/strict/handlers.rs:159-1235`
- Modify: `src/strict/adapter.rs:110-145`
- Test: `src/lib.rs:3280-3610`
- Test: `src/strict/handlers.rs:3650-3760`
- Test: `src/strict/adapter.rs:930-970`

**Interfaces:**
- Consumes: `CanonicalReasoningRequest` from Task 2.
- Produces: generic pool preflight against every provider before an upstream attempt.
- Produces: strict adapter forwarding that attaches the original Responses context to each internal Chat request.

- [ ] **Step 1: Add failing integration tests for Chat and pool preflight**

Update all existing fixtures to `writes` plus required `unsupported_efforts`. Add tests asserting:

- a valid Chat request sends both downstream fields;
- omitted Chat max returns `422 reasoning_budget_requires_max_tokens` with no upstream request;
- equal and oversized Chat budgets return `400 reasoning_budget_exceeds_max_tokens` with no upstream request;
- `max_completion_tokens` takes precedence over a smaller `max_tokens`;
- any incompatible provider in a fallback pool rejects before the first upstream request;
- native `/reasoning_effort` and binary `/thinking` mappings accept requests without a max.

- [ ] **Step 2: Add failing strict Responses adapter tests**

Configure `open_responses.adapter: true` with a Chat token-budget mapping. Send `reasoning: { effort: "high" }` with missing, too-small, and valid `max_output_tokens`. Assert missing and small errors name `max_output_tokens`, while the valid upstream Chat body contains `reasoning_effort`, `thinking_token_budget`, and the adapted `max_tokens`.

- [ ] **Step 3: Run integration tests and verify RED**

Run: `cargo test tests::test_ --all-features && cargo test strict::handlers::tests --all-features`

Expected: failures from the old configuration shape, missing context propagation, and missing status-aware error conversion.

- [ ] **Step 4: Refactor the generic handler to parse once and preflight every provider**

Read an optional `CanonicalReasoningRequest` from request extensions; otherwise derive it from the request path and body. For every provider with a translation, call `validate_request` using the actual upstream path before rate limiting. On each provider attempt, call the new `apply(path, request, body)` and convert all failures with `OnwardsErrorResponse::reasoning`.

- [ ] **Step 5: Carry Responses context through every adapter request**

Keep the normalized context returned in `responses_handler` and pass it through `handle_adapter_request`, `handle_streaming_adapter_request`, and every tool-loop call to `forward_request_raw`. Extend `forward_request_raw` with an optional context and insert it into the internal request extensions before calling `target_message_handler`:

```rust
if let Some(reasoning) = reasoning {
    request.extensions_mut().insert(reasoning);
}
```

Clone the same context for each streaming or non-streaming iteration so errors always reference the original Responses `max_output_tokens` field.

- [ ] **Step 6: Run handler and adapter tests and verify GREEN**

Run: `cargo test tests::test_ --all-features && cargo test strict::handlers::tests --all-features && cargo test strict::adapter::tests --all-features`

Expected: all selected tests pass and no invalid request reaches the mock provider.

- [ ] **Step 7: Commit request-flow integration**

```bash
git add src/handlers.rs src/strict/handlers.rs src/strict/adapter.rs src/lib.rs
git commit -m "feat: enforce reasoning budgets across API surfaces"
```

### Task 4: OpenAI-compatible controls, documentation, and full verification

**Files:**
- Modify: `src/reasoning.rs`
- Modify: `src/strict/schemas/completions.rs`
- Modify: `src/strict/handlers.rs`
- Modify: `docs/src/configuration.md`
- Test: `src/reasoning.rs` inline test module
- Test: `src/strict/mod.rs` inline test module

**Interfaces:**
- Produces: rejection of caller-controlled `/thinking_token_budget` on Chat Completions, Responses, and legacy Completions.
- Produces: documentation for all three capability mappings and all seven OpenAI efforts.

- [ ] **Step 1: Add failing tests for caller-supplied budgets**

Test all three request surfaces with a caller-supplied `thinking_token_budget`. Assert `400 unsupported_parameter`, param `thinking_token_budget`, and no upstream request.

- [ ] **Step 2: Run the provider-control tests and verify RED**

Run: `cargo test thinking_token_budget --all-features`

Expected: failures because the client field is not currently rejected or captured by the strict legacy schema.

- [ ] **Step 3: Reject client-supplied `thinking_token_budget`**

Add `/thinking_token_budget` to canonical provider-control checks for Chat, Responses, and legacy Completions. Add a `thinking_token_budget: Option<Value>` capture field to the strict legacy Completions request so strict mode can return the parameter-specific compatibility error.

- [ ] **Step 4: Replace reasoning translation documentation**

Document the required `unsupported_efforts` and `writes` fields. Include complete examples for:

- GPT-OSS native `low`, `medium`, `high`, with `none`, `minimal`, `xhigh`, and `max` explicitly unsupported;
- a recent vLLM token-budget model with all seven efforts mapped and an explicit output-limit requirement;
- a binary model mapping `none` to `false` and every other OpenAI effort to `true`.

State that budget numbers are model-specific, eval-derived configuration and not Onwards defaults.

- [ ] **Step 5: Run focused tests, formatting, lint, and full tests**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
git diff --check
```

Expected: every command exits 0; the test run reports zero failures.

- [ ] **Step 6: Commit documentation and compatibility coverage**

```bash
git add src/reasoning.rs src/strict/schemas/completions.rs src/strict/handlers.rs src/strict/mod.rs docs/src/configuration.md
git commit -m "docs: document reasoning capability mappings"
```

- [ ] **Step 7: Review the final branch diff against the design**

Run: `git diff --stat origin/main...HEAD && git diff --check && git status --short --branch`

Expected: only reasoning translation, strict-schema compatibility, documentation, tests, and the approved design/plan files differ; the working tree is clean.
