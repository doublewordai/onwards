//! Load balancer for distributing requests across multiple providers
//!
//! This module implements weighted load balancing with rate limit awareness.
//! Providers can be assigned different weights, and the load balancer will
//! select providers proportionally while respecting their rate limits.
//!
//! Pool-level configuration (keys, rate limits) is shared across all providers.

use crate::auth::KeySet;
use crate::target::{ConcurrencyLimiter, FallbackConfig, LoadBalanceStrategy, RateLimiter, Target};
use rand::Rng;
use std::sync::Arc;

/// A pool of providers that share an alias, with load balancing support
#[derive(Debug, Clone)]
pub struct ProviderPool {
    /// The list of providers in this pool
    providers: Vec<Provider>,
    /// Total weight of all providers (for weighted random selection)
    total_weight: u32,
    /// Pool-level access control keys (who can call this alias)
    keys: Option<KeySet>,
    /// Pool-level rate limiter (applies to all requests to this alias)
    pool_limiter: Option<Arc<dyn RateLimiter>>,
    /// Pool-level concurrency limiter (applies to all requests to this alias)
    pool_concurrency_limiter: Option<Arc<dyn ConcurrencyLimiter>>,
    /// Fallback configuration for retrying failed requests
    fallback: Option<FallbackConfig>,
    /// Load balancing strategy
    strategy: LoadBalanceStrategy,
}

/// A single provider within a pool
#[derive(Debug, Clone)]
pub struct Provider {
    /// The target configuration for this provider
    pub target: Target,
    /// Weight for load balancing (higher = more traffic)
    pub weight: u32,
}

impl ProviderPool {
    /// Create a new provider pool from a list of providers
    pub fn new(providers: Vec<Provider>) -> Self {
        let total_weight = providers.iter().map(|p| p.weight).sum();
        Self {
            providers,
            total_weight,
            keys: None,
            pool_limiter: None,
            pool_concurrency_limiter: None,
            fallback: None,
            strategy: LoadBalanceStrategy::default(),
        }
    }

    /// Create a new provider pool with pool-level configuration
    pub fn with_config(
        providers: Vec<Provider>,
        keys: Option<KeySet>,
        pool_limiter: Option<Arc<dyn RateLimiter>>,
        pool_concurrency_limiter: Option<Arc<dyn ConcurrencyLimiter>>,
        fallback: Option<FallbackConfig>,
        strategy: LoadBalanceStrategy,
    ) -> Self {
        let total_weight = providers.iter().map(|p| p.weight).sum();
        Self {
            providers,
            total_weight,
            keys,
            pool_limiter,
            pool_concurrency_limiter,
            fallback,
            strategy,
        }
    }

    /// Create a pool with a single provider
    pub fn single(target: Target, weight: u32) -> Self {
        Self::new(vec![Provider { target, weight }])
    }

    /// Select a provider from the pool using weighted random selection
    ///
    /// This method:
    /// 1. Uses weighted random selection to pick a provider proportionally to weights
    /// 2. Falls back to trying each provider in order if the selected one is rate limited
    /// 3. Returns None if all providers are rate limited
    pub fn select(&self) -> Option<&Target> {
        if self.providers.is_empty() {
            return None;
        }

        // If only one provider, return it directly (rate limit check done by handler)
        if self.providers.len() == 1 {
            return Some(&self.providers[0].target);
        }

        // Weighted random selection
        let mut rng = rand::rng();
        let random_weight: u32 = rng.random_range(0..self.total_weight);

        let mut cumulative_weight = 0;
        let mut selected_idx = 0;

        for (idx, provider) in self.providers.iter().enumerate() {
            cumulative_weight += provider.weight;
            if random_weight < cumulative_weight {
                selected_idx = idx;
                break;
            }
        }

        // Check if the selected provider is available (not rate limited)
        let selected = &self.providers[selected_idx];
        if !is_rate_limited(&selected.target) {
            return Some(&selected.target);
        }

        // If selected provider is rate limited, try others in weighted order
        // Build list of indices sorted by weight (descending)
        let mut indices: Vec<usize> = (0..self.providers.len())
            .filter(|&i| i != selected_idx)
            .collect();
        indices.sort_by(|&a, &b| self.providers[b].weight.cmp(&self.providers[a].weight));

        for idx in indices {
            let provider = &self.providers[idx];
            if !is_rate_limited(&provider.target) {
                return Some(&provider.target);
            }
        }

        // All providers are rate limited - return the originally selected one
        // The handler will return the rate limit error
        Some(&self.providers[selected_idx].target)
    }

    /// Get all providers in the pool (for listing models, etc.)
    pub fn providers(&self) -> &[Provider] {
        &self.providers
    }

    /// Get the number of providers in the pool
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Check if the pool is empty
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Get the first provider's target (useful for getting shared config like keys)
    pub fn first_target(&self) -> Option<&Target> {
        self.providers.first().map(|p| &p.target)
    }

    /// Get pool-level access control keys
    pub fn keys(&self) -> Option<&KeySet> {
        self.keys.as_ref()
    }

