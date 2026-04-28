//! Unit tests for [`super::run_response_loop`].
//!
//! Uses an in-memory `MockStore` that implements [`ResponseStore`]'s new
//! multi-step methods. The mock's `next_action_for` is driven by a
//! script (a `Vec<NextAction>` consumed in order), and per-call
//! invocations are recorded so tests can assert what the loop actually
//! did.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::json;

use crate::response_loop::{LoopConfig, LoopError, run_response_loop};
use crate::traits::{NextAction, ResponseStore, StepDescriptor, StepKind, StoreError};

#[derive(Debug, Default)]
struct StoreState {
    /// Scripted responses for `next_action_for`. Keyed by
    /// `scope_parent` so top-level vs. sub-loop iterations have
    /// separate scripts.
    actions: HashMap<Option<String>, std::collections::VecDeque<NextAction>>,
    /// All step rows the loop has persisted, keyed by step id.
    steps: HashMap<String, StoredStep>,
    /// Monotonic per-request sequence allocator.
    next_seq: HashMap<String, i64>,
    /// IDs assigned in `record_step` order, for assertions.
    record_order: Vec<String>,
    /// IDs touched by `mark_step_processing`, in order.
    mark_processing_order: Vec<String>,
    /// IDs completed (with their payload), in order.
    complete_order: Vec<(String, serde_json::Value)>,
    /// IDs failed (with their error payload), in order.
    fail_order: Vec<(String, serde_json::Value)>,
    /// IDs whose model-call execution was invoked.
    model_call_invocations: Vec<String>,
    /// IDs whose tool-call execution was invoked.
    tool_call_invocations: Vec<String>,
    /// Optional per-step model_call result override (default = success).
    model_call_results: HashMap<String, Result<serde_json::Value, String>>,
    /// Optional per-step tool_call result override (default = success).
    tool_call_results: HashMap<String, Result<serde_json::Value, String>>,
    /// Counter to give synthesized step IDs deterministic ordering for
    /// test assertions.
    id_counter: u64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // request_id + sequence are recorded for completeness
                    // but the current tests don't assert on them.
struct StoredStep {
    request_id: String,
    scope_parent: Option<String>,
    prev_step: Option<String>,
    kind: StepKind,
    sequence: i64,
    state: &'static str,
}

#[derive(Default)]
struct MockStore {
    inner: Mutex<StoreState>,
}

impl MockStore {
    fn new() -> Self {
        Self::default()
    }

    fn script(&self, scope: Option<&str>, actions: Vec<NextAction>) {
        let mut state = self.inner.lock().unwrap();
        state
            .actions
            .entry(scope.map(|s| s.to_string()))
            .or_default()
            .extend(actions);
    }

    fn snapshot(&self) -> StoreState {
        let state = self.inner.lock().unwrap();
        StoreState {
            actions: state.actions.clone(),
            steps: state.steps.clone(),
            next_seq: state.next_seq.clone(),
            record_order: state.record_order.clone(),
            mark_processing_order: state.mark_processing_order.clone(),
            complete_order: state.complete_order.clone(),
            fail_order: state.fail_order.clone(),
            model_call_invocations: state.model_call_invocations.clone(),
            tool_call_invocations: state.tool_call_invocations.clone(),
            model_call_results: state.model_call_results.clone(),
            tool_call_results: state.tool_call_results.clone(),
            id_counter: state.id_counter,
        }
    }
}

#[async_trait]
impl ResponseStore for MockStore {
    async fn store(&self, _response: &serde_json::Value) -> Result<String, StoreError> {
        Ok("noop".into())
    }

    async fn get_context(
        &self,
        _response_id: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        Ok(None)
    }

    async fn next_action_for(
        &self,
        _request_id: &str,
        scope_parent: Option<&str>,
    ) -> Result<NextAction, StoreError> {
        let mut state = self.inner.lock().unwrap();
        let key = scope_parent.map(|s| s.to_string());
        let queue = state.actions.get_mut(&key).ok_or_else(|| {
            StoreError::StorageError(format!("no scripted actions for scope {:?}", scope_parent))
        })?;
        queue.pop_front().ok_or_else(|| {
            StoreError::StorageError(format!(
                "scripted actions exhausted for scope {:?}",
                scope_parent
            ))
        })
    }

