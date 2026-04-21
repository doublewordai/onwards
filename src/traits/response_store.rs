//! Response storage trait for stateful conversations and response lifecycle tracking.
//!
//! Implement this trait to enable:
//! - `previous_response_id` support in the Open Responses adapter
//! - Response lifecycle tracking (create → complete/fail) for `GET /v1/responses/{id}`
//! - `background` mode where responses are created before proxying and polled later

use async_trait::async_trait;
use std::fmt;

/// Error type for response store operations
#[derive(Debug, Clone)]
pub enum StoreError {
    /// Response not found with the given ID
    NotFound(String),
    /// Storage backend error
    StorageError(String),
    /// Serialization/deserialization error
    SerializationError(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotFound(id) => write!(f, "Response not found: {}", id),
            StoreError::StorageError(msg) => write!(f, "Storage error: {}", msg),
            StoreError::SerializationError(msg) => write!(f, "Serialization error: {}", msg),
        }
    }
}

impl std::error::Error for StoreError {}

/// Trait for response storage and retrieval.
///
/// Implement this to enable:
/// - **Conversation state**: `get_context` retrieves previous response context for
///   `previous_response_id` support in the adapter.
/// - **Response retrieval**: `get` retrieves a response by ID for `GET /v1/responses/{id}`.
/// - **Adapter storage**: `store` persists a completed response (used by the adapter
///   after constructing the final response).
///
/// Request lifecycle management (creating pending records, completing/failing them)
/// is handled externally (e.g. by dwctl middleware and outlet handlers), not by this trait.
#[async_trait]
pub trait ResponseStore: Send + Sync {
    /// Retrieve a response by ID (for `GET /v1/responses/{id}`).
    async fn get(&self, response_id: &str) -> Result<Option<serde_json::Value>, StoreError> {
        let _ = response_id;
        Ok(None)
    }

    /// Store a completed response and return its ID for future reference.
    ///
    /// Used by the adapter after constructing the final `ResponsesResponse`.
    async fn store(&self, response: &serde_json::Value) -> Result<String, StoreError>;

    /// Retrieve the context (items/messages) for a previous response.
    ///
    /// Used to resolve `previous_response_id` — the adapter extracts output items
    /// from the stored response and prepends them as conversation context.
    async fn get_context(&self, response_id: &str)
    -> Result<Option<serde_json::Value>, StoreError>;
}

/// No-op implementation that always returns None.
///
/// This is the default implementation used when no store is configured.
/// It makes the adapter stateless - `previous_response_id` requests will fail
/// with an appropriate error.
#[derive(Debug, Clone, Default)]
pub struct NoOpResponseStore;

#[async_trait]
impl ResponseStore for NoOpResponseStore {
    async fn store(&self, _response: &serde_json::Value) -> Result<String, StoreError> {
        // Generate a dummy ID but don't actually store anything
        Ok(format!("noop_{}", uuid_simple()))
    }

    async fn get_context(
        &self,
        response_id: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        // Always return None - no state is preserved
        tracing::debug!(
            response_id = %response_id,
            "NoOpResponseStore: previous_response_id not supported without a configured store"
        );
        Ok(None)
    }
}

/// Simple UUID-like string generator (avoids adding uuid dependency)
fn uuid_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:032x}", timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_noop_store_returns_none() {
        let store = NoOpResponseStore;
        let result = store.get_context("any_id").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_noop_store_generates_id() {
        let store = NoOpResponseStore;
        let response = serde_json::json!({"test": "value"});
        let id = store.store(&response).await.unwrap();
        assert!(id.starts_with("noop_"));
    }

    #[tokio::test]
    async fn test_noop_get_returns_none() {
        let store = NoOpResponseStore;
        let result = store.get("resp_123").await.unwrap();
        assert!(result.is_none());
    }
}