    /// Get pool-level rate limiter
    pub fn pool_limiter(&self) -> Option<&Arc<dyn RateLimiter>> {
        self.pool_limiter.as_ref()
    }

    /// Get pool-level concurrency limiter
    pub fn pool_concurrency_limiter(&self) -> Option<&Arc<dyn ConcurrencyLimiter>> {
        self.pool_concurrency_limiter.as_ref()
    }

    /// Get the fallback configuration
    pub fn fallback(&self) -> Option<&FallbackConfig> {
        self.fallback.as_ref()
    }

    /// Check if fallback is enabled for this pool
    pub fn fallback_enabled(&self) -> bool {
        self.fallback.as_ref().is_some_and(|f| f.enabled)
    }

    /// Check if a status code should trigger fallback to the next provider
    pub fn should_fallback_on_status(&self, status_code: u16) -> bool {
        self.fallback
            .as_ref()
            .is_some_and(|f| f.should_fallback_on_status(status_code))
    }

    /// Check if local rate limits should trigger fallback
    pub fn should_fallback_on_rate_limit(&self) -> bool {
        self.fallback.as_ref().is_some_and(|f| f.enabled && f.on_rate_limit)
    }

    /// Get the load balancing strategy
    pub fn strategy(&self) -> LoadBalanceStrategy {
        self.strategy
    }

    /// Select providers in priority order for fallback scenarios.
    /// Returns an iterator yielding (index, &Target) based on the configured strategy:
    /// - WeightedRandom: Repeatedly samples from remaining pool using weighted random
    /// - Priority: Returns providers in definition order (first provider is primary)
    pub fn select_ordered(&self) -> impl Iterator<Item = (usize, &Target)> {
        let mut order = Vec::with_capacity(self.providers.len());

        if self.providers.is_empty() {
            return order.into_iter();
        }

        match self.strategy {
            LoadBalanceStrategy::Priority => {
                // Simple: return providers in definition order
                for (idx, provider) in self.providers.iter().enumerate() {
                    order.push((idx, &provider.target));
                }
            }
            LoadBalanceStrategy::WeightedRandom => {
                // Repeatedly sample from remaining pool using weighted random
                let mut remaining: Vec<(usize, u32)> = self
                    .providers
                    .iter()
                    .enumerate()
                    .map(|(i, p)| (i, p.weight))
                    .collect();
                let mut rng = rand::rng();

                while !remaining.is_empty() {
                    let total: u32 = remaining.iter().map(|(_, w)| w).sum();
                    let random_weight: u32 = if total > 0 {
                        rng.random_range(0..total)
                    } else {
                        0
                    };

                    let mut cumulative = 0;
                    let mut selected_pos = 0;
                    for (pos, (_, weight)) in remaining.iter().enumerate() {
                        cumulative += weight;
                        if random_weight < cumulative {
                            selected_pos = pos;
                            break;
                        }
                    }

                    let (idx, _) = remaining.remove(selected_pos);
                    order.push((idx, &self.providers[idx].target));
                }
            }
        }

        order.into_iter()
    }
}