    async fn record_step(
        &self,
        request_id: &str,
        scope_parent: Option<&str>,
        prev_step: Option<&str>,
        descriptor: &StepDescriptor,
        sequence: i64,
    ) -> Result<String, StoreError> {
        let mut state = self.inner.lock().unwrap();
        state.id_counter += 1;
        let id = format!("step_{:03}", state.id_counter);
        state.steps.insert(
            id.clone(),
            StoredStep {
                request_id: request_id.to_string(),
                scope_parent: scope_parent.map(|s| s.to_string()),
                prev_step: prev_step.map(|s| s.to_string()),
                kind: descriptor.kind,
                sequence,
                state: "pending",
            },
        );
        state.record_order.push(id.clone());
        Ok(id)
    }

    async fn mark_step_processing(&self, step_id: &str) -> Result<(), StoreError> {
        let mut state = self.inner.lock().unwrap();
        if let Some(step) = state.steps.get_mut(step_id) {
            step.state = "processing";
        }
        state.mark_processing_order.push(step_id.to_string());
        Ok(())
    }

    async fn complete_step(
        &self,
        step_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let mut state = self.inner.lock().unwrap();
        if let Some(step) = state.steps.get_mut(step_id) {
            step.state = "completed";
        }
        state
            .complete_order
            .push((step_id.to_string(), payload.clone()));
        Ok(())
    }

    async fn fail_step(&self, step_id: &str, error: &serde_json::Value) -> Result<(), StoreError> {
        let mut state = self.inner.lock().unwrap();
        if let Some(step) = state.steps.get_mut(step_id) {
            step.state = "failed";
        }
        state.fail_order.push((step_id.to_string(), error.clone()));
        Ok(())
    }

    async fn execute_model_call(
        &self,
        step_id: &str,
        _request_payload: &serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        let mut state = self.inner.lock().unwrap();
        state.model_call_invocations.push(step_id.to_string());
        match state.model_call_results.remove(step_id) {
            Some(Ok(payload)) => Ok(payload),
            Some(Err(err)) => Err(StoreError::StorageError(err)),
            None => Ok(json!({"output": format!("model:{step_id}")})),
        }
    }

    async fn execute_tool_call(
        &self,
        step_id: &str,
        _request_payload: &serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        let mut state = self.inner.lock().unwrap();
        state.tool_call_invocations.push(step_id.to_string());
        match state.tool_call_results.remove(step_id) {
            Some(Ok(payload)) => Ok(payload),
            Some(Err(err)) => Err(StoreError::StorageError(err)),
            None => Ok(json!({"tool_output": format!("tool:{step_id}")})),
        }
    }

    async fn next_sequence(&self, request_id: &str) -> Result<i64, StoreError> {
        let mut state = self.inner.lock().unwrap();
        let next = state.next_seq.entry(request_id.to_string()).or_insert(0);
        *next += 1;
        Ok(*next)
    }

