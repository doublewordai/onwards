# Model Session Affinity Design

## Summary

This document proposes a v1 session affinity design for model-based provider selection in Onwards.

The goal is to keep requests for the same logical session on the same upstream provider for the same requested model alias during a configurable TTL window.

Session input resolution in v1 uses this precedence:

1. read explicit client-provided `session_id` if present
2. read `Authorization` if present
3. derive the final session fingerprint from the composition rules below

The primary use case is upstream cache affinity for chat-style workloads:

- prompt cache locality
- KV cache locality
- warm conversational context on the same backend
- avoiding unnecessary cache misses caused by provider hopping

The design preserves existing authentication, routing rules, rate limiting, concurrency limits, strict-mode validation, and fallback behavior.

## Implementation Status

The design in this document is now implemented in the current codebase with the following scope:

- pool-level `session_affinity` config is supported
- session identity is derived from configured session header and/or `Authorization: Bearer`
- provider identity is hashed and cached on `Provider`
- affinity applies to all model-routed requests that pass through `target_message_handler`
- fallback can optionally rebind according to `rebind_on_fallback`
- request-path lazy stale cleanup is implemented
- single-process hot reload eager cleanup is implemented for the default in-memory store
- Prometheus counters are emitted for affinity result and reload cleanup events

Current implementation files:

- `src/session_affinity/mod.rs`
- `src/session_affinity/context.rs`
- `src/session_affinity/key.rs`
- `src/session_affinity/store.rs`
- `src/session_affinity/metrics.rs`
- `src/handlers.rs`
- `src/load_balancer.rs`
- `src/target.rs`
- `src/main.rs`

## Current State

Today, provider selection is purely pool-based:

1. Extract `model` from header or request body.
2. Resolve the model alias to a `ProviderPool`.
3. Apply auth, routing rules, rate limits, and concurrency limits.
4. Select a provider using `ProviderPool::select_iter()`.
5. Retry on fallback conditions if configured.

Relevant code:

- `src/handlers.rs`: `target_message_handler`
- `src/load_balancer.rs`: `ProviderPool`, `select()`, `select_iter()`
- `src/strict/handlers.rs`: validated requests are forwarded into the same handler path
- `src/target.rs`: config loading and hot reload

There is currently no persistent binding from `(model alias, session identity)` to a provider.

## Problem Statement

Some clients can provide a stable `session_id`, but many OpenAI-compatible clients do not.

In practice, the proxy already has another stable signal: `Authorization: Bearer <token>`.

If the same logical session repeatedly hits the same model alias within a short time window, forwarding those requests to the same upstream provider can materially improve cache hit rates and reduce latency variance.

## Goals

- Support session affinity for any request that selects a provider via model alias.
- Enable affinity per model alias, not globally.
- Prefer explicit `session_id` as the session discriminator when provided by the client.
- Fold `Authorization` into the fingerprint whenever it is available so affinity stays scoped to the caller boundary.
- Preserve current behavior when affinity is not configured and when neither `session_id` nor `Authorization` is available.
- Work in both passthrough mode and strict mode.
- Interoperate correctly with fallback and provider hot reload.
- Keep the design compatible with single-instance and multi-instance deployments.

## Non-Goals

- Affinity for requests that do not resolve a model alias.
- Inferring identity from request payload content.
- Cross-model affinity. Bindings are scoped to one requested model alias.
- Durable conversation state management. This is routing affinity, not transcript storage.
- Perfect per-end-user affinity when multiple real users share one API key and no explicit `session_id` is provided.

## Design Position

### Affinity overlay, not a new strategy

Although the user-visible behavior may look like a load balancing strategy, affinity should not replace `weighted_random` or `priority`.

Affinity is better modeled as an overlay on top of existing selection:

1. If a valid binding exists, prefer the bound provider.
2. Otherwise, use the existing pool strategy to choose a provider.

This keeps the current load balancing semantics intact for first selection and fallback selection.

Concretely, provider selection becomes two-phase:

