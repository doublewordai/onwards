//! Target management and configuration
//!
//! This module handles the configuration and management of proxy targets - the
//! upstream services that requests are forwarded to. Targets are dynamically
//! loaded from configuration files and support hot-reloading when files change.
use crate::auth::{ConstantTimeString, HashAlgorithm, KeySet};
use anyhow::anyhow;
use async_trait::async_trait;
use bon::Builder;
use dashmap::DashMap;
use futures_util::{Stream, StreamExt};
use governor::{DefaultDirectRateLimiter, Quota};
use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, num::NonZeroU32, path::PathBuf, pin::Pin, sync::Arc};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, WatchStream};
use tracing::{debug, error, info, trace};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitParameters {
    pub requests_per_second: NonZeroU32,
    pub burst_size: Option<NonZeroU32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
pub struct TargetSpec {
    pub url: Url,
    pub keys: Option<Vec<ApiKey>>,
    pub onwards_key: Option<String>,
    pub onwards_model: Option<String>,
    pub rate_limit: Option<RateLimitParameters>,
}

impl TargetSpec {
    fn into_target(self, hash_algorithm: Option<&HashAlgorithm>) -> Target {
        // Convert Vec<ApiKey> to KeySet (HashSet<ConstantTimeString>)
        // Apply hashing if configured
        let keys = self.keys.map(|keys_vec| {
            keys_vec
                .into_iter()
                .map(|api_key| {
                    let hashed_key = crate::auth::hash_key(&api_key.key, hash_algorithm);
                    ConstantTimeString::from(hashed_key)
                })
                .collect::<KeySet>()
        });

        Target {
            url: self.url,
            keys,
            onwards_key: self.onwards_key,
            onwards_model: self.onwards_model,
            limiter: self.rate_limit.map(|rl| {
                Arc::new(governor::RateLimiter::direct(
                    Quota::per_second(rl.requests_per_second)
                        .allow_burst(rl.burst_size.unwrap_or(rl.requests_per_second)),
                )) as Arc<dyn RateLimiter>
            }),
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
pub struct Target {
    pub url: Url,
    pub keys: Option<KeySet>,
    pub onwards_key: Option<String>,
    pub onwards_model: Option<String>,
    pub limiter: Option<Arc<dyn RateLimiter>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub key: String,
    pub rate_limit: Option<RateLimitParameters>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
pub struct Auth {
    /// global keys are merged with the per-target keys.
    keys: Option<Vec<ApiKey>>,
    /// Hash algorithm to use for API keys
    hash: Option<HashAlgorithm>,
}

/// The config file contains a map of target names to targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    pub targets: HashMap<String, TargetSpec>,
    pub auth: Option<Auth>,
}

/// The live-updating collection of targets.
#[derive(Debug, Clone)]
pub struct Targets {
    pub targets: Arc<DashMap<String, Target>>,
    /// Direct rate limiters per actual API key (actual key -> rate limiter)
    pub key_rate_limiters: Arc<DashMap<String, Arc<DefaultDirectRateLimiter>>>,
    /// Hash algorithm used for API keys (if configured)
    pub hash_algorithm: Option<HashAlgorithm>,
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
        let (global_keys, hash_algorithm) = config_file
            .auth
            .take()
            .map(|auth| (auth.keys.unwrap_or_default(), auth.hash))
            .unwrap_or_default();
        debug!("{} global keys configured", global_keys.len());
        if let Some(ref hash) = hash_algorithm {
            debug!("Hash algorithm configured: {:?}", hash);
        }

        // Set up rate limiters keyed by actual API keys
        let key_rate_limiters = Arc::new(DashMap::new());

        // Process global keys for rate limiting
        for key_obj in &global_keys {
            if let Some(ref rate_limit) = key_obj.rate_limit {
                let limiter = Arc::new(governor::RateLimiter::direct(
                    Quota::per_second(rate_limit.requests_per_second).allow_burst(
                        rate_limit
                            .burst_size
                            .unwrap_or(rate_limit.requests_per_second),
                    ),
                ));
                key_rate_limiters.insert(key_obj.key.clone(), limiter);
            }
        }

        let targets = Arc::new(DashMap::new());
        for (name, mut target_spec) in config_file.targets {
            // Process target-specific keys for rate limiting
            if let Some(ref keys) = target_spec.keys {
                for key_obj in keys {
                    if let Some(ref rate_limit) = key_obj.rate_limit {
                        let limiter = Arc::new(governor::RateLimiter::direct(
                            Quota::per_second(rate_limit.requests_per_second).allow_burst(
                                rate_limit
                                    .burst_size
                                    .unwrap_or(rate_limit.requests_per_second),
                            ),
                        ));
                        key_rate_limiters.insert(key_obj.key.clone(), limiter);
                    }
                }

                debug!(
                    "Target {}:{:?} has {} keys configured",
                    target_spec.url,
                    target_spec.onwards_model,
                    keys.len()
                );
            }

            // Merge global keys with target keys
            if let Some(ref mut keys) = target_spec.keys {
                keys.extend(global_keys.clone());
            } else if !global_keys.is_empty() {
                target_spec.keys = Some(global_keys.clone());
            }

            targets.insert(name, target_spec.into_target(hash_algorithm.as_ref()));
        }

        Ok(Targets {
            targets,
            key_rate_limiters,
            hash_algorithm,
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
    use crate::auth::{ConstantTimeString, HashAlgorithm};
    use dashmap::DashMap;
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
                    .build(),
            );
        }
        Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            hash_algorithm: None,
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
            hash_algorithm: None,
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
                .build(),
        );
        let updated_targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            hash_algorithm: None,
        };

        let mock_watcher = MockConfigWatcher::with_targets(vec![updated_targets]);

        // Start watching
        initial_targets.receive_updates(mock_watcher).await.unwrap();

        // Give some time for the watcher to process
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Verify target properties were updated
        let target = initial_targets.targets.get("gpt-4").unwrap();
        assert_eq!(target.url.as_str(), "https://api.openai.com/v2");
        assert_eq!(target.onwards_key, Some("new-key".to_string()));
        assert_eq!(target.onwards_model, Some("gpt-4-turbo".to_string()));
    }

    #[test]
    fn test_from_config_merges_global_keys_with_target_keys() {
        let target_keys = vec![
            ApiKey {
                key: "target-key-1".to_string(),
                rate_limit: None,
            },
            ApiKey {
                key: "target-key-2".to_string(),
                rate_limit: None,
            },
            ApiKey {
                key: "shared-key".to_string(),
                rate_limit: None,
            },
        ];

        let global_keys = vec![
            ApiKey {
                key: "global-key-1".to_string(),
                rate_limit: None,
            },
            ApiKey {
                key: "global-key-2".to_string(),
                rate_limit: None,
            },
            ApiKey {
                key: "shared-key".to_string(), // Duplicate with target
                rate_limit: None,
            },
        ];

        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpec::builder()
                .url("https://api.example.com".parse().unwrap())
                .onwards_key("test-key".to_string())
                .keys(target_keys)
                .build(),
        );

        let config_file = ConfigFile {
            targets,
            auth: Some(Auth {
                keys: Some(global_keys),
                hash: None,
            }),
        };

        let targets = Targets::from_config(config_file).unwrap();
        let target = targets.targets.get("test-model").unwrap();

        // Target should have both its own keys and global keys (5 unique keys after HashSet conversion)
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
        let global_keys = vec![
            ApiKey {
                key: "global-key-1".to_string(),
                rate_limit: None,
            },
            ApiKey {
                key: "global-key-2".to_string(),
                rate_limit: None,
            },
        ];

        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpec::builder()
                .url("https://api.example.com".parse().unwrap())
                .onwards_key("test-key".to_string())
                .build(),
        );

        let config_file = ConfigFile {
            targets,
            auth: Some(Auth {
                keys: Some(global_keys),
                hash: None,
            }),
        };

        let targets = Targets::from_config(config_file).unwrap();
        let target = targets.targets.get("test-model").unwrap();

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
            TargetSpec::builder()
                .url("https://api.example.com".parse().unwrap())
                .rate_limit(RateLimitParameters {
                    requests_per_second: NonZeroU32::new(10).unwrap(),
                    burst_size: Some(NonZeroU32::new(20).unwrap()),
                })
                .build(),
        );

        let config_file = ConfigFile {
            targets,
            auth: None,
        };

        let targets = Targets::from_config(config_file).unwrap();
        let target = targets.targets.get("test-model").unwrap();

        // Target should have rate limiter configured
        assert!(target.limiter.is_some());
    }

    #[test]
    fn test_target_without_rate_limit_config() {
        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpec::builder()
                .url("https://api.example.com".parse().unwrap())
                .build(),
        );

        let config_file = ConfigFile {
            targets,
            auth: None,
        };

        let targets = Targets::from_config(config_file).unwrap();
        let target = targets.targets.get("test-model").unwrap();

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

        // Create keys with rate limits
        let keys = vec![ApiKey {
            key: "sk-user-123".to_string(),
            rate_limit: Some(RateLimitParameters {
                requests_per_second: NonZeroU32::new(10).unwrap(),
                burst_size: Some(NonZeroU32::new(20).unwrap()),
            }),
        }];

        let config_file = ConfigFile {
            targets: HashMap::new(),
            auth: Some(Auth {
                keys: Some(keys),
                hash: None,
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

        // Create key without rate limits
        let keys = vec![ApiKey {
            key: "sk-unlimited-123".to_string(),
            rate_limit: None,
        }];

        let config_file = ConfigFile {
            targets: HashMap::new(),
            auth: Some(Auth {
                keys: Some(keys),
                hash: None,
            }),
        };

        let targets = Targets::from_config(config_file).unwrap();

        // Key without rate limits should not have a rate limiter
        assert!(!targets.key_rate_limiters.contains_key("sk-unlimited-123"));
    }

    #[test]
    fn test_sha256_hashed_keys() {
        // Test that keys are properly hashed when SHA256 is configured
        let keys = vec![
            ApiKey {
                key: "test-key-1".to_string(),
                rate_limit: None,
            },
            ApiKey {
                key: "test-key-2".to_string(),
                rate_limit: None,
            },
        ];

        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpec::builder()
                .url("https://api.example.com".parse().unwrap())
                .build(),
        );

        let config_file = ConfigFile {
            targets,
            auth: Some(Auth {
                keys: Some(keys),
                hash: Some(HashAlgorithm::Sha256),
            }),
        };

        let targets = Targets::from_config(config_file).unwrap();
        let target = targets.targets.get("test-model").unwrap();

        // The keys should be hashed
        // SHA256 of "test-key-1" is "d8b9130293b21e91d008b65f9dc33b379e78ca73e8e13428e76a03b6db7c15de"
        // SHA256 of "test-key-2" is "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08" (for "test")
        // Let's actually compute the correct hashes
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update("test-key-1");
        let hash1 = format!("{:x}", hasher.finalize());

        let mut hasher = Sha256::new();
        hasher.update("test-key-2");
        let hash2 = format!("{:x}", hasher.finalize());

        assert!(
            target
                .keys
                .as_ref()
                .unwrap()
                .contains(&ConstantTimeString::from(hash1))
        );
        assert!(
            target
                .keys
                .as_ref()
                .unwrap()
                .contains(&ConstantTimeString::from(hash2))
        );
        assert_eq!(target.keys.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn test_from_config_no_global_keys() {
        let target_keys = vec![
            ApiKey {
                key: "target-key-1".to_string(),
                rate_limit: None,
            },
            ApiKey {
                key: "target-key-2".to_string(),
                rate_limit: None,
            },
        ];

        let mut targets = HashMap::new();
        targets.insert(
            "model-with-keys".to_string(),
            TargetSpec::builder()
                .url("https://api.example.com".parse().unwrap())
                .onwards_key("test-key".to_string())
                .keys(target_keys)
                .build(),
        );
        targets.insert(
            "model-without-keys".to_string(),
            TargetSpec::builder()
                .url("https://api.example.com".parse().unwrap())
                .onwards_key("test-key".to_string())
                .build(),
        );

        let config_file = ConfigFile {
            targets,
            auth: None,
        };

        let targets = Targets::from_config(config_file).unwrap();

        // Target with keys should keep them unchanged
        let target_with_keys = targets.targets.get("model-with-keys").unwrap();
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
        let target_without_keys = targets.targets.get("model-without-keys").unwrap();
        assert_eq!(target_without_keys.keys, None);
    }
}