    async fn assemble_response(&self, _request_id: &str) -> Result<serde_json::Value, StoreError> {
        Ok(json!({"assembled": true}))
    }
}

fn model_call(payload: serde_json::Value) -> StepDescriptor {
    StepDescriptor {
        kind: StepKind::ModelCall,
        request_payload: payload,
        is_subagent: false,
    }
}

fn tool_call(payload: serde_json::Value) -> StepDescriptor {
    StepDescriptor {
        kind: StepKind::ToolCall,
        request_payload: payload,
        is_subagent: false,
    }
}

fn subagent_call(payload: serde_json::Value) -> StepDescriptor {
    StepDescriptor {
        kind: StepKind::ToolCall,
        request_payload: payload,
        is_subagent: true,
    }
}

#[tokio::test]
async fn complete_immediately_returns_payload() {
    let store = MockStore::new();
    store.script(None, vec![NextAction::Complete(json!({"final": true}))]);

    let result = run_response_loop(&store, "req_1", None, LoopConfig::default(), 0).await;

    assert_eq!(result.unwrap(), json!({"final": true}));
    let snap = store.snapshot();
    assert!(snap.record_order.is_empty(), "no steps should be recorded");
}

#[tokio::test]
async fn fail_immediately_returns_loop_error_failed() {
    let store = MockStore::new();
    store.script(None, vec![NextAction::Fail(json!({"reason": "bad"}))]);

    let result = run_response_loop(&store, "req_1", None, LoopConfig::default(), 0).await;
    match result {
        Err(LoopError::Failed(payload)) => assert_eq!(payload, json!({"reason": "bad"})),
        other => panic!("expected LoopError::Failed, got {:?}", other),
    }
}

#[tokio::test]
async fn single_model_call_then_complete() {
    let store = MockStore::new();
    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![model_call(json!({"prompt": "hi"}))]),
            NextAction::Complete(json!({"done": true})),
        ],
    );

    let result = run_response_loop(&store, "req_1", None, LoopConfig::default(), 0)
        .await
        .unwrap();
    assert_eq!(result, json!({"done": true}));

    let snap = store.snapshot();
    assert_eq!(snap.record_order, vec!["step_001"]);
    assert_eq!(snap.mark_processing_order, vec!["step_001"]);
    assert_eq!(snap.model_call_invocations, vec!["step_001"]);
    assert_eq!(snap.complete_order.len(), 1);
    assert_eq!(snap.complete_order[0].0, "step_001");
}

#[tokio::test]
async fn parallel_fan_out_runs_concurrently_and_chains_prev_step_id() {
    let store = MockStore::new();
    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![
                tool_call(json!({"a": 1})),
                tool_call(json!({"b": 2})),
                tool_call(json!({"c": 3})),
            ]),
            NextAction::Complete(json!({"final": "ok"})),
        ],
    );

    run_response_loop(&store, "req_1", None, LoopConfig::default(), 0)
        .await
        .unwrap();

    let snap = store.snapshot();
    // All three steps recorded in the iteration's chain order
    assert_eq!(snap.record_order, vec!["step_001", "step_002", "step_003"]);
    // Each step's prev_step_id chains linearly through the siblings
    assert_eq!(snap.steps["step_001"].prev_step, None);
    assert_eq!(
        snap.steps["step_002"].prev_step,
        Some("step_001".to_string())
    );
    assert_eq!(
        snap.steps["step_003"].prev_step,
        Some("step_002".to_string())
    );
    // All three saw execute_tool_call (and were marked processing)
    assert_eq!(snap.tool_call_invocations.len(), 3);
    assert_eq!(snap.mark_processing_order.len(), 3);
    assert_eq!(snap.complete_order.len(), 3);
}

#[tokio::test]
async fn step_failure_does_not_abort_loop_and_is_recorded() {
    let store = MockStore::new();
    {
        let mut state = store.inner.lock().unwrap();
        // step_001 will fail when invoked
        state
            .tool_call_results
            .insert("step_001".to_string(), Err("upstream broke".into()));
    }
    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![tool_call(json!({}))]),
            // Transition function decides to recover: fire another step.
            NextAction::AppendSteps(vec![model_call(json!({}))]),
            NextAction::Complete(json!({"final": "ok"})),
        ],
    );

    run_response_loop(&store, "req_1", None, LoopConfig::default(), 0)
        .await
        .unwrap();

    let snap = store.snapshot();
    assert_eq!(snap.fail_order.len(), 1, "step_001 should be failed");
    assert_eq!(snap.fail_order[0].0, "step_001");
    assert_eq!(snap.complete_order.len(), 1, "step_002 should complete");
    assert_eq!(snap.complete_order[0].0, "step_002");
}

