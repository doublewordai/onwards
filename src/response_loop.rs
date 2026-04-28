//! Multi-step Open Responses orchestration loop.
//!
//! [`run_response_loop`] is the single source of truth for driving a
//! multi-step response from `pending` to terminal state. It is a free
//! function over the [`ResponseStore`] trait so that both execution paths
//! described in `fusillade/docs/plans/2026-04-28-multi-step-responses.md`
//! — onwards inline (warm) and a fusillade daemon worker (cold) — invoke
//! the same code with the same semantics.
//!
//! ## Responsibilities split (see plan §"Service Responsibilities")
//!
//! - **Onwards (this file):** the loop itself, parallel fan-out, sub-agent
//!   recursion, and the safety caps that bound the recursion tree.
//! - **Storage implementation (dwctl):** the transition function
//!   ([`ResponseStore::next_action_for`]) and assembly logic
//!   ([`ResponseStore::assemble_response`]).
//!
//! ## Why a free function (and not a struct method)
//!
//! A free function makes the recursion in
//! [`StepKind::ToolCall`]-with-sub-agent dispatch trivial: the recursive
//! call is just `Box::pin(run_response_loop(...))` against the same store
//! reference. There is no per-loop state to thread through; everything is
//! a pure function of the chain in storage.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::traits::{NextAction, ResponseStore, StepDescriptor, StepKind, StoreError};

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
        }
    }
}

impl std::error::Error for LoopError {}

impl From<StoreError> for LoopError {
    fn from(e: StoreError) -> Self {
        LoopError::Store(e)
    }
}

/// Drive a multi-step Open Responses request to a terminal state.
///
/// Returns a boxed `Send` future (`LoopFuture`) rather than a bare async
/// function so the recursive sub-agent dispatch can name the recursive
/// call's type — async fns cannot recurse directly because their return
/// type would be infinitely nested.
///
/// `scope_parent`:
/// - `None` — top-level loop for the user-visible response. The returned
///   value is the final response payload (typically materialized via
///   [`ResponseStore::assemble_response`] inside the implementor's
///   `next_action_for` returning `Complete`).
/// - `Some(step_id)` — sub-agent loop scoped under that step. The
///   returned value is what gets persisted as the spawning step's
///   `response_payload`.
///
/// Behavior contract:
///
/// 1. Each iteration calls `store.next_action_for(request_id, scope_parent)`.
/// 2. `Complete(v)` returns `Ok(v)`.
/// 3. `Fail(v)` returns `Err(LoopError::Failed(v))`.
/// 4. `AppendSteps(descriptors)`:
///    - For each descriptor, allocate a new `step_sequence`, persist a
///      pending step row whose `prev_step_id` chains linearly through
///      the siblings (so transcript order is stable even though execution
///      is concurrent).
///    - Execute all sibling steps with `futures_util::future::join_all`.
///    - Each step is dispatched on its kind: `ModelCall` →
///      `execute_model_call`; `ToolCall` with `is_subagent=false` →
///      `execute_tool_call`; `ToolCall` with `is_subagent=true` →
///      recursive `run_response_loop` call with `scope_parent =
///      Some(step_id)` and `depth + 1`.
///    - Per-step success → `complete_step`. Per-step failure →
///      `fail_step` with a structured error payload.
/// 5. After fan-out, the loop continues; the next call to
///    `next_action_for` sees the now-completed/failed sibling rows and
///    decides the next move.
pub fn run_response_loop<'a, S>(
    store: &'a S,
    request_id: &'a str,
    scope_parent: Option<&'a str>,
    config: LoopConfig,
    depth: u32,
) -> LoopFuture<'a>
where
    S: ResponseStore + ?Sized,
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
                    // below. The UNIQUE
                    // (request_id, parent_step_id, prev_step_id, step_kind)
                    // constraint on the storage table is the idempotency
                    // safety net for crash recovery.
                    let mut step_ids: Vec<String> = Vec::with_capacity(descriptors.len());
                    let mut current_prev: Option<String> = prev_step.clone();
                    for descriptor in &descriptors {
                        let sequence = store.next_sequence(request_id).await?;
                        let id = store
                            .record_step(
                                request_id,
                                scope_parent,
                                current_prev.as_deref(),
                                descriptor,
                                sequence,
                            )
                            .await?;
                        current_prev = Some(id.clone());
                        step_ids.push(id);
                    }

                    // Execute siblings concurrently. Each future drives
                    // one step from pending to terminal; per-step failures
                    // are recorded but do not abort the fan-out — the next
                    // next_action_for iteration sees the failed sibling
                    // row and the implementor decides whether the response
                    // continues, fails, or is partially recoverable.
                    let futures =
                        descriptors
                            .iter()
                            .zip(step_ids.iter())
                            .map(|(descriptor, step_id)| {
                                execute_step(store, request_id, step_id, descriptor, config, depth)
                            });
                    let results: Vec<Result<(), LoopError>> =
                        futures_util::future::join_all(futures).await;

                    // Step-level failures (model_call/tool_call execution,
                    // sub-agent recursion outcomes) are persisted by
                    // execute_step via fail_step and then swallowed; the
                    // next next_action_for iteration sees the failed
                    // sibling row and the implementor's transition
                    // function decides whether to recover, fail, or
                    // continue.
                    //
                    // Errors that *do* surface here are storage-layer
                    // failures from record_step/mark_processing/fail_step/
                    // complete_step — the loop cannot make progress past
                    // those.
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
///
/// Marks the step processing, dispatches on kind, and persists the
/// terminal outcome. Sub-agent dispatch (`ToolCall` with
/// `is_subagent=true`) recurses via `run_response_loop`.
///
/// Returns a boxed `Send` future to mirror [`run_response_loop`]'s
/// signature — both functions form a mutually recursive pair through
/// sub-agent dispatch, so the trick is the same.
fn execute_step<'a, S>(
    store: &'a S,
    request_id: &'a str,
    step_id: &'a str,
    descriptor: &'a StepDescriptor,
    config: LoopConfig,
    depth: u32,
) -> Pin<Box<dyn Future<Output = Result<(), LoopError>> + Send + 'a>>
where
    S: ResponseStore + ?Sized,
{
    Box::pin(async move {
        store.mark_step_processing(step_id).await?;

        let outcome: Result<serde_json::Value, LoopError> = match descriptor.kind {
            StepKind::ModelCall => store
                .execute_model_call(step_id, &descriptor.request_payload)
                .await
                .map_err(LoopError::Store),
            StepKind::ToolCall if descriptor.is_subagent => {
                // Pre-check the depth cap before recursing so the
                // spawning step is failed cleanly with
                // `max_depth_exceeded` rather than entering a sub-loop
                // that's about to bail out. This matches plan §C11.
                if depth + 1 > config.max_response_step_depth {
                    Err(LoopError::MaxDepthExceeded)
                } else {
                    run_response_loop(store, request_id, Some(step_id), config, depth + 1).await
                }
            }
            StepKind::ToolCall => store
                .execute_tool_call(step_id, &descriptor.request_payload)
                .await
                .map_err(LoopError::Store),
        };

        match outcome {
            Ok(payload) => {
                store.complete_step(step_id, &payload).await?;
                Ok(())
            }
            Err(loop_err) => {
                let error_payload = error_to_payload(&loop_err);
                // fail_step itself failing is a real storage problem —
                // propagate it. Otherwise, the step's terminal state is
                // recorded; the parent loop level can continue and let
                // the transition function react to the failed sibling.
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
    }
}
