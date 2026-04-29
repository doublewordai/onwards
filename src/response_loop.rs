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
        let mut prev_step: Option<String> = store
            .list_chain(request_id, scope_parent)
            .await?
            .last()
            .map(|step: &ChainStep| step.id.clone());

        loop {
            if iterations >= config.max_response_iterations {
                return Err(LoopError::MaxIterationsExceeded);
            }
            iterations += 1;

            let action = store.next_action_for(request_id, scope_parent).await?;

            match action {
                NextAction::Complete(payload) => return Ok(payload),
                NextAction::Fail(payload) => return Err(LoopError::Failed(payload)),
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
                    }

                    // Execute siblings concurrently. Per-step failures
                    // are persisted via fail_step and swallowed; storage
                    // failures propagate. Sub-agent recursion happens
                    // inside execute_step.
                    let futures = descriptors.iter().zip(step_ids.iter()).map(
                        |(descriptor, step_id)| {
                            execute_step(
                                store,
                                tool_executor,
                                tool_ctx,
                                model_target,
                                http_client.clone(),
                                &kinds,
                                request_id,
                                step_id,
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
    kinds: &'a HashMap<String, ToolKind>,
    request_id: &'a str,
    step_id: &'a str,
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
                fire_model_call(&*http_client, model_target, &descriptor.request_payload).await
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

async fn fire_model_call(
    http_client: &(dyn HttpClient + Send + Sync),
    target: &UpstreamTarget,
    request_payload: &Value,
) -> Result<Value, LoopError> {
    // Build an axum::Request the same way the rest of onwards does, so
    // the model call goes through the same connection pool, TLS, and
    // observability surface as single-step proxying.
    let body_bytes = serde_json::to_vec(request_payload).map_err(|e| {
        LoopError::Executor(ExecutorError::ExecutionError(format!(
            "model call body serialize: {e}"
        )))
    })?;

    let mut builder = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri(&target.url)
        .header(axum::http::header::CONTENT_TYPE, "application/json");
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
    let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .map_err(|e| {
            LoopError::Executor(ExecutorError::ExecutionError(format!(
                "model call body read: {e}"
            )))
        })?;

    if !status.is_success() {
        let body_text = String::from_utf8_lossy(&body_bytes).into_owned();
        return Err(LoopError::Executor(ExecutorError::ExecutionError(format!(
            "model call returned HTTP {status}: {body_text}"
        ))));
    }

    serde_json::from_slice::<Value>(&body_bytes).map_err(|e| {
        LoopError::Executor(ExecutorError::ExecutionError(format!(
            "model call body parse: {e}"
        )))
    })
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
