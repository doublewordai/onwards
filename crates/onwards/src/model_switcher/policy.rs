//! Switch policies for model switching decisions
//!
//! Policies determine WHEN to trigger a model switch and HOW to prepare for it.

use super::SleepLevel;
use async_trait::async_trait;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// Context provided to policies when making switch decisions
#[derive(Debug, Clone)]
pub struct PolicyContext {
    /// The model that the pending request is for
    pub target_model: String,
    /// Currently active/awake model (if any)
    pub active_model: Option<String>,
    /// Number of requests queued for the target model
    pub target_queue_depth: usize,
    /// How long the oldest request has been waiting
    pub oldest_waiting: Duration,
    /// Number of in-flight requests for the active model
    pub active_in_flight: usize,
}

/// Context provided when preparing for a switch
pub struct SwitchContext {
    /// The model being switched FROM (will be put to sleep)
    pub from_model: Option<String>,
    /// The model being switched TO (will be woken)
    pub to_model: String,
    /// Notifier for when in-flight requests complete
    in_flight_drained: Arc<Notify>,
    /// Current in-flight count accessor
    get_in_flight: Box<dyn Fn() -> usize + Send + Sync>,
}

impl SwitchContext {
    /// Create a new switch context
    pub fn new(
        from_model: Option<String>,
        to_model: String,
        in_flight_drained: Arc<Notify>,
        get_in_flight: Box<dyn Fn() -> usize + Send + Sync>,
    ) -> Self {
        Self {
            from_model,
            to_model,
            in_flight_drained,
            get_in_flight,
        }
    }

    /// Wait for all in-flight requests to complete
    pub async fn wait_for_in_flight(&self) {
        while (self.get_in_flight)() > 0 {
            self.in_flight_drained.notified().await;
        }
    }

    /// Get current in-flight count
    pub fn in_flight_count(&self) -> usize {
        (self.get_in_flight)()
    }
}

impl std::fmt::Debug for SwitchContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwitchContext")
            .field("from_model", &self.from_model)
            .field("to_model", &self.to_model)
            .field("in_flight_count", &(self.get_in_flight)())
            .finish()
    }
}

/// Decision returned by policy when a request arrives for an inactive model
pub enum PolicyDecision {
    /// Trigger model switch immediately
    SwitchNow,
    /// Defer the switch - policy will signal when ready via the returned future
    Defer(Pin<Box<dyn Future<Output = ()> + Send + 'static>>),
}

impl std::fmt::Debug for PolicyDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyDecision::SwitchNow => write!(f, "SwitchNow"),
            PolicyDecision::Defer(_) => write!(f, "Defer(...)"),
        }
    }
}

/// Policy trait for controlling model switching behavior
///
/// Implement this trait to customize:
/// - When to trigger a model switch (immediately, after batching, etc.)
/// - How to prepare for a switch (drain in-flight, wait for conditions, etc.)
/// - Sleep level and request timeout
#[async_trait]
pub trait SwitchPolicy: Send + Sync {
    /// Called when a request arrives for a model that isn't currently active.
    ///
    /// Return `SwitchNow` to trigger an immediate switch, or `Defer` with a future
    /// that resolves when the switch should happen (e.g., after batching timeout).
    async fn on_pending_request(&self, ctx: &PolicyContext) -> PolicyDecision;

    /// Called before switching models. Can wait for conditions like in-flight drain.
    ///
    /// The default implementation does nothing (no waiting).
    async fn prepare_switch(&self, ctx: &mut SwitchContext) {
        let _ = ctx;
    }

    /// The sleep level to use when hibernating a model.
    fn sleep_level(&self) -> SleepLevel;

    /// Maximum time a request should wait in queue before timing out.
    fn request_timeout(&self) -> Duration;
}

/// FIFO (First-In-First-Out) policy - switch immediately on first queued request
///
/// This is the simplest policy: as soon as a request arrives for an inactive model,
/// trigger a switch. Optionally drain in-flight requests before switching.
#[derive(Debug, Clone)]
pub struct FifoPolicy {
    sleep_level: SleepLevel,
    request_timeout: Duration,
    drain_before_switch: bool,
}

impl FifoPolicy {
    /// Create a new FIFO policy
    ///
    /// # Arguments
    /// * `sleep_level` - Level to use when putting models to sleep
    /// * `request_timeout` - Max time requests wait in queue
    /// * `drain_before_switch` - If true, wait for in-flight requests to complete before switching
    pub fn new(sleep_level: SleepLevel, request_timeout: Duration, drain_before_switch: bool) -> Self {
        Self {
            sleep_level,
            request_timeout,
            drain_before_switch,
        }
    }

    /// Create with default settings (L1 sleep, 30s timeout, drain before switch)
    pub fn default_config() -> Self {
        Self::new(SleepLevel::L1, Duration::from_secs(30), true)
    }
}

impl Default for FifoPolicy {
    fn default() -> Self {
        Self::default_config()
    }
}

