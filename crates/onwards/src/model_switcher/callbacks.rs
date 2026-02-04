//! Callbacks for model wake/sleep operations
//!
//! This module defines the callback trait that abstracts the underlying
//! model serving infrastructure (e.g., vLLM, TGI, custom).

use async_trait::async_trait;
use std::fmt;
use thiserror::Error;

/// Sleep level for hibernating models
///
/// Different levels trade off wake time vs memory usage:
/// - L1: Offload to CPU RAM (fastest wake, uses RAM)
/// - L2: Discard weights (slower wake, minimal RAM)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SleepLevel {
    /// Level 1: Offload weights to CPU RAM
    /// - Fast wake time (~0.1-0.8s for small models, ~3-6s for large)
    /// - Requires CPU RAM to store model weights
    #[default]
    L1,
    /// Level 2: Discard weights entirely
    /// - Slower wake time (~0.8-2.6s for small models)
    /// - Minimal CPU RAM usage
    L2,
}

impl SleepLevel {
    /// Get the numeric level (1 or 2)
    pub fn as_u8(&self) -> u8 {
        match self {
            SleepLevel::L1 => 1,
            SleepLevel::L2 => 2,
        }
    }
}

impl fmt::Display for SleepLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SleepLevel::L1 => write!(f, "L1"),
            SleepLevel::L2 => write!(f, "L2"),
        }
    }
}

/// Errors that can occur during model switching
#[derive(Error, Debug)]
pub enum SwitchError {
    /// Model not found in the switch group
    #[error("model not found: {0}")]
    ModelNotFound(String),

    /// Failed to wake model
    #[error("failed to wake model '{model}': {reason}")]
    WakeFailed { model: String, reason: String },

    /// Failed to put model to sleep
    #[error("failed to sleep model '{model}': {reason}")]
    SleepFailed { model: String, reason: String },

    /// Model is not ready for requests
    #[error("model '{0}' is not ready")]
    NotReady(String),

    /// Request timed out waiting for model switch
    #[error("request timed out waiting for model switch")]
    Timeout,

    /// Internal error
    #[error("internal error: {0}")]
    Internal(String),

    /// Callback error from underlying implementation
    #[error("callback error: {0}")]
    Callback(#[from] Box<dyn std::error::Error + Send + Sync>),
}

/// Callback trait for model wake/sleep operations
///
/// Implement this trait to integrate with your model serving infrastructure.
/// The model switcher uses these callbacks to coordinate model state.
///
/// # Implementation Notes
///
/// - All methods are async and should handle their own timeouts
/// - Implementations should be thread-safe (Send + Sync)
/// - Wake/sleep operations may be called concurrently for different models
///
/// # Example: vLLM Implementation
///
/// ```ignore
/// struct VllmCallbacks {
///     endpoints: HashMap<String, VllmEndpoint>,
/// }
///
/// #[async_trait]
/// impl ModelSwitcherCallbacks for VllmCallbacks {
///     async fn wake_model(&self, model: &str) -> Result<(), SwitchError> {
///         let endpoint = self.endpoints.get(model)
///             .ok_or_else(|| SwitchError::ModelNotFound(model.to_string()))?;
///
///         // POST /wake_up
///         endpoint.post("/wake_up").await?;
///
///         // For L2, also reload weights
///         if endpoint.sleep_level == SleepLevel::L2 {
///             endpoint.post("/collective_rpc", json!({"method": "reload_weights"})).await?;
///             endpoint.post("/reset_prefix_cache").await?;
///         }
///
///         Ok(())
///     }
///
///     // ... other methods
/// }
/// ```
#[async_trait]
pub trait ModelSwitcherCallbacks: Send + Sync {
    /// Wake a model from sleep, making it ready to handle requests.
    ///
    /// This should:
    /// 1. Send wake signal to the model server
    /// 2. Wait for model to be ready (optional - can return early)
    /// 3. For L2 sleep, reload weights and reset caches
    ///
    /// # Arguments
    /// * `model` - The model identifier to wake
    ///
    /// # Errors
    /// * `SwitchError::ModelNotFound` - Model not registered
    /// * `SwitchError::WakeFailed` - Wake operation failed
    async fn wake_model(&self, model: &str) -> Result<(), SwitchError>;

