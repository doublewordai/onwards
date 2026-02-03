//! Model Switcher - Zero-reload model switching for GPU-constrained environments
//!
//! This module provides transparent request queuing and model switching for scenarios
//! where multiple models share a single GPU (or set of GPUs) and only one can be
//! "awake" at a time.
//!
//! ## Architecture
//!
//! The model switcher sits between incoming requests and the upstream provider:
//!
//! ```text
//! Request → ModelSwitcher → [Queue if model sleeping] → Wake model → Forward
//! ```
//!
//! ## Key Abstractions
//!
//! - [`SwitchPolicy`]: Decides WHEN to switch models (FIFO, batching, priority)
//! - [`ModelSwitcherCallbacks`]: HOW to switch models (vLLM sleep/wake, custom)
//! - [`ModelSwitcher`]: Coordinates queuing and switching
//!
//! ## Example
//!
//! ```ignore
//! use onwards::model_switcher::{ModelSwitcher, FifoPolicy, SleepLevel};
//!
//! // Create callbacks for your backend (e.g., vLLM)
//! let callbacks = VllmCallbacks::new(/* ... */);
//!
//! // Create policy
//! let policy = FifoPolicy::new(SleepLevel::L1, Duration::from_secs(30), true);
//!
//! // Create switcher
//! let switcher = ModelSwitcher::new(callbacks, policy, vec!["model-a", "model-b"]);
//!
//! // Use in request handling
//! switcher.forward_request("model-a", request).await?;
//! ```

mod callbacks;
mod policy;
mod switcher;
pub mod vllm;

pub use callbacks::{ModelSwitcherCallbacks, NoOpCallbacks, SleepLevel, SwitchError};
pub use policy::{
    BatchingPolicy, FifoPolicy, PolicyContext, PolicyDecision, SwitchContext, SwitchPolicy,
};
pub use switcher::{InFlightGuard, ModelSwitcher, ModelSwitcherConfig, PendingRequest, SwitcherState};
