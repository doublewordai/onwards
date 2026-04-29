//! Unit tests for [`super::run_response_loop`].
//!
//! Two in-memory mocks: `MockStore` (implements [`MultiStepStore`]) drives
//! transition decisions via a script, and `MockExecutor` (implements
//! [`StepExecutor`]) records calls and lets tests override per-step
//! results. The split mirrors the production trait factoring so test
//! coverage tracks the real boundary.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::json;

use crate::response_loop::{LoopConfig, LoopError, run_response_loop};
use crate::traits::{
    ChainStep, ExecutorError, MultiStepStore, NextAction, RecordedStep, StepDescriptor,
    StepExecutor, StepKind, StepState, StoreError, ToolDispatch,
};

#[derive(Debug, Default)]
struct StoreState {
    /// Scripted responses for `next_action_for`. Keyed by `scope_parent`.
    actions: HashMap<Option<String>, std::collections::VecDeque<NextAction>>,
    /// All step rows the loop has persisted, keyed by step id.
    steps: HashMap<String, StoredStep>,
    /// Sequence allocator per request_id (monotonic across nesting).
    next_seq: HashMap<String, i64>,
    record_order: Vec<String>,
    mark_processing_order: Vec<String>,
    complete_order: Vec<(String, serde_json::Value)>,
    fail_order: Vec<(String, serde_json::Value)>,
    /// Counter to give synthesized step IDs deterministic ordering.
    id_counter: u64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct StoredStep {
    request_id: String,
    scope_parent: Option<String>,
    prev_step: Option<String>,
    kind: StepKind,
    sequence: i64,
    state: StepState,
    response_payload: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
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

    fn snapshot(&self) -> StoreSnapshot {
        let state = self.inner.lock().unwrap();
        StoreSnapshot {
            steps: state.steps.clone(),
            record_order: state.record_order.clone(),
            mark_processing_order: state.mark_processing_order.clone(),
            complete_order: state.complete_order.clone(),
            fail_order: state.fail_order.clone(),
        }
    }
}

struct StoreSnapshot {
    steps: HashMap<String, StoredStep>,
    record_order: Vec<String>,
    mark_processing_order: Vec<String>,
    complete_order: Vec<(String, serde_json::Value)>,
    fail_order: Vec<(String, serde_json::Value)>,
}

#[async_trait]
impl MultiStepStore for MockStore {
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
    ) -> Result<RecordedStep, StoreError> {
        let mut state = self.inner.lock().unwrap();
        state.id_counter += 1;
        let id = format!("step_{:03}", state.id_counter);
        let sequence = {
            let next = state.next_seq.entry(request_id.to_string()).or_insert(0);
            *next += 1;
            *next
        };
        state.steps.insert(
            id.clone(),
            StoredStep {
                request_id: request_id.to_string(),
                scope_parent: scope_parent.map(|s| s.to_string()),
                prev_step: prev_step.map(|s| s.to_string()),
                kind: descriptor.kind,
                sequence,
                state: StepState::Pending,
                response_payload: None,
                error: None,
            },
        );
        state.record_order.push(id.clone());
        Ok(RecordedStep { id, sequence })
    }

    async fn mark_step_processing(&self, step_id: &str) -> Result<(), StoreError> {
        let mut state = self.inner.lock().unwrap();
        if let Some(step) = state.steps.get_mut(step_id) {
            step.state = StepState::Processing;
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
            step.state = StepState::Completed;
            step.response_payload = Some(payload.clone());
        }
        state
            .complete_order
            .push((step_id.to_string(), payload.clone()));
        Ok(())
    }

    async fn fail_step(&self, step_id: &str, error: &serde_json::Value) -> Result<(), StoreError> {
        let mut state = self.inner.lock().unwrap();
        if let Some(step) = state.steps.get_mut(step_id) {
            step.state = StepState::Failed;
            step.error = Some(error.clone());
        }
        state.fail_order.push((step_id.to_string(), error.clone()));
        Ok(())
    }

