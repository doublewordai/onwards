//! Example: Script-based tool executor
//!
//! Runs the full onwards proxy with an additional `--tools-dir` flag that
//! wires in a [`ToolExecutor`] backed by bash scripts. This is identical to
//! running `onwards` itself, but with server-side tool execution enabled.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example script_tools -- \
//!     -f examples/tool-calling-config.json \
//!     --tools-dir examples/tools
//! ```
//!
//! Then test with:
//!
//! ```bash
//! curl http://localhost:3000/v1/responses \
//!   -H "Content-Type: application/json" \
//!   -d '{
//!     "model": "Qwen/Qwen3-VL-235B-A22B-Instruct-FP8",
//!     "input": "What is the weather in London?",
//!     "tools": [{
//!       "type": "function",
//!       "name": "get_weather",
//!       "description": "Get weather for a location",
//!       "parameters": {
//!         "type": "object",
//!         "properties": { "location": {"type": "string"} },
//!         "required": ["location"]
//!       }
//!     }]
//!   }'
//! ```

use async_trait::async_trait;
use clap::Parser;
use onwards::{
    AppState, ToolError, ToolExecutor, build_metrics_layer_and_handle, build_metrics_router,
    build_router, create_openai_sanitizer,
    strict::build_strict_router,
    target::{Targets, WatchedFile},
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::task::JoinSet;
use tracing::{debug, error, info};

// ---------------------------------------------------------------------------
// ScriptExecutor — a ToolExecutor backed by a directory of bash scripts
// ---------------------------------------------------------------------------

struct ScriptExecutor {
    tools_dir: PathBuf,
    timeout: Duration,
}

impl ScriptExecutor {
    fn new(tools_dir: PathBuf) -> Self {
        Self {
            tools_dir,
            timeout: Duration::from_secs(30),
        }
    }

    /// Validate that a tool name contains only safe characters.
    /// Prevents path traversal and shell metacharacter injection.
    fn is_valid_tool_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    fn script_path(&self, tool_name: &str) -> Option<PathBuf> {
        if Self::is_valid_tool_name(tool_name) {
            Some(self.tools_dir.join(format!("{tool_name}.sh")))
        } else {
            None
        }
    }
}

#[async_trait]
impl ToolExecutor for ScriptExecutor {
    fn can_handle(&self, tool_name: &str) -> bool {
        self.script_path(tool_name)
            .is_some_and(|path| path.is_file())
    }

    async fn execute(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        arguments: &serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let path = self.script_path(tool_name).ok_or_else(|| {
            ToolError::InvalidArguments(format!(
                "Invalid tool name '{tool_name}': only alphanumeric, hyphen, underscore allowed"
            ))
        })?;

        if !path.is_file() {
            return Err(ToolError::NotFound(format!(
                "No script at {}",
                path.display()
            )));
        }

        debug!(tool_name, tool_call_id, "Executing tool script");

        // Spawn bash with a clean environment — no inherited secrets
        let mut child = Command::new("bash")
            .arg(&path)
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::ExecutionError(format!("spawn failed: {e}")))?;

        // Write arguments as JSON to stdin
        if let Some(mut stdin) = child.stdin.take() {
            let json = serde_json::to_vec(arguments)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            stdin
                .write_all(&json)
                .await
                .map_err(|e| ToolError::ExecutionError(format!("stdin write: {e}")))?;
        }

        // Wait with timeout
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| ToolError::Timeout(format!("timed out after {:?}", self.timeout)))?
            .map_err(|e| ToolError::ExecutionError(format!("wait: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ToolError::ExecutionError(stderr.into_owned()));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stdout = stdout.trim();

        // Try JSON, fall back to plain string
        Ok(serde_json::from_str(stdout)
            .unwrap_or_else(|_| serde_json::Value::String(stdout.to_string())))
    }
}

// ---------------------------------------------------------------------------
// CLI config — same as onwards, plus --tools-dir
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Parser)]
#[command(
    name = "script_tools",
    about = "Onwards proxy with script-based tool execution"
)]
struct Config {
    #[arg(short = 'p', long, default_value_t = 3000)]
    port: u16,

    #[arg(long, default_value_t = 9090)]
    metrics_port: u16,

    #[arg(short = 'm', long, default_value_t = true)]
    metrics: bool,

    #[arg(short = 'f', long)]
    targets: PathBuf,

    #[arg(short = 'w', long, default_value_t = true)]
    watch: bool,

    #[arg(long, default_value = "onwards")]
    metrics_prefix: String,

    /// Directory containing tool scripts (each <name>.sh becomes a callable tool)
    #[arg(long)]
    tools_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// main — full onwards proxy with ScriptExecutor wired in
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::parse();
    info!("Starting AI Gateway with config: {:?}", config);

    if !config.targets.exists() {
        anyhow::bail!("Config file '{}' does not exist", config.targets.display());
    }

    let targets = Targets::from_config_file(&config.targets).await?;
    let strict_mode = targets.strict_mode;

    if config.watch {
        targets.receive_updates(WatchedFile(config.targets)).await?;
    }

    let mut serves = JoinSet::new();

    let prometheus_layer = if config.metrics {
        let (prometheus_layer, prometheus_handle) =
            build_metrics_layer_and_handle(config.metrics_prefix);
        let metrics_router = build_metrics_router(prometheus_handle);
        let bind_addr = format!("0.0.0.0:{}", config.metrics_port);
        let listener = TcpListener::bind(&bind_addr).await?;
        serves.spawn(axum::serve(listener, metrics_router).into_future());
        info!("Metrics endpoint enabled on {}", bind_addr);
        Some(prometheus_layer)
    } else {
        info!("Metrics endpoint disabled");
        None
    };

    // Wire in the script-based tool executor
    let executor = Arc::new(ScriptExecutor::new(config.tools_dir.clone()));
    info!("Tool executor loaded from {}", config.tools_dir.display());

    let app_state = AppState::new(targets)
        .with_response_transform(create_openai_sanitizer())
        .with_tool_executor(executor);

    let mut router = if strict_mode {
        info!("Strict mode enabled - using typed request validation");
        build_strict_router(app_state)
    } else {
        build_router(app_state)
    };

    if let Some(prometheus_layer) = prometheus_layer {
        router = router.layer(prometheus_layer);
    }

    let bind_addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&bind_addr).await?;
    serves.spawn(axum::serve(listener, router).into_future());
    info!("AI Gateway listening on {}", bind_addr);

    if let Some(result) = serves.join_next().await {
        result?.map_err(anyhow::Error::from)
    } else {
        error!("No server tasks were spawned");
        Err(anyhow::anyhow!("No server tasks were spawned"))
    }
}