#[tokio::test]
async fn max_iterations_cap_fires() {
    let store = MockStore::new();
    let descriptors = vec![model_call(json!({}))];
    // Script enough actions to exceed the cap — every iteration appends one step.
    let mut script = Vec::new();
    for _ in 0..20 {
        script.push(NextAction::AppendSteps(descriptors.clone()));
    }
    store.script(None, script);

    let config = LoopConfig {
        max_response_step_depth: 8,
        max_response_iterations: 3,
    };
    let result = run_response_loop(&store, "req_1", None, config, 0).await;
    assert!(matches!(result, Err(LoopError::MaxIterationsExceeded)));
}

#[tokio::test]
async fn max_depth_cap_fires_inside_subagent_recursion() {
    let store = MockStore::new();

    // Top-level: spawn a sub-agent.
    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![subagent_call(json!({"agent": "lvl1"}))]),
            NextAction::Complete(json!({"top": "done"})),
        ],
    );
    // Sub-loop level 1 (scope_parent = step_001): spawn another sub-agent.
    store.script(
        Some("step_001"),
        vec![NextAction::AppendSteps(vec![subagent_call(
            json!({"agent": "lvl2"}),
        )])],
    );

    // depth cap = 1 → top level is depth 0, first sub-agent is depth 1
    // (allowed), second sub-agent would be depth 2 (not allowed). The
    // cap-check fires from execute_step's pre-check of the spawning
    // tool_call rather than entering an out-of-bounds sub-loop.
    let config = LoopConfig {
        max_response_step_depth: 1,
        max_response_iterations: 10,
    };
    let _ = run_response_loop(&store, "req_1", None, config, 0).await;

    let snap = store.snapshot();
    // step_002 (the sub-sub-agent attempt) should be marked failed with
    // a max_depth_exceeded error.
    let failed: Vec<_> = snap
        .fail_order
        .iter()
        .filter(|(_, payload)| payload["type"] == "max_depth_exceeded")
        .collect();
    assert!(
        !failed.is_empty(),
        "expected max_depth_exceeded fail_step, got {:?}",
        snap.fail_order
    );
}

#[tokio::test]
async fn subagent_recursion_executes_sub_loop_under_correct_scope() {
    let store = MockStore::new();
    // Top-level: spawn a sub-agent.
    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![subagent_call(json!({"agent": "search"}))]),
            NextAction::Complete(json!({"top": "done"})),
        ],
    );
    // Sub-loop: do one model_call then complete.
    store.script(
        Some("step_001"),
        vec![
            NextAction::AppendSteps(vec![model_call(json!({"sub": true}))]),
            NextAction::Complete(json!({"sub_result": 42})),
        ],
    );

    run_response_loop(&store, "req_1", None, LoopConfig::default(), 0)
        .await
        .unwrap();

    let snap = store.snapshot();
    // Sub-agent step (step_001) and its inner model_call (step_002).
    assert_eq!(snap.steps["step_001"].kind, StepKind::ToolCall);
    assert_eq!(snap.steps["step_001"].scope_parent, None);
    assert_eq!(snap.steps["step_002"].kind, StepKind::ModelCall);
    assert_eq!(
        snap.steps["step_002"].scope_parent,
        Some("step_001".to_string())
    );
    // Sub-agent step is completed with the sub-loop's final payload
    let (sub_id, sub_payload) = &snap.complete_order[1];
    assert_eq!(sub_id, "step_001");
    assert_eq!(sub_payload, &json!({"sub_result": 42}));
}

#[tokio::test]
async fn empty_action_returns_empty_action_error() {
    let store = MockStore::new();
    store.script(None, vec![NextAction::AppendSteps(vec![])]);

    let result = run_response_loop(&store, "req_1", None, LoopConfig::default(), 0).await;
    assert!(matches!(result, Err(LoopError::EmptyAction)));
}

#[tokio::test]
async fn store_error_in_next_action_propagates() {
    let store = MockStore::new();
    // No script for top-level — next_action_for returns StorageError.

    let result = run_response_loop(&store, "req_1", None, LoopConfig::default(), 0).await;
    assert!(matches!(result, Err(LoopError::Store(_))));
}
