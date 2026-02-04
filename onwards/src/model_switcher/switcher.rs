//! Core model switcher implementation
//!
//! The switcher coordinates request queuing and model wake/sleep transitions.

use super::callbacks::{ModelSwitcherCallbacks, SwitchError};
use super::policy::{PolicyContext, PolicyDecision, SwitchContext, SwitchPolicy};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, RwLock, oneshot};
use tracing::{debug, error, info, trace, warn};

/// Configuration for the model switcher
#[derive(Debug, Clone)]
pub struct ModelSwitcherConfig {
    /// Initial model to wake on startup (if any)
    pub initial_model: Option<String>,
    /// Whether to wake initial model during initialization
    pub wake_on_init: bool,
}

impl Default for ModelSwitcherConfig {
    fn default() -> Self {
        Self {
            initial_model: None,
            wake_on_init: true,
        }
    }
}

/// State of the model switcher
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitcherState {
    /// No model is currently active
    Idle,
    /// A model is awake and ready for requests
    Active { model: String },
    /// Currently switching from one model to another
    Switching { from: Option<String>, to: String },
}

/// A pending request waiting for its model to be ready
pub struct PendingRequest {
    /// The model this request is for
    pub model: String,
    /// When the request was queued
    pub queued_at: Instant,
    /// Channel to signal when the model is ready
    ready_tx: oneshot::Sender<Result<(), SwitchError>>,
}

impl std::fmt::Debug for PendingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingRequest")
            .field("model", &self.model)
            .field("queued_at", &self.queued_at)
            .finish()
    }
}

/// Per-model state tracking
#[derive(Debug)]
struct ModelState {
    /// Number of in-flight requests for this model
    in_flight: AtomicUsize,
    /// Queue of pending requests waiting for this model
    pending: Mutex<Vec<PendingRequest>>,
    /// Notifier for when in-flight count changes
    in_flight_changed: Notify,
}

impl Default for ModelState {
    fn default() -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
            pending: Mutex::new(Vec::new()),
            in_flight_changed: Notify::new(),
        }
    }
}

/// Shared inner state for the model switcher
struct SwitcherInner<C: ModelSwitcherCallbacks, P: SwitchPolicy> {
    /// Callbacks for wake/sleep operations
    callbacks: C,
    /// Policy for switch decisions
    policy: P,
    /// Current state of the switcher
    state: RwLock<SwitcherState>,
    /// Per-model state (in-flight counts, pending queues)
    model_states: HashMap<String, Arc<ModelState>>,
    /// Lock for serializing switch operations
    switch_lock: Mutex<()>,
    /// Configuration
    config: ModelSwitcherConfig,
}

/// The core model switcher
///
/// Coordinates request queuing and model wake/sleep transitions.
///
/// This type is cheaply cloneable via `Arc`.
pub struct ModelSwitcher<C, P>
where
    C: ModelSwitcherCallbacks + 'static,
    P: SwitchPolicy + 'static,
{
    inner: Arc<SwitcherInner<C, P>>,
}

