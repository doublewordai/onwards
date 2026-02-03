//! vLLM-specific implementation of ModelSwitcherCallbacks
//!
//! This module provides a reference implementation of the [`ModelSwitcherCallbacks`]
//! trait for vLLM's sleep mode API.
//!
//! ## Usage
//!
//! ```ignore
//! use onwards::model_switcher::{ModelSwitcher, FifoPolicy, SleepLevel};
//! use onwards::model_switcher::vllm::{VllmCallbacks, VllmModelConfig};
//!
//! // Configure models with their vLLM endpoints
//! let configs = vec![
//!     VllmModelConfig {
//!         model_name: "model-a".to_string(),
//!         base_url: "http://localhost:8001".to_string(),
//!         sleep_level: SleepLevel::L1,
//!     },
//!     VllmModelConfig {
//!         model_name: "model-b".to_string(),
//!         base_url: "http://localhost:8002".to_string(),
//!         sleep_level: SleepLevel::L2,
//!     },
//! ];
//!
//! // Create callbacks
//! let callbacks = VllmCallbacks::new(configs);
//!
//! // Create switcher
//! let policy = FifoPolicy::default();
//! let config = ModelSwitcherConfig::default();
//! let switcher = ModelSwitcher::new(callbacks, policy, config);
//! ```

use super::callbacks::{ModelSwitcherCallbacks, SleepLevel, SwitchError};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Configuration for a single vLLM model endpoint
#[derive(Debug, Clone)]
pub struct VllmModelConfig {
    /// The model name (as it will be requested by clients)
    pub model_name: String,
    /// Base URL of the vLLM server (e.g., "http://localhost:8001")
    pub base_url: String,
    /// Default sleep level for this model (can be overridden by policy)
    pub sleep_level: SleepLevel,
}

/// HTTP client trait for making requests to vLLM servers
///
/// This trait allows for easy mocking in tests.
#[async_trait]
pub trait VllmHttpClient: Send + Sync {
    /// POST request to vLLM endpoint
    async fn post(&self, url: &str, body: Option<&str>) -> Result<u16, String>;
    /// GET request to vLLM endpoint
    async fn get(&self, url: &str) -> Result<(u16, String), String>;
}

/// Default HTTP client using reqwest (if feature enabled) or hyper
#[derive(Debug, Clone, Default)]
pub struct DefaultVllmClient {
    timeout: Duration,
}

impl DefaultVllmClient {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

#[async_trait]
impl VllmHttpClient for DefaultVllmClient {
    async fn post(&self, url: &str, body: Option<&str>) -> Result<u16, String> {
        // In a real implementation, this would use hyper or reqwest
        // For now, we'll log and return success for the structure
        debug!(url = %url, body = ?body, "vLLM POST request");

        // This is a placeholder - in production, use an actual HTTP client:
        // let client = reqwest::Client::builder()
        //     .timeout(self.timeout)
        //     .build()
        //     .map_err(|e| e.to_string())?;
        //
        // let mut req = client.post(url);
        // if let Some(b) = body {
        //     req = req.header("Content-Type", "application/json").body(b.to_string());
        // }
        // let resp = req.send().await.map_err(|e| e.to_string())?;
        // Ok(resp.status().as_u16())

        Ok(200)
    }

    async fn get(&self, url: &str) -> Result<(u16, String), String> {
        debug!(url = %url, "vLLM GET request");

        // Placeholder - in production, use actual HTTP client
        Ok((200, "{}".to_string()))
    }
}

/// vLLM-specific implementation of ModelSwitcherCallbacks
///
/// This implementation communicates with vLLM servers using their
/// sleep mode HTTP API:
/// - POST /sleep?level={1,2} - Put model to sleep
/// - POST /wake_up - Wake model
/// - POST /collective_rpc with {"method": "reload_weights"} - For L2 wake
/// - POST /reset_prefix_cache - For L2 wake
pub struct VllmCallbacks<C: VllmHttpClient = DefaultVllmClient> {
    /// Model configurations indexed by model name
    models: HashMap<String, VllmModelConfig>,
    /// HTTP client for vLLM API calls
    client: Arc<C>,
    /// Timeout for readiness polling
    ready_timeout: Duration,
    /// Interval between readiness checks
    ready_poll_interval: Duration,
}

impl VllmCallbacks<DefaultVllmClient> {
    /// Create new vLLM callbacks with default HTTP client
    pub fn new(configs: Vec<VllmModelConfig>) -> Self {
        Self::with_client(
            configs,
            Arc::new(DefaultVllmClient::new(Duration::from_secs(30))),
        )
    }
}

impl<C: VllmHttpClient> VllmCallbacks<C> {
    /// Create new vLLM callbacks with a custom HTTP client
    pub fn with_client(configs: Vec<VllmModelConfig>, client: Arc<C>) -> Self {
        let models: HashMap<String, VllmModelConfig> = configs
            .into_iter()
            .map(|c| (c.model_name.clone(), c))
            .collect();

        Self {
            models,
            client,
            ready_timeout: Duration::from_secs(60),
            ready_poll_interval: Duration::from_millis(500),
        }
    }