1. normal selection path: existing `strategy` and `fallback`
2. session-aware selection path: reuse an existing binding if one exists, otherwise delegate to the normal path

This separation keeps the feature easy to reason about and aligns with proven designs used in other projects.

### Pool-level opt-in

Affinity should be configured on the `PoolSpec` for a model alias. This keeps the feature local to aliases that benefit from cache affinity.

### Session identity with fallback

The proxy should prefer an explicit client-provided session identifier when available.

When the client does not provide one, the proxy should fall back to bearer-token-derived identity.

This matches current integration reality:

- supports clients that already have first-class session semantics
- requires no client-side protocol change when they do not
- aligns with the current auth flow
- gives stable affinity per session and per model alias

### Affinity must stay within auth boundaries

Session affinity should not allow one caller to steer itself into another caller's affinity bucket simply by guessing a `session_id`.

Recommended v1 rule:

- if `session_id` exists and `Authorization` exists, fingerprint both together
- if only `session_id` exists, fingerprint `session_id` alone
- if only `Authorization` exists, fingerprint `Authorization` alone

This keeps explicit session affinity scoped within the caller's auth boundary when auth is available, which is important for multi-tenant deployments.

### Affinity is best-effort

If the bound provider is unavailable, removed by config reload, at concurrency capacity, or rejected by local fallback policy, the request should still proceed through normal provider selection.

### Rebinding happens after successful takeover

The binding should only move to a new provider after that provider successfully handles the request.

## Proposed Configuration

Add pool-level configuration in `src/target.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionAffinityConfig {
    #[serde(default = "default_session_affinity_header_name")]
    pub header_name: String,

    #[serde(default = "default_session_affinity_ttl_secs")]
    pub ttl_secs: u64,

    #[serde(default = "default_session_affinity_rebind_on_fallback")]
    pub rebind_on_fallback: bool,
}
```

Add to `PoolSpec`:

```rust
#[serde(default)]
pub session_affinity: Option<SessionAffinityConfig>,
```

Suggested defaults:

- `header_name = "x-session-id"`
- `ttl_secs = 300`
- `rebind_on_fallback = true`

Enablement rule:

- `session_affinity` absent: disabled
- `session_affinity` present: enabled

First-version scope:

- the explicit session id is read from a configured header
- `Authorization` is used as a fallback identity source and as a caller boundary when present
- no per-pool capacity tuning is configurable yet
- no separate affinity strategy enum is introduced

Example:

```json
{
  "targets": {
    "gpt-4o": {
      "strategy": "weighted_random",
      "fallback": {
        "enabled": true,
        "on_status": [429, 5],
        "on_rate_limit": true
      },
      "session_affinity": {
        "header_name": "x-session-id",
        "ttl_secs": 300,
        "rebind_on_fallback": true
      },
      "providers": [
        { "url": "https://api.openai.com", "onwards_key": "sk-a", "weight": 3 },
        { "url": "https://api.openai.com", "onwards_key": "sk-b", "weight": 1 }
      ]
    }
  }
}
```

## Configuration Validation

`session_affinity` should be validated at config load time.

Required validation rules:

- `header_name` must be non-empty after trimming
- `ttl_secs` must be greater than zero
- the pool must still contain at least one selectable provider

Normalization rules:

- trim `header_name`
- normalize `header_name` to lowercase at config load time
- use the normalized value at request time

Recommended implementation:

```rust
#[serde(deserialize_with = "deserialize_lowercase")]
pub header_name: String,
```

This keeps runtime request handling simple and avoids repeated case normalization.

Advisory validation:

- if `ttl_secs > 3600`, emit a warning because long TTLs may increase memory pressure and reduce rebalance responsiveness
- if `rebind_on_fallback = false`, emit an informational log because sessions may remain sticky to a degraded original provider

Failing fast at config load time is preferable to silently disabling affinity at runtime.

## Provider Identity

Affinity mappings must not depend on provider index in the pool because provider order can change on config reload.