    async fn list_chain(
        &self,
        request_id: &str,
        scope_parent: Option<&str>,
    ) -> Result<Vec<ChainStep>, StoreError> {
        let state = self.inner.lock().unwrap();
        let mut out: Vec<ChainStep> = state
            .steps
            .iter()
            .filter(|(_, step)| {
                step.request_id == request_id && step.scope_parent.as_deref() == scope_parent
            })
            .map(|(id, step)| ChainStep {
                id: id.clone(),
                kind: step.kind,
                state: step.state,
                sequence: step.sequence,
                prev_step_id: step.prev_step.clone(),
                parent_step_id: step.scope_parent.clone(),
                response_payload: step.response_payload.clone(),
                error: step.error.clone(),
            })
            .collect();
        out.sort_by_key(|s| s.sequence);
        Ok(out)
    }

    async fn assemble_response(&self, _request_id: &str) -> Result<serde_json::Value, StoreError> {
        Ok(json!({"assembled": true}))
    }
}

#[derive(Default)]
struct MockExecutor {
    inner: Mutex<ExecutorState>,
}

#[derive(Debug, Default)]
struct ExecutorState {
    model_call_invocations: Vec<String>,
    tool_call_invocations: Vec<String>,
    /// Per-step model_call result override (default = success).
    model_call_results: HashMap<String, Result<serde_json::Value, String>>,
    /// Per-step tool dispatch override (default = Executed with synthetic
    /// payload).
    tool_dispatches: HashMap<String, ToolDispatch>,
}

impl MockExecutor {
    fn new() -> Self {
        Self::default()
    }

    fn set_tool_dispatch(&self, step_id: &str, dispatch: ToolDispatch) {
        self.inner
            .lock()
            .unwrap()
            .tool_dispatches
            .insert(step_id.to_string(), dispatch);
    }

    fn fail_model_call(&self, step_id: &str, message: &str) {
        self.inner
            .lock()
            .unwrap()
            .model_call_results
            .insert(step_id.to_string(), Err(message.to_string()));
    }

    fn snapshot(&self) -> ExecutorSnapshot {
        let state = self.inner.lock().unwrap();
        ExecutorSnapshot {
            model_call_invocations: state.model_call_invocations.clone(),
            tool_call_invocations: state.tool_call_invocations.clone(),
        }
    }
}

#[derive(Debug)]
struct ExecutorSnapshot {
    model_call_invocations: Vec<String>,
    tool_call_invocations: Vec<String>,
}

#[async_trait]
impl StepExecutor for MockExecutor {
    async fn execute_model_call(
        &self,
        step_id: &str,
        _request_payload: &serde_json::Value,
    ) -> Result<serde_json::Value, ExecutorError> {
        let mut state = self.inner.lock().unwrap();
        state.model_call_invocations.push(step_id.to_string());
        match state.model_call_results.remove(step_id) {
            Some(Ok(payload)) => Ok(payload),
            Some(Err(err)) => Err(ExecutorError::ExecutionError(err)),
            None => Ok(json!({"output": format!("model:{step_id}")})),
        }
    }

    async fn dispatch_tool_call(
        &self,
        step_id: &str,
        _request_payload: &serde_json::Value,
    ) -> Result<ToolDispatch, ExecutorError> {
        let mut state = self.inner.lock().unwrap();
        state.tool_call_invocations.push(step_id.to_string());
        match state.tool_dispatches.remove(step_id) {
            Some(dispatch) => Ok(dispatch),
            None => Ok(ToolDispatch::Executed(
                json!({"tool_output": format!("tool:{step_id}")}),
            )),
        }
    }
}

fn model_call(payload: serde_json::Value) -> StepDescriptor {
    StepDescriptor {
        kind: StepKind::ModelCall,
        request_payload: payload,
    }
}

fn tool_call(payload: serde_json::Value) -> StepDescriptor {
    StepDescriptor {
        kind: StepKind::ToolCall,
        request_payload: payload,
    }
}

#[tokio::test]
async fn complete_immediately_returns_payload() {
    let store = MockStore::new();
    let executor = MockExecutor::new();
    store.script(None, vec![NextAction::Complete(json!({"final": true}))]);

    let result =
        run_response_loop(&store, &executor, "req_1", None, LoopConfig::default(), 0).await;

    assert_eq!(result.unwrap(), json!({"final": true}));
    assert!(store.snapshot().record_order.is_empty());
    assert!(executor.snapshot().model_call_invocations.is_empty());
}

