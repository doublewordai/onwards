//! Target management and configuration
//!
//! This module handles the configuration and management of proxy targets - the
//! upstream services that requests are forwarded to. Targets are dynamically
//! loaded from configuration files and support hot-reloading when files change.
//!
//! ## Load Balancing
//!
//! Targets support load balancing across multiple providers. A single alias can
//! route to multiple downstream providers with different API keys, weights, and
//! rate limits. The configuration is backwards compatible - a single object is
//! treated as a list with one provider.
use crate::auth::KeySet;
use crate::load_balancer::{Provider, ProviderPool};
use anyhow::anyhow;
use async_trait::async_trait;
use bon::Builder;
use dashmap::DashMap;
use futures_util::{Stream, StreamExt};
use governor::{DefaultDirectRateLimiter, Quota};
use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, num::NonZeroU32, path::PathBuf, pin::Pin, sync::Arc};
use tokio::sync::{Semaphore, SemaphorePermit, mpsc};
use tokio_stream::wrappers::{ReceiverStream, WatchStream};
use tracing::{debug, error, info, trace};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitParameters {
    pub requests_per_second: NonZeroU32,
    pub burst_size: Option<NonZeroU32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcurrencyLimitParameters {
    pub max_concurrent_requests: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
pub struct TargetSpec {
    pub url: Url,
    pub keys: Option<KeySet>,
    pub onwards_key: Option<String>,
    pub onwards_model: Option<String>,
    pub rate_limit: Option<RateLimitParameters>,
    pub concurrency_limit: Option<ConcurrencyLimitParameters>,
    #[serde(default)]
    pub upstream_auth_header_name: Option<String>,
    #[serde(default)]
    pub upstream_auth_header_prefix: Option<String>,

    /// Custom headers to include in responses (e.g., pricing, metadata)
    #[serde(default)]
    pub response_headers: Option<HashMap<String, String>>,

    /// Weight for load balancing (higher = more traffic). Defaults to 1.
    #[serde(default = "default_weight")]
    #[builder(default = default_weight())]
    pub weight: u32,
}

fn default_weight() -> u32 {
    1
}

/// A target specification that can be either a single provider or a list of providers.
/// This enables backwards-compatible configuration where a single object is treated
/// as a list with one provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TargetSpecOrList {
    /// Multiple providers for load balancing
    List(Vec<TargetSpec>),
    /// Single provider (backwards compatible)
    Single(TargetSpec),
}

impl TargetSpecOrList {
    /// Convert to a list of target specs
    pub fn into_list(self) -> Vec<TargetSpec> {
        match self {
            TargetSpecOrList::List(list) => list,
            TargetSpecOrList::Single(spec) => vec![spec],
        }
    }
}

/// Normalizes a URL to ensure it has a trailing slash
///
/// This is critical for correct path joining behavior. The url::Url::join() method
/// treats URLs without trailing slashes as having a "file" component that gets replaced:
/// - Without trailing slash: `https://api.example.com/v1` + `messages` = `https://api.example.com/messages` (wrong)
/// - With trailing slash: `https://api.example.com/v1/` + `messages` = `https://api.example.com/v1/messages` (correct)
fn normalize_url(mut url: Url) -> Url {
    let path = url.path();
    if !path.ends_with('/') {
        url.set_path(&format!("{}/", path));
    }
    url
}

impl From<TargetSpec> for Target {
    fn from(value: TargetSpec) -> Self {
        Target {
            url: normalize_url(value.url),
            keys: value.keys,
            onwards_key: value.onwards_key,
            onwards_model: value.onwards_model,
            limiter: value.rate_limit.map(|rl| {
                Arc::new(governor::RateLimiter::direct(
                    Quota::per_second(rl.requests_per_second)
                        .allow_burst(rl.burst_size.unwrap_or(rl.requests_per_second)),
                )) as Arc<dyn RateLimiter>
            }),
            concurrency_limiter: value.concurrency_limit.map(|cl| {
                SemaphoreConcurrencyLimiter::new(cl.max_concurrent_requests)
                    as Arc<dyn ConcurrencyLimiter>
            }),
            upstream_auth_header_name: value.upstream_auth_header_name,
            upstream_auth_header_prefix: value.upstream_auth_header_prefix,
            response_headers: value.response_headers,
        }
    }
}

pub trait RateLimiter: std::fmt::Debug + Send + Sync {
    fn check(&self) -> Result<(), ()>;
}

impl RateLimiter for DefaultDirectRateLimiter {
    fn check(&self) -> Result<(), ()> {
        self.check().map_err(|_| ())
    }
}

/// RAII guard that holds a concurrency permit
/// When dropped, the permit is automatically released back to the semaphore
#[derive(Debug)]
pub struct ConcurrencyGuard {
    _permit: SemaphorePermit<'static>,
}

#[async_trait]
pub trait ConcurrencyLimiter: std::fmt::Debug + Send + Sync {
    /// Try to acquire a concurrency permit
    /// Returns Ok(guard) if permit was acquired, Err(()) if at capacity
    async fn acquire(&self) -> Result<ConcurrencyGuard, ()>;
}

/// Semaphore-based concurrency limiter
#[derive(Debug)]
pub struct SemaphoreConcurrencyLimiter {
    semaphore: &'static Semaphore,
}

impl SemaphoreConcurrencyLimiter {
    pub fn new(max_concurrent: usize) -> Arc<Self> {
        Arc::new(Self {
            semaphore: Box::leak(Box::new(Semaphore::new(max_concurrent))),
        })
    }
}

#[async_trait]
impl ConcurrencyLimiter for SemaphoreConcurrencyLimiter {
    async fn acquire(&self) -> Result<ConcurrencyGuard, ()> {
        // Try to acquire without blocking - fail immediately if at capacity
        match self.semaphore.try_acquire() {
            Ok(permit) => Ok(ConcurrencyGuard { _permit: permit }),
            Err(_) => Err(()),
        }
    }
}

/// A target represents a destination for requests, specified by its URL.
///
/// ## Validating incoming requests
/// A target can have a set of keys associated with it, one of which will be required in the Authorization: Bearer header.
///
/// ## Forwarding requests
/// A target can have a onwards_key and a onwards_model. The key is put into the Authorization:
/// Bearer {} header of the request. The onwards_model is used to determine which model to put in
/// the json body when forwarding the request.
#[derive(Debug, Clone, Builder)]
#[builder(derive(Clone))]
pub struct Target {
    pub url: Url,
    pub keys: Option<KeySet>,
    pub onwards_key: Option<String>,
    pub onwards_model: Option<String>,
    pub limiter: Option<Arc<dyn RateLimiter>>,
    pub concurrency_limiter: Option<Arc<dyn ConcurrencyLimiter>>,
    pub upstream_auth_header_name: Option<String>,
    pub upstream_auth_header_prefix: Option<String>,
    /// Custom headers to include in responses (e.g., pricing, metadata)
    pub response_headers: Option<HashMap<String, String>>,
}

impl Target {
    /// Convert this target into a single-provider pool with weight 1
    /// This is a convenience method for backwards compatibility and testing
    pub fn into_pool(self) -> ProviderPool {
        ProviderPool::single(self, 1)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyDefinition {
    pub key: String,
    pub rate_limit: Option<RateLimitParameters>,
    pub concurrency_limit: Option<ConcurrencyLimitParameters>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
pub struct Auth {
    /// global keys are merged with the per-target keys.
    global_keys: KeySet,
    /// key definitions with their actual keys and per-key rate limits
    pub key_definitions: Option<HashMap<String, KeyDefinition>>,
}

/// The config file contains a map of target names to targets.
/// Each target can be a single provider or a list of providers for load balancing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    pub targets: HashMap<String, TargetSpecOrList>,
    pub auth: Option<Auth>,
}

/// The live-updating collection of targets.
#[derive(Debug, Clone)]
pub struct Targets {
    /// Map of alias names to provider pools (supports load balancing)
    pub targets: Arc<DashMap<String, ProviderPool>>,
    /// Direct rate limiters per actual API key (actual key -> rate limiter)
    pub key_rate_limiters: Arc<DashMap<String, Arc<DefaultDirectRateLimiter>>>,
    /// Concurrency limiters per actual API key (actual key -> concurrency limiter)
    pub key_concurrency_limiters: Arc<DashMap<String, Arc<dyn ConcurrencyLimiter>>>,
}

#[async_trait]
pub trait TargetsStream {
    // TODO: Replace Into<anyhow::Error> with a custom error type using thiserror
    type Error: Into<anyhow::Error> + Send + Sync + 'static;

    /// Returns a stream of Targets updates
    async fn stream(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Targets, Self::Error>> + Send>>, Self::Error>;
}

pub struct WatchedFile(pub PathBuf);

#[async_trait]
impl TargetsStream for WatchedFile {
    type Error = anyhow::Error;

    /// Watches a file for changes and returns a stream of Targets updates.
    async fn stream(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Targets, Self::Error>> + Send>>, Self::Error> {
        // TODO(fergus): think about buffers
        let (targets_tx, targets_rx) = mpsc::channel(100);
        let (file_tx, mut file_rx) = mpsc::channel(100);

        let mut watcher = RecommendedWatcher::new(
            move |res| {
                let _ = file_tx.blocking_send(res);
            },
            NotifyConfig::default(),
        )?;

        watcher.watch(&self.0, RecursiveMode::NonRecursive)?;

        let config_path = self.0.clone();
        // TODO(fergus): stash the handle to this thread somewhere
        tokio::spawn(async move {
            while let Some(res) = file_rx.recv().await {
                match res {
                    Ok(event) => {
                        if event.kind.is_modify() {
                            info!("Config file changed, reloading targets...");
                            match Targets::from_config_file(&config_path).await {
                                Ok(new_targets) => {
                                    if targets_tx.send(Ok(new_targets)).await.is_err() {
                                        break; // Receiver dropped
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to reload config: {}", e);
                                    if targets_tx.send(Err(e)).await.is_err() {
                                        break; // Receiver dropped
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Watch error: {}", e);
                        if targets_tx
                            .send(Err(anyhow!("Watch error: {}", e)))
                            .await
                            .is_err()
                        {
                            break; // Receiver dropped
                        }
                    }
                }
            }
        });

        // Keep the watcher alive
        std::mem::forget(watcher);

        Ok(Box::pin(ReceiverStream::new(targets_rx)))
    }
}

/// A watch channel-based implementation of TargetsStream for receiving configuration updates
pub struct WatchTargetsStream {
    receiver: tokio::sync::watch::Receiver<Targets>,
}

impl WatchTargetsStream {
    pub fn new(receiver: tokio::sync::watch::Receiver<Targets>) -> Self {
        Self { receiver }
    }
}

#[async_trait]
impl TargetsStream for WatchTargetsStream {
    type Error = std::convert::Infallible;

    async fn stream(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Targets, Self::Error>> + Send>>, Self::Error> {
        let stream = WatchStream::from_changes(self.receiver.clone()).map(Ok);
        Ok(Box::pin(stream))
    }
}

impl Targets {
    pub async fn from_config_file(config_path: &PathBuf) -> Result<Self, anyhow::Error> {
        let contents = tokio::fs::read_to_string(config_path).await.map_err(|e| {
            anyhow!(
                "Failed to read config file {}: {}",
                config_path.display(),
                e
            )
        })?;

        let config_file: ConfigFile = serde_json::from_str(&contents).map_err(|e| {
            anyhow!(
                "Failed to parse config file {}: {}",
                config_path.display(),
                e
            )
        })?;

        let targets = Self::from_config(config_file)?;

        info!(
            "Loaded {} targets from {}",
            targets.targets.len(),
            config_path.display()
        );
        Ok(targets)
    }

    pub fn from_config(mut config_file: ConfigFile) -> Result<Self, anyhow::Error> {
        let (global_keys, key_definitions) = config_file
            .auth
            .take()
            .map(|auth| (auth.global_keys, auth.key_definitions.unwrap_or_default()))
            .unwrap_or_default();
        debug!("{} global keys configured", global_keys.len());
        debug!("{} key definitions configured", key_definitions.len());

        // Set up rate limiters keyed by actual API keys
        let key_rate_limiters = Arc::new(DashMap::new());
        let key_concurrency_limiters = Arc::new(DashMap::new());

        for (_key_id, key_def) in key_definitions {
            if let Some(ref rate_limit) = key_def.rate_limit {
                let limiter = Arc::new(governor::RateLimiter::direct(
                    Quota::per_second(rate_limit.requests_per_second).allow_burst(
                        rate_limit
                            .burst_size
                            .unwrap_or(rate_limit.requests_per_second),
                    ),
                ));
                // Map the actual API key to its rate limiter
                key_rate_limiters.insert(key_def.key.clone(), limiter);
            }
            if let Some(ref concurrency_limit) = key_def.concurrency_limit {
                let limiter: Arc<dyn ConcurrencyLimiter> =
                    SemaphoreConcurrencyLimiter::new(concurrency_limit.max_concurrent_requests);
                // Map the actual API key to its concurrency limiter
                key_concurrency_limiters.insert(key_def.key.clone(), limiter);
            }
        }

        let targets = Arc::new(DashMap::new());
        for (name, target_spec_or_list) in config_file.targets {
            let target_specs = target_spec_or_list.into_list();
            let mut providers = Vec::with_capacity(target_specs.len());

            for mut target_spec in target_specs {
                // Merge global keys with target-specific keys
                if let Some(ref mut keys) = target_spec.keys {
                    debug!(
                        "Target {}:{:?} has {} keys configured",
                        target_spec.url,
                        target_spec.onwards_model,
                        keys.len()
                    );
                    keys.extend(global_keys.clone());
                } else if !global_keys.is_empty() {
                    target_spec.keys = Some(global_keys.clone());
                }

                let weight = target_spec.weight;
                let target: Target = target_spec.into();
                providers.push(Provider { target, weight });
            }

            let pool = ProviderPool::new(providers);
            debug!(
                "Created provider pool '{}' with {} provider(s)",
                name,
                pool.len()
            );
            targets.insert(name, pool);
        }

        Ok(Targets {
            targets,
            key_rate_limiters,
            key_concurrency_limiters,
        })
    }

    /// Receives updates from a stream of targets and updates the internal targets map.
    pub async fn receive_updates<W>(&self, targets_stream: W) -> Result<(), W::Error>
    where
        W: TargetsStream + Send + 'static,
        W::Error: Into<anyhow::Error>,
    {
        let targets = Arc::clone(&self.targets);
        let key_rate_limiters = Arc::clone(&self.key_rate_limiters);
        let key_concurrency_limiters = Arc::clone(&self.key_concurrency_limiters);

        let mut stream = targets_stream.stream().await?;

        // TODO(fergus): stash the handle to this thread somewhere
        tokio::spawn(async move {
            while let Some(result) = stream.next().await {
                match result {
                    Ok(new_targets) => {
                        info!("Config file changed, updating targets...");
                        trace!("{:?}", new_targets);

                        // Update targets atomically
                        let current_target_keys: Vec<String> =
                            targets.iter().map(|entry| entry.key().clone()).collect();

                        // Do it like this for atomicity (if you delete and recreate, there's a
                        // moment with no targets during which requests can fail)

                        // Remove deleted targets
                        for key in current_target_keys {
                            if !new_targets.targets.contains_key(&key) {
                                targets.remove(&key);
                            }
                        }

                        // Insert/update targets
                        for entry in new_targets.targets.iter() {
                            targets.insert(entry.key().clone(), entry.value().clone());
                        }

                        // Update key rate limiters atomically (same pattern)
                        let current_rate_limiter_keys: Vec<String> = key_rate_limiters
                            .iter()
                            .map(|entry| entry.key().clone())
                            .collect();

                        // Remove deleted rate limiters
                        for key in current_rate_limiter_keys {
                            if !new_targets.key_rate_limiters.contains_key(&key) {
                                key_rate_limiters.remove(&key);
                            }
                        }

                        // Insert/update key rate limiters
                        for entry in new_targets.key_rate_limiters.iter() {
                            key_rate_limiters.insert(entry.key().clone(), entry.value().clone());
                        }

                        // Update key concurrency limiters atomically (same pattern)
                        let current_concurrency_limiter_keys: Vec<String> =
                            key_concurrency_limiters
                                .iter()
                                .map(|entry| entry.key().clone())
                                .collect();

                        // Remove deleted concurrency limiters
                        for key in current_concurrency_limiter_keys {
                            if !new_targets.key_concurrency_limiters.contains_key(&key) {
                                key_concurrency_limiters.remove(&key);
                            }
                        }

                        // Insert/update key concurrency limiters
                        for entry in new_targets.key_concurrency_limiters.iter() {
                            key_concurrency_limiters
                                .insert(entry.key().clone(), entry.value().clone());
                        }
                    }
                    Err(e) => {
                        let err: anyhow::Error = e.into();
                        error!("Failed to reload config: {}", err);
                    }
                }
            }
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::ConstantTimeString;
    use dashmap::DashMap;
    use std::collections::HashSet;
    use std::sync::Arc;
    pub struct MockConfigWatcher {
        configs: Vec<Result<Targets, String>>,
    }

    impl MockConfigWatcher {
        pub fn with_targets(targets_list: Vec<Targets>) -> Self {
            Self {
                configs: targets_list.into_iter().map(Ok).collect(),
            }
        }

        pub fn with_error(error: String) -> Self {
            Self {
                configs: vec![Err(error)],
            }
        }
    }

    #[async_trait]
    impl TargetsStream for MockConfigWatcher {
        type Error = anyhow::Error;

        async fn stream(
            &self,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<Targets, Self::Error>> + Send>>, Self::Error>
        {
            use tokio_stream::wrappers::ReceiverStream;

            let (tx, rx) = mpsc::channel(100);

            let configs = self.configs.clone();
            tokio::spawn(async move {
                for config in configs {
                    let result = config.map_err(|e| anyhow::anyhow!(e));
                    if tx.send(result).await.is_err() {
                        break; // Receiver dropped
                    }
                    // Small delay to simulate file watching
                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                }
            });

            Ok(Box::pin(ReceiverStream::new(rx)))
        }
    }

    fn create_test_targets(models: Vec<(&str, &str)>) -> Targets {
        let targets_map = Arc::new(DashMap::new());
        for (model, url) in models {
            targets_map.insert(
                model.to_string(),
                Target::builder()
                    .url(url.parse().unwrap())
                    .onwards_key(format!("key-{model}"))
                    .build()
                    .into_pool(),
            );
        }
        Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        }
    }

    #[tokio::test]
    async fn test_config_watcher_updates_targets() {
        // Create initial targets
        let initial_targets = create_test_targets(vec![("gpt-4", "https://api.openai.com")]);

        // Create new targets that will be "watched"
        let updated_targets = create_test_targets(vec![
            ("gpt-4", "https://api.openai.com"),
            ("claude-3", "https://api.anthropic.com"),
        ]);

        let mock_watcher = MockConfigWatcher::with_targets(vec![updated_targets]);

        // Start watching
        initial_targets.receive_updates(mock_watcher).await.unwrap();

        // Give some time for the watcher to process
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Verify targets were updated
        assert_eq!(initial_targets.targets.len(), 2);
        assert!(initial_targets.targets.contains_key("gpt-4"));
        assert!(initial_targets.targets.contains_key("claude-3"));
    }

    #[tokio::test]
    async fn test_config_watcher_removes_deleted_targets() {
        // Create initial targets with multiple models
        let initial_targets = create_test_targets(vec![
            ("gpt-4", "https://api.openai.com"),
            ("claude-3", "https://api.anthropic.com"),
        ]);

        // Create updated targets with only one model (remove claude-3)
        let updated_targets = create_test_targets(vec![("gpt-4", "https://api.openai.com")]);

        let mock_watcher = MockConfigWatcher::with_targets(vec![updated_targets]);

        // Start watching
        initial_targets.receive_updates(mock_watcher).await.unwrap();

        // Give some time for the watcher to process
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Verify claude-3 was removed but gpt-4 remains
        assert_eq!(initial_targets.targets.len(), 1);
        assert!(initial_targets.targets.contains_key("gpt-4"));
        assert!(!initial_targets.targets.contains_key("claude-3"));
    }

    #[tokio::test]
    async fn test_config_watcher_multiple_updates() {
        // Create initial empty targets
        let targets = Targets {
            targets: Arc::new(DashMap::new()),
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        // Create sequence of target updates
        let update1 = create_test_targets(vec![("gpt-4", "https://api.openai.com")]);
        let update2 = create_test_targets(vec![
            ("gpt-4", "https://api.openai.com"),
            ("claude-3", "https://api.anthropic.com"),
        ]);
        let update3 = create_test_targets(vec![("claude-3", "https://api.anthropic.com")]);

        let mock_watcher = MockConfigWatcher::with_targets(vec![update1, update2, update3]);

        // Start watching
        targets.receive_updates(mock_watcher).await.unwrap();

        // Give time for all updates to process
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Verify final state (should only have claude-3)
        assert_eq!(targets.targets.len(), 1);
        assert!(!targets.targets.contains_key("gpt-4"));
        assert!(targets.targets.contains_key("claude-3"));
    }

    #[tokio::test]
    async fn test_config_watcher_handles_errors() {
        // Create initial targets
        let targets = create_test_targets(vec![("gpt-4", "https://api.openai.com")]);

        let mock_watcher = MockConfigWatcher::with_error("Invalid config file".to_string());

        // Start watching - should not panic
        targets.receive_updates(mock_watcher).await.unwrap();

        // Give some time for the error to be processed
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Original targets should remain unchanged
        assert_eq!(targets.targets.len(), 1);
        assert!(targets.targets.contains_key("gpt-4"));
    }

    #[tokio::test]
    async fn test_config_watcher_updates_target_properties() {
        // Create initial targets
        let initial_targets = create_test_targets(vec![("gpt-4", "https://api.openai.com")]);

        // Create updated targets with different URL and key
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com/v2".parse().unwrap()) // Different URL
                .onwards_key("new-key".to_string()) // Different key
                .onwards_model("gpt-4-turbo".to_string()) // Added model_key
                .build()
                .into_pool(),
        );
        let updated_targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
        };

        let mock_watcher = MockConfigWatcher::with_targets(vec![updated_targets]);

        // Start watching
        initial_targets.receive_updates(mock_watcher).await.unwrap();

        // Give some time for the watcher to process
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Verify target properties were updated
        let pool = initial_targets.targets.get("gpt-4").unwrap();
        let target = pool.first_target().unwrap();
        // Note: URL normalization only happens during from_config, not direct Target creation
        assert_eq!(target.url.as_str(), "https://api.openai.com/v2");
        assert_eq!(target.onwards_key, Some("new-key".to_string()));
        assert_eq!(target.onwards_model, Some("gpt-4-turbo".to_string()));
    }

    #[test]
    fn test_from_config_merges_global_keys_with_target_keys() {
        let mut target_keys = HashSet::new();
        target_keys.insert(ConstantTimeString::from("target-key-1".to_string()));
        target_keys.insert(ConstantTimeString::from("target-key-2".to_string()));
        target_keys.insert(ConstantTimeString::from("shared-key".to_string()));

        let mut global_keys = HashSet::new();
        global_keys.insert(ConstantTimeString::from("global-key-1".to_string()));
        global_keys.insert(ConstantTimeString::from("global-key-2".to_string()));
        global_keys.insert(ConstantTimeString::from("shared-key".to_string())); // Duplicate with target

        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .onwards_key("test-key".to_string())
                    .keys(target_keys)
                    .build(),
            ),
        );

        let config_file = ConfigFile {
            targets,
            auth: Some(Auth {
                global_keys,
                key_definitions: None,
            }),
        };

        let targets = Targets::from_config(config_file).unwrap();
        let pool = targets.targets.get("test-model").unwrap();
        let target = pool.first_target().unwrap();

        // Target should have both its own keys and global keys (5 unique keys)
        assert_eq!(target.keys.as_ref().unwrap().len(), 5);
        assert!(
            target
                .keys
                .as_ref()
                .unwrap()
                .contains(&ConstantTimeString::from("target-key-1".to_string()))
        );
        assert!(
            target
                .keys
                .as_ref()
                .unwrap()
                .contains(&ConstantTimeString::from("target-key-2".to_string()))
        );
        assert!(
            target
                .keys
                .as_ref()
                .unwrap()
                .contains(&ConstantTimeString::from("global-key-1".to_string()))
        );
        assert!(
            target
                .keys
                .as_ref()
                .unwrap()
                .contains(&ConstantTimeString::from("global-key-2".to_string()))
        );
        assert!(
            target
                .keys
                .as_ref()
                .unwrap()
                .contains(&ConstantTimeString::from("shared-key".to_string()))
        );
    }

    #[test]
    fn test_from_config_target_without_keys_gets_global_keys() {
        let mut global_keys = HashSet::new();
        global_keys.insert(ConstantTimeString::from("global-key-1".to_string()));
        global_keys.insert(ConstantTimeString::from("global-key-2".to_string()));

        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .onwards_key("test-key".to_string())
                    .build(),
            ),
        );

        let config_file = ConfigFile {
            targets,
            auth: Some(Auth {
                global_keys,
                key_definitions: None,
            }),
        };

        let targets = Targets::from_config(config_file).unwrap();
        let pool = targets.targets.get("test-model").unwrap();
        let target = pool.first_target().unwrap();

        // Target without keys should get global keys
        assert_eq!(target.keys.as_ref().unwrap().len(), 2);
        assert!(
            target
                .keys
                .as_ref()
                .unwrap()
                .contains(&ConstantTimeString::from("global-key-1".to_string()))
        );
        assert!(
            target
                .keys
                .as_ref()
                .unwrap()
                .contains(&ConstantTimeString::from("global-key-2".to_string()))
        );
    }

    #[test]
    fn test_target_with_rate_limit_config() {
        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .rate_limit(RateLimitParameters {
                        requests_per_second: NonZeroU32::new(10).unwrap(),
                        burst_size: Some(NonZeroU32::new(20).unwrap()),
                    })
                    .build(),
            ),
        );

        let config_file = ConfigFile {
            targets,
            auth: None,
        };

        let targets = Targets::from_config(config_file).unwrap();
        let pool = targets.targets.get("test-model").unwrap();
        let target = pool.first_target().unwrap();

        // Target should have rate limiter configured
        assert!(target.limiter.is_some());
    }

    #[test]
    fn test_target_without_rate_limit_config() {
        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .build(),
            ),
        );

        let config_file = ConfigFile {
            targets,
            auth: None,
        };

        let targets = Targets::from_config(config_file).unwrap();
        let pool = targets.targets.get("test-model").unwrap();
        let target = pool.first_target().unwrap();

        // Target should not have rate limiter
        assert!(target.limiter.is_none());
    }

    #[derive(Debug)]
    struct MockRateLimiter {
        should_allow: std::sync::Mutex<bool>,
    }

    impl MockRateLimiter {
        fn new(should_allow: bool) -> Self {
            Self {
                should_allow: std::sync::Mutex::new(should_allow),
            }
        }

        fn set_should_allow(&self, allow: bool) {
            *self.should_allow.lock().unwrap() = allow;
        }
    }

    impl RateLimiter for MockRateLimiter {
        fn check(&self) -> Result<(), ()> {
            if *self.should_allow.lock().unwrap() {
                Ok(())
            } else {
                Err(())
            }
        }
    }

    #[test]
    fn test_rate_limiter_trait_allows_requests() {
        let limiter = MockRateLimiter::new(true);
        assert!(limiter.check().is_ok());
    }

    #[test]
    fn test_rate_limiter_trait_blocks_requests() {
        let limiter = MockRateLimiter::new(false);
        assert!(limiter.check().is_err());
    }

    #[test]
    fn test_rate_limiter_trait_can_change_state() {
        let limiter = MockRateLimiter::new(true);
        assert!(limiter.check().is_ok());

        limiter.set_should_allow(false);
        assert!(limiter.check().is_err());

        limiter.set_should_allow(true);
        assert!(limiter.check().is_ok());
    }

    #[test]
    fn test_per_key_rate_limiting_with_actual_key() {
        use std::collections::HashMap;
        use std::num::NonZeroU32;

        // Create key definitions with rate limits
        let mut key_definitions = HashMap::new();
        key_definitions.insert(
            "basic_user".to_string(),
            KeyDefinition {
                key: "sk-user-123".to_string(),
                rate_limit: Some(RateLimitParameters {
                    requests_per_second: NonZeroU32::new(10).unwrap(),
                    burst_size: Some(NonZeroU32::new(20).unwrap()),
                }),
                concurrency_limit: None,
            },
        );

        let config_file = ConfigFile {
            targets: HashMap::new(),
            auth: Some(Auth {
                global_keys: std::collections::HashSet::new(),
                key_definitions: Some(key_definitions),
            }),
        };

        let targets = Targets::from_config(config_file).unwrap();

        // The actual API key should have a rate limiter
        assert!(targets.key_rate_limiters.contains_key("sk-user-123"));

        // First request should be allowed
        let limiter = targets.key_rate_limiters.get("sk-user-123").unwrap();
        assert!(limiter.check().is_ok());
    }

    #[test]
    fn test_per_key_rate_limiting_with_literal_key() {
        use std::collections::HashMap;

        let config_file = ConfigFile {
            targets: HashMap::new(),
            auth: None,
        };

        let targets = Targets::from_config(config_file).unwrap();

        // Literal keys should not have rate limiters
        assert!(!targets.key_rate_limiters.contains_key("sk-literal-key"));
    }

    #[test]
    fn test_per_key_rate_limiting_no_rate_limit_configured() {
        use std::collections::HashMap;

        // Create key definition without rate limits
        let mut key_definitions = HashMap::new();
        key_definitions.insert(
            "unlimited_user".to_string(),
            KeyDefinition {
                key: "sk-unlimited-123".to_string(),
                rate_limit: None,
                concurrency_limit: None,
            },
        );

        let config_file = ConfigFile {
            targets: HashMap::new(),
            auth: Some(Auth {
                global_keys: std::collections::HashSet::new(),
                key_definitions: Some(key_definitions),
            }),
        };

        let targets = Targets::from_config(config_file).unwrap();

        // Key without rate limits should not have a rate limiter
        assert!(!targets.key_rate_limiters.contains_key("sk-unlimited-123"));
    }

    #[test]
    fn test_from_config_no_global_keys() {
        let mut target_keys = HashSet::new();
        target_keys.insert(ConstantTimeString::from("target-key-1".to_string()));
        target_keys.insert(ConstantTimeString::from("target-key-2".to_string()));

        let mut targets = HashMap::new();
        targets.insert(
            "model-with-keys".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .onwards_key("test-key".to_string())
                    .keys(target_keys)
                    .build(),
            ),
        );
        targets.insert(
            "model-without-keys".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .onwards_key("test-key".to_string())
                    .build(),
            ),
        );

        let config_file = ConfigFile {
            targets,
            auth: None,
        };

        let targets = Targets::from_config(config_file).unwrap();

        // Target with keys should keep them unchanged
        let pool_with_keys = targets.targets.get("model-with-keys").unwrap();
        let target_with_keys = pool_with_keys.first_target().unwrap();
        assert_eq!(target_with_keys.keys.as_ref().unwrap().len(), 2);
        assert!(
            target_with_keys
                .keys
                .as_ref()
                .unwrap()
                .contains(&ConstantTimeString::from("target-key-1".to_string()))
        );
        assert!(
            target_with_keys
                .keys
                .as_ref()
                .unwrap()
                .contains(&ConstantTimeString::from("target-key-2".to_string()))
        );

        // Target without keys should remain None
        let pool_without_keys = targets.targets.get("model-without-keys").unwrap();
        let target_without_keys = pool_without_keys.first_target().unwrap();
        assert_eq!(target_without_keys.keys, None);
    }

    #[test]
    fn test_normalize_url_adds_trailing_slash() {
        // URL without trailing slash should get one added
        let url_without_slash: Url = "https://api.example.com/v1".parse().unwrap();
        let normalized = super::normalize_url(url_without_slash);
        assert_eq!(normalized.as_str(), "https://api.example.com/v1/");

        // URL with trailing slash should remain unchanged
        let url_with_slash: Url = "https://api.example.com/v1/".parse().unwrap();
        let normalized = super::normalize_url(url_with_slash);
        assert_eq!(normalized.as_str(), "https://api.example.com/v1/");

        // Root URL without path should get trailing slash
        let root_url: Url = "https://api.example.com".parse().unwrap();
        let normalized = super::normalize_url(root_url);
        assert_eq!(normalized.as_str(), "https://api.example.com/");
    }

    #[test]
    fn test_url_joining_after_normalization() {
        // Test that normalized URLs join correctly with paths
        let base_url: Url = "https://api.example.com/v1".parse().unwrap();
        let normalized = super::normalize_url(base_url);

        // Should append path segments correctly
        let joined = normalized.join("messages").unwrap();
        assert_eq!(joined.as_str(), "https://api.example.com/v1/messages");

        // Should also work with leading slash
        let normalized_again: Url = "https://api.example.com/v1".parse().unwrap();
        let normalized_again = super::normalize_url(normalized_again);
        let joined_with_slash = normalized_again.join("messages/create").unwrap();
        assert_eq!(
            joined_with_slash.as_str(),
            "https://api.example.com/v1/messages/create"
        );
    }

    #[test]
    fn test_target_spec_conversion_normalizes_url() {
        let target_spec = TargetSpec::builder()
            .url("https://api.example.com/v1".parse().unwrap())
            .onwards_key("test-key".to_string())
            .build();

        let target: Target = target_spec.into();

        // URL should have trailing slash after conversion
        assert_eq!(target.url.as_str(), "https://api.example.com/v1/");
    }

    #[test]
    fn test_target_with_concurrency_limit_config() {
        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .concurrency_limit(ConcurrencyLimitParameters {
                        max_concurrent_requests: 5,
                    })
                    .weight(1)
                    .build(),
            ),
        );

        let config_file = ConfigFile {
            targets,
            auth: None,
        };

        let targets = Targets::from_config(config_file).unwrap();
        let pool = targets.targets.get("test-model").unwrap();
        let target = pool.first_target().unwrap();

        // Target should have concurrency limiter configured
        assert!(target.concurrency_limiter.is_some());
    }

    #[test]
    fn test_target_without_concurrency_limit_config() {
        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .weight(1)
                    .build(),
            ),
        );

        let config_file = ConfigFile {
            targets,
            auth: None,
        };

        let targets = Targets::from_config(config_file).unwrap();
        let pool = targets.targets.get("test-model").unwrap();
        let target = pool.first_target().unwrap();

        // Target should not have concurrency limiter
        assert!(target.concurrency_limiter.is_none());
    }

    #[tokio::test]
    async fn test_concurrency_limiter_allows_requests() {
        let limiter = SemaphoreConcurrencyLimiter::new(2);

        // First request should succeed
        let guard1 = limiter.acquire().await;
        assert!(guard1.is_ok());

        // Second request should succeed
        let guard2 = limiter.acquire().await;
        assert!(guard2.is_ok());
    }

    #[tokio::test]
    async fn test_concurrency_limiter_blocks_at_capacity() {
        let limiter = SemaphoreConcurrencyLimiter::new(2);

        // Acquire all available permits
        let _guard1 = limiter.acquire().await.unwrap();
        let _guard2 = limiter.acquire().await.unwrap();

        // Third request should fail
        let guard3 = limiter.acquire().await;
        assert!(guard3.is_err());
    }

    #[tokio::test]
    async fn test_concurrency_limiter_releases_on_drop() {
        let limiter = SemaphoreConcurrencyLimiter::new(1);

        {
            // Acquire the only permit
            let _guard = limiter.acquire().await.unwrap();

            // Second request should fail while guard is held
            assert!(limiter.acquire().await.is_err());
        } // guard drops here

        // After guard is dropped, should be able to acquire again
        let guard = limiter.acquire().await;
        assert!(guard.is_ok());
    }

    #[test]
    fn test_per_key_concurrency_limiting_configured() {
        use std::collections::HashMap;

        // Create key definitions with concurrency limits
        let mut key_definitions = HashMap::new();
        key_definitions.insert(
            "limited_user".to_string(),
            KeyDefinition {
                key: "sk-limited-123".to_string(),
                rate_limit: None,
                concurrency_limit: Some(ConcurrencyLimitParameters {
                    max_concurrent_requests: 3,
                }),
            },
        );

        let config_file = ConfigFile {
            targets: HashMap::new(),
            auth: Some(Auth {
                global_keys: std::collections::HashSet::new(),
                key_definitions: Some(key_definitions),
            }),
        };

        let targets = Targets::from_config(config_file).unwrap();

        // The actual API key should have a concurrency limiter
        assert!(
            targets
                .key_concurrency_limiters
                .contains_key("sk-limited-123")
        );
    }

    #[test]
    fn test_per_key_concurrency_limiting_not_configured() {
        use std::collections::HashMap;

        // Create key definition without concurrency limits
        let mut key_definitions = HashMap::new();
        key_definitions.insert(
            "unlimited_user".to_string(),
            KeyDefinition {
                key: "sk-unlimited-456".to_string(),
                rate_limit: None,
                concurrency_limit: None,
            },
        );

        let config_file = ConfigFile {
            targets: HashMap::new(),
            auth: Some(Auth {
                global_keys: std::collections::HashSet::new(),
                key_definitions: Some(key_definitions),
            }),
        };

        let targets = Targets::from_config(config_file).unwrap();

        // Key without concurrency limits should not have a concurrency limiter
        assert!(
            !targets
                .key_concurrency_limiters
                .contains_key("sk-unlimited-456")
        );
    }
}