For v1, provider identity should remain an internal implementation detail.

At load time, derive a deterministic provider identity from:

- `url`
- `onwards_key`
- `onwards_model`

The runtime affinity mapping should store `provider_id`, not provider index.

The identity should be computed by hashing structured inputs rather than concatenating raw strings.

Recommended v1 approach:

```rust
fn compute_provider_id(target: &Target) -> String {
    let mut hasher = Sha256::new();
    hasher.update(target.url.as_str().as_bytes());
    hasher.update(b"|");
    hasher.update(target.onwards_key.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(target.onwards_model.as_deref().unwrap_or("").as_bytes());
    hex::encode(hasher.finalize())[..16].to_string()
}
```

Notes:

- do not expose raw `onwards_key` in logs, metrics, or debug output
- hashing avoids leaking secrets through identity construction
- a short stable digest is sufficient as an internal provider identifier
- the computed identity should be cached on `Provider` at construction time, not recomputed on the request hot path
- explicit separators prevent accidental ambiguity between adjacent fields

Suggested shape:

```rust
pub struct Provider {
    target: Target,
    weight: u32,
    limiter: ConcurrencyLimiter,
    identity: String,
}

impl Provider {
    pub fn identity(&self) -> &str {
        &self.identity
    }
}
```

## Affinity Key

The affinity binding key should be:

```text
(requested_model_alias, session_fingerprint)
```

The value is:

```text
provider_id
```

Where:

- `requested_model_alias` is the model name requested by the client
- `session_fingerprint` is derived from either explicit `session_id` or `Authorization`

The mapping must use the client-requested model alias, not the provider-rewritten `onwards_model`.

## Session Identity Resolution

V1 uses the following steps to derive session input:

1. Read the configured session header and treat a non-empty value as `session_id`.
2. Read `Authorization: Bearer <token>` and treat a valid bearer token as `authorization`.
3. If neither exists, affinity is skipped.

This keeps the feature named `session affinity` while still supporting clients that do not explicitly send a session id.

Header handling requirements:

- HTTP header names are case-insensitive
- `header_name` should be normalized once during config loading
- request handling should not perform repeated header-name normalization on every request
- empty or whitespace-only session header values should be treated as missing
- if multiple values are present for the same header, use the effective value returned by the HTTP framework and verify that behavior in tests
- bearer token extraction should treat the `Bearer` scheme case-insensitively
- leading and trailing whitespace around the extracted token should be trimmed
- non-`Bearer` authorization schemes should be ignored for affinity fallback

## Fingerprinting

Raw `session_id` and raw bearer tokens should not be used directly in logs, metrics, or cache keys exposed outside process memory.

Recommended v1 approach:

```text
session_fingerprint = SHA256(session_identity)
```

Recommended composition rule:

```text
if session_id and authorization:
    session_identity = authorization + "|" + session_id
else if session_id:
    session_identity = session_id
else if authorization:
    session_identity = authorization
```

This composition rule is normative for v1. The proxy must not treat `session_id` as a globally sufficient affinity key when `Authorization` is also available.

Requirements:

- deterministic for the same identity
- non-reversible in operational surfaces
- safe to use as an in-memory or external-store key

V1 should not introduce a separate HMAC secret configuration.

## Runtime State

Add a shared store to `AppState`:

```rust
pub session_affinity_store: Arc<dyn SessionAffinityStore>
```

Suggested trait:

```rust
pub trait SessionAffinityStore: Send + Sync {
    fn get(&self, key: &AffinityKey) -> Option<AffinityEntry>;
    fn insert(&self, key: AffinityKey, entry: AffinityEntry, ttl: Duration);
    fn delete(&self, key: &AffinityKey);
    fn touch(&self, key: &AffinityKey, ttl: Duration);
    fn remove_stale_for_model(
        &self,
        model: &str,
        valid_provider_ids: &HashSet<String>,
    ) -> usize {
        0
    }
}

pub struct AffinityKey {
    pub model: String,
    pub session_fingerprint: String,
}

pub struct AffinityEntry {
    pub provider_id: String,
}
```

