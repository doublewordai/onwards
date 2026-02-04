//! Integration tests for model-switcher using mock vLLM servers
//!
//! These tests spawn actual mock-vllm processes and verify the full integration.
//! All tests use event-driven synchronization (no polling).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use serial_test::serial;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// Port allocator for orchestrator tests that need fixed ports.
/// Starts at a high port to avoid conflicts with system services.
static NEXT_PORT: AtomicU16 = AtomicU16::new(21000);

fn allocate_port() -> u16 {
    NEXT_PORT.fetch_add(1, Ordering::SeqCst)
}

/// A running mock-vllm server.
///
/// Waits for the server to signal readiness before returning.
/// Automatically kills the server when dropped.
struct MockServer {
    child: Child,
    port: u16,
    model: String,
}

impl MockServer {
    /// Spawn a mock-vllm server and wait for it to be ready.
    ///
    /// Uses dynamic port allocation (port 0) to avoid conflicts.
    /// Waits for the "READY <port>" signal from stdout (event-driven).
    async fn spawn(model: &str) -> Self {
        Self::spawn_with_args(model, &[]).await
    }

    /// Spawn with additional arguments.
    async fn spawn_with_args(model: &str, extra_args: &[&str]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mock-vllm"));
        cmd.args(["--port", "0", "--model", model, "--latency-ms", "5"])
            .args(extra_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("Failed to spawn mock-vllm");

        // Read stdout to get the READY signal with the actual port
        let stdout = child.stdout.take().expect("Failed to capture stdout");
        let mut reader = BufReader::new(stdout).lines();

        let port = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(line) = reader.next_line().await.expect("Failed to read stdout") {
                if let Some(port_str) = line.strip_prefix("READY ") {
                    return port_str.parse::<u16>().expect("Failed to parse port");
                }
            }
            panic!("Server never signaled READY");
        })
        .await
        .expect("Timeout waiting for server to be ready");

        Self {
            child,
            port,
            model: model.to_string(),
        }
    }

    /// Get the port this server is listening on.
    fn port(&self) -> u16 {
        self.port
    }

    /// Make a chat completion request to this server.
    async fn chat(&self, message: &str) -> serde_json::Value {
        let client = reqwest::Client::new();
        let url = format!("http://localhost:{}/v1/chat/completions", self.port);

        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": message}]
        });

        client
            .post(&url)
            .json(&body)
            .send()
            .await
            .expect("Request failed")
            .json()
            .await
            .expect("Failed to parse response")
    }

    /// Get stats from this server.
    async fn stats(&self) -> serde_json::Value {
        let client = reqwest::Client::new();
        let url = format!("http://localhost:{}/stats", self.port);

        client
            .get(&url)
            .send()
            .await
            .expect("Request failed")
            .json()
            .await
            .expect("Failed to parse response")
    }

    /// Put the server to sleep.
    async fn sleep(&self, level: u8) {
        let client = reqwest::Client::new();
        let url = format!("http://localhost:{}/sleep?level={}", self.port, level);
        client
            .post(&url)
            .send()
            .await
            .expect("Sleep request failed");
    }

    /// Wake up the server.
    async fn wake(&self) {
        let client = reqwest::Client::new();
        let url = format!("http://localhost:{}/wake_up", self.port);
        client
            .post(&url)
            .send()
            .await
            .expect("Wake request failed");
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        // Use synchronous kill since we're in Drop
        let _ = self.child.start_kill();
    }
}

// =============================================================================
// Mock vLLM Server Tests
// =============================================================================

