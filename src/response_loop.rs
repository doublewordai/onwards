//! Multi-step Open Responses orchestration loop.
//!
//! [`run_response_loop`] is the single source of truth for driving a
//! multi-step response from `pending` to terminal state. It is a free
//! function over the [`MultiStepStore`] and [`StepExecutor`] traits so
//! that both execution paths described in
//! `fusillade/docs/plans/2026-04-28-multi-step-responses.md` — onwards
//! inline (warm) and a fusillade daemon worker (cold) — invoke the same
//! code with the same semantics.
//!
//! ## Storage / execution split
//!
//! Onwards holds two trait objects, never one:
//!
//! - [`MultiStepStore`] — pure CRUD + transition + assembly + chain walk.
//!   Self-contained: implementors expose chain state via
//!   [`MultiStepStore::list_chain`] so onwards never reaches around the
//!   trait into storage internals.
//! - [`StepExecutor`] — model and tool execution. Decides what kind of
//!   tool a `tool_call` step is via [`ToolDispatch`]. Onwards stays
//!   agnostic to tool kinds (HTTP, sub-agent, MCP, etc.).
//!
//! ## Why a free function (and not a struct method)
//!
//! A free function makes the recursion in [`ToolDispatch::Recurse`]
//! trivial: the recursive call is just `run_response_loop(store,
//! executor, ...)` against the same trait references. There is no
//! per-loop state to thread through; everything is a pure function of
//! the chain in storage.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::traits::{
    ExecutorError, MultiStepStore, NextAction, StepDescriptor, StepExecutor, StepKind, StoreError,
    ToolDispatch,
};

/// Type alias for the recursive sub-loop call. Required because async fns
/// can't directly recurse — the future type is its own type, infinitely.
type LoopFuture<'a> =
    Pin<Box<dyn Future<Output = Result<serde_json::Value, LoopError>> + Send + 'a>>;

#[cfg(test)]
#[path = "response_loop_tests.rs"]
mod tests;

/// Configuration for [`run_response_loop`]'s safety caps.
///
/// Two independent caps bound the absolute worst-case work for any single
/// response (per plan §C11):
///
/// - `max_response_step_depth` — how many levels deep sub-agent recursion
///   can go. A `tool_call` step that would exceed this depth is failed
///   with [`LoopError::MaxDepthExceeded`] before the sub-loop is entered.
/// - `max_response_iterations` — how many model_call ↔ tool_call
///   iterations a single loop level can run. When a level reaches this
///   cap, that level fails with [`LoopError::MaxIterationsExceeded`].
///
/// With the default values (depth 8, iterations 10) the absolute upper
/// bound on work for one response is `8 × 10 × fan_out_width`.
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