The store API should stay intentionally small:

- normal selection remains in `ProviderPool`
- affinity state remains in the store
- selection orchestration remains in the handler

This avoids coupling session state to the load balancer implementation.

V1 note:

- `touch()` is preferred over a dedicated `get_and_refresh()` API because it keeps the store interface small and works for both in-memory caches and external stores
- returning owned values is acceptable for v1 because provider ids are small; avoiding extra lifetime complexity is preferable here
- `AffinityEntry` intentionally does not expose TTL or expiration time; expiration remains a store implementation detail
- `touch(ttl)` refreshes TTL only when the key already exists
- `touch()` on a missing key is a silent no-op
- `remove_stale_for_model()` is optional for store implementations; the default no-op is suitable for shared external stores that prefer lazy cleanup only

Expected store behavior:

| Scenario | Expected behavior |
|----------|-------------------|
| `get()` finds an expired entry | Return `None`; cleanup may be eager or lazy |
| `insert()` on an existing key | Overwrite `provider_id` and refresh TTL |
| `delete()` on a missing key | Succeed silently |
| `touch()` on a missing key | Succeed silently with no creation |
| `remove_stale_for_model()` on unsupported store | Return `0` and leave cleanup to lazy lookup |

Single-instance implementation:

- in-memory TTL cache such as `moka::sync::Cache`, or `DashMap` plus cleanup

Multi-instance implementation:

- Redis or another shared external store

V1 should treat store capacity as an implementation detail of the default in-memory store, not as a per-pool config knob.

Current implementation note:

- the default in-memory store uses `DashMap`
- expired entries are evicted lazily on `get()`
- successful affinity reuse refreshes TTL via `touch(ttl)`
- hot reload can eagerly remove stale provider bindings for the in-memory store
- there is no background sweeper yet for expired entries; expiration remains lazy by default

## Applicability

If a model pool has `session_affinity` configured, affinity should apply to all requests that:

- resolve a model alias for provider selection
- route through the standard provider selection path

This includes:

- `/v1/chat/completions`
- `/v1/responses` when the request resolves a model
- `/v1/completions`
- `/v1/embeddings`
- other model-based passthrough routes supported by the proxy

This does not include:

- requests that do not resolve a model
- endpoints that bypass model-based provider selection

Chat completions remain the primary motivation, but the routing behavior should be consistent for the entire model pool once `session_affinity` is enabled.

## Request Flow

The affinity logic should live in the provider selection layer, not in a special-case route branch.

Proposed flow for any model-routed request:

1. Extract `model` as today.
2. Resolve pool and apply auth, routing rules, rate limits, and pool/key concurrency checks as today.
3. If `session_affinity` is not configured for the pool, skip affinity.
4. Read the configured session header as optional `session_id`.
5. Read `Authorization` as optional bearer token.
6. If neither exists, skip affinity and continue with the existing pool strategy.
7. Compute `session_fingerprint` using the composition rule:
   - `authorization + "|" + session_id` when both exist
   - `session_id` when only session id exists
   - `authorization` when only authorization exists
8. Construct `AffinityKey { model, session_fingerprint }`.
9. Look up the key in the affinity store.
10. If a bound provider exists and is still present in the current pool:
   - try that provider first
   - if it is at provider concurrency capacity, unavailable, or locally rate-limited, continue to normal selection
11. If no valid binding exists, use the existing pool selection logic.
12. If a provider successfully handles the request:
   - first bind via `insert(key, entry, ttl)`
   - successful reuse of the same bound provider can refresh TTL via `touch(key, ttl)`
   - successful fallback takeover can rebind via `insert(key, entry, ttl)` when allowed
13. If fallback selects a different provider and that provider succeeds:
   - rebind according to `rebind_on_fallback`

Affinity should only run after the request has passed existing authentication checks. It is not an authentication primitive.
When explicit `session_id` is present, it only affects routing stickiness and does not replace auth.

