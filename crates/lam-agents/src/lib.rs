//! Optional multi-actor scheduling and subagent capabilities for Lam.
//!
//! `lam` remains the minimal single-actor library. This crate adds a bounded
//! pool of current-thread executors and a manifest-driven `lam.agents`
//! namespace without changing the model's one-tool interface.

mod address;
mod config;
mod error;
mod event;
mod namespace;
mod outcome;
mod system;

pub use address::{ActorAddress, InvalidActorAddress};
pub use config::{ModelTarget, SubagentConfig, SubagentConfigBuilder};
pub use error::{AgentSystemBuildError, AgentSystemError, SubagentConfigError};
pub use event::{AgentSystemEvent, AgentSystemEvents, StopReason};
pub use namespace::{
    AgentIdentity, ListError, ListRequest, SendError, SendReceipt, SendRequest, SpawnError,
    SpawnReceipt, SpawnRequest, StopError, StopReceipt, StopRequest,
};
pub use outcome::AgentOutcome;
pub use system::{Agent, AgentAbortHandle, AgentSystem, AgentSystemBuilder};
