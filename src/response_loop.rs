//! Multi-step Open Responses orchestration loop.
//!
//! [`run_response_loop`] drives a multi-step response from `pending` to
//! terminal state. It is a free function over the [`MultiStepStore`] and
//! [`ToolExecutor`] traits — both already exist in onwards for other
//! purposes, so the multi-step path adds no parallel execution
//! abstraction.
//!
//! ## Wiring
//!
//! - **Storage** — [`MultiStepStore`] handles CRUD + transition +
//!   chain walk + assembly.
//! - **Tool dispatch** — [`ToolExecutor::tools`] declares the tools and
//!   their [`ToolKind`]; [`ToolExecutor::execute`] runs `Http`-kind
//!   tools. `Agent`-kind tools cause the loop to recurse into a sub-loop
//!   instead of calling `execute`.
//! - **Model calls** — fired directly by the loop using the supplied
//!   `reqwest::Client` against the configured [`UpstreamTarget`]. No
//!   trait abstraction.
//!
//! This means dwctl's existing `HttpToolExecutor` (which already
//! implements `ToolExecutor`) plugs straight in — no wrapping, no
//! adapter, no parallel multi-step trait.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Body;
use axum::http::header::AUTHORIZATION;
use serde_json::Value;

use crate::client::HttpClient;
use crate::streaming::EventSink;
use crate::traits::{
    ChainStep, ExecutorError, MultiStepStore, NextAction, RequestContext, StepDescriptor,
    StepKind, StoreError, ToolError, ToolExecutor, ToolKind,
};

/// Type alias for the recursive sub-loop call. Required because async fns
/// can't directly recurse — the future type is its own type, infinitely.
type LoopFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, LoopError>> + Send + 'a>>;

#[cfg(test)]
#[path = "response_loop_tests.rs"]
mod tests;

/// Configuration for [`run_response_loop`]'s safety caps.
#[derive(Debug, Clone, Copy)]
pub struct LoopConfig {
    pub max_response_step_depth: u32,
    pub max_response_iterations: u32,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_response_step_depth: 8,
            max_response_iterations: 10,
        }
    }
}

/// Where the loop should send model_call HTTP requests.
///
/// The integration test passes a wiremock URL; production wiring will
/// pass a target resolved through onwards' load balancer (this is the
/// follow-up coupling for COR-349).
#[derive(Debug, Clone)]
pub struct UpstreamTarget {
    pub url: String,
    pub api_key: Option<String>,
}

/// Errors returned by [`run_response_loop`].
#[derive(Debug)]
pub enum LoopError {
    Failed(Value),
    MaxIterationsExceeded,
    MaxDepthExceeded,
    EmptyAction,
    Store(StoreError),
    Executor(ExecutorError),
}

impl fmt::Display for LoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoopError::Failed(payload) => write!(f, "response loop failed: {}", payload),
            LoopError::MaxIterationsExceeded => {
                write!(f, "response loop exceeded max_response_iterations cap")
            }
            LoopError::MaxDepthExceeded => write!(
                f,
                "response loop sub-agent recursion exceeded max_response_step_depth cap"
            ),
            LoopError::EmptyAction => write!(
                f,
                "transition function returned AppendSteps with no descriptors"
            ),
            LoopError::Store(e) => write!(f, "response loop storage error: {}", e),
            LoopError::Executor(e) => write!(f, "response loop executor error: {}", e),
        }
    }
}

impl std::error::Error for LoopError {}

impl From<StoreError> for LoopError {
    fn from(e: StoreError) -> Self {
        LoopError::Store(e)
    }
}

impl From<ExecutorError> for LoopError {
    fn from(e: ExecutorError) -> Self {
        LoopError::Executor(e)
    }
}

