//! Persistent, capability-oriented TypeScript execution for Lam.
//!
//! This crate owns the embedded `deno_core` isolate and the typed bridge from
//! Rust functions to Promise-native TypeScript namespaces. It intentionally
//! supplies no filesystem, process, or network authority by default.

mod bridge;
mod builtin;
mod error;
mod inspector;
mod isolate;
mod transpile;

pub use builtin::{Namespace, Never, OperationContext};
pub use error::{EvalError, IsolateBuildError, RuntimeException};
pub use isolate::{
    ConsoleEntry, ConsoleLevel, EvalOptions, EvalOutput, EvalValue, Isolate, IsolateBuilder,
    IsolateInterrupt,
};