#[tokio::test]
#[serial]
async fn test_mock_server_basic() {
    let server = MockServer::spawn("test-model").await;

    // Verify initial stats
    let stats = server.stats().await;
    assert_eq!(stats["model"], "test-model");
    assert_eq!(stats["sleeping"], false);
    assert_eq!(stats["request_count"], 0);

    // Make a request
    let response = server.chat("Hello!").await;
    assert!(response["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .contains("Hello!"));

    // Verify request was counted
    let stats = server.stats().await;
    assert_eq!(stats["request_count"], 1);
}

#[tokio::test]
#[serial]
async fn test_mock_server_sleep_wake() {
    let server = MockServer::spawn("sleepy-model").await;

    // Initially awake
    let stats = server.stats().await;
    assert_eq!(stats["sleeping"], false);

    // Sleep at L1
    server.sleep(1).await;
    let stats = server.stats().await;
    assert_eq!(stats["sleeping"], true);
    assert_eq!(stats["sleep_level"], 1);

    // Wake up
    server.wake().await;
    let stats = server.stats().await;
    assert_eq!(stats["sleeping"], false);

    // Request should succeed after wake
    let response = server.chat("Hello again!").await;
    assert!(response.get("choices").is_some());
}

#[tokio::test]
#[serial]
async fn test_mock_server_l2_sleep() {
    let server = MockServer::spawn("deep-model").await;

    // Sleep at L2
    server.sleep(2).await;
    let stats = server.stats().await;
    assert_eq!(stats["sleeping"], true);
    assert_eq!(stats["sleep_level"], 2);

    // Wake up
    server.wake().await;
    let stats = server.stats().await;
    assert_eq!(stats["sleeping"], false);
}

#[tokio::test]
#[serial]
async fn test_mock_server_rejects_while_sleeping() {
    let server = MockServer::spawn("strict-model").await;

    server.sleep(1).await;

    // Request should fail while sleeping
    let client = reqwest::Client::new();
    let url = format!("http://localhost:{}/v1/chat/completions", server.port());
    let body = serde_json::json!({
        "model": "strict-model",
        "messages": [{"role": "user", "content": "test"}]
    });

    let response = client.post(&url).json(&body).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
}

// =============================================================================
// Orchestrator Tests
// =============================================================================

#[tokio::test]
#[serial]
async fn test_orchestrator_spawns_and_manages_process() {
    use model_switcher::{ModelConfig, Orchestrator, ProcessState};
    use std::sync::Arc;

    let mock_vllm_path = env!("CARGO_BIN_EXE_mock-vllm");

    let mut models = HashMap::new();
    models.insert(
        "test-model".to_string(),
        ModelConfig {
            model_path: "test-model".to_string(),
            port: 0, // Will use dynamic port, but orchestrator needs a fixed port
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );

    // Allocate a unique port for this test
    let port = allocate_port();
    models.get_mut("test-model").unwrap().port = port;

    let orchestrator = Arc::new(Orchestrator::with_command(models, mock_vllm_path.to_string()));

    // Initial state
    assert_eq!(
        orchestrator.process_state("test-model").await,
        Some(ProcessState::NotStarted)
    );

    // Start via ensure_running
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        orchestrator.ensure_running("test-model"),
    )
    .await;

    assert!(result.is_ok(), "Timed out waiting for process to start");
    assert!(result.unwrap().is_ok(), "Failed to start process");

    // Should be running
    assert_eq!(
        orchestrator.process_state("test-model").await,
        Some(ProcessState::Running { sleeping: false })
    );

    // Make a request to verify it's actually running
    let client = reqwest::Client::new();
    let url = format!("http://localhost:{}/v1/chat/completions", port);
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}]
    });

    let response = client.post(&url).json(&body).send().await.unwrap();
    assert!(response.status().is_success());

    // Sleep via orchestrator
    orchestrator
        .sleep_model("test-model", model_switcher::SleepLevel::L1)
        .await
        .unwrap();

    assert_eq!(
        orchestrator.process_state("test-model").await,
        Some(ProcessState::Running { sleeping: true })
    );

    // Wake via orchestrator
    orchestrator.wake_model("test-model").await.unwrap();

    assert_eq!(
        orchestrator.process_state("test-model").await,
        Some(ProcessState::Running { sleeping: false })
    );
}

