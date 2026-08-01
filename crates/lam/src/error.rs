/// Failure to construct or start an actor runner.
#[derive(Debug, thiserror::Error)]
pub enum ActorBuildError {
    /// The actor identifier was empty.
    #[error(transparent)]
    InvalidActorId(#[from] lam_core::InvalidIdentifier),
    /// The dedicated runner thread could not be created.
    #[error("failed to start actor thread: {0}")]
    ThreadSpawn(#[source] std::io::Error),
    /// The runner failed before its actor became available.
    #[error("failed to initialize actor: {message}")]
    Initialization {
        /// Isolate or executor diagnostic.
        message: String,
    },
}

/// One actor operation could not complete.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ActorError {
    /// A Rust input could not cross Lam's JSON boundary.
    #[error("input could not be serialized: {message}")]
    InputSerialization {
        /// Serde diagnostic.
        message: String,
    },
    /// A configured journal backend failed.
    #[error("actor journal failed: {message}")]
    Journal {
        /// Backend or contract diagnostic.
        message: String,
    },
    /// Durable actor events could not form a coherent projection.
    #[error("actor state is invalid: {message}")]
    State {
        /// Projection diagnostic.
        message: String,
    },
    /// The configured provider failed before returning a native response.
    #[error("model provider failed: {message}")]
    Provider {
        /// Provider diagnostic.
        message: String,
    },
    /// The configured codec could not encode or interpret a payload.
    #[error("model codec failed: {message}")]
    Codec {
        /// Codec diagnostic.
        message: String,
    },
    /// The actor runner rejected a second overlapping call.
    #[error("the actor already has an active call")]
    Busy,
    /// The actor thread is no longer available.
    #[error("the actor runner is unavailable")]
    Unavailable,
    /// Forceful actor cancellation stopped the current operation.
    #[error("the actor operation was aborted")]
    Aborted,
    /// The dedicated actor thread could not be joined cleanly.
    #[error("failed to join the actor runner: {message}")]
    RunnerJoin {
        /// Thread or blocking-task diagnostic.
        message: String,
    },
    /// A terminal value did not match the requested Rust output type.
    #[error("terminal output did not match its Rust type: {message}")]
    OutputDecode {
        /// Serde diagnostic.
        message: String,
    },
}
