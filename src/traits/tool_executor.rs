//! Tool execution trait for server-side tool handling
//!
//! Implement this trait to enable automatic tool execution during agent loops.
//! When the model calls a tool that your executor can handle, the adapter will
//! execute it locally and feed the result back to the model.

use async_trait::async_trait;
use std::fmt;

/// Error type for tool execution
#[derive(Debug, Clone)]
pub enum ToolError {
    /// Tool is not recognized/supported
    NotFound(String),
    /// Tool execution failed
    ExecutionError(String),
    /// Invalid arguments provided to tool
    InvalidArguments(String),
    /// Tool execution timed out
    Timeout(String),
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::NotFound(name) => write!(f, "Tool not found: {}", name),
            ToolError::ExecutionError(msg) => write!(f, "Tool execution error: {}", msg),
            ToolError::InvalidArguments(msg) => write!(f, "Invalid arguments: {}", msg),
            ToolError::Timeout(msg) => write!(f, "Tool timeout: {}", msg),
        }
    }
}

impl std::error::Error for ToolError {}

/// Trait for executing tools server-side during agent loops.
///
/// Implement this to enable automatic tool execution in the Open Responses adapter.
/// When the model returns a tool call that your executor can handle (via `can_handle`),
/// the adapter will:
/// 1. Call `execute()` with the tool call details
/// 2. Feed the result back to the model
/// 3. Continue until the model produces a final response
///
/// Tools that `can_handle` returns `false` for will be:
/// - Returned to the client with `requires_action` status, OR
/// - Passed through to the upstream if it supports hosted tools
///
/// # Example
///
/// ```ignore
/// use onwards::traits::{ToolExecutor, ToolError};
/// use async_trait::async_trait;
///
/// struct WeatherTool;
///
/// #[async_trait]
/// impl ToolExecutor for WeatherTool {
///     async fn execute(
///         &self,
///         tool_name: &str,
///         tool_call_id: &str,
///         arguments: &serde_json::Value,
///     ) -> Result<serde_json::Value, ToolError> {
///         if tool_name == "get_weather" {
///             let location = arguments["location"].as_str()
///                 .ok_or_else(|| ToolError::InvalidArguments("missing location".into()))?;
///             // Fetch weather data...
///             Ok(serde_json::json!({"temperature": 72, "conditions": "sunny"}))
///         } else {
///             Err(ToolError::NotFound(tool_name.to_string()))
///         }
///     }
///
///     fn can_handle(&self, tool_name: &str) -> bool {
///         tool_name == "get_weather"
///     }
/// }
/// ```
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Execute a tool call and return the result.
    ///
    /// # Arguments
    /// * `tool_name` - The name of the tool to execute
    /// * `tool_call_id` - The unique ID for this tool call (for correlation)
    /// * `arguments` - The arguments passed to the tool (as JSON)
    ///
    /// # Returns
    /// * `Ok(Value)` - The tool's output as JSON
    /// * `Err(ToolError)` - If execution failed
    async fn execute(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        arguments: &serde_json::Value,
    ) -> Result<serde_json::Value, ToolError>;

    /// Check if this executor can handle the given tool.
    ///
    /// Return `true` if this executor should handle the tool, `false` to
    /// let it be handled by the client or upstream.
    fn can_handle(&self, tool_name: &str) -> bool;
}

/// No-op implementation that handles no tools.
///
/// This is the default implementation used when no executor is configured.
/// All tool calls will be returned to the client with `requires_action` status.
#[derive(Debug, Clone, Default)]
pub struct NoOpToolExecutor;

#[async_trait]
impl ToolExecutor for NoOpToolExecutor {
    async fn execute(
        &self,
        tool_name: &str,
        _tool_call_id: &str,
        _arguments: &serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        Err(ToolError::NotFound(tool_name.to_string()))
    }

    fn can_handle(&self, _tool_name: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_noop_executor_handles_nothing() {
        let executor = NoOpToolExecutor;
        assert!(!executor.can_handle("any_tool"));
    }

    #[tokio::test]
    async fn test_noop_executor_returns_not_found() {
        let executor = NoOpToolExecutor;
        let result = executor
            .execute("test_tool", "call_123", &serde_json::json!({}))
            .await;
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }
}