#[tokio::test]
#[serial]
async fn test_orchestrator_multiple_models() {
    use model_switcher::{ModelConfig, Orchestrator, ProcessState};
    use std::sync::Arc;

    let mock_vllm_path = env!("CARGO_BIN_EXE_mock-vllm");

    // Allocate unique ports for each model
    let port_alpha = allocate_port();
    let port_beta = allocate_port();

    let mut models = HashMap::new();
    models.insert(
        "model-alpha".to_string(),
        ModelConfig {
            model_path: "model-alpha".to_string(),
            port: port_alpha,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );
    models.insert(
        "model-beta".to_string(),
        ModelConfig {
            model_path: "model-beta".to_string(),
            port: port_beta,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );

    let orchestrator = Arc::new(Orchestrator::with_command(models, mock_vllm_path.to_string()));

    // Both should start as not started
    assert_eq!(
        orchestrator.process_state("model-alpha").await,
        Some(ProcessState::NotStarted)
    );
    assert_eq!(
        orchestrator.process_state("model-beta").await,
        Some(ProcessState::NotStarted)
    );

    // Start alpha
    orchestrator
        .ensure_running("model-alpha")
        .await
        .expect("Failed to start model-alpha");

    assert_eq!(
        orchestrator.process_state("model-alpha").await,
        Some(ProcessState::Running { sleeping: false })
    );
    assert_eq!(
        orchestrator.process_state("model-beta").await,
        Some(ProcessState::NotStarted)
    );

    // Start beta
    orchestrator
        .ensure_running("model-beta")
        .await
        .expect("Failed to start model-beta");

    // Both running
    assert_eq!(
        orchestrator.process_state("model-alpha").await,
        Some(ProcessState::Running { sleeping: false })
    );
    assert_eq!(
        orchestrator.process_state("model-beta").await,
        Some(ProcessState::Running { sleeping: false })
    );

    // Sleep alpha, beta stays awake
    orchestrator
        .sleep_model("model-alpha", model_switcher::SleepLevel::L1)
        .await
        .unwrap();

    assert_eq!(
        orchestrator.process_state("model-alpha").await,
        Some(ProcessState::Running { sleeping: true })
    );
    assert_eq!(
        orchestrator.process_state("model-beta").await,
        Some(ProcessState::Running { sleeping: false })
    );

    // Wake alpha
    orchestrator.wake_model("model-alpha").await.unwrap();

    assert_eq!(
        orchestrator.process_state("model-alpha").await,
        Some(ProcessState::Running { sleeping: false })
    );
}

// =============================================================================
// Switcher Tests (Unit - no process spawning)
// =============================================================================

#[tokio::test]
async fn test_switcher_basic_registration() {
    use model_switcher::{FifoPolicy, ModelConfig, ModelSwitcher, Orchestrator};
    use std::sync::Arc;

    let mut configs = HashMap::new();
    configs.insert(
        "model-a".to_string(),
        ModelConfig {
            model_path: "test".to_string(),
            port: 8001,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );
    configs.insert(
        "model-b".to_string(),
        ModelConfig {
            model_path: "test".to_string(),
            port: 8002,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );

    let orchestrator = Arc::new(Orchestrator::new(configs));
    let policy = Box::new(FifoPolicy::default());
    let switcher = ModelSwitcher::new(orchestrator, policy);

    assert!(switcher.is_registered("model-a"));
    assert!(switcher.is_registered("model-b"));
    assert!(!switcher.is_registered("model-c"));
}

#[tokio::test]
async fn test_switcher_unregistered_model_error() {
    use model_switcher::{FifoPolicy, ModelConfig, ModelSwitcher, Orchestrator, SwitchError};
    use std::sync::Arc;

    let mut configs = HashMap::new();
    configs.insert(
        "model-a".to_string(),
        ModelConfig {
            model_path: "test".to_string(),
            port: 8001,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );

    let orchestrator = Arc::new(Orchestrator::new(configs));
    let policy = Box::new(FifoPolicy::default());
    let switcher = ModelSwitcher::new(orchestrator, policy);

    // Request for unregistered model should fail
    let result = switcher.ensure_model_ready("nonexistent").await;
    assert!(matches!(result, Err(SwitchError::ModelNotFound(_))));
}

#[tokio::test]
async fn test_switcher_in_flight_tracking() {
    use model_switcher::{FifoPolicy, ModelConfig, ModelSwitcher, Orchestrator};
    use std::sync::Arc;

    let mut configs = HashMap::new();
    configs.insert(
        "model-a".to_string(),
        ModelConfig {
            model_path: "test".to_string(),
            port: 8001,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );

    let orchestrator = Arc::new(Orchestrator::new(configs));
    let policy = Box::new(FifoPolicy::default());
    let switcher = ModelSwitcher::new(orchestrator, policy);

    // Initially no in-flight
    assert_eq!(switcher.in_flight_count("model-a"), 0);

    // Acquire guard
    let guard1 = switcher.acquire_in_flight("model-a");
    assert!(guard1.is_some());
    assert_eq!(switcher.in_flight_count("model-a"), 1);

    // Acquire another
    let guard2 = switcher.acquire_in_flight("model-a");
    assert!(guard2.is_some());
    assert_eq!(switcher.in_flight_count("model-a"), 2);

    // Drop one
    drop(guard1);
    assert_eq!(switcher.in_flight_count("model-a"), 1);

    // Drop the other
    drop(guard2);
    assert_eq!(switcher.in_flight_count("model-a"), 0);

    // Unregistered model returns None
    assert!(switcher.acquire_in_flight("nonexistent").is_none());
}

#[tokio::test]
async fn test_switcher_initial_state() {
    use model_switcher::{FifoPolicy, ModelConfig, ModelSwitcher, Orchestrator, SwitcherState};
    use std::sync::Arc;

    let mut configs = HashMap::new();
    configs.insert(
        "model-a".to_string(),
        ModelConfig {
            model_path: "test".to_string(),
            port: 8001,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );

    let orchestrator = Arc::new(Orchestrator::new(configs));
    let policy = Box::new(FifoPolicy::default());
    let switcher = ModelSwitcher::new(orchestrator, policy);

    // Initially idle
    assert_eq!(switcher.state().await, SwitcherState::Idle);
    assert_eq!(switcher.active_model().await, None);
}

// =============================================================================
// Switcher Integration Tests (with process spawning)
// =============================================================================

#[tokio::test]
#[serial]
async fn test_switcher_ensure_model_ready() {
    use model_switcher::{FifoPolicy, ModelConfig, ModelSwitcher, Orchestrator, SwitcherState};
    use std::sync::Arc;

    let mock_vllm_path = env!("CARGO_BIN_EXE_mock-vllm");
    let port = allocate_port();

    let mut configs = HashMap::new();
    configs.insert(
        "test-model".to_string(),
        ModelConfig {
            model_path: "test-model".to_string(),
            port,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );

    let orchestrator = Arc::new(Orchestrator::with_command(configs, mock_vllm_path.to_string()));
    let policy = Box::new(FifoPolicy::default());
    let switcher = ModelSwitcher::new(orchestrator, policy);

    // Initially idle
    assert_eq!(switcher.state().await, SwitcherState::Idle);

    // Request model - should start it and make it active
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        switcher.ensure_model_ready("test-model"),
    )
    .await;

    assert!(result.is_ok(), "Timeout");
    assert!(result.unwrap().is_ok(), "Failed to ensure model ready");

    // Should now be active
    assert_eq!(
        switcher.state().await,
        SwitcherState::Active {
            model: "test-model".to_string()
        }
    );
    assert_eq!(switcher.active_model().await, Some("test-model".to_string()));
}

#[tokio::test]
#[serial]
async fn test_switcher_model_switching() {
    use model_switcher::{FifoPolicy, ModelConfig, ModelSwitcher, Orchestrator, SwitcherState};
    use std::sync::Arc;

    let mock_vllm_path = env!("CARGO_BIN_EXE_mock-vllm");
    let port_a = allocate_port();
    let port_b = allocate_port();

    let mut configs = HashMap::new();
    configs.insert(
        "model-a".to_string(),
        ModelConfig {
            model_path: "model-a".to_string(),
            port: port_a,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );
    configs.insert(
        "model-b".to_string(),
        ModelConfig {
            model_path: "model-b".to_string(),
            port: port_b,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );

    let orchestrator = Arc::new(Orchestrator::with_command(configs, mock_vllm_path.to_string()));
    let policy = Box::new(FifoPolicy::default());
    let switcher = ModelSwitcher::new(orchestrator, policy);

    // Start with model-a
    switcher
        .ensure_model_ready("model-a")
        .await
        .expect("Failed to start model-a");

    assert_eq!(
        switcher.state().await,
        SwitcherState::Active {
            model: "model-a".to_string()
        }
    );

    // Switch to model-b
    switcher
        .ensure_model_ready("model-b")
        .await
        .expect("Failed to switch to model-b");

    assert_eq!(
        switcher.state().await,
        SwitcherState::Active {
            model: "model-b".to_string()
        }
    );

    // Switch back to model-a
    switcher
        .ensure_model_ready("model-a")
        .await
        .expect("Failed to switch back to model-a");

    assert_eq!(
        switcher.state().await,
        SwitcherState::Active {
            model: "model-a".to_string()
        }
    );
}

#[tokio::test]
#[serial]
async fn test_switcher_same_model_no_switch() {
    use model_switcher::{FifoPolicy, ModelConfig, ModelSwitcher, Orchestrator, SwitcherState};
    use std::sync::Arc;

    let mock_vllm_path = env!("CARGO_BIN_EXE_mock-vllm");
    let port = allocate_port();

    let mut configs = HashMap::new();
    configs.insert(
        "model-a".to_string(),
        ModelConfig {
            model_path: "model-a".to_string(),
            port,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );

    let orchestrator = Arc::new(Orchestrator::with_command(configs, mock_vllm_path.to_string()));
    let policy = Box::new(FifoPolicy::default());
    let switcher = ModelSwitcher::new(orchestrator, policy);

    // Start model-a
    switcher
        .ensure_model_ready("model-a")
        .await
        .expect("Failed to start model-a");

    // Request same model again - should return immediately
    let start = std::time::Instant::now();
    switcher
        .ensure_model_ready("model-a")
        .await
        .expect("Failed second request");
    let elapsed = start.elapsed();

    // Should be very fast (no switch needed)
    assert!(
        elapsed < Duration::from_millis(100),
        "Same model request took too long: {:?}",
        elapsed
    );

    assert_eq!(
        switcher.state().await,
        SwitcherState::Active {
            model: "model-a".to_string()
        }
    );
}

// =============================================================================
// Error Handling Tests
// =============================================================================

#[tokio::test]
async fn test_orchestrator_unknown_model() {
    use model_switcher::{ModelConfig, Orchestrator, OrchestratorError};
    use std::sync::Arc;

    let mut configs = HashMap::new();
    configs.insert(
        "known-model".to_string(),
        ModelConfig {
            model_path: "test".to_string(),
            port: 8001,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );

    let orchestrator = Arc::new(Orchestrator::new(configs));

    // Unknown model should return None for state
    assert_eq!(orchestrator.process_state("unknown").await, None);

    // ensure_running should fail for unknown model
    let result = orchestrator.ensure_running("unknown").await;
    assert!(matches!(result, Err(OrchestratorError::ModelNotFound(_))));

    // sleep/wake should fail for unknown model
    let result = orchestrator
        .sleep_model("unknown", model_switcher::SleepLevel::L1)
        .await;
    assert!(matches!(result, Err(OrchestratorError::ModelNotFound(_))));

    let result = orchestrator.wake_model("unknown").await;
    assert!(matches!(result, Err(OrchestratorError::ModelNotFound(_))));
}

// =============================================================================
// End-to-End Tests (Full HTTP stack)
// =============================================================================

#[tokio::test]
#[serial]
async fn test_end_to_end_single_model() {
    use axum::Router;
    use model_switcher::{
        Config, FifoPolicy, ModelConfig, ModelSwitcher, ModelSwitcherLayer, Orchestrator,
        PolicyConfig,
    };
    use std::sync::Arc;
    use tokio::net::TcpListener;

    let mock_vllm_path = env!("CARGO_BIN_EXE_mock-vllm");
    let backend_port = allocate_port();
    let proxy_port = allocate_port();

    // Build config
    let mut models = HashMap::new();
    models.insert(
        "test-model".to_string(),
        ModelConfig {
            model_path: "test-model".to_string(),
            port: backend_port,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );

    let config = Config {
        models: models.clone(),
        policy: PolicyConfig::default(),
        port: proxy_port,
        metrics_port: 0,
        vllm_command: mock_vllm_path.to_string(),
    };

    // Build the full app stack
    let orchestrator = Arc::new(Orchestrator::with_command(
        config.models.clone(),
        config.vllm_command.clone(),
    ));
    let policy = Box::new(FifoPolicy::default());
    let switcher = ModelSwitcher::new(orchestrator.clone(), policy);

    // Build onwards targets
    let targets = config.build_onwards_targets().unwrap();
    let onwards_state = onwards::AppState::new(targets);
    let onwards_router = onwards::build_router(onwards_state);

    // Wrap with middleware
    let app: Router = onwards_router.layer(ModelSwitcherLayer::new(switcher));

    // Start server
    let listener = TcpListener::bind(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send request through proxy
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://127.0.0.1:{}/v1/chat/completions", proxy_port))
        .json(&serde_json::json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Hello from e2e test!"}]
        }))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .expect("Request failed");

    assert!(
        response.status().is_success(),
        "Response status: {}",
        response.status()
    );

    let body: serde_json::Value = response.json().await.unwrap();
    let content = body["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        content.contains("Hello from e2e test!"),
        "Unexpected response: {}",
        content
    );

    server.abort();
}

#[tokio::test]
#[serial]
async fn test_end_to_end_model_switching() {
    use axum::Router;
    use model_switcher::{
        Config, FifoPolicy, ModelConfig, ModelSwitcher, ModelSwitcherLayer, Orchestrator,
        PolicyConfig,
    };
    use std::sync::Arc;
    use tokio::net::TcpListener;

    let mock_vllm_path = env!("CARGO_BIN_EXE_mock-vllm");
    let port_a = allocate_port();
    let port_b = allocate_port();
    let proxy_port = allocate_port();

    // Build config with two models
    let mut models = HashMap::new();
    models.insert(
        "model-a".to_string(),
        ModelConfig {
            model_path: "model-a".to_string(),
            port: port_a,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );
    models.insert(
        "model-b".to_string(),
        ModelConfig {
            model_path: "model-b".to_string(),
            port: port_b,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );

    let config = Config {
        models: models.clone(),
        policy: PolicyConfig::default(),
        port: proxy_port,
        metrics_port: 0,
        vllm_command: mock_vllm_path.to_string(),
    };

    // Build the full app stack
    let orchestrator = Arc::new(Orchestrator::with_command(
        config.models.clone(),
        config.vllm_command.clone(),
    ));
    let policy = Box::new(FifoPolicy::default());
    let switcher = ModelSwitcher::new(orchestrator.clone(), policy);

    let targets = config.build_onwards_targets().unwrap();
    let onwards_state = onwards::AppState::new(targets);
    let onwards_router = onwards::build_router(onwards_state);
    let app: Router = onwards_router.layer(ModelSwitcherLayer::new(switcher));

    // Start server
    let listener = TcpListener::bind(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/v1/chat/completions", proxy_port);

    // Request to model-a
    let response = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "model-a",
            "messages": [{"role": "user", "content": "Hello A!"}]
        }))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .expect("Request to model-a failed");

    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .contains("Hello A!"));

    // Switch to model-b
    let response = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "model-b",
            "messages": [{"role": "user", "content": "Hello B!"}]
        }))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .expect("Request to model-b failed");

    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .contains("Hello B!"));

    // Switch back to model-a
    let response = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "model-a",
            "messages": [{"role": "user", "content": "Back to A!"}]
        }))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .expect("Request back to model-a failed");

    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .contains("Back to A!"));

    server.abort();
}

#[tokio::test]
#[serial]
async fn test_end_to_end_unknown_model_passthrough() {
    use axum::Router;
    use model_switcher::{
        Config, FifoPolicy, ModelConfig, ModelSwitcher, ModelSwitcherLayer, Orchestrator,
        PolicyConfig,
    };
    use std::sync::Arc;
    use tokio::net::TcpListener;

    let mock_vllm_path = env!("CARGO_BIN_EXE_mock-vllm");
    let backend_port = allocate_port();
    let proxy_port = allocate_port();

    let mut models = HashMap::new();
    models.insert(
        "known-model".to_string(),
        ModelConfig {
            model_path: "known-model".to_string(),
            port: backend_port,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );

    let config = Config {
        models: models.clone(),
        policy: PolicyConfig::default(),
        port: proxy_port,
        metrics_port: 0,
        vllm_command: mock_vllm_path.to_string(),
    };

    let orchestrator = Arc::new(Orchestrator::with_command(
        config.models.clone(),
        config.vllm_command.clone(),
    ));
    let policy = Box::new(FifoPolicy::default());
    let switcher = ModelSwitcher::new(orchestrator.clone(), policy);

    let targets = config.build_onwards_targets().unwrap();
    let onwards_state = onwards::AppState::new(targets);
    let onwards_router = onwards::build_router(onwards_state);
    let app: Router = onwards_router.layer(ModelSwitcherLayer::new(switcher));

    let listener = TcpListener::bind(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::new();

    // Request to unknown model should be passed through (and fail at onwards level)
    let response = client
        .post(format!("http://127.0.0.1:{}/v1/chat/completions", proxy_port))
        .json(&serde_json::json!({
            "model": "unknown-model",
            "messages": [{"role": "user", "content": "test"}]
        }))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect("Request failed");

    // Should get a 404 from onwards (model not found in targets)
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

    server.abort();
}

#[tokio::test]
#[serial]
async fn test_end_to_end_concurrent_requests() {
    use axum::Router;
    use model_switcher::{
        Config, FifoPolicy, ModelConfig, ModelSwitcher, ModelSwitcherLayer, Orchestrator,
        PolicyConfig,
    };
    use std::sync::Arc;
    use tokio::net::TcpListener;

    let mock_vllm_path = env!("CARGO_BIN_EXE_mock-vllm");
    let backend_port = allocate_port();
    let proxy_port = allocate_port();

    let mut models = HashMap::new();
    models.insert(
        "test-model".to_string(),
        ModelConfig {
            model_path: "test-model".to_string(),
            port: backend_port,
            gpu_memory_utilization: 0.9,
            tensor_parallel_size: 1,
            dtype: "auto".to_string(),
            extra_args: vec![],
            sleep_level: 1,
        },
    );

    let config = Config {
        models: models.clone(),
        policy: PolicyConfig::default(),
        port: proxy_port,
        metrics_port: 0,
        vllm_command: mock_vllm_path.to_string(),
    };

    let orchestrator = Arc::new(Orchestrator::with_command(
        config.models.clone(),
        config.vllm_command.clone(),
    ));
    let policy = Box::new(FifoPolicy::default());
    let switcher = ModelSwitcher::new(orchestrator.clone(), policy);

    let targets = config.build_onwards_targets().unwrap();
    let onwards_state = onwards::AppState::new(targets);
    let onwards_router = onwards::build_router(onwards_state);
    let app: Router = onwards_router.layer(ModelSwitcherLayer::new(switcher));

    let listener = TcpListener::bind(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send multiple concurrent requests
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/v1/chat/completions", proxy_port);

    let mut handles = vec![];
    for i in 0..5 {
        let client = client.clone();
        let url = url.clone();
        handles.push(tokio::spawn(async move {
            client
                .post(&url)
                .json(&serde_json::json!({
                    "model": "test-model",
                    "messages": [{"role": "user", "content": format!("Request {}", i)}]
                }))
                .timeout(Duration::from_secs(15))
                .send()
                .await
        }));
    }

    // All should succeed
    for (i, handle) in handles.into_iter().enumerate() {
        let response = handle.await.expect("Task panicked").expect("Request failed");
        assert!(
            response.status().is_success(),
            "Request {} failed with status {}",
            i,
            response.status()
        );
    }

    server.abort();
}
