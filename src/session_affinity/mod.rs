pub mod context;
pub mod key;
pub mod metrics;
pub mod store;

use std::collections::HashSet;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AffinityKey {
    pub model: String,
    pub session_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffinityEntry {
    pub provider_id: String,
}

pub trait SessionAffinityStore: Send + Sync + std::fmt::Debug {
    fn get(&self, key: &AffinityKey) -> Option<AffinityEntry>;
    fn insert(&self, key: AffinityKey, entry: AffinityEntry, ttl: Duration);
    fn delete(&self, key: &AffinityKey);
    fn touch(&self, key: &AffinityKey, ttl: Duration);

    /// Remove bindings for `model` whose provider is no longer valid in the
    /// current process. In-memory stores can eagerly clean up on hot reload;
    /// distributed stores may choose to keep the default no-op and rely on
    /// request-path lazy cleanup to avoid cross-instance races.
    fn remove_stale_for_model(&self, _model: &str, _valid_provider_ids: &HashSet<String>) -> usize {
        0
    }
}
