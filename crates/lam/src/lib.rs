//! Public facade for the Lam coding-agent runtime.
//!
//! The actor builder will arrive in a later implementation slice. The completed
//! persistent-eval kernel is re-exported here so embedders can use and test the
//! first Lam primitive through the intended public crate.

pub use lam_deno::{
    ConsoleEntry, ConsoleLevel, EvalError, EvalOptions, EvalOutput, EvalValue, Isolate,
    IsolateBuildError, IsolateBuilder, Namespace, Never, OperationContext, RuntimeException,
};
