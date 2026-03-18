use crate::session_affinity::{AffinityEntry, AffinityKey, SessionAffinityStore};
use dashmap::DashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct InMemorySessionAffinityStore {
    entries: Arc<DashMap<AffinityKey, StoredEntry>>,
}

#[derive(Debug, Clone)]
struct StoredEntry {
    provider_id: String,
    expires_at: Instant,
    ttl: Duration,
}

impl InMemorySessionAffinityStore {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(DashMap::new()),
        }
    }
}

impl Default for InMemorySessionAffinityStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionAffinityStore for InMemorySessionAffinityStore {
    fn get(&self, key: &AffinityKey) -> Option<AffinityEntry> {
        let entry = self.entries.get(key)?;
        if Instant::now() >= entry.expires_at {
            drop(entry);
            self.entries.remove(key);
            return None;
        }

        Some(AffinityEntry {
            provider_id: entry.provider_id.clone(),
        })
    }

    fn insert(&self, key: AffinityKey, entry: AffinityEntry, ttl: Duration) {
        self.entries.insert(
            key,
            StoredEntry {
                provider_id: entry.provider_id,
                expires_at: Instant::now() + ttl,
                ttl,
            },
        );
    }

    fn delete(&self, key: &AffinityKey) {
        self.entries.remove(key);
    }

    fn touch(&self, key: &AffinityKey, ttl: Duration) {
        if let Some(mut entry) = self.entries.get_mut(key) {
            let effective_ttl = if ttl.is_zero() { entry.ttl } else { ttl };
            entry.ttl = effective_ttl;
            entry.expires_at = Instant::now() + effective_ttl;
        }
    }

    fn remove_stale_for_model(&self, model: &str, valid_provider_ids: &HashSet<String>) -> usize {
        let keys_to_remove: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| {
                entry.key().model == model && !valid_provider_ids.contains(&entry.provider_id)
            })
            .map(|entry| entry.key().clone())
            .collect();

        let removed = keys_to_remove.len();
        for key in keys_to_remove {
            self.entries.remove(&key);
        }

        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_entries_are_evicted() {
        let store = InMemorySessionAffinityStore::new();
        let key = AffinityKey {
            model: "gpt-4o".into(),
            session_fingerprint: "abc".into(),
        };
        store.insert(
            key.clone(),
            AffinityEntry {
                provider_id: "p1".into(),
            },
            Duration::from_millis(1),
        );
        std::thread::sleep(Duration::from_millis(5));
        assert!(store.get(&key).is_none());
    }

    #[test]
    fn remove_stale_for_model_prunes_invalid_bindings_only() {
        let store = InMemorySessionAffinityStore::new();
        let keep_key = AffinityKey {
            model: "gpt-4o".into(),
            session_fingerprint: "keep".into(),
        };
        let remove_key = AffinityKey {
            model: "gpt-4o".into(),
            session_fingerprint: "remove".into(),
        };
        let other_model_key = AffinityKey {
            model: "claude-3".into(),
            session_fingerprint: "other".into(),
        };

        store.insert(
            keep_key.clone(),
            AffinityEntry {
                provider_id: "p-valid".into(),
            },
            Duration::from_secs(60),
        );
        store.insert(
            remove_key.clone(),
            AffinityEntry {
                provider_id: "p-stale".into(),
            },
            Duration::from_secs(60),
        );
        store.insert(
            other_model_key.clone(),
            AffinityEntry {
                provider_id: "p-stale".into(),
            },
            Duration::from_secs(60),
        );

        let valid_provider_ids = HashSet::from([String::from("p-valid")]);
        let removed = store.remove_stale_for_model("gpt-4o", &valid_provider_ids);

        assert_eq!(removed, 1);
        assert!(store.get(&keep_key).is_some());
        assert!(store.get(&remove_key).is_none());
        assert!(store.get(&other_model_key).is_some());
    }
}
