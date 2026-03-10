# Session Affinity

Onwards supports pool-level session affinity for model-routed requests. When enabled, requests for the same logical session and model alias are preferentially forwarded to the same upstream provider during a sliding TTL window.

This is useful for:

- prompt cache locality
- KV cache locality
- reducing provider hopping during multi-turn conversations
- keeping fallback behavior explicit instead of implicit

## How It Works

Session affinity is an overlay on top of normal load balancing.

For each request:

1. Onwards resolves the requested model alias to a `ProviderPool`.
2. If `session_affinity` is enabled for that pool, Onwards derives a session fingerprint.
3. If a valid binding exists, Onwards tries the bound provider first.
4. If the binding is missing, stale, unavailable, or fails under fallback rules, Onwards falls back to normal provider selection.
5. On success, Onwards creates, refreshes, or rebinds the mapping according to configuration.

Affinity applies to all requests that route through the standard model-based provider selection path, not just `/v1/chat/completions`.

Important detail for first requests:

- session affinity does not override the pool strategy for an unbound session
- a session's first successful request still goes through normal provider selection
- with `weighted_random`, two different sessions can both hit the same provider by chance
- over many distinct first requests, distribution should still follow the underlying pool strategy rather than always picking the first provider

## Configuration

Add `session_affinity` at the pool level:

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

## Fields

| Field | Type | Default | Description |
|------|------|---------|-------------|
| `header_name` | string | `x-session-id` | Header used to read an explicit client session id |
| `ttl_secs` | integer | `300` | Sliding TTL for the session binding |
| `rebind_on_fallback` | bool | `true` | Whether a successful fallback provider should replace the previous binding |

Validation rules:

- `header_name` must be non-empty after trimming
- `header_name` is normalized to lowercase at config load time
- `ttl_secs` must be greater than `0`

## Session Identity Rules

Onwards uses two possible identity inputs:

- explicit session header from `header_name`
- `Authorization: Bearer <token>`

Fingerprint rules:

- if both exist: `SHA256(authorization + "|" + session_id)`
- if only session id exists: `SHA256(session_id)`
- if only authorization exists: `SHA256(authorization)`
- if neither exists: affinity is skipped

This means:

- explicit `session_id` is preferred for routing stickiness
- when `Authorization` is present, it is folded into the fingerprint to keep affinity scoped to the caller boundary
- two tenants using the same `x-session-id` will not collide if their bearer tokens differ

## Fallback Behavior

If the bound provider:

- is missing after reload
- is at concurrency capacity
- is locally rate limited
- times out
- returns a fallback-eligible status

Onwards continues with normal provider selection.

Binding updates on success:

- first successful request for a session creates a binding
- success on the same bound provider refreshes TTL
- success on a different provider updates the binding only if `rebind_on_fallback = true`
- if `rebind_on_fallback = false`, the request still succeeds but the original binding is preserved

## Hot Reload Semantics

Affinity state lives in `AppState`, not in `Targets`, so it survives config reload.

Onwards uses two stale-cleanup modes:

- lazy cleanup on request path: if a stored `provider_id` no longer exists in the current pool, the binding is deleted and the request falls back to normal selection
- eager cleanup on successful reload: the default in-memory store removes bindings for providers that are no longer valid in the new local pool view

Important deployment note:

- eager cleanup is appropriate for the default in-process store
- for a shared external store across multiple gateway instances, prefer lazy cleanup only to avoid cross-instance races during staggered reloads

## Metrics

Onwards exports Prometheus counters for affinity outcomes:

- `session_affinity_total{model, result}`
- `session_affinity_cleanup_total{model, reason="reload"}`

Current `result` values:

- `hit`
- `miss`
- `stale`
- `bound`
- `unavailable`
- `error`
- `rebound`
- `fallback_no_rebind`

Raw session ids, bearer tokens, fingerprints, and provider ids are not emitted as metric labels.

## Example Client Requests

Explicit session id plus bearer token:

```bash
curl http://localhost:3000/v1/chat/completions \
  -H 'Authorization: Bearer tenant-a' \
  -H 'Content-Type: application/json' \
  -H 'X-Session-Id: conv-123' \
  -d '{
    "model": "gpt-4o",
    "messages": [
      { "role": "user", "content": "Hello" }
    ]
  }'
```

Bearer-only fallback:

```bash
curl http://localhost:3000/v1/chat/completions \
  -H 'Authorization: Bearer tenant-a' \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "gpt-4o",
    "messages": [
      { "role": "user", "content": "Hello" }
    ]
  }'
```

## When To Enable It

Enable session affinity when:

- the upstream provider benefits from prompt or KV cache reuse
- multi-turn workloads dominate traffic
- occasional local stickiness is a better tradeoff than pure short-term balance

Avoid enabling it by default for every pool when:

- requests are mostly stateless
- traffic distribution matters more than cache locality
- you run multiple gateway instances without an external affinity store and without outer instance stickiness