Concurrency note:

- preferred-provider selection must attempt to acquire the provider's concurrency guard first
- only if guard acquisition fails should the request fall back to normal selection
- once guard acquisition succeeds, the provider choice for that attempt is fixed

If the same session issues concurrent requests, races are acceptable as long as:

- each individual request selects a valid provider
- the final stored binding is one of the successfully used providers
- the store remains internally consistent

Provider existence checks should use a pool helper such as:

```rust
impl ProviderPool {
    pub fn get_provider_by_id(&self, provider_id: &str) -> Option<(usize, &Provider)> {
        self.providers
            .iter()
            .enumerate()
            .find(|(_, provider)| provider.identity() == provider_id)
    }
}
```

## Load Balancer Changes

`ProviderPool` should expose affinity-aware selection instead of forcing handler-level provider iteration.

Suggested additions in `src/load_balancer.rs`:

```rust
pub fn select_by_provider_id(
    &self,
    provider_id: &str,
    exclude: &HashSet<usize>,
) -> Option<(usize, &Target, ConcurrencyGuard)>;

pub fn select_iter_with_preferred(
    &self,
    preferred_provider_id: Option<&str>,
) -> SelectIter<'_>;
```

Behavior:

- if `preferred_provider_id` is provided and the provider is selectable, the iterator yields it first
- remaining attempts follow normal `strategy` and `fallback` behavior
- provider-level concurrency and local rate-limit checks still apply

This keeps affinity layered on top of the current load balancing strategy rather than replacing it.

An important implementation property is to preserve two explicit entry points:

- normal provider selection
- provider selection with a preferred bound provider

That keeps the code path clear and mirrors the strongest idea from external reference designs without inheriting their tighter coupling to a specific selector implementation.

`SelectIter` likely needs explicit preferred-provider state, for example:

```rust
pub struct SelectIter<'a> {
    pool: &'a ProviderPool,
    excluded: HashSet<usize>,
    max_attempts: usize,
    attempts: usize,
    with_replacement: bool,
    preferred: Option<usize>,
    preferred_tried: bool,
}
```

Expected `next()` behavior:

1. if `preferred` exists and has not yet been tried, attempt it first
2. if preferred selection fails, mark it as tried
3. all later attempts use the existing selection strategy

Additional rules:

- if `preferred` is already in `excluded`, treat it as already tried
- attempting `preferred` consumes one attempt, just like any other provider attempt
- once `preferred_tried` becomes true, it must not be revisited by the iterator

## Final Config Shape

For the first implementation, the configuration surface should be:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionAffinityConfig {
    #[serde(default = "default_session_affinity_header_name")]
    pub header_name: String,

    #[serde(default = "default_session_affinity_ttl_secs")]
    pub ttl_secs: u64,

    #[serde(default = "default_session_affinity_rebind_on_fallback")]
    pub rebind_on_fallback: bool,
}
```

Attached at pool level:

```rust
pub struct PoolSpec {
    // existing fields...
    #[serde(default)]
    pub session_affinity: Option<SessionAffinityConfig>,
}
```

Semantics:

- absence of `session_affinity` means affinity is disabled
- presence of `session_affinity` means affinity is enabled for that model alias
- the configured header provides the explicit session discriminator
- `Authorization` is folded into the fingerprint whenever it exists
- TTL uses sliding expiration
- store capacity is an implementation detail of the default in-memory store

This is the recommended final v1 config because it is minimal, directly implementable, and does not expose extension points that the first release will not support.

## Handler Integration

The main integration point is `target_message_handler` in `src/handlers.rs`.

The handler already owns:

- model extraction
- request header access
- bearer token extraction
- pool resolution
- routing rules
- rate limiting
- request forwarding
- fallback loop

Affinity should be inserted after pool resolution and policy checks, but before provider iteration.

High-level integration:

1. Resolve the affinity config from the selected pool.
2. Build a `SessionContext` by reading the configured session header and falling back to bearer token extraction.
3. Compute the session fingerprint once and reuse it for routing, logs, and metrics.
4. Pass the preferred provider id into the pool selection iterator.
5. On successful upstream selection and request completion, update the mapping.

Because strict handlers call into `target_message_handler`, no separate affinity implementation is needed for strict mode.

This also means the affinity behavior is naturally backward compatible:

- models without `session_affinity` keep existing behavior
- requests without session identity keep existing behavior
- existing load balancing strategies continue to work unchanged

Suggested helper types:

```rust
pub struct SessionContext {
    pub fingerprint: String,
    pub source: SessionSource,
}