// Manual Clone implementation that doesn't require C and P to be Clone
impl<C, P> Clone for ModelSwitcher<C, P>
where
    C: ModelSwitcherCallbacks + 'static,
    P: SwitchPolicy + 'static,
{
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<C, P> ModelSwitcher<C, P>
where
    C: ModelSwitcherCallbacks + 'static,
    P: SwitchPolicy + 'static,
{
    /// Create a new model switcher
    pub fn new(callbacks: C, policy: P, config: ModelSwitcherConfig) -> Self {
        // Initialize model states for all registered models
        let model_states: HashMap<String, Arc<ModelState>> = callbacks
            .registered_models()
            .into_iter()
            .map(|model| (model, Arc::new(ModelState::default())))
            .collect();

        Self {
            inner: Arc::new(SwitcherInner {
                callbacks,
                policy,
                state: RwLock::new(SwitcherState::Idle),
                model_states,
                switch_lock: Mutex::new(()),
                config,
            }),
        }
    }

    /// Initialize the switcher (wake initial model if configured)
    pub async fn initialize(&self) -> Result<(), SwitchError> {
        self.inner.callbacks.initialize().await?;

        if self.inner.config.wake_on_init {
            if let Some(ref initial_model) = self.inner.config.initial_model {
                info!(model = %initial_model, "Waking initial model");
                self.ensure_model_ready(initial_model).await?;
            }
        }

        Ok(())
    }

    /// Get the current state of the switcher
    pub async fn state(&self) -> SwitcherState {
        self.inner.state.read().await.clone()
    }

    /// Get the currently active model (if any)
    pub async fn active_model(&self) -> Option<String> {
        match &*self.inner.state.read().await {
            SwitcherState::Active { model } => Some(model.clone()),
            _ => None,
        }
    }

    /// Check if a model is registered with this switcher
    pub fn is_registered(&self, model: &str) -> bool {
        self.inner.model_states.contains_key(model)
    }

    /// Get the number of in-flight requests for a model
    pub fn in_flight_count(&self, model: &str) -> usize {
        self.inner
            .model_states
            .get(model)
            .map(|s| s.in_flight.load(Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// Get the number of pending requests for a model
    pub async fn pending_count(&self, model: &str) -> usize {
        if let Some(state) = self.inner.model_states.get(model) {
            state.pending.lock().await.len()
        } else {
            0
        }
    }

    /// Ensure a model is ready for requests, switching if necessary
    ///
    /// This is the main entry point for request handling. It will:
    /// 1. Return immediately if the model is already active
    /// 2. Queue the request and trigger a switch if needed
    /// 3. Wait for the switch to complete (up to timeout)
    pub async fn ensure_model_ready(&self, model: &str) -> Result<(), SwitchError> {
        // Validate model is registered
        let model_state = self
            .inner
            .model_states
            .get(model)
            .ok_or_else(|| SwitchError::ModelNotFound(model.to_string()))?;

        // Fast path: model is already active
        {
            let state = self.inner.state.read().await;
            if let SwitcherState::Active { model: active } = &*state {
                if active == model {
                    trace!(model = %model, "Model already active");
                    return Ok(());
                }
            }
        }

        // Need to queue and potentially switch
        let (ready_tx, ready_rx) = oneshot::channel();
        let pending = PendingRequest {
            model: model.to_string(),
            queued_at: Instant::now(),
            ready_tx,
        };

        // Add to queue
        {
            let mut queue = model_state.pending.lock().await;
            queue.push(pending);
            debug!(model = %model, queue_depth = queue.len(), "Request queued");
        }

        // Check policy for switch decision
        self.maybe_trigger_switch(model).await;

        // Wait for ready signal (with timeout)
        let timeout = self.inner.policy.request_timeout();
        match tokio::time::timeout(timeout, ready_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                // Channel closed - switcher shutting down
                Err(SwitchError::Internal("switcher shutdown".to_string()))
            }
            Err(_) => {
                // Timeout
                warn!(model = %model, timeout = ?timeout, "Request timed out waiting for model");
                Err(SwitchError::Timeout)
            }
        }
    }

    /// Acquire an in-flight guard for a model
    ///
    /// Call this when starting to process a request, and drop the guard when done.
    /// This tracks in-flight requests for the drain-before-switch logic.
    pub fn acquire_in_flight(&self, model: &str) -> Option<InFlightGuard> {
        let model_state = self.inner.model_states.get(model)?;
        model_state.in_flight.fetch_add(1, Ordering::SeqCst);
        Some(InFlightGuard {
            model_state: Arc::clone(model_state),
        })
    }

    /// Check policy and maybe trigger a switch
    async fn maybe_trigger_switch(&self, target_model: &str) {
        let model_state = match self.inner.model_states.get(target_model) {
            Some(s) => s,
            None => return,
        };

        // Build policy context
        let ctx = {
            let state = self.inner.state.read().await;
            let queue = model_state.pending.lock().await;

            let oldest_waiting = queue
                .first()
                .map(|p| p.queued_at.elapsed())
                .unwrap_or(Duration::ZERO);

            let (active_model, active_in_flight) = match &*state {
                SwitcherState::Active { model } => {
                    let in_flight = self.in_flight_count(model);
                    (Some(model.clone()), in_flight)
                }
                _ => (None, 0),
            };

            PolicyContext {
                target_model: target_model.to_string(),
                active_model,
                target_queue_depth: queue.len(),
                oldest_waiting,
                active_in_flight,
            }
        };

        // Already switching to this model?
        {
            let state = self.inner.state.read().await;
            if let SwitcherState::Switching { to, .. } = &*state {
                if to == target_model {
                    trace!(model = %target_model, "Already switching to this model");
                    return;
                }
            }
        }

        // Ask policy and get decision
        let decision = self.inner.policy.on_pending_request(&ctx).await;

        // Handle decision (no more borrows of self needed for policy call)
        match decision {
            PolicyDecision::SwitchNow => {
                debug!(model = %target_model, "Policy decided: switch now");
                self.do_switch(target_model).await;
            }
            PolicyDecision::Defer(future) => {
                debug!(model = %target_model, "Policy decided: defer switch");
                // Clone self and target for the spawned task
                let switcher_clone = self.clone();
                let target_owned = target_model.to_string();
                // Spawn task to wait and then do the switch
                tokio::spawn(async move {
                    future.await;
                    // After defer completes, do the switch directly
                    switcher_clone.do_switch(&target_owned).await;
                });
            }
        }
    }

    /// Perform the actual model switch
    async fn do_switch(&self, target_model: &str) {
        // Acquire switch lock (serialize switches)
        let _switch_guard = self.inner.switch_lock.lock().await;

        // Double-check state (may have changed while waiting for lock)
        {
            let state = self.inner.state.read().await;
            match &*state {
                SwitcherState::Active { model } if model == target_model => {
                    debug!(model = %target_model, "Model became active while waiting for lock");
                    self.notify_pending(target_model, Ok(())).await;
                    return;
                }
                SwitcherState::Switching { to, .. } if to == target_model => {
                    debug!(model = %target_model, "Already switching to this model");
                    return;
                }
                _ => {}
            }
        }

        // Get current active model
        let from_model = {
            let state = self.inner.state.read().await;
            match &*state {
                SwitcherState::Active { model } => Some(model.clone()),
                _ => None,
            }
        };

        // Update state to switching
        {
            let mut state = self.inner.state.write().await;
            *state = SwitcherState::Switching {
                from: from_model.clone(),
                to: target_model.to_string(),
            };
        }

        info!(
            from = ?from_model,
            to = %target_model,
            "Starting model switch"
        );

        // Prepare for switch (policy can wait for in-flight drain)
        if let Some(ref from) = from_model {
            if let Some(from_state) = self.inner.model_states.get(from) {
                let in_flight_drained = Arc::new(Notify::new());
                let from_state_clone = Arc::clone(from_state);

                let mut switch_ctx = SwitchContext::new(
                    from_model.clone(),
                    target_model.to_string(),
                    Arc::clone(&in_flight_drained),
                    Box::new(move || from_state_clone.in_flight.load(Ordering::SeqCst)),
                );

                self.inner.policy.prepare_switch(&mut switch_ctx).await;
            }
        }

        // Put old model to sleep (if any)
        if let Some(ref from) = from_model {
            let sleep_level = self.inner.policy.sleep_level();
            debug!(model = %from, level = %sleep_level, "Putting model to sleep");

            if let Err(e) = self.inner.callbacks.sleep_model(from, sleep_level).await {
                error!(model = %from, error = %e, "Failed to sleep model");
                // Continue anyway - try to wake the target
            }
        }

        // Wake new model
        debug!(model = %target_model, "Waking model");
        match self.inner.callbacks.wake_model(target_model).await {
            Ok(()) => {
                // Wait for model to be ready
                let mut ready = false;
                for attempt in 0..10 {
                    if self.inner.callbacks.is_ready(target_model).await {
                        ready = true;
                        break;
                    }
                    debug!(model = %target_model, attempt, "Waiting for model to be ready");
                    tokio::time::sleep(Duration::from_millis(100 * (attempt + 1) as u64)).await;
                }

                if ready {
                    info!(model = %target_model, "Model is now active");
                    {
                        let mut state = self.inner.state.write().await;
                        *state = SwitcherState::Active {
                            model: target_model.to_string(),
                        };
                    }
                    self.notify_pending(target_model, Ok(())).await;
                } else {
                    error!(model = %target_model, "Model failed to become ready");
                    {
                        let mut state = self.inner.state.write().await;
                        *state = SwitcherState::Idle;
                    }
                    self.notify_pending(
                        target_model,
                        Err(SwitchError::NotReady(target_model.to_string())),
                    )
                    .await;
                }
            }
            Err(e) => {
                error!(model = %target_model, error = %e, "Failed to wake model");
                {
                    let mut state = self.inner.state.write().await;
                    *state = SwitcherState::Idle;
                }
                self.notify_pending(target_model, Err(e)).await;
            }
        }
    }

    /// Notify all pending requests for a model
    async fn notify_pending(&self, model: &str, result: Result<(), SwitchError>) {
        if let Some(model_state) = self.inner.model_states.get(model) {
            let mut queue = model_state.pending.lock().await;
            let count = queue.len();

            for pending in queue.drain(..) {
                let r = match &result {
                    Ok(()) => Ok(()),
                    Err(e) => Err(SwitchError::Internal(e.to_string())),
                };
                let _ = pending.ready_tx.send(r);
            }

            debug!(model = %model, count, "Notified pending requests");
        }
    }
}

/// Guard that tracks an in-flight request
///
/// When dropped, decrements the in-flight count and notifies waiters.
pub struct InFlightGuard {
    model_state: Arc<ModelState>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let prev = self.model_state.in_flight.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 {
            // Was last in-flight request
            self.model_state.in_flight_changed.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_switcher::callbacks::NoOpCallbacks;
    use crate::model_switcher::policy::FifoPolicy;

    #[tokio::test]
    async fn test_switcher_initialization() {
        let callbacks = NoOpCallbacks::new(vec!["model-a".to_string(), "model-b".to_string()]);
        let policy = FifoPolicy::default();
        let config = ModelSwitcherConfig::default();

        let switcher = ModelSwitcher::new(callbacks, policy, config);

        assert!(switcher.is_registered("model-a"));
        assert!(switcher.is_registered("model-b"));
        assert!(!switcher.is_registered("model-c"));

        assert_eq!(switcher.state().await, SwitcherState::Idle);
    }

    #[tokio::test]
    async fn test_ensure_model_ready() {
        let callbacks = NoOpCallbacks::new(vec!["model-a".to_string(), "model-b".to_string()]);
        let policy = FifoPolicy::default();
        let config = ModelSwitcherConfig::default();

        let switcher = ModelSwitcher::new(callbacks, policy, config);

        // First call should trigger switch
        let result = switcher.ensure_model_ready("model-a").await;
        assert!(result.is_ok());

        assert_eq!(
            switcher.state().await,
            SwitcherState::Active { model: "model-a".to_string() }
        );
    }

    #[tokio::test]
    async fn test_model_not_found() {
        let callbacks = NoOpCallbacks::new(vec!["model-a".to_string()]);
        let policy = FifoPolicy::default();
        let config = ModelSwitcherConfig::default();

        let switcher = ModelSwitcher::new(callbacks, policy, config);

        let result = switcher.ensure_model_ready("model-unknown").await;
        assert!(matches!(result, Err(SwitchError::ModelNotFound(_))));
    }

    #[tokio::test]
    async fn test_in_flight_tracking() {
        let callbacks = NoOpCallbacks::new(vec!["model-a".to_string()]);
        let policy = FifoPolicy::default();
        let config = ModelSwitcherConfig::default();

        let switcher = ModelSwitcher::new(callbacks, policy, config);

        assert_eq!(switcher.in_flight_count("model-a"), 0);

        {
            let _guard1 = switcher.acquire_in_flight("model-a");
            assert_eq!(switcher.in_flight_count("model-a"), 1);

            {
                let _guard2 = switcher.acquire_in_flight("model-a");
                assert_eq!(switcher.in_flight_count("model-a"), 2);
            }

            assert_eq!(switcher.in_flight_count("model-a"), 1);
        }

        assert_eq!(switcher.in_flight_count("model-a"), 0);
    }

    #[tokio::test]
    async fn test_switch_between_models() {
        let callbacks = NoOpCallbacks::new(vec!["model-a".to_string(), "model-b".to_string()]);
        let policy = FifoPolicy::default();
        let config = ModelSwitcherConfig::default();

        let switcher = ModelSwitcher::new(callbacks, policy, config);

        // Switch to model-a
        switcher.ensure_model_ready("model-a").await.unwrap();
        assert_eq!(
            switcher.state().await,
            SwitcherState::Active { model: "model-a".to_string() }
        );

        // Switch to model-b
        switcher.ensure_model_ready("model-b").await.unwrap();
        assert_eq!(
            switcher.state().await,
            SwitcherState::Active { model: "model-b".to_string() }
        );

        // Switch back to model-a
        switcher.ensure_model_ready("model-a").await.unwrap();
        assert_eq!(
            switcher.state().await,
            SwitcherState::Active { model: "model-a".to_string() }
        );
    }

    #[tokio::test]
    async fn test_no_switch_for_active_model() {
        let callbacks = NoOpCallbacks::new(vec!["model-a".to_string()]);
        let policy = FifoPolicy::default();
        let config = ModelSwitcherConfig::default();

        let switcher = ModelSwitcher::new(callbacks, policy, config);

        // First switch
        switcher.ensure_model_ready("model-a").await.unwrap();

        // Second call should be fast (no switch needed)
        let start = Instant::now();
        switcher.ensure_model_ready("model-a").await.unwrap();
        let elapsed = start.elapsed();

        // Should be very fast since no actual switch happens
        assert!(elapsed < Duration::from_millis(10));
    }
}
