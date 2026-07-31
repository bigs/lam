use serde::Serialize;
use serde_json::Value;

/// A structured view of an unhandled JavaScript exception.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeException {
    /// Best available human-readable exception text or stack.
    pub message: String,
    /// Untouched Chrome DevTools Protocol exception details.
    pub details: Value,
}

/// A failed TypeScript cell evaluation.
#[derive(Clone, Debug, PartialEq, Serialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EvalError {
    /// TypeScript parsing, transpilation, or unsupported module syntax failed.
    #[error("TypeScript transpilation failed: {message}")]
    Transpile {
        /// Human-readable diagnostic.
        message: String,
    },

    /// JavaScript threw an exception or a host bridge failed.
    #[error("JavaScript evaluation failed: {exception:?}")]
    Runtime {
        /// Exception text and native protocol details.
        exception: RuntimeException,
    },

    /// A Rust builtin rejected its Promise with a typed domain failure.
    #[error("a Rust builtin failed")]
    BuiltinFailure {
        /// The serializable failure value produced by the builtin.
        error: Value,
    },

    /// The returned value cannot cross Lam's JSON-only boundary.
    #[error("the evaluation result is not JSON serializable: {message}")]
    ResultNotSerializable {
        /// Explanation produced by the isolate's serializer.
        message: String,
    },

    /// The host deadline interrupted the cell and replaced its isolate.
    ///
    /// The previous heap is lost. Host side effects completed before the
    /// interruption are not rolled back.
    #[error("evaluation timed out after {timeout_ms} ms; isolate restarted")]
    TimedOut {
        /// Effective host-bounded timeout.
        timeout_ms: u64,
        /// Generation which was interrupted.
        previous_generation: u64,
        /// Fresh generation installed before this error was returned.
        new_generation: u64,
    },

    /// The poisoned isolate was dropped, but its replacement could not start.
    ///
    /// The previous heap is lost. Host side effects completed before the
    /// interruption are not rolled back.
    #[error("evaluation timed out and isolate restart failed: {message}")]
    RestartFailed {
        /// Effective host-bounded timeout.
        timeout_ms: u64,
        /// Generation which was interrupted.
        previous_generation: u64,
        /// Generation which failed to initialize.
        attempted_generation: u64,
        /// Startup failure.
        message: String,
    },

    /// No usable isolate is currently installed.
    #[error("the isolate is unavailable after a failed restart")]
    Poisoned,

    /// Lam or `deno_core` violated an internal invariant.
    #[error("internal isolate error: {message}")]
    Internal {
        /// Diagnostic intended for the embedding application.
        message: String,
    },
}

impl EvalError {
    pub(crate) fn internal(error: impl std::fmt::Display) -> Self {
        Self::Internal {
            message: error.to_string(),
        }
    }
}

/// Failure to validate namespaces or initialize an isolate.
#[derive(Debug, thiserror::Error)]
pub enum IsolateBuildError {
    /// A namespace segment or function is not a valid JavaScript identifier.
    #[error("invalid {kind} name `{name}`")]
    InvalidName {
        /// Kind of name being validated.
        kind: &'static str,
        /// Invalid segment.
        name: String,
    },
    /// Two namespaces use the same fully-qualified path.
    #[error("duplicate namespace `{path}`")]
    DuplicateNamespace {
        /// Repeated fully-qualified namespace path.
        path: String,
    },
    /// Two builtins use the same path.
    #[error("duplicate builtin `{path}`")]
    DuplicateFunction {
        /// Repeated fully-qualified builtin path.
        path: String,
    },
    /// A function path is also required as a namespace object.
    #[error("namespace `{namespace}` conflicts with builtin `{function}`")]
    NamespaceFunctionConflict {
        /// Namespace which needs the function path as an object.
        namespace: String,
        /// Function occupying that path.
        function: String,
    },
    /// The default or maximum timeout was zero.
    #[error("isolate timeouts must be greater than zero")]
    InvalidTimeout,
    /// `rusty_v8` keeps an isolate entered for its full lifetime, so another
    /// Lam isolate cannot safely be interleaved on the same system thread.
    #[error("this system thread already owns a live Lam isolate")]
    ThreadAlreadyOwnsIsolate,
    /// V8, the Lam bootstrap, or the local inspector failed to initialize.
    #[error("failed to initialize the JavaScript runtime: {message}")]
    RuntimeInitialization {
        /// Startup diagnostic.
        message: String,
    },
}