pub enum SessionSource {
    HeaderOnly,
    AuthorizationOnly,
    HeaderAndAuthorization,
    None,
}
```

## Fallback Semantics

Fallback behavior needs explicit rules or the affinity mapping will flap.

### Recommended semantics

- if the bound provider succeeds, keep the binding
- if the bound provider is locally unavailable for selection, temporarily bypass it for the current request and keep the old binding unless another provider successfully takes over and `rebind_on_fallback = true`
- if the bound provider fails and another provider is attempted but does not succeed, keep the old binding
- if another provider succeeds via fallback, update the binding to that provider when `rebind_on_fallback = true`
- if the bound provider is removed from config or no longer exists in the pool, discard the stale mapping and select normally

Explicit v1 decision:

- if a fallback request succeeds and rebinding occurs, the proxy does not automatically switch the session back to the original provider later
- returning to the original provider only happens if the current binding expires or a future successful fallback rebinds again
- no `rebind_cooldown_secs` is introduced in v1; if flapping appears in production, it should be added later based on observed behavior

### Why this matters

Updating the binding too early causes session churn under transient errors. Not updating it after successful takeover causes repeated attempts to a bad provider.

## TTL Semantics

Use sliding expiration:

- every successful request refreshes the TTL
- active sessions remain sticky
- inactive sessions age out naturally

Recommended initial value:

- `ttl_secs = 300` or `600`

Long TTLs improve cache locality but reduce rebalancing flexibility. Short TTLs preserve balance but weaken cache affinity.

## Concurrency and Streaming

### Concurrency

Affinity does not bypass existing pool-level or provider-level concurrency controls.

If the bound provider is full:

- prefer temporary fallback for the current request
- do not immediately rewrite the mapping unless the fallback provider successfully completes the request and rebind policy allows it

Tradeoff:

- immediate fallback is simple and keeps latency low
- in cache-sensitive workloads, immediate fallback may reduce cache hit rate and cause session flapping across providers

Future optimization:

- a bounded affinity wait such as `max_wait_ms` could allow short waiting on the bound provider before falling back
- v1 intentionally does not introduce queueing semantics at the affinity layer

### Streaming

Streaming requests should behave the same as non-streaming requests:

- provider choice is made once, before the upstream request is sent
- the stream remains on that provider for the lifetime of the request
- the affinity mapping can be refreshed as soon as the request has been accepted by the selected provider

V1 decision:

- refreshing TTL when the upstream accepts the HTTP request is acceptable

Future optimization:

- for streaming requests, TTL refresh could instead be delayed until the first valid upstream chunk is observed
- this would reduce the chance of extending affinity for a provider that accepts the connection but fails before meaningful output begins

V1 does not add this extra complexity.

## Hot Reload Semantics

Today, config reload preserves active connection counters but not affinity state.

That is acceptable only if the affinity store lives outside `Targets`.

Recommended approach:

- keep affinity state in `AppState`, not in `Targets`
- on each request, validate that the stored `provider_id` still exists in the current pool
- if it does not exist, delete the stale mapping and fall back to normal selection

Implemented single-process optimization:

- the default in-memory store participates in eager stale cleanup after successful config reload
- cleanup is model-scoped and removes bindings whose `provider_id` is no longer valid in the new local pool view
- deleted models are also cleaned if they previously had affinity-enabled pools
- request-path lazy validation remains the correctness fallback even when eager cleanup runs

Distributed deployment rule:

- when using an external shared store such as Redis across multiple gateway instances, use lazy cleanup only
- avoid eager cleanup during config reload in shared-store mode because different instances may observe the new provider set at slightly different times
- stale bindings should only be removed when the current instance validates that the bound provider no longer exists in its active pool view

This keeps affinity stable across config reloads while remaining robust to provider removal or identity changes.

## Deployment Modes

### Single instance

Use an in-memory TTL cache. This is the simplest implementation and likely sufficient for development or a single gateway node.

### Multiple gateway instances

Use a shared external store. Otherwise the same logical session may hit different gateway replicas and receive different provider bindings.

For multi-instance deployments, session affinity should be considered incomplete unless:

- requests are already pinned to one gateway instance by an outer load balancer, or
- the affinity store is externalized

## Observability

Add logs and metrics for:

- affinity lookup hit
- affinity lookup miss
- stale mapping detected
- bound provider unavailable
- fallback from bound provider
- successful rebind
- reload-time stale cleanup

Suggested metric dimensions:

- `model`
- `result`

Suggested counters or events:

- total affinity lookups
- affinity hit ratio
- rebind count
- stale-binding eviction count
- fallback-after-affinity count

Current metric set for `session_affinity_total{model, result}`:

- `hit`
- `miss`
- `stale`
- `bound`
- `rebound`
- `unavailable`
- `error`
- `fallback_no_rebind`

Definitions:

- `hit`: affinity binding existed and the bound provider was selected
- `miss`: no affinity binding existed
- `stale`: affinity binding existed but pointed to a provider no longer valid in the current pool
- `bound`: no prior binding existed and the request successfully created one
- `rebound`: a different provider succeeded and the binding was updated
- `unavailable`: the bound provider existed but could not be selected locally, for example due to concurrency or local rate limits
- `error`: the bound provider was selected but failed at the upstream interaction stage
- `fallback_no_rebind`: fallback succeeded but the binding was intentionally preserved because `rebind_on_fallback = false`

Cleanup metrics:

- `session_affinity_cleanup_total{model, reason="reload"}`

Do not emit raw `session_id`, raw bearer tokens, derived fingerprints, or `provider_id` as metric labels.

Reasoning:

- `provider_id` is safe enough for targeted debug logging if needed
- `provider_id` should still stay out of metric labels in v1 to keep cardinality low and metrics stable
- separate `unavailable` from `error` makes it easier to distinguish local selection failure from upstream failure after affinity hit
- `bound` distinguishes first-write success from ordinary misses
- cleanup is tracked separately because it is driven by config reload rather than request selection outcome

## External Reference Notes

The design is informed by ideas seen in other projects that support multi-key or multi-upstream session stickiness.

The most useful ideas to keep are:

- session binding should be configured independently from the base selection strategy
- the implementation should expose both a normal selection path and a session-aware selection path
- TTL and expiration behavior should be explicit
- validation and testing should be part of the design, not an afterthought

The ideas intentionally not adopted are:

- exposing a broad `source = auto` style configuration in v1
- coupling affinity state directly into a selector-specific in-memory map
- replacing the existing load balancing strategy with a dedicated affinity strategy

This keeps the design aligned with Onwards' current architecture, where provider selection, fallback, and config reload are already more sophisticated than a simple multi-key round-robin selector.

## Risks and Tradeoffs

### Authorization fallback is not always a real end-user identity

If many users share one API key and no explicit `session_id` is provided, the affinity behavior is effectively per key, not per end user.

This is acceptable for cache affinity, but the semantic scope must be documented clearly.

### Explicit session id alone can be unsafe in multi-tenant deployments

If explicit `session_id` were used as the sole affinity input even when `Authorization` is present, different callers could intentionally or accidentally collide onto the same affinity bucket.

V1 avoids this by folding `Authorization` into the fingerprint whenever both values are present.

### Affinity can create hotspots

High-throughput sessions may stick to one provider and reduce balance quality. This is an intentional tradeoff in exchange for higher cache hit rates.

### Missing session identity disables affinity

That is acceptable. Requests should degrade to the current routing behavior.

### Provider identity changes invalidate bindings

Changing a provider's `url`, `onwards_key`, or `onwards_model` changes its derived identity.

This invalidates bindings for sessions previously attached to that provider and can cause a temporary cache miss wave after config reload.

V1 accepts this tradeoff because identity changes may represent a materially different upstream target or account boundary.

## Testing Plan

Add tests covering:

1. first request with explicit `session_id` creates a binding
2. subsequent request with same `(model, session_id)` reuses the same provider
3. missing `session_id` falls back to `Authorization`
4. same `(model, authorization)` reuses the same provider when no `session_id` is present
5. different explicit session ids can route to different providers
6. missing both `session_id` and `Authorization` preserves existing load balancing behavior
7. the same affinity behavior applies to `/chat/completions`, `/responses`, `/completions`, and `/embeddings` when they resolve the same model alias
8. bound provider removed on config reload causes lazy rebinding
9. bound provider at concurrency capacity falls back without crashing
10. fallback success updates binding when rebind is enabled
11. fallback success does not update binding when rebind is disabled
12. strict-mode handlers use the same affinity behavior
13. streaming requests preserve provider affinity
14. invalid `session_affinity` config is rejected at load time
15. header name matching behaves correctly regardless of request header casing
16. concurrent requests for the same session behave consistently
17. in-memory cleanup removes expired entries under load
18. config reload preserves valid bindings and eagerly cleans stale bindings for the default in-memory store while preserving lazy cleanup as fallback
19. after TTL expiration, the next request falls back to normal selection
20. concurrent requests creating the same session binding behave correctly
21. provider identity change caused by url/key/model change triggers lazy rebinding
22. large-session-count tests validate memory and latency characteristics
23. the same session issuing requests with sub-millisecond spacing behaves correctly
24. preferred provider selection is unaffected by provider weight once the provider is bound
25. session header values containing Unicode or special characters produce stable fingerprints
26. explicit `session_id` from different authorization tokens does not collide into the same affinity bucket

## Rollout Plan

1. add config types and internal stable provider identity
2. add store abstraction and default in-memory implementation
3. add config validation helpers
4. add session identity extraction and fingerprinting helper
5. extend `ProviderPool` with preferred-provider selection and `get_provider_by_id()`
6. integrate affinity into `target_message_handler`
7. add metrics and logs
8. add tests for passthrough, strict mode, streaming, concurrency, and reload
9. document multi-instance requirements before enabling in production

## Suggested Code Layout

To keep the implementation isolated, session affinity code should live in a dedicated module:

```text
src/
  session_affinity/
    mod.rs
    context.rs
    key.rs
    store.rs
    metrics.rs
```

Suggested responsibilities:

- `mod.rs`: public types and trait definitions
- `context.rs`: session extraction and `SessionContext`
- `key.rs`: session identity extraction and fingerprinting
- `store.rs`: in-memory store implementation
- `metrics.rs`: counters and helper recording functions

`key.rs` should own:

- `AffinityKey`
- session fingerprint computation
- provider identity computation

`compute_provider_id()` should preferably be exposed as a method on `Target` or `Provider` rather than as an unrelated free function.

## Optional Feature Flag

A Cargo feature flag is optional if the team wants to isolate the dependency footprint:

```toml
[features]
default = ["session-affinity"]
session-affinity = []
```

This is not required for v1 and should only be added if dependency management or rollout strategy makes it worthwhile.

## Recommended Minimal Implementation

For the first iteration:

- apply to all model-routed provider selection
- prefer explicit `session_id` and fall back to `Authorization`
- use pool-level opt-in config
- use an in-memory TTL cache by default
- use sliding TTL refresh
- support rebinding on successful fallback
- keep the feature best-effort and fail-open

This gives the expected cache-affinity behavior with minimal disruption to the existing architecture.