#[tokio::test]
async fn fail_immediately_returns_loop_error_failed() {
    let store = MockStore::new();
    let executor = MockExecutor::new();
    store.script(None, vec![NextAction::Fail(json!({"reason": "bad"}))]);

    let result =
        run_response_loop(&store, &executor, "req_1", None, LoopConfig::default(), 0).await;
    match result {
        Err(LoopError::Failed(payload)) => assert_eq!(payload, json!({"reason": "bad"})),
        other => panic!("expected LoopError::Failed, got {:?}", other),
    }
}

#[tokio::test]
async fn single_model_call_then_complete() {
    let store = MockStore::new();
    let executor = MockExecutor::new();
    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![model_call(json!({"prompt": "hi"}))]),
            NextAction::Complete(json!({"done": true})),
        ],
    );

    let result = run_response_loop(&store, &executor, "req_1", None, LoopConfig::default(), 0)
        .await
        .unwrap();
    assert_eq!(result, json!({"done": true}));

    let snap = store.snapshot();
    assert_eq!(snap.record_order, vec!["step_001"]);
    assert_eq!(snap.mark_processing_order, vec!["step_001"]);
    assert_eq!(executor.snapshot().model_call_invocations, vec!["step_001"]);
    assert_eq!(snap.complete_order.len(), 1);
}

#[tokio::test]
async fn parallel_fan_out_runs_concurrently_and_chains_prev_step_id() {
    let store = MockStore::new();
    let executor = MockExecutor::new();
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

    run_response_loop(&store, &executor, "req_1", None, LoopConfig::default(), 0)
        .await
        .unwrap();

    let snap = store.snapshot();
    assert_eq!(snap.record_order, vec!["step_001", "step_002", "step_003"]);
    assert_eq!(snap.steps["step_001"].prev_step, None);
    assert_eq!(snap.steps["step_002"].prev_step, Some("step_001".into()));
    assert_eq!(snap.steps["step_003"].prev_step, Some("step_002".into()));
    // Sequence is allocated atomically by record_step
    assert_eq!(snap.steps["step_001"].sequence, 1);
    assert_eq!(snap.steps["step_002"].sequence, 2);
    assert_eq!(snap.steps["step_003"].sequence, 3);
    assert_eq!(executor.snapshot().tool_call_invocations.len(), 3);
    assert_eq!(snap.complete_order.len(), 3);
}

#[tokio::test]
async fn step_failure_does_not_abort_loop_and_is_recorded() {
    let store = MockStore::new();
    let executor = MockExecutor::new();
    executor.fail_model_call("step_001", "upstream broke");
    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![model_call(json!({}))]),
            // Transition function decides to recover: fire another step.
            NextAction::AppendSteps(vec![model_call(json!({}))]),
            NextAction::Complete(json!({"final": "ok"})),
        ],
    );

    run_response_loop(&store, &executor, "req_1", None, LoopConfig::default(), 0)
        .await
        .unwrap();

    let snap = store.snapshot();
    assert_eq!(snap.fail_order.len(), 1);
    assert_eq!(snap.fail_order[0].0, "step_001");
    assert_eq!(snap.complete_order.len(), 1);
    assert_eq!(snap.complete_order[0].0, "step_002");
}

#[tokio::test]
async fn max_iterations_cap_fires() {
    let store = MockStore::new();
    let executor = MockExecutor::new();
    let descriptors = vec![model_call(json!({}))];
    let mut script = Vec::new();
    for _ in 0..20 {
        script.push(NextAction::AppendSteps(descriptors.clone()));
    }
    store.script(None, script);

    let config = LoopConfig {
        max_response_step_depth: 8,
        max_response_iterations: 3,
    };
    let result = run_response_loop(&store, &executor, "req_1", None, config, 0).await;
    assert!(matches!(result, Err(LoopError::MaxIterationsExceeded)));
}

