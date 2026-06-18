//! Cached-input-classification seam (the onwards ↔ dwctl boundary).
//!
//! This module defines the *dormant plumbing* for Anthropic-style cached input
//! classification. onwards owns the abstraction (the [`CacheClassifier`] trait, the
//! neutral [`CacheStats`] result, and the [`NoopCacheClassifier`] default); the
//! real classification logic — index lookup, prefix hashing, tokenizer calls —
//! is implemented downstream in dwctl and injected at startup via
//! [`crate::AppState::with_cache_classifier`]. This is dependency inversion: the
//! compile-time dependency points one way (dwctl → onwards), the runtime call
//! goes the other way through the trait object, so there is no crate cycle.
//!
//! ## Dormant by default
//!
//! When no classifier is wired (the default), onwards behaviour is byte-identical
//! to today: no request forking, no `cache_control` stripping, and no usage
//! injection. The entire cache path in [`crate::handlers`] is gated on
//! `Some(classifier)`. A standalone onwards therefore sees zero observable change.
//!
//! ## The active path (when a classifier is wired)
//!
//! On each request onwards *forks*: it forwards to the upstream model and, in
//! parallel, calls [`CacheClassifier::classify`] under a deadline. The neutral
//! [`CacheStats`] split is then injected into the response `usage` object by
//! the OpenAI shaping helpers in [`crate::cache_usage`]. The no-op classifier
//! returns all-zero stats, so even with the classifier wired the injected fields
//! are zero and downstream billing is unaffected.
//!
//! The seam lives in `target_message_handler`, which both the regular and the strict
//! routers route through — so strict-mode requests get the same treatment
//! automatically, with no extra wiring.
//!
//! See `input-token-cache-pricing.md` §5.3, §6.1, §6.2, §6.3 for the design.

use async_trait::async_trait;
use std::time::Duration;

/// Default deadline for the parallel classify branch.
///
/// On a real classifier this bounds how long the response join will wait for the
/// classification before falling back to all-zero stats (best-effort, billed
/// as un-cached). The no-op classifier returns instantly so this is never hit in
/// practice, but the deadline structure is built so a real classifier slots in.
pub const DEFAULT_CLASSIFY_DEADLINE: Duration = Duration::from_secs(5);

/// The input handed to a [`CacheClassifier`] for classification.
///
/// Carries what a real classifier needs to compute the read/write split: the
/// **virtual** model string (the user-facing alias / `OriginalModel`, *not*
/// the rewritten underlying `model_name` — see `handlers.rs`), and the request
/// body it must parse for `cache_control` markers. Kept minimal but real.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ClassifyInput {
    /// The virtual model string the client sent (the cache key dimension).
    /// This is the `OriginalModel` retained in request extensions, before
    /// onwards rewrites the outbound `model` field to the underlying name.
    pub virtual_model: String,
    /// The request path (e.g. `/v1/chat/completions`). Lets a classifier scope
    /// itself to the endpoints it understands and ignore the rest.
    pub path: String,
    /// The raw request body bytes (the messages / content blocks the classifier
    /// parses for `cache_control` markers). This is the *pre-strip* body, so
    /// the markers are still present for the classifier to read.
    pub body: bytes::Bytes,
    /// The raw bearer token the request authenticated with — the API key secret
    /// in this system. onwards holds no notion of users; the classifier resolves
    /// this to a billing principal (e.g. `user_id`/`org_id`) to scope the cache
    /// per customer. `None` when the request carried no bearer token, in which
    /// case the request is un-scopable and a classifier should not cache it.
    pub api_key: Option<String>,
}

/// The paradigm-neutral result of classification: the read/write token split.
///
/// This is deliberately *not* OpenAI- or Anthropic-shaped — it is the neutral
/// representation that a per-endpoint module (the `openai` shaping today, an
/// `anthropic` shaping later) projects into the wire `usage` object. All
/// counts default to zero, which is exactly what [`NoopCacheClassifier`] returns.
///
/// `read` is the number of cached input tokens read back (charged at the
/// discounted rate); `creation_*` are the new tokens written to the cache,
/// split per TTL tier because their write premiums differ.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Cached input tokens read back from a prior write (the discounted span).
    pub read: u64,
    /// New tokens written under the 5-minute TTL tier.
    pub creation_5m: u64,
    /// New tokens written under the 1-hour TTL tier.
    pub creation_1h: u64,
    /// New tokens written under the 24-hour TTL tier.
    pub creation_24h: u64,
}

impl CacheStats {
    /// Total tokens written to the cache across all TTL tiers.
    pub fn creation_total(&self) -> u64 {
        self.creation_5m
            .saturating_add(self.creation_1h)
            .saturating_add(self.creation_24h)
    }

    /// True when every count is zero (the no-op / "no cached tokens" case).
    ///
    /// A convenience predicate only. The injection path deliberately still writes
    /// the zeroed cache fields when a classifier is wired — the usage shape is kept
    /// consistent with Anthropic/OpenAI, which always include the cache fields when
    /// caching is active (see [`crate::cache_usage`]). The dormant case (no
    /// classifier wired) is what leaves the response untouched, not a zero result.
    pub fn is_zero(&self) -> bool {
        *self == CacheStats::default()
    }
}

/// The abstraction onwards owns: given a request, return the read/write token
/// split. Implemented by dwctl (with the real index/tokenizer logic) and
/// injected at startup; onwards holds it as a trait object and never names or
/// imports the implementor.
///
/// `classify` must be cheap relative to the model call — it runs concurrently
/// with generation and is joined under a deadline, so a slow or failing classifier
/// degrades to all-zero stats rather than delaying the response.
#[async_trait]
pub trait CacheClassifier: Send + Sync {
    /// Classify a request into a neutral [`CacheStats`] split.
    async fn classify(&self, req: &ClassifyInput) -> CacheStats;
}

/// The default no-op classifier: always returns all-zero [`CacheStats`].
///
/// This is what makes onwards runnable standalone with the cache path "on" but
/// effectively invisible — the injected `usage` cache fields are all zero, so
/// downstream billing sees zeros and is unaffected. It is also the classifier
/// dwctl wires for local end-to-end validation of the spine.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopCacheClassifier;

#[async_trait]
impl CacheClassifier for NoopCacheClassifier {
    async fn classify(&self, _req: &ClassifyInput) -> CacheStats {
        CacheStats::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_stats_default_is_all_zero() {
        let s = CacheStats::default();
        assert_eq!(s.read, 0);
        assert_eq!(s.creation_5m, 0);
        assert_eq!(s.creation_1h, 0);
        assert_eq!(s.creation_24h, 0);
        assert_eq!(s.creation_total(), 0);
        assert!(s.is_zero());
    }

    #[test]
    fn creation_total_sums_tiers() {
        let s = CacheStats {
            read: 100,
            creation_5m: 1,
            creation_1h: 2,
            creation_24h: 3,
        };
        assert_eq!(s.creation_total(), 6);
        assert!(!s.is_zero());
    }

    #[tokio::test]
    async fn noop_classifier_returns_zero_stats() {
        let classifier = NoopCacheClassifier;
        let input = ClassifyInput {
            virtual_model: "my-virtual-model".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: bytes::Bytes::from_static(
                br#"{"model":"my-virtual-model","messages":[{"role":"user","content":"hi"}]}"#,
            ),
            api_key: Some("sk-test".to_string()),
        };
        let stats = classifier.classify(&input).await;
        assert_eq!(stats, CacheStats::default());
        assert!(stats.is_zero());
    }
}
