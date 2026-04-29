//! Extensibility traits for Open Responses adapter
//!
//! This module provides traits that users can implement to extend the adapter's capabilities.
//! By default, the adapter operates statelessly, but these traits enable:
//!
//! - **ResponseStore**: Persistent storage for `previous_response_id` support
//! - **ToolExecutor**: Server-side tool execution during agent loops
//! - **MultiStepStore** + **StepExecutor**: Multi-step Open Responses
//!   orchestration. The pair drives [`crate::run_response_loop`].
//!   Independently optional — implementations that only need
//!   `previous_response_id` continue to use `ResponseStore` alone.

mod multi_step_store;
mod response_store;
mod step_executor;
mod tool_executor;

pub use multi_step_store::{
    ChainStep, ExecutorError, MultiStepStore, NextAction, RecordedStep, StepDescriptor, StepKind,
    StepState,
};
pub use response_store::{NoOpResponseStore, ResponseStore, StoreError};
pub use step_executor::{StepExecutor, ToolDispatch};
pub use tool_executor::{NoOpToolExecutor, RequestContext, ToolError, ToolExecutor, ToolSchema};