#[tokio::test]
async fn subagent_recursion_executes_sub_loop_under_correct_scope() {
    let store = MockStore::new();
    let executor = MockExecutor::new();

    // Top-level: spawn one tool_call that the executor will signal as a
    // sub-agent dispatch.
    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![tool_call(json!({"agent": "search"}))]),
            NextAction::Complete(json!({"top": "done"})),
        ],
    );
    // Sub-loop: do one model_call then complete with a known payload.
    store.script(
        Some("step_001"),
        vec![
            NextAction::AppendSteps(vec![model_call(json!({"sub": true}))]),
            NextAction::Complete(json!({"sub_result": 42})),
        ],
    );
    // Mark step_001 as a Recurse dispatch — the loop should recurse
    // rather than treat this as an Executed tool call.
    executor.set_tool_dispatch("step_001", ToolDispatch::Recurse);

    run_response_loop(&store, &executor, "req_1", None, LoopConfig::default(), 0)
        .await
        .unwrap();

    let snap = store.snapshot();
    assert_eq!(snap.steps["step_001"].kind, StepKind::ToolCall);
    assert_eq!(snap.steps["step_001"].scope_parent, None);
    assert_eq!(snap.steps["step_002"].kind, StepKind::ModelCall);
    assert_eq!(snap.steps["step_002"].scope_parent, Some("step_001".into()));
    // step_001 (the sub-agent dispatch) is completed with the sub-loop's
    // final payload.
    let (sub_id, sub_payload) = &snap.complete_order[1];
    assert_eq!(sub_id, "step_001");
    assert_eq!(sub_payload, &json!({"sub_result": 42}));
}

#[tokio::test]
async fn max_depth_cap_fires_inside_subagent_recursion() {
    let store = MockStore::new();
    let executor = MockExecutor::new();

    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![tool_call(json!({"agent": "lvl1"}))]),
            NextAction::Complete(json!({"top": "done"})),
        ],
    );
    store.script(
        Some("step_001"),
        vec![NextAction::AppendSteps(vec![tool_call(
            json!({"agent": "lvl2"}),
        )])],
    );
    executor.set_tool_dispatch("step_001", ToolDispatch::Recurse);
    executor.set_tool_dispatch("step_002", ToolDispatch::Recurse);

    let config = LoopConfig {
        max_response_step_depth: 1,
        max_response_iterations: 10,
    };
    let _ = run_response_loop(&store, &executor, "req_1", None, config, 0).await;

    let snap = store.snapshot();
    let failed: Vec<_> = snap
        .fail_order
        .iter()
        .filter(|(_, payload)| payload["type"] == "max_depth_exceeded")
        .collect();
    assert!(
        !failed.is_empty(),
        "expected a max_depth_exceeded fail_step, got {:?}",
        snap.fail_order
    );
}

#[tokio::test]
async fn empty_action_returns_empty_action_error() {
    let store = MockStore::new();
    let executor = MockExecutor::new();
    store.script(None, vec![NextAction::AppendSteps(vec![])]);

    let result =
        run_response_loop(&store, &executor, "req_1", None, LoopConfig::default(), 0).await;
    assert!(matches!(result, Err(LoopError::EmptyAction)));
}

#[tokio::test]
async fn store_error_in_next_action_propagates() {
    let store = MockStore::new();
    let executor = MockExecutor::new();
    // No script for top-level — next_action_for returns StorageError.

    let result =
        run_response_loop(&store, &executor, "req_1", None, LoopConfig::default(), 0).await;
    assert!(matches!(result, Err(LoopError::Store(_))));
}

#[tokio::test]
async fn list_chain_observes_persisted_steps() {
    // Verifies the storage trait is self-contained: list_chain returns
    // the chain that was just recorded, scoped to (request_id, parent).
    let store = MockStore::new();
    let executor = MockExecutor::new();
    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![
                model_call(json!({"a": 1})),
                model_call(json!({"a": 2})),
            ]),
            NextAction::Complete(json!({})),
        ],
    );

    run_response_loop(&store, &executor, "req_1", None, LoopConfig::default(), 0)
        .await
        .unwrap();

    let chain = store.list_chain("req_1", None).await.unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].sequence, 1);
    assert_eq!(chain[1].sequence, 2);
    assert!(matches!(chain[0].state, StepState::Completed));
    assert_eq!(chain[1].prev_step_id.as_deref(), Some(&*chain[0].id));
}
