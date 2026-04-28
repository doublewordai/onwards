//! Response storage trait for stateful conversations and response lifecycle tracking.
//!
//! Implement this trait to enable:
//! - `previous_response_id` support in the Open Responses adapter
//! - Response lifecycle tracking (create → complete/fail) for `GET /v1/responses/{id}`
//! - `background` mode where responses are created before proxying and polled later

use async_trait::async_trait;
use std::fmt;

/// Error type for response store operations
#[derive(Debug, Clone)]
pub enum StoreError {
    /// Response not found with the given ID
    NotFound(String),
    /// Storage backend error
    StorageError(String),
    /// Serialization/deserialization error
    SerializationError(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotFound(id) => write!(f, "Response not found: {}", id),
            StoreError::StorageError(msg) => write!(f, "Storage error: {}", msg),
            StoreError::SerializationError(msg) => write!(f, "Serialization error: {}", msg),
        }
    }
}

impl std::error::Error for StoreError {}

/// Trait for response lifecycle management and stateful conversations.
///
/// Implement this to enable:
/// - **Lifecycle tracking**: `create_pending` before proxying, `complete`/`fail` after.
///   This makes every response retrievable via `GET /v1/responses/{id}`.
/// - **Conversation state**: `get_context` retrieves previous response context for
///   `previous_response_id` support.
/// - **Legacy storage**: `store` persists a completed response (used by the adapter
///   after constructing the final response).
///
/// All methods have default no-op implementations, so existing `ResponseStore`
/// implementations continue to compile without changes.
#[async_trait]
pub trait ResponseStore: Send + Sync {
    /// Pre-request: create a response record before proxying to the provider.
    ///
    /// Called by the responses handler before forwarding the request. The returned
    /// ID becomes the canonical response ID (e.g. `resp_<uuid>`).
    ///
    /// The record should be in a "processing" or equivalent state so that
    /// `GET /v1/responses/{id}` returns `status: in_progress` while the request
    /// is in flight.
    async fn create_pending(
        &self,
        request: &serde_json::Value,
        model: &str,
        endpoint: &str,
    ) -> Result<String, StoreError> {
        let _ = (request, model, endpoint);
        Ok(format!("resp_{}", uuid_simple()))
    }

    /// Post-request: mark the response as completed with the response body.
    async fn complete(
        &self,
        response_id: &str,
        response: &serde_json::Value,
        status_code: u16,
    ) -> Result<(), StoreError> {
        let _ = (response_id, response, status_code);
        Ok(())
    }

    /// Post-request: mark the response as failed.
    async fn fail(&self, response_id: &str, error: &str) -> Result<(), StoreError> {
        let _ = (response_id, error);
        Ok(())
    }

    /// Retrieve a response by ID (for `GET /v1/responses/{id}`).
    async fn get(&self, response_id: &str) -> Result<Option<serde_json::Value>, StoreError> {
        let _ = response_id;
        Ok(None)
    }

    /// Store a completed response and return its ID for future reference.
    ///
    /// Used by the adapter after constructing the final `ResponsesResponse`.
    async fn store(&self, response: &serde_json::Value) -> Result<String, StoreError>;

    /// Retrieve the context (items/messages) for a previous response.
    ///
    /// Used to resolve `previous_response_id` — the adapter extracts output items
    /// from the stored response and prepends them as conversation context.
    async fn get_context(&self, response_id: &str)
    -> Result<Option<serde_json::Value>, StoreError>;

    // ====================================================================
    // Multi-step orchestration (additive — default impls fall through)
    // ====================================================================
    //
    // The following methods make the trait sufficient for
    // [`crate::run_response_loop`] to drive a multi-step Open Responses
    // request end-to-end. Implementations that don't support multi-step
    // (e.g. `NoOpResponseStore`) inherit the default impls, which return
    // `StoreError::StorageError` — `run_response_loop` will surface that
    // as a `LoopError::Store` and exit cleanly.
    //
    // The "transition" + "assembly" responsibility lives in the
    // implementor (per the plan, dwctl owns Open Responses domain logic);
    // the loop itself is a free function that's purely a function of the
    // chain-walk state these methods expose.

    /// Decide the next action for a request given the chain so far.
    ///
    /// Called once per loop iteration. The implementor walks the existing
    /// `response_steps` chain (filtered to `scope_parent`'s sub-loop), looks
    /// at the most recent step's `response_payload`, and returns whether to
    /// append more steps, complete, or fail.
    ///
    /// `scope_parent`:
    /// - `None`  — the top-level chain (user-visible response)
    /// - `Some(step_id)` — the sub-loop spawned by that step (sub-agent)
    async fn next_action_for(
        &self,
        request_id: &str,
        scope_parent: Option<&str>,
    ) -> Result<NextAction, StoreError> {
        let _ = (request_id, scope_parent);
        Err(StoreError::StorageError(
            "next_action_for not implemented".into(),
        ))
    }

    /// Persist a new step row in `pending` state and return its id.
    async fn record_step(
        &self,
        request_id: &str,
        scope_parent: Option<&str>,
        prev_step: Option<&str>,
        descriptor: &StepDescriptor,
        sequence: i64,
    ) -> Result<String, StoreError> {
        let _ = (request_id, scope_parent, prev_step, descriptor, sequence);
        Err(StoreError::StorageError(
            "record_step not implemented".into(),
        ))
    }