    /// Put a model to sleep at the specified level.
    ///
    /// # Arguments
    /// * `model` - The model identifier to sleep
    /// * `level` - Sleep level (L1 or L2)
    ///
    /// # Errors
    /// * `SwitchError::ModelNotFound` - Model not registered
    /// * `SwitchError::SleepFailed` - Sleep operation failed
    async fn sleep_model(&self, model: &str, level: SleepLevel) -> Result<(), SwitchError>;

    /// Check if a model is ready to handle requests.
    ///
    /// This is called after waking to confirm the model is ready.
    /// Implementations can poll this or return immediately if wake blocks until ready.
    ///
    /// # Arguments
    /// * `model` - The model identifier to check
    ///
    /// # Returns
    /// * `true` if model is ready for requests
    /// * `false` if model is still warming up or not available
    async fn is_ready(&self, model: &str) -> bool;

    /// Get all registered models in this switch group.
    ///
    /// Returns the list of model identifiers that can be switched between.
    fn registered_models(&self) -> Vec<String>;

    /// Optional: Perform any initialization after creating the switcher.
    ///
    /// This is called once when the switcher is created.
    /// Default implementation does nothing.
    async fn initialize(&self) -> Result<(), SwitchError> {
        Ok(())
    }

    /// Optional: Perform cleanup when the switcher is dropped.
    ///
    /// Default implementation does nothing.
    async fn shutdown(&self) -> Result<(), SwitchError> {
        Ok(())
    }
}

/// A no-op callbacks implementation for testing
#[derive(Debug, Clone, Default)]
pub struct NoOpCallbacks {
    models: Vec<String>,
}

impl NoOpCallbacks {
    pub fn new(models: Vec<String>) -> Self {
        Self { models }
    }
}

#[async_trait]
impl ModelSwitcherCallbacks for NoOpCallbacks {
    async fn wake_model(&self, model: &str) -> Result<(), SwitchError> {
        if !self.models.contains(&model.to_string()) {
            return Err(SwitchError::ModelNotFound(model.to_string()));
        }
        Ok(())
    }

    async fn sleep_model(&self, model: &str, _level: SleepLevel) -> Result<(), SwitchError> {
        if !self.models.contains(&model.to_string()) {
            return Err(SwitchError::ModelNotFound(model.to_string()));
        }
        Ok(())
    }

    async fn is_ready(&self, model: &str) -> bool {
        self.models.contains(&model.to_string())
    }

    fn registered_models(&self) -> Vec<String> {
        self.models.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sleep_level_display() {
        assert_eq!(format!("{}", SleepLevel::L1), "L1");
        assert_eq!(format!("{}", SleepLevel::L2), "L2");
    }

    #[test]
    fn test_sleep_level_as_u8() {
        assert_eq!(SleepLevel::L1.as_u8(), 1);
        assert_eq!(SleepLevel::L2.as_u8(), 2);
    }

    #[tokio::test]
    async fn test_noop_callbacks() {
        let callbacks = NoOpCallbacks::new(vec!["model-a".to_string(), "model-b".to_string()]);

        // Should succeed for registered models
        assert!(callbacks.wake_model("model-a").await.is_ok());
        assert!(callbacks.sleep_model("model-b", SleepLevel::L1).await.is_ok());
        assert!(callbacks.is_ready("model-a").await);

        // Should fail for unregistered models
        assert!(matches!(
            callbacks.wake_model("model-c").await,
            Err(SwitchError::ModelNotFound(_))
        ));
        assert!(!callbacks.is_ready("model-c").await);
    }
}
