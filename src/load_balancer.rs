//! Load balancer for distributing requests across multiple providers
//!
//! This module implements weighted load balancing with rate limit awareness.
//! Providers can be assigned different weights, and the load balancer will
//! select providers proportionally while respecting their rate limits.

use crate::target::Target;
use rand::Rng;

/// A pool of providers that share an alias, with load balancing support
#[derive(Debug, Clone)]
pub struct ProviderPool {
    /// The list of providers in this pool
    providers: Vec<Provider>,
    /// Total weight of all providers (for weighted random selection)
    total_weight: u32,
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
}
