//! Execution trait for response steps.
//!
//! [`StepExecutor`] is the execution half of the contract that
//! [`crate::run_response_loop`] drives. It is deliberately decoupled from
//! storage ([`super::MultiStepStore`]) so:
//!
//! - tool dispatch decisions (HTTP tool, sub-agent recursion, MCP, etc.)
//!   live entirely in the implementor and never leak into onwards types;
//! - storage and execution can be mocked independently in tests;
//! - existing tool execution machinery (e.g.
//!   [`super::ToolExecutor`](super::ToolExecutor)) can be wrapped without
//!   touching the storage layer.

use async_trait::async_trait;

use super::multi_step_store::ExecutorError;

/// Outcome of dispatching a `tool_call` step.
///
/// The executor decides what kind of tool this is. Onwards stays
/// kind-agnostic: it sees only `Executed(_)` (the tool ran here, here is
/// its output) or `Recurse` (this is a sub-agent dispatch, the loop
/// should recurse into a sub-loop scoped under the current step).
#[derive(Debug, Clone)]
pub enum ToolDispatch {
    /// The tool was executed by the implementor; this is its raw output
    /// payload, ready to be persisted as the step's `response_payload`.
    Executed(serde_json::Value),
    /// The tool is a sub-agent. The loop should recurse with
    /// `scope_parent = Some(step_id)` and `depth + 1`. The sub-loop's
    /// final return value will be persisted as this step's
    /// `response_payload`.
    Recurse,
}

/// Execution trait for response steps.
///
/// Implementations resolve the model or tool referenced by the step's
/// `request_payload` and either execute it or signal a sub-agent recursion.
#[async_trait]
pub trait StepExecutor: Send + Sync {
    /// Execute a `model_call` step: fire the upstream model and return
    /// the raw response payload to persist in `response_payload`.
    async fn execute_model_call(
        &self,
        step_id: &str,
        request_payload: &serde_json::Value,
    ) -> Result<serde_json::Value, ExecutorError>;

    /// Dispatch a `tool_call` step. The executor decides whether to run
    /// the tool itself (returning [`ToolDispatch::Executed`]) or to
    /// signal a sub-agent recursion (returning [`ToolDispatch::Recurse`]).
    async fn dispatch_tool_call(
        &self,
        step_id: &str,
        request_payload: &serde_json::Value,
    ) -> Result<ToolDispatch, ExecutorError>;
}
