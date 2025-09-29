//! Authentication utilities for secure API key validation
//!
//! This module provides secure authentication mechanisms for validating API keys
//! against timing attacks using constant-time comparison operations.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use subtle::ConstantTimeEq;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgorithm {
    Sha256,
}

/// A wrapper around String that uses constant-time equality comparison
/// to prevent timing attacks on API key validation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConstantTimeString(String);

impl From<String> for ConstantTimeString {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// This is the point: use the subtle crate for comparisons
impl PartialEq for ConstantTimeString {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_bytes().ct_eq(other.0.as_bytes()).into()
    }
}

impl Eq for ConstantTimeString {}

impl Hash for ConstantTimeString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

/// Type alias for a HashSet of (best effort) constant-time strings, used to API keys
pub type KeySet = HashSet<ConstantTimeString>;

/// Applies the specified hash algorithm to a key
pub fn hash_key(key: &str, algorithm: Option<&HashAlgorithm>) -> String {
    match algorithm {
        Some(HashAlgorithm::Sha256) => {
            let mut hasher = Sha256::new();
            hasher.update(key.as_bytes());
            format!("{:x}", hasher.finalize())
        }
        None => key.to_string(),
    }
}

/// Validates a bearer token against a set of valid keys using constant-time comparison
pub fn validate_bearer_token(
    keys: &KeySet,
    token: &str,
    hash_algorithm: Option<&HashAlgorithm>,
) -> bool {
    let processed_token = hash_key(token, hash_algorithm);
    keys.contains(&ConstantTimeString::from(processed_token))
}