    /// Transition a `pending` step to `processing`.
    async fn mark_step_processing(&self, step_id: &str) -> Result<(), StoreError> {
        let _ = step_id;
        Err(StoreError::StorageError(
            "mark_step_processing not implemented".into(),
        ))
    }

    /// Persist a step's terminal output (transition to `completed`).
    async fn complete_step(
        &self,
        step_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let _ = (step_id, payload);
        Err(StoreError::StorageError(
            "complete_step not implemented".into(),
        ))
    }

    /// Persist a step's terminal error (transition to `failed`).
    async fn fail_step(&self, step_id: &str, error: &serde_json::Value) -> Result<(), StoreError> {
        let _ = (step_id, error);
        Err(StoreError::StorageError("fail_step not implemented".into()))
    }

    /// Execute a `model_call` step: fire the upstream model and return the
    /// raw response payload to persist in `response_payload`. The loop
    /// will call `complete_step` with the returned value on success or
    /// `fail_step` with a structured error on failure.
    async fn execute_model_call(
        &self,
        step_id: &str,
        request_payload: &serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        let _ = (step_id, request_payload);
        Err(StoreError::StorageError(
            "execute_model_call not implemented".into(),
        ))
    }

    /// Execute a `tool_call` step that is NOT a sub-agent dispatch.
    /// Sub-agent dispatch is handled by [`crate::run_response_loop`]
    /// directly (it recurses into a sub-loop).
    async fn execute_tool_call(
        &self,
        step_id: &str,
        request_payload: &serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        let _ = (step_id, request_payload);
        Err(StoreError::StorageError(
            "execute_tool_call not implemented".into(),
        ))
    }

    /// Allocate the next monotonic `step_sequence` value for a request.
    /// The sequence is global across nesting levels; it doubles as the
    /// `Last-Event-ID` cursor for the user-visible event stream.
    async fn next_sequence(&self, request_id: &str) -> Result<i64, StoreError> {
        let _ = request_id;
        Err(StoreError::StorageError(
            "next_sequence not implemented".into(),
        ))
    }

    /// Materialize the final `Response` JSON (per the OpenAI Responses
    /// API contract) from the chain of completed steps.
    async fn assemble_response(&self, request_id: &str) -> Result<serde_json::Value, StoreError> {
        let _ = request_id;
        Err(StoreError::StorageError(
            "assemble_response not implemented".into(),
        ))
    }
}

/// Discrete kinds of work a response step can represent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// Upstream LLM invocation.
    ModelCall,
    /// Server-side tool invocation. When `StepDescriptor::is_subagent` is
    /// true, [`crate::run_response_loop`] recurses into a sub-loop instead
    /// of calling the storage's tool executor.
    ToolCall,
}

/// Description of a single step the transition function wants the loop to
/// execute next. Returned (one or more, for fan-out) inside
/// [`NextAction::AppendSteps`].
#[derive(Debug, Clone)]
pub struct StepDescriptor {
    pub kind: StepKind,
    /// Step-specific input payload (instructions, tool name + arguments,
    /// etc.). Persisted verbatim into `response_steps.request_payload`.
    pub request_payload: serde_json::Value,
    /// When `true` and `kind == ToolCall`, the loop recurses into a
    /// sub-agent loop scoped under this step. The implementation's
    /// `execute_tool_call` is *not* called for this step; instead, the
    /// loop runs `next_action_for(request_id, Some(step_id))` to drive
    /// the sub-loop.
    pub is_subagent: bool,
}

/// The transition function's verdict for the next iteration.
#[derive(Debug, Clone)]
pub enum NextAction {
    /// Append one or more steps. A single descriptor = serial; multiple =
    /// parallel fan-out (the loop fires them concurrently via
    /// `futures::future::join_all`).
    AppendSteps(Vec<StepDescriptor>),
    /// Stop — the response is complete with the given final payload.
    /// `run_response_loop` returns this payload to the caller.
    Complete(serde_json::Value),
    /// Stop — the response failed with the given structured error.
    /// `run_response_loop` returns `LoopError::Failed`.
    Fail(serde_json::Value),
}

/// No-op implementation that always returns None.
///
/// This is the default implementation used when no store is configured.
/// It makes the adapter stateless - `previous_response_id` requests will fail
/// with an appropriate error.
#[derive(Debug, Clone, Default)]
pub struct NoOpResponseStore;

#[async_trait]
impl ResponseStore for NoOpResponseStore {
    async fn store(&self, _response: &serde_json::Value) -> Result<String, StoreError> {
        // Generate a dummy ID but don't actually store anything
        Ok(format!("noop_{}", uuid_simple()))
    }

    async fn get_context(
        &self,
        response_id: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        // Always return None - no state is preserved
        tracing::debug!(
            response_id = %response_id,
            "NoOpResponseStore: previous_response_id not supported without a configured store"
        );
        Ok(None)
    }
}

/// Simple UUID-like string generator (avoids adding uuid dependency)
fn uuid_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:032x}", timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_noop_store_returns_none() {
        let store = NoOpResponseStore;
        let result = store.get_context("any_id").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_noop_store_generates_id() {
        let store = NoOpResponseStore;
        let response = serde_json::json!({"test": "value"});
        let id = store.store(&response).await.unwrap();
        assert!(id.starts_with("noop_"));
    }
}