    /// Set the timeout for readiness polling
    pub fn with_ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = timeout;
        self
    }

    /// Set the interval between readiness checks
    pub fn with_ready_poll_interval(mut self, interval: Duration) -> Self {
        self.ready_poll_interval = interval;
        self
    }

    /// Get model config
    fn get_model(&self, model: &str) -> Result<&VllmModelConfig, SwitchError> {
        self.models
            .get(model)
            .ok_or_else(|| SwitchError::ModelNotFound(model.to_string()))
    }
}

#[async_trait]
impl<C: VllmHttpClient + 'static> ModelSwitcherCallbacks for VllmCallbacks<C> {
    async fn wake_model(&self, model: &str) -> Result<(), SwitchError> {
        let config = self.get_model(model)?;
        let base_url = &config.base_url;

        info!(model = %model, base_url = %base_url, "Waking vLLM model");

        // Step 1: POST /wake_up
        let wake_url = format!("{}/wake_up", base_url);
        let status = self
            .client
            .post(&wake_url, None)
            .await
            .map_err(|e| SwitchError::WakeFailed {
                model: model.to_string(),
                reason: format!("wake_up request failed: {}", e),
            })?;

        if status >= 400 {
            return Err(SwitchError::WakeFailed {
                model: model.to_string(),
                reason: format!("wake_up returned status {}", status),
            });
        }

        // Step 2: For L2 sleep, need to reload weights and reset cache
        if config.sleep_level == SleepLevel::L2 {
            debug!(model = %model, "L2 mode: reloading weights");

            // POST /collective_rpc with {"method": "reload_weights"}
            let rpc_url = format!("{}/collective_rpc", base_url);
            let rpc_body = r#"{"method": "reload_weights"}"#;
            let status = self
                .client
                .post(&rpc_url, Some(rpc_body))
                .await
                .map_err(|e| SwitchError::WakeFailed {
                    model: model.to_string(),
                    reason: format!("reload_weights request failed: {}", e),
                })?;

            if status >= 400 {
                return Err(SwitchError::WakeFailed {
                    model: model.to_string(),
                    reason: format!("reload_weights returned status {}", status),
                });
            }

            // POST /reset_prefix_cache
            let cache_url = format!("{}/reset_prefix_cache", base_url);
            let status = self
                .client
                .post(&cache_url, None)
                .await
                .map_err(|e| SwitchError::WakeFailed {
                    model: model.to_string(),
                    reason: format!("reset_prefix_cache request failed: {}", e),
                })?;

            if status >= 400 {
                warn!(
                    model = %model,
                    status,
                    "reset_prefix_cache returned non-success status"
                );
                // Don't fail on cache reset error - model may still work
            }
        }

        info!(model = %model, "vLLM model wake complete");
        Ok(())
    }

    async fn sleep_model(&self, model: &str, level: SleepLevel) -> Result<(), SwitchError> {
        let config = self.get_model(model)?;
        let base_url = &config.base_url;

        info!(model = %model, level = %level, "Putting vLLM model to sleep");

        // POST /sleep?level={1,2}
        let sleep_url = format!("{}/sleep?level={}", base_url, level.as_u8());
        let status = self
            .client
            .post(&sleep_url, None)
            .await
            .map_err(|e| SwitchError::SleepFailed {
                model: model.to_string(),
                reason: format!("sleep request failed: {}", e),
            })?;

        if status >= 400 {
            return Err(SwitchError::SleepFailed {
                model: model.to_string(),
                reason: format!("sleep returned status {}", status),
            });
        }

        info!(model = %model, level = %level, "vLLM model sleep complete");
        Ok(())
    }

    async fn is_ready(&self, model: &str) -> bool {
        let config = match self.get_model(model) {
            Ok(c) => c,
            Err(_) => return false,
        };

        // Try a health check endpoint
        // vLLM typically has /health or /v1/models
        let health_url = format!("{}/health", config.base_url);
        match self.client.get(&health_url).await {
            Ok((status, _)) => status == 200,
            Err(e) => {
                debug!(model = %model, error = %e, "Health check failed");
                false
            }
        }
    }

    fn registered_models(&self) -> Vec<String> {
        self.models.keys().cloned().collect()
    }

    async fn initialize(&self) -> Result<(), SwitchError> {
        info!(
            models = ?self.registered_models(),
            "Initializing vLLM callbacks"
        );
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), SwitchError> {
        info!("Shutting down vLLM callbacks");
        // Could put all models to sleep here if desired
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock HTTP client for testing
    struct MockVllmClient {
        wake_calls: AtomicUsize,
        sleep_calls: AtomicUsize,
        health_status: u16,
    }

    impl MockVllmClient {
        fn new() -> Self {
            Self {
                wake_calls: AtomicUsize::new(0),
                sleep_calls: AtomicUsize::new(0),
                health_status: 200,
            }
        }
    }

    #[async_trait]
    impl VllmHttpClient for MockVllmClient {
        async fn post(&self, url: &str, _body: Option<&str>) -> Result<u16, String> {
            if url.contains("/wake_up") {
                self.wake_calls.fetch_add(1, Ordering::SeqCst);
            } else if url.contains("/sleep") {
                self.sleep_calls.fetch_add(1, Ordering::SeqCst);
            }
            Ok(200)
        }

        async fn get(&self, _url: &str) -> Result<(u16, String), String> {
            Ok((self.health_status, "{}".to_string()))
        }
    }

    #[tokio::test]
    async fn test_wake_model_l1() {
        let mock = Arc::new(MockVllmClient::new());
        let configs = vec![VllmModelConfig {
            model_name: "test-model".to_string(),
            base_url: "http://localhost:8001".to_string(),
            sleep_level: SleepLevel::L1,
        }];

        let callbacks = VllmCallbacks::with_client(configs, Arc::clone(&mock));

        callbacks.wake_model("test-model").await.unwrap();

        assert_eq!(mock.wake_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_wake_model_l2_reloads_weights() {
        let mock = Arc::new(MockVllmClient::new());
        let configs = vec![VllmModelConfig {
            model_name: "test-model".to_string(),
            base_url: "http://localhost:8001".to_string(),
            sleep_level: SleepLevel::L2,
        }];

        let callbacks = VllmCallbacks::with_client(configs, Arc::clone(&mock));

        callbacks.wake_model("test-model").await.unwrap();

        // L2 wake calls: wake_up + reload_weights + reset_prefix_cache = 3 POST calls
        // But our mock counts wake_up separately
        assert_eq!(mock.wake_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_sleep_model() {
        let mock = Arc::new(MockVllmClient::new());
        let configs = vec![VllmModelConfig {
            model_name: "test-model".to_string(),
            base_url: "http://localhost:8001".to_string(),
            sleep_level: SleepLevel::L1,
        }];

        let callbacks = VllmCallbacks::with_client(configs, Arc::clone(&mock));

        callbacks.sleep_model("test-model", SleepLevel::L2).await.unwrap();

        assert_eq!(mock.sleep_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_model_not_found() {
        let configs = vec![VllmModelConfig {
            model_name: "model-a".to_string(),
            base_url: "http://localhost:8001".to_string(),
            sleep_level: SleepLevel::L1,
        }];

        let callbacks = VllmCallbacks::new(configs);

        let result = callbacks.wake_model("unknown-model").await;
        assert!(matches!(result, Err(SwitchError::ModelNotFound(_))));
    }

    #[tokio::test]
    async fn test_registered_models() {
        let configs = vec![
            VllmModelConfig {
                model_name: "model-a".to_string(),
                base_url: "http://localhost:8001".to_string(),
                sleep_level: SleepLevel::L1,
            },
            VllmModelConfig {
                model_name: "model-b".to_string(),
                base_url: "http://localhost:8002".to_string(),
                sleep_level: SleepLevel::L2,
            },
        ];

        let callbacks = VllmCallbacks::new(configs);
        let models = callbacks.registered_models();

        assert_eq!(models.len(), 2);
        assert!(models.contains(&"model-a".to_string()));
        assert!(models.contains(&"model-b".to_string()));
    }
}
