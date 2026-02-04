//! Orchestrator - manages vLLM process lifecycle
//!
//! The orchestrator is responsible for:
//! - Lazily starting vLLM processes on first request
//! - Tracking process state (NotStarted, Starting, Running, etc.)
//! - Health checking to confirm processes are ready
//! - Coordinating with the switcher for wake/sleep operations

use crate::config::ModelConfig;
use crate::switcher::SleepLevel;
use anyhow::Result;
use dashmap::DashMap;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};

/// State of a model's vLLM process
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessState {
    /// Process has not been started yet
    NotStarted,
    /// Process is starting up
    Starting,
    /// Process is running and ready
    Running {
        /// Whether the model is currently sleeping
        sleeping: bool,
    },
    /// Process failed to start or crashed
    Failed { reason: String },
}

/// Internal state for a managed process
struct ManagedProcess {
    #[allow(dead_code)] // Reserved for future use
    config: ModelConfig,
    state: ProcessState,
    child: Option<Child>,
    /// Notifies waiters when process becomes ready
    ready_notify: Arc<Notify>,
}

/// Orchestrator manages vLLM process lifecycle
pub struct Orchestrator {
    /// Model configurations
    configs: HashMap<String, ModelConfig>,
    /// Process state for each model
    processes: DashMap<String, Arc<Mutex<ManagedProcess>>>,
    /// Lock for serializing process operations
    operation_lock: Mutex<()>,
    /// Health check timeout
    health_timeout: Duration,
    /// Startup timeout
    startup_timeout: Duration,
}

impl Orchestrator {
    /// Create a new orchestrator with the given model configurations
    pub fn new(configs: HashMap<String, ModelConfig>) -> Self {
        let processes = DashMap::new();

        for (name, config) in &configs {
            processes.insert(
                name.clone(),
                Arc::new(Mutex::new(ManagedProcess {
                    config: config.clone(),
                    state: ProcessState::NotStarted,
                    child: None,
                    ready_notify: Arc::new(Notify::new()),
                })),
            );
        }

        Self {
            configs,
            processes,
            operation_lock: Mutex::new(()),
            health_timeout: Duration::from_secs(5),
            startup_timeout: Duration::from_secs(300), // 5 minutes for large models
        }
    }

    /// Get the current state of a model's process
    pub async fn process_state(&self, model: &str) -> Option<ProcessState> {
        let process = self.processes.get(model)?;
        let guard = process.lock().await;
        Some(guard.state.clone())
    }

    /// Get all registered model names
    pub fn registered_models(&self) -> Vec<String> {
        self.configs.keys().cloned().collect()
    }

    /// Ensure a model's process is running and ready
    ///
    /// This will:
    /// 1. Start the process if not started
    /// 2. Wait for it to become healthy
    /// 3. Return once the model is ready to serve requests
    pub async fn ensure_running(&self, model: &str) -> Result<(), OrchestratorError> {
        let process = self
            .processes
            .get(model)
            .ok_or_else(|| OrchestratorError::ModelNotFound(model.to_string()))?;

        // Check current state
        {
            let guard = process.lock().await;
            match &guard.state {
                ProcessState::Running { sleeping: false } => {
                    // Already running and awake
                    return Ok(());
                }
                ProcessState::Running { sleeping: true } => {
                    // Running but sleeping - need to wake
                    // This is handled by wake_model
                    return Ok(());
                }
                ProcessState::Starting => {
                    // Someone else is starting it, wait for ready
                    let notify = guard.ready_notify.clone();
                    drop(guard);
                    notify.notified().await;
                    return Ok(());
                }
                ProcessState::Failed { reason } => {
                    return Err(OrchestratorError::ProcessFailed {
                        model: model.to_string(),
                        reason: reason.clone(),
                    });
                }
                ProcessState::NotStarted => {
                    // Need to start it
                }
            }
        }

        // Acquire operation lock to serialize starts
        let _op_guard = self.operation_lock.lock().await;

        // Double-check state after acquiring lock
        {
            let guard = process.lock().await;
            if matches!(guard.state, ProcessState::Running { .. } | ProcessState::Starting) {
                return Ok(());
            }
        }

        // Start the process
        self.start_process_internal(model, &process).await
    }