/// Errors returned by [`run_response_loop`].
#[derive(Debug)]
pub enum LoopError {
    /// The transition function returned `Fail(payload)` — the implementor
    /// decided the response cannot continue.
    Failed(serde_json::Value),
    /// A loop level exhausted `max_response_iterations` without reaching
    /// a `Complete` or `Fail` action.
    MaxIterationsExceeded,
    /// A `tool_call` step's sub-agent dispatch would exceed
    /// `max_response_step_depth`. The spawning step is failed with this
    /// error before the sub-loop is entered, so the parent loop can
    /// observe it like any other tool failure.
    MaxDepthExceeded,
    /// The transition function returned `AppendSteps(vec![])`. This is a
    /// programming error in the implementor — the loop has no meaningful
    /// way to continue.
    EmptyAction,
    /// The storage layer returned an error.
    Store(StoreError),
    /// The executor returned an error.
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
/// `scope_parent`:
/// - `None` — top-level loop for the user-visible response. The returned
///   value is the final response payload (typically materialized via
///   [`MultiStepStore::assemble_response`] inside the implementor's
///   `next_action_for` returning `Complete`).
/// - `Some(step_id)` — sub-agent loop scoped under that step. The
///   returned value is what gets persisted as the spawning step's
///   `response_payload`.
///
/// Behavior contract:
///
/// 1. Each iteration calls
///    `store.next_action_for(request_id, scope_parent)`.
/// 2. `Complete(v)` returns `Ok(v)`.
/// 3. `Fail(v)` returns `Err(LoopError::Failed(v))`.
/// 4. `AppendSteps(descriptors)`:
///    - For each descriptor, the store records a step (allocating its
///      sequence atomically) whose `prev_step_id` chains linearly through
///      the siblings — transcript order is stable even though execution
///      is concurrent.
///    - All sibling steps execute concurrently via
///      `futures_util::future::join_all`.
///    - Per-step dispatch: `ModelCall` →
///      `executor.execute_model_call(...)`; `ToolCall` →
///      `executor.dispatch_tool_call(...)`. A `ToolDispatch::Recurse`
///      result triggers a recursive `run_response_loop` call with
///      `scope_parent = Some(step_id)` and `depth + 1`.
///    - Per-step success → `complete_step`. Per-step failure →
///      `fail_step` with a structured error payload; the loop continues
///      to the next iteration so the implementor's transition function
///      can react to the failed sibling.
pub fn run_response_loop<'a, S, E>(
    store: &'a S,
    executor: &'a E,
    request_id: &'a str,
    scope_parent: Option<&'a str>,
    config: LoopConfig,
    depth: u32,
) -> LoopFuture<'a>
where
    S: MultiStepStore + ?Sized,
    E: StepExecutor + ?Sized,
{
    Box::pin(async move {
        if depth > config.max_response_step_depth {
            return Err(LoopError::MaxDepthExceeded);
        }

        let mut iterations: u32 = 0;
        let mut prev_step: Option<String> = None;

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

                    // Insert each step in order, linking prev_step_id
                    // through siblings so the chain reads as a stable
                    // transcript even though we'll fire them concurrently
                    // below. record_step allocates the sequence atomically
                    // so there's no separate round-trip.
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

                    // Execute siblings concurrently. Per-step failures are
                    // recorded but do not abort the fan-out — the next
                    // next_action_for iteration sees the failed sibling
                    // row and the implementor decides recovery.
                    let futures =
                        descriptors
                            .iter()
                            .zip(step_ids.iter())
                            .map(|(descriptor, step_id)| {
                                execute_step(
                                    store, executor, request_id, step_id, descriptor, config, depth,
                                )
                            });
                    let results: Vec<Result<(), LoopError>> =
                        futures_util::future::join_all(futures).await;

                    // Step-level outcomes are persisted by execute_step via
                    // complete_step / fail_step and then swallowed; only
                    // storage-layer failures propagate up here.
                    for outcome in results {
                        outcome?;
                    }

                    prev_step = step_ids.last().cloned();
                }
            }
        }
    })
}

/// Execute one step in the chain.
fn execute_step<'a, S, E>(
    store: &'a S,
    executor: &'a E,
    request_id: &'a str,
    step_id: &'a str,
    descriptor: &'a StepDescriptor,
    config: LoopConfig,
    depth: u32,
) -> Pin<Box<dyn Future<Output = Result<(), LoopError>> + Send + 'a>>
where
    S: MultiStepStore + ?Sized,
    E: StepExecutor + ?Sized,
{
    Box::pin(async move {
        store.mark_step_processing(step_id).await?;

        let outcome: Result<serde_json::Value, LoopError> = match descriptor.kind {
            StepKind::ModelCall => executor
                .execute_model_call(step_id, &descriptor.request_payload)
                .await
                .map_err(LoopError::Executor),
            StepKind::ToolCall => {
                match executor
                    .dispatch_tool_call(step_id, &descriptor.request_payload)
                    .await
                {
                    Ok(ToolDispatch::Executed(payload)) => Ok(payload),
                    Ok(ToolDispatch::Recurse) => {
                        // Pre-check the depth cap before recursing so the
                        // spawning step is failed cleanly with
                        // `max_depth_exceeded` rather than entering a
                        // sub-loop that's about to bail out (plan §C11).
                        if depth + 1 > config.max_response_step_depth {
                            Err(LoopError::MaxDepthExceeded)
                        } else {
                            run_response_loop(
                                store,
                                executor,
                                request_id,
                                Some(step_id),
                                config,
                                depth + 1,
                            )
                            .await
                        }
                    }
                    Err(e) => Err(LoopError::Executor(e)),
                }
            }
        };

        match outcome {
            Ok(payload) => {
                store.complete_step(step_id, &payload).await?;
                Ok(())
            }
            Err(loop_err) => {
                let error_payload = error_to_payload(&loop_err);
                store.fail_step(step_id, &error_payload).await?;
                Ok(())
            }
        }
    })
}

fn error_to_payload(e: &LoopError) -> serde_json::Value {
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
