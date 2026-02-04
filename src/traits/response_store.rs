//! Response storage trait for stateful conversations
//!
//! Implement this trait to enable `previous_response_id` support in the Open Responses adapter.
//! The adapter will use this store to persist responses and retrieve context for follow-up requests.

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

/// Trait for storing and retrieving response context.
///
/// Implement this to enable `previous_response_id` support in the Open Responses adapter.
/// When a client sends a request with `previous_response_id`, the adapter will use this
/// store to retrieve the conversation context.
///
/// # Example
///
/// ```ignore
/// use onwards::traits::{ResponseStore, StoreError};
/// use async_trait::async_trait;
/// use std::collections::HashMap;
/// use std::sync::RwLock;
///
/// struct InMemoryStore {
///     responses: RwLock<HashMap<String, serde_json::Value>>,
/// }
///
/// #[async_trait]
/// impl ResponseStore for InMemoryStore {
///     async fn store(&self, response: &serde_json::Value) -> Result<String, StoreError> {
///         let id = uuid::Uuid::new_v4().to_string();
///         self.responses.write().unwrap().insert(id.clone(), response.clone());
///         Ok(id)
///     }
///
///     async fn get_context(&self, response_id: &str) -> Result<Option<serde_json::Value>, StoreError> {
///         Ok(self.responses.read().unwrap().get(response_id).cloned())
///     }
/// }
/// ```
#[async_trait]
pub trait ResponseStore: Send + Sync {
    /// Store a response and return its ID for future reference.
    ///
    /// # Arguments
    /// * `response` - The complete response object to store
    ///
    /// # Returns
    /// * `Ok(String)` - The unique ID assigned to this response
    /// * `Err(StoreError)` - If storage failed
    async fn store(&self, response: &serde_json::Value) -> Result<String, StoreError>;

    /// Retrieve the context (items/messages) for a previous response.
    ///
    /// # Arguments
    /// * `response_id` - The ID returned from a previous `store()` call
    ///
    /// # Returns
    /// * `Ok(Some(Value))` - The stored context if found
    /// * `Ok(None)` - If no response exists with this ID
    /// * `Err(StoreError)` - If retrieval failed
    async fn get_context(&self, response_id: &str) -> Result<Option<serde_json::Value>, StoreError>;
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

    async fn get_context(&self, response_id: &str) -> Result<Option<serde_json::Value>, StoreError> {
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
}