    /// Internal: start a vLLM process
    async fn start_process_internal(
        &self,
        model: &str,
        process: &Arc<Mutex<ManagedProcess>>,
    ) -> Result<(), OrchestratorError> {
        let config = self
            .configs
            .get(model)
            .ok_or_else(|| OrchestratorError::ModelNotFound(model.to_string()))?;

        info!(model = %model, port = config.port, "Starting vLLM process");

        // Mark as starting
        {
            let mut guard = process.lock().await;
            guard.state = ProcessState::Starting;
        }

        // Build vLLM command
        let args = config.vllm_args();
        debug!(model = %model, args = ?args, "vLLM command args");

        // Spawn process
        let child = Command::new("vllm")
            .args(&args)
            .env("VLLM_SERVER_DEV_MODE", "1") // Required for sleep mode endpoints
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| OrchestratorError::SpawnFailed {
                model: model.to_string(),
                reason: e.to_string(),
            })?;

        // Store child process
        {
            let mut guard = process.lock().await;
            guard.child = Some(child);
        }

        // Wait for health check to pass
        let health_url = format!("http://localhost:{}/health", config.port);
        let start = std::time::Instant::now();

        loop {
            if start.elapsed() > self.startup_timeout {
                let mut guard = process.lock().await;
                guard.state = ProcessState::Failed {
                    reason: "Startup timeout".to_string(),
                };
                // Kill the process
                if let Some(ref mut child) = guard.child {
                    let _ = child.kill().await;
                }
                return Err(OrchestratorError::StartupTimeout {
                    model: model.to_string(),
                });
            }

            // Try health check
            match self.check_health(&health_url).await {
                Ok(true) => {
                    info!(model = %model, "vLLM process is ready");
                    let mut guard = process.lock().await;
                    guard.state = ProcessState::Running { sleeping: false };
                    guard.ready_notify.notify_waiters();
                    return Ok(());
                }
                Ok(false) => {
                    debug!(model = %model, "Health check returned unhealthy, retrying...");
                }
                Err(e) => {
                    debug!(model = %model, error = %e, "Health check failed, retrying...");
                }
            }

            // Check if process died
            {
                let mut guard = process.lock().await;
                if let Some(ref mut child) = guard.child {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            let reason = format!("Process exited with status: {}", status);
                            guard.state = ProcessState::Failed {
                                reason: reason.clone(),
                            };
                            return Err(OrchestratorError::ProcessFailed {
                                model: model.to_string(),
                                reason,
                            });
                        }
                        Ok(None) => {
                            // Still running
                        }
                        Err(e) => {
                            warn!(model = %model, error = %e, "Failed to check process status");
                        }
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Check if a vLLM endpoint is healthy
    async fn check_health(&self, url: &str) -> Result<bool, String> {
        use http_body_util::Empty;

        let client: hyper_util::client::legacy::Client<_, Empty<bytes::Bytes>> =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build_http();

        let uri: hyper::Uri = url.parse().map_err(|e| format!("Invalid URL: {}", e))?;

        let request = hyper::Request::builder()
            .method("GET")
            .uri(uri)
            .body(Empty::new())
            .map_err(|e| format!("Failed to build request: {}", e))?;

        let result = tokio::time::timeout(self.health_timeout, client.request(request)).await;

        match result {
            Ok(Ok(response)) => Ok(response.status().is_success()),
            Ok(Err(e)) => Err(format!("Request failed: {}", e)),
            Err(_) => Err("Health check timeout".to_string()),
        }
    }

    /// Wake a model from sleep
    pub async fn wake_model(&self, model: &str) -> Result<(), OrchestratorError> {
        // First ensure the process is running
        self.ensure_running(model).await?;

        let process = self
            .processes
            .get(model)
            .ok_or_else(|| OrchestratorError::ModelNotFound(model.to_string()))?;

        let config = self
            .configs
            .get(model)
            .ok_or_else(|| OrchestratorError::ModelNotFound(model.to_string()))?;

        // Check if already awake
        {
            let guard = process.lock().await;
            if let ProcessState::Running { sleeping: false } = guard.state {
                return Ok(());
            }
        }

        info!(model = %model, "Waking model");

        let base_url = format!("http://localhost:{}", config.port);

        // POST /wake_up
        self.post_request(&format!("{}/wake_up", base_url), None)
            .await
            .map_err(|e| OrchestratorError::WakeFailed {
                model: model.to_string(),
                reason: e,
            })?;

        // For L2 sleep, need to reload weights
        if config.sleep_level == 2 {
            debug!(model = %model, "L2 sleep: reloading weights");

            self.post_request(
                &format!("{}/collective_rpc", base_url),
                Some(r#"{"method": "reload_weights"}"#),
            )
            .await
            .map_err(|e| OrchestratorError::WakeFailed {
                model: model.to_string(),
                reason: e,
            })?;

            self.post_request(&format!("{}/reset_prefix_cache", base_url), None)
                .await
                .map_err(|e| {
                    warn!(model = %model, error = %e, "Failed to reset prefix cache");
                    // Don't fail on cache reset
                })
                .ok();
        }

        // Update state
        {
            let mut guard = process.lock().await;
            guard.state = ProcessState::Running { sleeping: false };
        }

        info!(model = %model, "Model is now awake");
        Ok(())
    }

    /// Put a model to sleep
    pub async fn sleep_model(&self, model: &str, level: SleepLevel) -> Result<(), OrchestratorError> {
        let process = self
            .processes
            .get(model)
            .ok_or_else(|| OrchestratorError::ModelNotFound(model.to_string()))?;

        let config = self
            .configs
            .get(model)
            .ok_or_else(|| OrchestratorError::ModelNotFound(model.to_string()))?;

        // Check if already sleeping or not running
        {
            let guard = process.lock().await;
            match &guard.state {
                ProcessState::Running { sleeping: true } => return Ok(()),
                ProcessState::Running { sleeping: false } => {
                    // Proceed with sleep
                }
                _ => return Ok(()), // Not running, nothing to sleep
            }
        }

        info!(model = %model, level = ?level, "Putting model to sleep");

        let level_num = match level {
            SleepLevel::L1 => 1,
            SleepLevel::L2 => 2,
        };

        let url = format!(
            "http://localhost:{}/sleep?level={}",
            config.port, level_num
        );

        self.post_request(&url, None)
            .await
            .map_err(|e| OrchestratorError::SleepFailed {
                model: model.to_string(),
                reason: e,
            })?;

        // Update state
        {
            let mut guard = process.lock().await;
            guard.state = ProcessState::Running { sleeping: true };
        }

        info!(model = %model, "Model is now sleeping");
        Ok(())
    }

    /// Check if a model is ready
    pub async fn is_ready(&self, model: &str) -> bool {
        let Some(process) = self.processes.get(model) else {
            return false;
        };

        let guard = process.lock().await;
        matches!(guard.state, ProcessState::Running { sleeping: false })
    }

    /// Helper to make POST requests
    async fn post_request(&self, url: &str, body: Option<&str>) -> Result<(), String> {
        use http_body_util::Full;
        use hyper::Request;

        let client: hyper_util::client::legacy::Client<_, Full<bytes::Bytes>> =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build_http();

        let uri: hyper::Uri = url.parse().map_err(|e| format!("Invalid URL: {}", e))?;

        let has_body = body.is_some();
        let body_bytes = body.map(|b| b.as_bytes().to_vec()).unwrap_or_default();
        let request_body = Full::new(bytes::Bytes::from(body_bytes));

        let mut req_builder = Request::builder().method("POST").uri(uri);

        if has_body {
            req_builder = req_builder.header("Content-Type", "application/json");
        }

        let request = req_builder
            .body(request_body)
            .map_err(|e| format!("Failed to build request: {}", e))?;

        let response = tokio::time::timeout(Duration::from_secs(30), client.request(request))
            .await
            .map_err(|_| "Request timeout".to_string())?
            .map_err(|e| format!("Request failed: {}", e))?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("Request failed with status: {}", response.status()))
        }
    }
}

/// Errors from the orchestrator
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("failed to spawn process for {model}: {reason}")]
    SpawnFailed { model: String, reason: String },

    #[error("startup timeout for {model}")]
    StartupTimeout { model: String },

    #[error("process failed for {model}: {reason}")]
    ProcessFailed { model: String, reason: String },

    #[error("failed to wake {model}: {reason}")]
    WakeFailed { model: String, reason: String },

    #[error("failed to sleep {model}: {reason}")]
    SleepFailed { model: String, reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orchestrator_creation() {
        let mut configs = HashMap::new();
        configs.insert(
            "model-a".to_string(),
            ModelConfig {
                model_path: "test/model".to_string(),
                port: 8001,
                gpu_memory_utilization: 0.9,
                tensor_parallel_size: 1,
                dtype: "auto".to_string(),
                extra_args: vec![],
                sleep_level: 1,
            },
        );

        let orchestrator = Orchestrator::new(configs);
        assert_eq!(orchestrator.registered_models(), vec!["model-a"]);
    }
}
