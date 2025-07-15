/// 'Targets' are destinations to which the proxy can forward requests. They're continually read
/// from a config file (currently in JSON format, but open to discussion). When the config file
/// changes, the list of targets is updated.
///
/// Incoming requests are forwarded to one of the targets, based on the 'model' field in the
/// incoming request.
use crate::auth::KeySet;
use anyhow::anyhow;
use async_trait::async_trait;
use bon::Builder;
use dashmap::DashMap;
use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use url::Url;

/// A target represents a destination for requests, specified by its URL.
///
/// ## Validating incoming requests
/// A target can have a set of keys associated with it, one of which will be required in the Authorization: Bearer header.
///
/// ## Forwarding requests
/// A target can have a onwards_key and a onwards_model. The key is put into the Authorization:
/// Bearer {} header of the request. The onwards_model is used to determine which model to put in
/// the json body when forwarding the request.
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
pub(crate) struct Target {
    pub(crate) url: Url,
    pub(crate) keys: Option<KeySet>,
    pub(crate) onwards_key: Option<String>,
    pub(crate) onwards_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Auth {
    /// global keys are merged with the per-target keys.
    global_keys: KeySet,
}

/// The config file contains a map of target names to targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ConfigFile {
    pub(crate) targets: HashMap<String, Target>,
    pub(crate) auth: Option<Auth>,
}

/// The live-updating collection of targets.
#[derive(Debug, Clone)]
pub(crate) struct Targets {
    pub(crate) targets: Arc<DashMap<String, Target>>,
}

#[async_trait]
pub trait TargetsStream {
    /// TODO(fergus): This would probably be nicer if the error were associated types and it
    /// returned a Result<Box<dyn Stream<Item = Result<Targets, E>>, U> + Send>
    async fn receive(
        &self,
    ) -> Result<mpsc::Receiver<Result<Targets, anyhow::Error>>, anyhow::Error>;
}

pub struct WatchedFile(pub PathBuf);

#[async_trait]
impl TargetsStream for WatchedFile {
    /// Watches a file for changes and returns a stream of Targets updates.
    async fn receive(
        &self,
    ) -> Result<mpsc::Receiver<Result<Targets, anyhow::Error>>, anyhow::Error> {
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

        Ok(targets_rx)
    }
}

impl Targets {
    pub(crate) async fn from_config_file(config_path: &PathBuf) -> Result<Self, anyhow::Error> {
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

    pub(crate) fn from_config(mut config_file: ConfigFile) -> Result<Self, anyhow::Error> {
        let global_keys = config_file
            .auth
            .take()
            .map(|x| x.global_keys)
            .unwrap_or_default();
        debug!("{} global keys configured", global_keys.len());

        let targets = Arc::new(DashMap::new());
        for (name, mut target) in config_file.targets {
            if let Some(ref mut keys) = target.keys {
                debug!(
                    "Target {}:{:?} has {} keys configured",
                    target.url,
                    target.onwards_model,
                    keys.len()
                );
                keys.extend(global_keys.clone());
            } else if !global_keys.is_empty() {
                target.keys = Some(global_keys.clone());
            }
            targets.insert(name, target);
        }

        Ok(Targets { targets })
    }

    /// Receives updates from a stream of targets and updates the internal targets map.
    pub(crate) async fn receive_updates<W: TargetsStream + Send + 'static>(
        &self,
        targets_stream: W,
    ) -> Result<(), anyhow::Error> {
        let targets = Arc::clone(&self.targets);

        let mut rx = targets_stream.receive().await?;

        // TODO(fergus): stash the handle to this thread somewhere
        tokio::spawn(async move {
            while let Some(result) = rx.recv().await {
                match result {
                    Ok(new_targets) => {
                        info!("Config file changed, updating targets...");
                        // Copy the new targets to our existing targets map
                        let current_keys: Vec<String> =
                            targets.iter().map(|entry| entry.key().clone()).collect();

                        // Do it like this for atomicity (if you delete and recreate, there's a
                        // moment with no targets during which requests can fail)

                        // Remove deleted targets
                        for key in current_keys {
                            if !new_targets.targets.contains_key(&key) {
                                targets.remove(&key);
                            }
                        }

                        // Insert/update targets
                        for entry in new_targets.targets.iter() {
                            targets.insert(entry.key().clone(), entry.value().clone());
                        }
                    }
                    Err(e) => {
                        error!("Failed to reload config: {}", e);
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
        async fn receive(
            &self,
        ) -> Result<mpsc::Receiver<Result<Targets, anyhow::Error>>, anyhow::Error> {
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

            Ok(rx)
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
            Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .onwards_key("test-key".to_string())
                .keys(target_keys)
                .build(),
        );

        let config_file = ConfigFile {
            targets,
            auth: Some(Auth { global_keys }),
        };

        let targets = Targets::from_config(config_file).unwrap();
        let target = targets.targets.get("test-model").unwrap();

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
            Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .onwards_key("test-key".to_string())
                .build(),
        );

        let config_file = ConfigFile {
            targets,
            auth: Some(Auth { global_keys }),
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
    fn test_from_config_no_global_keys() {
        let mut target_keys = HashSet::new();
        target_keys.insert(ConstantTimeString::from("target-key-1".to_string()));
        target_keys.insert(ConstantTimeString::from("target-key-2".to_string()));

        let mut targets = HashMap::new();
        targets.insert(
            "model-with-keys".to_string(),
            Target::builder()
                .url("https://api.example.com".parse().unwrap())
                .onwards_key("test-key".to_string())
                .keys(target_keys)
                .build(),
        );
        targets.insert(
            "model-without-keys".to_string(),
            Target::builder()
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
