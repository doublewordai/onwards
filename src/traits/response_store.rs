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

/// Trait for response lifecycle management and stateful conversations.
///
/// Implement this to enable:
/// - **Lifecycle tracking**: `create_pending` before proxying, `complete`/`fail` after.
///   This makes every response retrievable via `GET /v1/responses/{id}`.
/// - **Conversation state**: `get_context` retrieves previous response context for
///   `previous_response_id` support.
/// - **Legacy storage**: `store` persists a completed response (used by the adapter
///   after constructing the final response).
///
/// All methods have default no-op implementations, so existing `ResponseStore`
/// implementations continue to compile without changes.
#[async_trait]
pub trait ResponseStore: Send + Sync {
    /// Pre-request: create a response record before proxying to the provider.
    ///
    /// Called by the responses handler before forwarding the request. The returned
    /// ID becomes the canonical response ID (e.g. `resp_<uuid>`).
    ///
    /// The record should be in a "processing" or equivalent state so that
    /// `GET /v1/responses/{id}` returns `status: in_progress` while the request
    /// is in flight.
    async fn create_pending(
        &self,
        request: &serde_json::Value,
        model: &str,
        endpoint: &str,
    ) -> Result<String, StoreError> {
        let _ = (request, model, endpoint);
        Ok(format!("resp_{}", uuid_simple()))
    }

    /// Post-request: mark the response as completed with the response body.
    async fn complete(
        &self,
        response_id: &str,
        response: &serde_json::Value,
        status_code: u16,
    ) -> Result<(), StoreError> {
        let _ = (response_id, response, status_code);
        Ok(())
    }

    /// Post-request: mark the response as failed.
    async fn fail(
        &self,
        response_id: &str,
        error: &str,
    ) -> Result<(), StoreError> {
        let _ = (response_id, error);
        Ok(())
    }

    /// Retrieve a response by ID (for `GET /v1/responses/{id}`).
    async fn get(
        &self,
        response_id: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
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
pub(crate) mod tests {
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
    async fn test_noop_create_pending_returns_resp_prefix() {
        let store = NoOpResponseStore;
        let request = serde_json::json!({"model": "gpt-4o", "input": "hello"});
        let id = store
            .create_pending(&request, "gpt-4o", "/v1/responses")
            .await
            .unwrap();
        assert!(
            id.starts_with("resp_"),
            "create_pending should return resp_ prefixed ID, got: {id}"
        );
    }

    #[tokio::test]
    async fn test_noop_complete_is_noop() {
        let store = NoOpResponseStore;
        let result = store
            .complete("resp_123", &serde_json::json!({"status": "completed"}), 200)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_noop_fail_is_noop() {
        let store = NoOpResponseStore;
        let result = store.fail("resp_123", "some error").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_noop_get_returns_none() {
        let store = NoOpResponseStore;
        let result = store.get("resp_123").await.unwrap();
        assert!(result.is_none());
    }

    /// A test store that tracks lifecycle calls for verifying handler integration.
    /// Exported for use in integration tests across the crate.
    #[derive(Default)]
    pub struct TrackingResponseStore {
        inner: std::sync::Mutex<TrackingState>,
    }

    #[derive(Default)]
    struct TrackingState {
        pending: Vec<(String, String, String)>, // (id, model, endpoint)
        completed: Vec<(String, u16)>,          // (id, status_code)
        failed: Vec<(String, String)>,          // (id, error)
        stored: Vec<serde_json::Value>,
    }

    #[async_trait]
    impl ResponseStore for TrackingResponseStore {
        async fn create_pending(
            &self,
            _request: &serde_json::Value,
            model: &str,
            endpoint: &str,
        ) -> Result<String, StoreError> {
            let id = format!("resp_{}", uuid_simple());
            self.inner.lock().unwrap().pending.push((
                id.clone(),
                model.to_string(),
                endpoint.to_string(),
            ));
            Ok(id)
        }

        async fn complete(
            &self,
            response_id: &str,
            _response: &serde_json::Value,
            status_code: u16,
        ) -> Result<(), StoreError> {
            self.inner
                .lock()
                .unwrap()
                .completed
                .push((response_id.to_string(), status_code));
            Ok(())
        }

        async fn fail(&self, response_id: &str, error: &str) -> Result<(), StoreError> {
            self.inner
                .lock()
                .unwrap()
                .failed
                .push((response_id.to_string(), error.to_string()));
            Ok(())
        }

        async fn store(&self, response: &serde_json::Value) -> Result<String, StoreError> {
            self.inner.lock().unwrap().stored.push(response.clone());
            Ok(format!("noop_{}", uuid_simple()))
        }

        async fn get_context(
            &self,
            _response_id: &str,
        ) -> Result<Option<serde_json::Value>, StoreError> {
            Ok(None)
        }
    }

    impl TrackingResponseStore {
        pub fn pending_count(&self) -> usize {
            self.inner.lock().unwrap().pending.len()
        }

        pub fn completed_count(&self) -> usize {
            self.inner.lock().unwrap().completed.len()
        }

        pub fn failed_count(&self) -> usize {
            self.inner.lock().unwrap().failed.len()
        }

        pub fn last_pending_id(&self) -> Option<String> {
            self.inner.lock().unwrap().pending.last().map(|p| p.0.clone())
        }

        pub fn last_completed_id(&self) -> Option<String> {
            self.inner.lock().unwrap().completed.last().map(|c| c.0.clone())
        }
    }
}
