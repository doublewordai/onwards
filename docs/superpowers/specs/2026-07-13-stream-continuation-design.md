# Stream Continuation Design

## Goal

Add an opt-in retry phase for interrupted OpenAI-compatible text completion
streams. When an upstream SSE response ends before a terminal event, Onwards
will request a continuation using the original prompt plus the text already
emitted to the client, then splice the new SSE chunks into the existing
downstream response.

This is best-effort continuation, not transparent replay of the original model
execution. It is intentionally separate from pre-response fallback because the
client has already observed output when this phase begins.

## Scope

The first version supports `POST /v1/completions` requests with:

- `stream: true`;
- a single string `prompt`;
- one completion (`n` omitted or equal to `1`);
- `echo` omitted or `false`.

Chat completions, Responses API streams, tool calls, structured output,
multi-choice completions, token-array prompts, and echoed prompts are not
eligible. Those shapes cannot be resumed from a single unambiguous text prefix.

Continuation is triggered by an upstream body error, EOF before either `[DONE]`
or a non-null `finish_reason`, or an optional per-event idle timeout. A stream
that has emitted `[DONE]` or a non-null `finish_reason` is complete and is never
continued.

## Alternatives Considered

Raw prefix continuation on `/v1/completions` is the selected approach because
the generated text is an unambiguous continuation of one prompt. Appending a
partial assistant message to `/v1/chat/completions` would cover more models, but
it starts a new chat turn and can introduce a preamble, repetition, tool-state
corruption, or a second assistant role. Buffering the entire response before
forwarding would preserve ordinary pre-response retry semantics, but removes
streaming latency and makes memory use proportional to every response. Neither
alternative is included in this version.

## Configuration

Continuation is configured under the pool's existing `fallback` object:

```json
{
  "fallback": {
    "enabled": true,
    "on_status": [429, 5],
    "max_attempts": 3,
    "backoff": {
      "initial_ms": 100,
      "max_ms": 1000,
      "jitter": "full"
    },
    "stream_continuation": {
      "enabled": true,
      "endpoints": ["/v1/completions"],
      "max_attempts": 1,
      "max_buffered_bytes": 1048576,
      "idle_timeout_ms": 30000
    }
  }
}
```

`fallback.enabled` remains the master retry switch. `fallback.max_attempts`
continues to control attempts before a response is committed;
`stream_continuation.max_attempts` independently controls continuation requests
after partial output has been sent. The continuation phase reuses the pool's
provider selection, retryable status/rate-limit rules, backoff, and cumulative
backoff cap, but starts a fresh backoff budget.

Defaults are conservative: continuation is disabled, `endpoints` is empty,
`max_attempts` is `1`, `max_buffered_bytes` is 1 MiB, and no idle timeout is
applied. Endpoint matching uses the request path without its query string and
requires an exact configured path.

## Data Flow

The existing handler retry loop remains responsible for selecting an upstream
and obtaining a successful response before any bytes are returned. For an
eligible response, the handler wraps the body in a continuation stream before
response sanitizers are applied.

The continuation stream buffers complete SSE events, forwards each event
immediately, and retains only the concatenated `choices[0].text`. Buffer growth
is checked before appending text. If the configured cap would be exceeded,
continuation is disabled for the rest of that response while forwarding the
original stream unchanged.

On interruption, the stream builds a request from the original JSON body and
sets `prompt` to `original_prompt + emitted_text`. It selects a provider using
the same pool strategy, applies the selected provider's model rewrite,
authentication, trace policy, rate limit, concurrency limit, and header timeout,
then begins forwarding the new SSE body. A continuation that itself interrupts
may consume another continuation attempt using the enlarged text prefix.

Provider concurrency guards live only for their corresponding upstream body.
The request inflight guard spans the complete downstream response, including all
continuation attempts.

## Response Semantics

Continuation chunks preserve the initial stream's top-level `id`, `model`, and
`created` values when those fields were observed, so one downstream response
does not visibly change identity when the upstream provider changes. Generated
text and terminal fields come from the active attempt.

Usage is not synthesized across attempts. Any usage event emitted by the final
provider describes that provider request, not the aggregate cost of all
attempts. Documentation must call this out because exact aggregate accounting
is unavailable without tokenizer-specific reconstruction.

If continuation attempts are exhausted, Onwards terminates the body without
inventing a successful `[DONE]` marker. An upstream body error is surfaced as a
body error after exhaustion; a plain premature EOF remains a premature EOF.

## Observability

The feature emits counters for continuation attempts, successful resumptions,
failures, buffer-limit exhaustion, and terminal reasons. Logs and spans include
the reason and attempt number but never prompt or generated text.

## Testing

Tests cover configuration defaults and parsing, endpoint and request
eligibility, SSE text/terminal parsing, bounded buffering, continuation request
construction, provider/model rewriting, normal streams that must not retry,
premature EOF continuation, body-error continuation, exhausted continuation
attempts, and the separation between pre-response and post-commit attempt
budgets.