/// Check if a target is rate limited
fn is_rate_limited(target: &Target) -> bool {
    if let Some(ref limiter) = target.limiter {
        // Use check() which consumes a token - if it fails, provider is rate limited
        // Note: This is a peek, we'll check again in the handler
        limiter.check().is_err()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Target;
    use std::collections::HashMap;

    fn create_test_target(url: &str) -> Target {
        Target::builder()
            .url(url.parse().unwrap())
            .build()
    }

    #[test]
    fn test_single_provider_pool() {
        let target = create_test_target("https://api.example.com");
        let pool = ProviderPool::single(target.clone(), 1);

        assert_eq!(pool.len(), 1);
        assert!(!pool.is_empty());

        let selected = pool.select();
        assert!(selected.is_some());
        assert_eq!(selected.unwrap().url.as_str(), "https://api.example.com/");
    }

    #[test]
    fn test_empty_pool_returns_none() {
        let pool = ProviderPool::new(vec![]);

        assert!(pool.is_empty());
        assert!(pool.select().is_none());
    }

    #[test]
    fn test_weighted_selection_distribution() {
        // Create a pool with providers having different weights
        let providers = vec![
            Provider {
                target: create_test_target("https://api1.example.com"),
                weight: 3,
            },
            Provider {
                target: create_test_target("https://api2.example.com"),
                weight: 1,
            },
        ];
        let pool = ProviderPool::new(providers);

        // Run many selections and check distribution
        let mut counts: HashMap<String, usize> = HashMap::new();
        for _ in 0..1000 {
            if let Some(target) = pool.select() {
                *counts.entry(target.url.to_string()).or_insert(0) += 1;
            }
        }

        // Provider with weight 3 should be selected roughly 3x more often than weight 1
        let count1 = *counts.get("https://api1.example.com/").unwrap_or(&0);
        let count2 = *counts.get("https://api2.example.com/").unwrap_or(&0);

        // Allow for some variance in random selection (within 50% of expected ratio)
        let ratio = count1 as f64 / count2 as f64;
        assert!(
            ratio > 1.5 && ratio < 6.0,
            "Expected ratio around 3.0, got {}",
            ratio
        );
    }

    #[test]
    fn test_first_target() {
        let providers = vec![
            Provider {
                target: create_test_target("https://api1.example.com"),
                weight: 1,
            },
            Provider {
                target: create_test_target("https://api2.example.com"),
                weight: 2,
            },
        ];
        let pool = ProviderPool::new(providers);

        let first = pool.first_target();
        assert!(first.is_some());
        assert_eq!(first.unwrap().url.as_str(), "https://api1.example.com/");
    }

    #[test]
    fn test_providers_accessor() {
        let providers = vec![
            Provider {
                target: create_test_target("https://api1.example.com"),
                weight: 1,
            },
            Provider {
                target: create_test_target("https://api2.example.com"),
                weight: 2,
            },
        ];
        let pool = ProviderPool::new(providers);

        assert_eq!(pool.providers().len(), 2);
        assert_eq!(pool.providers()[0].weight, 1);
        assert_eq!(pool.providers()[1].weight, 2);
    }

    #[test]
    fn test_select_ordered_priority_strategy() {
        use crate::target::LoadBalanceStrategy;

        let providers = vec![
            Provider {
                target: create_test_target("https://primary.example.com"),
                weight: 1,
            },
            Provider {
                target: create_test_target("https://secondary.example.com"),
                weight: 10,
            },
            Provider {
                target: create_test_target("https://tertiary.example.com"),
                weight: 5,
            },
        ];

        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            None,
            LoadBalanceStrategy::Priority,
        );

        // Priority strategy should always return providers in definition order
        // regardless of weights
        let order: Vec<_> = pool.select_ordered().collect();
        assert_eq!(order.len(), 3);
        assert_eq!(order[0].0, 0); // primary first
        assert_eq!(order[1].0, 1); // secondary second
        assert_eq!(order[2].0, 2); // tertiary third
        assert_eq!(order[0].1.url.as_str(), "https://primary.example.com/");
        assert_eq!(order[1].1.url.as_str(), "https://secondary.example.com/");
        assert_eq!(order[2].1.url.as_str(), "https://tertiary.example.com/");
    }

    #[test]
    fn test_select_ordered_weighted_random_includes_all() {
        use crate::target::LoadBalanceStrategy;

        let providers = vec![
            Provider {
                target: create_test_target("https://api1.example.com"),
                weight: 3,
            },
            Provider {
                target: create_test_target("https://api2.example.com"),
                weight: 1,
            },
        ];

        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            None,
            LoadBalanceStrategy::WeightedRandom,
        );

        // Weighted random should include all providers (order varies)
        let order: Vec<_> = pool.select_ordered().collect();
        assert_eq!(order.len(), 2);

        // Both providers should be present
        let urls: std::collections::HashSet<_> =
            order.iter().map(|(_, t)| t.url.as_str()).collect();
        assert!(urls.contains("https://api1.example.com/"));
        assert!(urls.contains("https://api2.example.com/"));
    }

    #[test]
    fn test_select_ordered_weighted_random_distribution() {
        use crate::target::LoadBalanceStrategy;

        let providers = vec![
            Provider {
                target: create_test_target("https://heavy.example.com"),
                weight: 9,
            },
            Provider {
                target: create_test_target("https://light.example.com"),
                weight: 1,
            },
        ];

        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            None,
            LoadBalanceStrategy::WeightedRandom,
        );

        // Run multiple times and count how often heavy is first
        let mut heavy_first = 0;
        let iterations = 1000;
        for _ in 0..iterations {
            let order: Vec<_> = pool.select_ordered().collect();
            if order[0].1.url.as_str() == "https://heavy.example.com/" {
                heavy_first += 1;
            }
        }

        // With 9:1 weight ratio, heavy should be first roughly 90% of the time
        // With 1000 iterations, allow for reasonable variance (80-98%)
        let percentage = (heavy_first * 100) / iterations;
        assert!(
            percentage >= 80 && percentage <= 98,
            "Expected heavy to be first ~90% of the time, got {}% ({}/{})",
            percentage,
            heavy_first,
            iterations
        );
    }

    #[test]
    fn test_select_ordered_empty_pool() {
        let pool = ProviderPool::new(vec![]);
        let order: Vec<_> = pool.select_ordered().collect();
        assert!(order.is_empty());
    }

    #[test]
    fn test_select_ordered_single_provider() {
        use crate::target::LoadBalanceStrategy;

        let providers = vec![Provider {
            target: create_test_target("https://only.example.com"),
            weight: 1,
        }];

        // Test both strategies with single provider
        for strategy in [LoadBalanceStrategy::Priority, LoadBalanceStrategy::WeightedRandom] {
            let pool = ProviderPool::with_config(
                providers.clone(),
                None,
                None,
                None,
                None,
                strategy,
            );

            let order: Vec<_> = pool.select_ordered().collect();
            assert_eq!(order.len(), 1);
            assert_eq!(order[0].1.url.as_str(), "https://only.example.com/");
        }
    }
}