/// Drive a multi-step Open Responses request to a terminal state.
///
/// `tool_executor` is the same trait dwctl already implements via
/// `HttpToolExecutor` for the single-step in-process loop — no separate
/// abstraction.
///
/// `model_target` is where model_call steps fire HTTP. The loop POSTs
/// the step's `request_payload` as JSON, optionally with a Bearer
/// token, and parses the response as JSON.
///
/// `tool_ctx` is the `RequestContext` passed to `ToolExecutor::tools`
/// and `::execute` — carries the per-request resolved tool set for
/// dwctl's middleware-driven model.
pub fn run_response_loop<'a, S, T>(
    store: &'a S,
    tool_executor: &'a T,
    tool_ctx: &'a RequestContext,
    model_target: &'a UpstreamTarget,
    http_client: Arc<dyn HttpClient + Send + Sync>,
    event_sink: Option<&'a (dyn EventSink + 'a)>,
    request_id: &'a str,
    scope_parent: Option<&'a str>,
    config: LoopConfig,
    depth: u32,
) -> LoopFuture<'a>
where
    S: MultiStepStore + ?Sized,
    T: ToolExecutor + ?Sized,
{
    Box::pin(async move {
        if depth > config.max_response_step_depth {
            return Err(LoopError::MaxDepthExceeded);
        }

        // Resolve tool kinds once at the start of this loop level; the
        // dispatch path looks up by name on each tool_call. Cached locally
        // so dispatch is O(1) and we don't spam the executor with
        // discovery calls.
        let kinds: HashMap<String, ToolKind> = tool_executor
            .tools(tool_ctx)
            .await
            .into_iter()
            .map(|s| (s.name, s.kind))
            .collect();

        let mut iterations: u32 = 0;
        // Resume-aware: chain new steps onto whatever this scope's
        // existing tail is. A worker picking up a partially-populated
        // chain (crash recovery, executor handoff) chains correctly
        // instead of starting a parallel chain at the head.
        let chain_at_start = store.list_chain(request_id, scope_parent).await?;
        let mut prev_step: Option<String> = chain_at_start.last().map(|s| s.id.clone());

        // Once-per-response `created` event for the user-visible loop
        // (top-level only; sub-agents don't emit their own created).
        if depth == 0 && scope_parent.is_none()
            && let Some(sink) = event_sink
        {
            crate::streaming::try_emit(
                sink,
                crate::streaming::LoopEvent {
                    sequence: 0,
                    kind: crate::streaming::LoopEventKind::Created,
                    data: serde_json::json!({
                        "id": format!("resp_{request_id}"),
                        "object": "response",
                        "status": "in_progress",
                    }),
                },
            )
            .await;
        }

        loop {
            if iterations >= config.max_response_iterations {
                emit_terminal(
                    event_sink,
                    depth,
                    scope_parent,
                    LoopTerminal::Failed(serde_json::json!({"type": "max_iterations_exceeded"})),
                    next_terminal_sequence(&chain_at_start, iterations as i64),
                )
                .await;
                return Err(LoopError::MaxIterationsExceeded);
            }
            iterations += 1;

            let action = store.next_action_for(request_id, scope_parent).await?;

            match action {
                NextAction::Complete(payload) => {
                    emit_terminal(
                        event_sink,
                        depth,
                        scope_parent,
                        LoopTerminal::Completed(payload.clone()),
                        next_terminal_sequence_after_run(store, request_id, scope_parent).await,
                    )
                    .await;
                    return Ok(payload);
                }
                NextAction::Fail(payload) => {
                    emit_terminal(
                        event_sink,
                        depth,
                        scope_parent,
                        LoopTerminal::Failed(payload.clone()),
                        next_terminal_sequence_after_run(store, request_id, scope_parent).await,
                    )
                    .await;
                    return Err(LoopError::Failed(payload));
                }
                NextAction::AppendSteps(descriptors) => {
                    if descriptors.is_empty() {
                        return Err(LoopError::EmptyAction);
                    }

                    // Insert each step in order, chaining prev_step_id
                    // through siblings for stable transcript ordering even
                    // though execution is concurrent below. record_step
                    // allocates the sequence atomically — N siblings = N
                    // queries (no separate sequence-allocation
                    // round-trip).
                    let mut step_ids: Vec<String> = Vec::with_capacity(descriptors.len());
                    let mut step_sequences: Vec<i64> = Vec::with_capacity(descriptors.len());
                    let mut current_prev: Option<String> = prev_step.clone();
                    for descriptor in &descriptors {
                        let recorded = store
                            .record_step(
                                request_id,
                                scope_parent,
                                current_prev.as_deref(),
                                descriptor,
                            )
                            .await?;
                        current_prev = Some(recorded.id.clone());
                        step_ids.push(recorded.id);
                        step_sequences.push(recorded.sequence);
                    }

                    // Execute siblings concurrently. Per-step failures
                    // are persisted via fail_step and swallowed; storage
                    // failures propagate. Sub-agent recursion happens
                    // inside execute_step.
                    let futures = descriptors
                        .iter()
                        .zip(step_ids.iter())
                        .zip(step_sequences.iter().copied())
                        .map(|((descriptor, step_id), step_sequence)| {
                            execute_step(
                                store,
                                tool_executor,
                                tool_ctx,
                                model_target,
                                http_client.clone(),
                                event_sink,
                                &kinds,
                                request_id,
                                step_id,
                                step_sequence,
                                descriptor,
                                config,
                                depth,
                            )
                        },
                    );
                    let results: Vec<Result<(), LoopError>> =
                        futures_util::future::join_all(futures).await;

                    for outcome in results {
                        outcome?;
                    }

                    prev_step = step_ids.last().cloned();
                }
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn execute_step<'a, S, T>(
    store: &'a S,
    tool_executor: &'a T,
    tool_ctx: &'a RequestContext,
    model_target: &'a UpstreamTarget,
    http_client: Arc<dyn HttpClient + Send + Sync>,
    event_sink: Option<&'a (dyn EventSink + 'a)>,
    kinds: &'a HashMap<String, ToolKind>,
    request_id: &'a str,
    step_id: &'a str,
    step_sequence: i64,
    descriptor: &'a StepDescriptor,
    config: LoopConfig,
    depth: u32,
) -> Pin<Box<dyn Future<Output = Result<(), LoopError>> + Send + 'a>>
where
    S: MultiStepStore + ?Sized,
    T: ToolExecutor + ?Sized,
{
    Box::pin(async move {
        store.mark_step_processing(step_id).await?;

        let outcome: Result<Value, LoopError> = match descriptor.kind {
            StepKind::ModelCall => {
                fire_model_call(
                    &*http_client,
                    model_target,
                    &descriptor.request_payload,
                    event_sink,
                    step_sequence,
                )
                .await
            }
            StepKind::ToolCall => {
                let tool_name = descriptor
                    .request_payload
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        LoopError::Executor(ExecutorError::ExecutionError(
                            "tool_call request_payload missing 'name'".into(),
                        ))
                    })?;
                let kind = kinds.get(tool_name).copied().unwrap_or(ToolKind::Http);

                match kind {
                    ToolKind::Agent => {
                        if depth + 1 > config.max_response_step_depth {
                            Err(LoopError::MaxDepthExceeded)
                        } else {
                            run_response_loop(
                                store,
                                tool_executor,
                                tool_ctx,
                                model_target,
                                http_client.clone(),
                                event_sink,
                                request_id,
                                Some(step_id),
                                config,
                                depth + 1,
                            )
                            .await
                        }
                    }
                    ToolKind::Http => {
                        let args = descriptor
                            .request_payload
                            .get("args")
                            .cloned()
                            .unwrap_or(serde_json::json!({}));
                        tool_executor
                            .execute(tool_name, step_id, &args, tool_ctx)
                            .await
                            .map_err(|e| LoopError::Executor(translate_tool_error(e)))
                    }
                }
            }
        };

        match outcome {
            Ok(payload) => {
                store.complete_step(step_id, &payload).await?;
                // For tool_call steps, emit `output_item.done` with the
                // tool's full payload to the sink at completion. Token
                // deltas don't apply here (tools don't stream); the
                // step's full result is the natural unit. ModelCall
                // emissions are handled inside fire_model_call (text
                // deltas during streaming + the upstream's own done
                // marker).
                if matches!(descriptor.kind, StepKind::ToolCall)
                    && let Some(sink) = event_sink
                {
                    crate::streaming::try_emit(
                        sink,
                        crate::streaming::LoopEvent {
                            sequence: step_sequence,
                            kind: crate::streaming::LoopEventKind::OutputItemDone,
                            data: serde_json::json!({
                                "type": "function_call_output",
                                "call_id": format!("call_step_{step_sequence}"),
                                "output": serde_json::to_string(&payload).unwrap_or_default(),
                            }),
                        },
                    )
                    .await;
                }
                Ok(())
            }
            Err(loop_err @ LoopError::Store(_)) => Err(loop_err),
            Err(loop_err) => {
                let error_payload = error_to_payload(&loop_err);
                store.fail_step(step_id, &error_payload).await?;
                Ok(())
            }
        }
    })
}

/// Helpers for terminal event emission. Used by run_response_loop on
/// Complete / Fail / cap-violation paths.
enum LoopTerminal {
    Completed(Value),
    Failed(Value),
}

async fn emit_terminal(
    sink: Option<&(dyn EventSink + '_)>,
    depth: u32,
    scope_parent: Option<&str>,
    terminal: LoopTerminal,
    sequence: i64,
) {
    // Only top-level terminal events are surfaced to the user. Sub-loop
    // (sub-agent) terminals become the spawning step's response_payload
    // via the loop's normal return path; emitting them at user level
    // would conflate sub-agent and top-level state.
    if depth != 0 || scope_parent.is_some() {
        return;
    }
    let Some(sink) = sink else { return };
    let (kind, data) = match terminal {
        LoopTerminal::Completed(v) => (crate::streaming::LoopEventKind::Completed, v),
        LoopTerminal::Failed(v) => (crate::streaming::LoopEventKind::Failed, v),
    };
    crate::streaming::try_emit(
        sink,
        crate::streaming::LoopEvent {
            sequence,
            kind,
            data,
        },
    )
    .await;
}

/// Cheap sequence allocator for terminal events: max(chain_at_start) +
/// iterations + 1. Used when the loop terminates without doing any
/// extra storage I/O.
fn next_terminal_sequence(chain_at_start: &[ChainStep], iterations: i64) -> i64 {
    chain_at_start.iter().map(|s| s.sequence).max().unwrap_or(0) + iterations + 1
}

/// Sequence allocator for the post-loop-success terminal event: walk
/// the chain once more to find the highest persisted sequence and
/// return +1. Slightly more expensive but exact (matches what a
/// reconnect-with-cursor expects).
async fn next_terminal_sequence_after_run<S>(
    store: &S,
    request_id: &str,
    scope_parent: Option<&str>,
) -> i64
where
    S: MultiStepStore + ?Sized,
{
    match store.list_chain(request_id, scope_parent).await {
        Ok(chain) => chain.iter().map(|s| s.sequence).max().unwrap_or(0) + 1,
        // Best-effort fallback if the chain walk fails — terminal
        // events should still emit so clients can close their stream.
        Err(_) => i64::MAX,
    }
}

async fn fire_model_call(
    http_client: &(dyn HttpClient + Send + Sync),
    target: &UpstreamTarget,
    request_payload: &Value,
    sink: Option<&dyn EventSink>,
    step_sequence: i64,
) -> Result<Value, LoopError> {
    // The user's `stream` flag propagates through the parent request →
    // transition function → request_payload here. When true, we open a
    // streaming HTTP request, parse SSE events, forward token deltas
    // to the sink, and reassemble the final body via the upstream's
    // own done-marker. When false, we POST and parse a single JSON
    // body, ignoring the sink even if one is supplied (no SSE events
    // to forward).
    let stream_mode = request_payload
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let body_bytes = serde_json::to_vec(request_payload).map_err(|e| {
        LoopError::Executor(ExecutorError::ExecutionError(format!(
            "model call body serialize: {e}"
        )))
    })?;

    let mut builder = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri(&target.url)
        .header(axum::http::header::CONTENT_TYPE, "application/json");
    if stream_mode {
        builder = builder.header(axum::http::header::ACCEPT, "text/event-stream");
    }
    if let Some(key) = &target.api_key {
        builder = builder.header(AUTHORIZATION, format!("Bearer {key}"));
    }
    let req = builder.body(Body::from(body_bytes)).map_err(|e| {
        LoopError::Executor(ExecutorError::ExecutionError(format!(
            "model call request build: {e}"
        )))
    })?;

    let resp = http_client.request(req).await.map_err(|e| {
        LoopError::Executor(ExecutorError::ExecutionError(format!(
            "model call HTTP error: {e}"
        )))
    })?;

    let status = resp.status();
    if !status.is_success() {
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .map_err(|e| {
                LoopError::Executor(ExecutorError::ExecutionError(format!(
                    "model call body read: {e}"
                )))
            })?;
        let body_text = String::from_utf8_lossy(&body_bytes).into_owned();
        return Err(LoopError::Executor(ExecutorError::ExecutionError(format!(
            "model call returned HTTP {status}: {body_text}"
        ))));
    }

    if stream_mode {
        consume_streaming_model_response(resp, sink, step_sequence).await
    } else {
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .map_err(|e| {
                LoopError::Executor(ExecutorError::ExecutionError(format!(
                    "model call body read: {e}"
                )))
            })?;
        serde_json::from_slice::<Value>(&body_bytes).map_err(|e| {
            LoopError::Executor(ExecutorError::ExecutionError(format!(
                "model call body parse: {e}"
            )))
        })
    }
}

/// Read an upstream `stream: true` response chunk by chunk, forwarding
/// token deltas to the sink (if any) and reassembling the final body
/// via [`openai_reassembler::reassemble`] for storage.
///
/// We forward two kinds of deltas:
/// - assistant text content → `response.output_text.delta`
/// - tool_call arguments → `response.function_call_arguments.delta`
///
/// Both carry the originating `step_sequence` as the SSE `id:` so
/// reconnect-with-cursor can resume.
async fn consume_streaming_model_response(
    resp: axum::response::Response,
    sink: Option<&dyn EventSink>,
    step_sequence: i64,
) -> Result<Value, LoopError> {
    use eventsource_stream::Eventsource;
    use futures_util::StreamExt;

    let mut events: Vec<eventsource_stream::Event> = Vec::new();
    let mut sse = resp.into_body().into_data_stream().eventsource();

    while let Some(item) = sse.next().await {
        let event = item.map_err(|e| {
            LoopError::Executor(ExecutorError::ExecutionError(format!(
                "SSE parse error: {e}"
            )))
        })?;

        // Upstream signals end-of-stream with `data: [DONE]`. The
        // openai-reassembler crate handles these markers internally, so
        // we just drop them from the forwarded set and leave them in
        // the event vec (their `data` is `[DONE]`, not JSON, and would
        // be rejected by `serde_json::from_str`).
        if event.data == "[DONE]" {
            events.push(event);
            break;
        }

        if let Some(sink) = sink {
            forward_chunk_deltas(sink, &event, step_sequence).await;
        }
        events.push(event);
    }

    let reassembled = openai_reassembler::reassemble(&events).map_err(|e| {
        LoopError::Executor(ExecutorError::ExecutionError(format!(
            "reassemble model stream: {e}"
        )))
    })?;
    serde_json::from_str::<Value>(&reassembled).map_err(|e| {
        LoopError::Executor(ExecutorError::ExecutionError(format!(
            "reassembled body parse: {e}"
        )))
    })
}

/// Inspect an upstream SSE event and forward any token deltas to the
/// sink. Best-effort: malformed events are skipped silently; the
/// reassembler will produce the canonical final body regardless.
async fn forward_chunk_deltas(sink: &dyn EventSink, event: &eventsource_stream::Event, sequence: i64) {
    let parsed: Value = match serde_json::from_str(&event.data) {
        Ok(v) => v,
        Err(_) => return,
    };

    let choices = match parsed.get("choices").and_then(|c| c.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => return,
    };

    let delta = match choices[0].get("delta") {
        Some(d) => d,
        None => return,
    };

    if let Some(text) = delta.get("content").and_then(|c| c.as_str())
        && !text.is_empty()
    {
        crate::streaming::try_emit(
            sink,
            crate::streaming::LoopEvent {
                sequence,
                kind: crate::streaming::LoopEventKind::OutputTextDelta,
                data: serde_json::json!({"delta": text}),
            },
        )
        .await;
    }

    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for call in tool_calls {
            if let Some(args) = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                && !args.is_empty()
            {
                let call_id = call
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("call_unknown");
                crate::streaming::try_emit(
                    sink,
                    crate::streaming::LoopEvent {
                        sequence,
                        kind: crate::streaming::LoopEventKind::FunctionCallArgumentsDelta,
                        data: serde_json::json!({"call_id": call_id, "delta": args}),
                    },
                )
                .await;
            }
        }
    }
}

fn translate_tool_error(e: ToolError) -> ExecutorError {
    match e {
        ToolError::NotFound(name) => ExecutorError::NotFound(name),
        ToolError::ExecutionError(msg)
        | ToolError::InvalidArguments(msg)
        | ToolError::Timeout(msg) => ExecutorError::ExecutionError(msg),
    }
}

fn error_to_payload(e: &LoopError) -> Value {
    match e {
        LoopError::Failed(payload) => payload.clone(),
        LoopError::MaxIterationsExceeded => serde_json::json!({
            "type": "max_iterations_exceeded",
            "message": e.to_string(),
        }),
        LoopError::MaxDepthExceeded => serde_json::json!({
            "type": "max_depth_exceeded",
            "message": e.to_string(),
        }),
        LoopError::EmptyAction => serde_json::json!({
            "type": "empty_action",
            "message": e.to_string(),
        }),
        LoopError::Store(err) => serde_json::json!({
            "type": "store_error",
            "message": err.to_string(),
        }),
        LoopError::Executor(err) => serde_json::json!({
            "type": "executor_error",
            "message": err.to_string(),
        }),
    }
}
