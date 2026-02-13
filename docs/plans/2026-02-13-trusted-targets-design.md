# Trusted Targets in Strict Mode - Design Document

**Date:** 2026-02-13
**Status:** Approved
**Author:** Claude Code (via brainstorming session)

## Overview

Add support for trusted targets in strict mode to allow specific providers to bypass all sanitization. This enables operators to get exact responses and error messages from providers they fully control or trust, while maintaining strict mode security for untrusted providers.

## Motivation

Strict mode provides strong security guarantees by sanitizing all responses and standardizing all errors. However, some operators run their own infrastructure or use providers they fully trust (e.g., their own OpenAI account) and want:

- Original error messages for better debugging
- Provider metadata (cost, trace IDs) for monitoring
- Exact responses without model field rewriting

Rather than disabling strict mode globally, we need per-target granularity to bypass sanitization for trusted providers.

## Design

### Configuration Schema

Add a new `trusted: bool` field to `ProviderSpec` in `src/target.rs`:

```rust
/// Mark this provider as trusted to bypass strict mode sanitization.
/// When strict_mode is enabled globally AND trusted is true for a target,
/// all sanitization (both success and error responses) is skipped.
/// WARNING: Trusted providers can leak metadata and non-standard responses.
/// Only use for providers you fully control or trust.
/// Defaults to false.
#[serde(default)]
pub trusted: bool,
```

**Example configuration:**

```json
{
  "strict_mode": true,
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-...",
      "trusted": true
    },
    "third-party": {
      "url": "https://some-provider.com",
      "onwards_key": "sk-..."
    }
  }
}
```

### Implementation Logic

In `src/strict/handlers.rs`, each handler (`chat_completions_handler`, `responses_handler`, `embeddings_handler`) will check if the target is trusted before applying sanitization:

```rust
// After forwarding request to upstream
let response = forward_request(state.clone(), headers, path, body_bytes).await;

// Check if this target is trusted - if so, bypass all sanitization
if let Some(pool) = state.targets.targets.get(&original_model) {
    if pool.providers.first().map(|p| p.config.trusted).unwrap_or(false) {
        debug!(model = %original_model, "Bypassing sanitization for trusted target");
        return response;
    }
}

// Normal strict mode sanitization for untrusted targets
if response.status().is_success() {
    // ... sanitize success responses
} else {
    // ... sanitize error responses
}
```

When `trusted: true`, the response is returned immediately without:
- Model field rewriting
- Provider metadata removal
- Error standardization
- Content-Length updates
- SSE comment stripping

### Testing Strategy

New tests in `src/strict/handlers.rs`:

1. **test_trusted_target_bypasses_sanitization** - Verify provider-specific fields pass through in success responses
2. **test_trusted_target_bypasses_error_sanitization** - Verify original error messages are forwarded
3. **test_untrusted_target_still_sanitized** - Verify strict mode sanitization still applies without trusted flag
4. **test_trusted_streaming_responses_passthrough** - Verify streaming responses from trusted targets pass through
5. **test_load_balancing_mixed_trust** - Verify behavior with multiple providers at different trust levels

### Documentation Updates

Add new "Trusted Targets" section to `docs/src/strict-mode.md`:

```markdown
## Trusted Targets

In strict mode, you can mark specific targets as trusted to bypass all sanitization. This is useful when you have providers you fully control (e.g., your own OpenAI account) and want their exact responses and error messages passed through.

### Configuration

Add `trusted: true` to any target configuration:

\`\`\`json
{
  "strict_mode": true,
  "targets": {
    "gpt-4": {
      "url": "https://api.openai.com",
      "onwards_key": "sk-...",
      "trusted": true
    }
  }
}
\`\`\`

### Behavior

When a target is marked as trusted:
- **Success responses**: Passed through completely with all provider metadata intact
- **Error responses**: Original error messages forwarded to clients
- **No model field rewriting**: Response model field matches provider's response
- **No Content-Length updates**: Headers passed through as-is

### Security Warning

⚠️ **Use trusted targets carefully.** Marking a target as trusted bypasses all strict mode security guarantees:
- Provider-specific metadata may leak to clients
- Third-party error details will be exposed
- Responses may not match OpenAI schema exactly

Only mark targets as trusted when you fully control or trust the provider.
```

## Security Considerations

**Risk:** Operators might mark untrusted providers as trusted without understanding the implications.

**Mitigation:**
- Clear warning in documentation
- Field name "trusted" clearly signals security implications
- Defaults to `false` (safe by default)
- Field includes warning comment in code

## Alternatives Considered

1. **Reuse `sanitize_response: false`** - Rejected because it has different meaning in non-strict mode and would be confusing
2. **Add `skip_strict_mode: bool`** - Rejected because verbose and could be confused with global strict_mode flag
3. **Per-target config with `trusted: bool`** - **Selected** because explicit, clear, and security-focused

## Success Criteria

- Trusted targets completely bypass all sanitization (success and error responses)
- Untrusted targets continue to receive full strict mode sanitization
- Configuration is simple (single boolean flag)
- Security implications are clearly documented
- All test cases pass