#[async_trait]
impl SwitchPolicy for FifoPolicy {
    async fn on_pending_request(&self, _ctx: &PolicyContext) -> PolicyDecision {
        // FIFO: always switch immediately
        PolicyDecision::SwitchNow
    }

    async fn prepare_switch(&self, ctx: &mut SwitchContext) {
        if self.drain_before_switch {
            ctx.wait_for_in_flight().await;
        }
    }

    fn sleep_level(&self) -> SleepLevel {
        self.sleep_level
    }

    fn request_timeout(&self) -> Duration {
        self.request_timeout
    }
}

/// Batching policy - wait for batch conditions before switching
///
/// This policy batches requests before triggering a switch, reducing the number
/// of model switches at the cost of some latency for individual requests.
#[derive(Debug, Clone)]
pub struct BatchingPolicy {
    sleep_level: SleepLevel,
    request_timeout: Duration,
    drain_before_switch: bool,
    /// Minimum requests before triggering switch
    min_batch_size: usize,
    /// Maximum time to wait for batch to fill
    max_batch_wait: Duration,
}

impl BatchingPolicy {
    /// Create a new batching policy
    pub fn new(
        sleep_level: SleepLevel,
        request_timeout: Duration,
        drain_before_switch: bool,
        min_batch_size: usize,
        max_batch_wait: Duration,
    ) -> Self {
        Self {
            sleep_level,
            request_timeout,
            drain_before_switch,
            min_batch_size,
            max_batch_wait,
        }
    }
}

#[async_trait]
impl SwitchPolicy for BatchingPolicy {
    async fn on_pending_request(&self, ctx: &PolicyContext) -> PolicyDecision {
        // Switch if we've hit the batch size
        if ctx.target_queue_depth >= self.min_batch_size {
            return PolicyDecision::SwitchNow;
        }

        // Switch if we've waited long enough
        if ctx.oldest_waiting >= self.max_batch_wait {
            return PolicyDecision::SwitchNow;
        }

        // Otherwise defer - wait for max_batch_wait
        let remaining_wait = self.max_batch_wait.saturating_sub(ctx.oldest_waiting);
        PolicyDecision::Defer(Box::pin(tokio::time::sleep(remaining_wait)))
    }

    async fn prepare_switch(&self, ctx: &mut SwitchContext) {
        if self.drain_before_switch {
            ctx.wait_for_in_flight().await;
        }
    }

    fn sleep_level(&self) -> SleepLevel {
        self.sleep_level
    }

    fn request_timeout(&self) -> Duration {
        self.request_timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_fifo_always_switches() {
        let policy = FifoPolicy::default();
        let ctx = PolicyContext {
            target_model: "model-b".to_string(),
            active_model: Some("model-a".to_string()),
            target_queue_depth: 1,
            oldest_waiting: Duration::ZERO,
            active_in_flight: 0,
        };

        match policy.on_pending_request(&ctx).await {
            PolicyDecision::SwitchNow => {}
            PolicyDecision::Defer(_) => panic!("FIFO should always return SwitchNow"),
        }
    }

    #[tokio::test]
    async fn test_batching_switches_at_threshold() {
        let policy = BatchingPolicy::new(
            SleepLevel::L1,
            Duration::from_secs(30),
            true,
            5, // min_batch_size
            Duration::from_secs(10),
        );

        // Below threshold - should defer
        let ctx = PolicyContext {
            target_model: "model-b".to_string(),
            active_model: Some("model-a".to_string()),
            target_queue_depth: 3,
            oldest_waiting: Duration::ZERO,
            active_in_flight: 0,
        };

        match policy.on_pending_request(&ctx).await {
            PolicyDecision::SwitchNow => panic!("Should defer when below threshold"),
            PolicyDecision::Defer(_) => {}
        }

        // At threshold - should switch
        let ctx = PolicyContext {
            target_model: "model-b".to_string(),
            active_model: Some("model-a".to_string()),
            target_queue_depth: 5,
            oldest_waiting: Duration::ZERO,
            active_in_flight: 0,
        };

        match policy.on_pending_request(&ctx).await {
            PolicyDecision::SwitchNow => {}
            PolicyDecision::Defer(_) => panic!("Should switch at threshold"),
        }
    }

    #[tokio::test]
    async fn test_batching_switches_on_timeout() {
        let policy = BatchingPolicy::new(
            SleepLevel::L1,
            Duration::from_secs(30),
            true,
            5,
            Duration::from_secs(10),
        );

        // Below threshold but past wait time
        let ctx = PolicyContext {
            target_model: "model-b".to_string(),
            active_model: Some("model-a".to_string()),
            target_queue_depth: 2,
            oldest_waiting: Duration::from_secs(15),
            active_in_flight: 0,
        };

        match policy.on_pending_request(&ctx).await {
            PolicyDecision::SwitchNow => {}
            PolicyDecision::Defer(_) => panic!("Should switch when past wait time"),
        }
    }
}
