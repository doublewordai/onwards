# Absolute Reasoning Budget Validation Design

## Goal

Complete configurable reasoning translation by allowing one canonical effort to write multiple provider fields, and reject requests whose configured absolute reasoning budget cannot leave room for a final answer.

The public client contract remains:

- Chat Completions: `reasoning_effort`
- Responses: `reasoning.effort`

Model capability discovery through `/v1/models` is out of scope for this PR and will be handled in the control layer.

## Configuration

Replace the current single `{ target_path, values }` surface configuration with a required `writes` array. There is no compatibility parser for the unreleased single-write shape.

```json
{
  "reasoning_translation": {
    "chat_completions": {
      "writes": [
        {
          "target_path": "/reasoning_effort",
          "values": {
            "none": "none",
            "low": "low",
            "medium": "medium",
            "high": "high"
          }
        },
        {
          "target_path": "/thinking_token_budget",
          "values": {
            "none": 0,
            "low": 1024,
            "medium": 4096,
            "high": 8192
          }
        }
      ]
    }
  }
}
```

Every write must explicitly map exactly the same non-empty effort set. A write is never silently skipped for one effort. Omitting an effort from the entire surface mapping intentionally marks that effort unsupported and requests for it receive `400 unsupported_value`.

Target paths within a surface must be unique. Existing reasoning-related target paths remain allowed, and `/thinking_token_budget` is added as an allowed top-level target. A `thinking_token_budget` write must be paired with a `/reasoning_effort` write so that recent vLLM and SGLang versions also activate the model's thinking mode.

All `thinking_token_budget` values must be non-negative JSON integers. Configuration validation runs while targets load or reload, so malformed mappings do not become request-time client errors.

The three provider capability shapes are represented without a separate strategy enum:

- Native effort: one `/reasoning_effort` write containing only the levels the model genuinely supports.
- Absolute token budget: `/reasoning_effort` and `/thinking_token_budget` writes.
- Binary thinking: one `/thinking`, `/chat_template_kwargs/thinking`, or `/chat_template_kwargs/enable_thinking` write.

## Normalized Reasoning Request

Canonical request validation produces a normalized reasoning context containing:

- the selected canonical effort;
- the original client API surface;
- the effective output-limit parameter name and value, when supplied.

Output limits are normalized as follows:

- Chat Completions uses non-null `max_completion_tokens`, falling back to non-null legacy `max_tokens`.
- Responses uses non-null `max_output_tokens`.

If a selected provider mapping does not write `thinking_token_budget`, output limits are not inspected for reasoning purposes. Native-effort models such as GPT-OSS and binary-thinking models therefore retain their existing behavior.

Client-supplied provider controls, including `thinking_token_budget`, remain rejected. They can only be synthesized from trusted provider configuration.

## Absolute Budget Validation

For a mapping that writes `thinking_token_budget`, the selected mapped budget must satisfy:

```text
thinking_token_budget < effective_max_output_tokens
```

Equality is invalid because it leaves no token capacity for a final answer. Budgets are never clipped, and this PR does not add relative or adaptive budget strategies.

If the applicable output limit is omitted, reject the request with `422 Unprocessable Entity`. Chat Completions identifies `max_completion_tokens` as the preferred missing parameter; Responses identifies `max_output_tokens`.

```json
{
  "error": {
    "message": "reasoning_effort 'high' maps to an 8192-token reasoning budget for this model, but max_completion_tokens is not set. Set max_completion_tokens above 8192 or select a lower reasoning_effort.",
    "type": "invalid_request_error",
    "param": "max_completion_tokens",
    "code": "reasoning_budget_requires_max_tokens"
  }
}
```

If the applicable limit is present but less than or equal to the budget, reject the request with `400 Bad Request`.

```json
{
  "error": {
    "message": "reasoning_effort 'high' maps to an 8192-token reasoning budget for this model, but max_completion_tokens is 4096. Increase max_completion_tokens above 8192 or select a lower reasoning_effort.",
    "type": "invalid_request_error",
    "param": "max_completion_tokens",
    "code": "reasoning_budget_exceeds_max_tokens"
  }
}
```

When an absolute-budget mapping is selected, a present non-null output limit that is not a non-negative integer receives a parameter-specific `400 invalid_type` response.

## Request Flow

Canonical reasoning is parsed once before provider selection. The resolved pool is then preflighted: every provider with a translation for the actual upstream surface must support the effort and accept the normalized output limit. A request is rejected before rate limiting or an upstream attempt if any fallback provider is incompatible.

For each provider attempt, the request body is rebuilt from the canonical body and every configured write for the selected effort is applied. The canonical effort field is removed only when none of the target writes preserves that field. Unique paths and complete effort maps make write order behavior-independent.

The strict Responses adapter must preserve the original normalized Responses context while forwarding its internal Chat Completions request. Translation selection follows the actual upstream Chat Completions surface, but missing or invalid budget errors continue to name the client-facing `max_output_tokens` parameter. The same context is reused across tool-loop and streaming iterations.

## Error Propagation

Reasoning errors carry their intended HTTP classification so all generic and strict handlers return the same OpenAI-compatible envelope. Existing validation errors remain `400`; only the missing required output limit for an absolute-budget mapping uses `422`.

## Testing

Unit coverage in `reasoning.rs` will verify:

- multi-write native, budget, and binary mappings;
- complete and identical effort sets across writes;
- duplicate and disallowed target paths;
- integer budget validation and the required `/reasoning_effort` companion;
- Chat limit precedence and legacy fallback;
- Responses limit normalization;
- missing-limit `422`, equal/oversized-budget `400`, and valid larger limits;
- native and binary mappings bypassing budget validation;
- rejection of client-supplied `thinking_token_budget`.

Integration coverage will verify:

- Chat Completions emits both `reasoning_effort` and `thinking_token_budget`;
- Responses passthrough uses `max_output_tokens` in errors;
- strict Responses adapter errors retain `max_output_tokens` and successful requests emit the Chat mapping;
- every provider in a fallback pool is preflighted before any upstream request;
- per-attempt translation remains isolated across fallback providers;
- invalid reasoning translation configuration fails target loading.

Documentation will show native-effort, token-budget, and binary examples and state that absolute-budget mappings require an explicit output limit greater than the selected budget.

## Non-Goals

- Publishing per-model reasoning capabilities through `/v1/models`.
- Inferring model families or default budgets.
- Global budget defaults.
- Silent budget clipping.
- Relative or percentage-based budget strategies.
