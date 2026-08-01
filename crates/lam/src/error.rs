/// Failure to construct or start an actor runner.
#[derive(Debug, thiserror::Error)]
pub enum ActorBuildError {
    /// The actor identifier was empty.
    #[error(transparent)]
    InvalidActorId(#[from] lam_core::InvalidIdentifier),
    /// A configured model registry identity was empty.
    #[error("invalid model id: {0}")]
    InvalidModelId(#[source] lam_core::InvalidIdentifier),
    /// The runtime registry contained the same model identity twice.
    #[error("model id `{model_id}` was registered more than once")]
    DuplicateModelId {
        /// Duplicated registry key.
        model_id: lam_core::ModelId,
    },
    /// Compaction thresholds or budgets are internally inconsistent.
    #[error("invalid compaction configuration: {0}")]
    InvalidCompactionConfig(#[source] lam_core::CompactionConfigError),
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
    /// A requested model registry identity was empty.
    #[error("invalid model id: {message}")]
    InvalidModelId {
        /// Validation diagnostic.
        message: String,
    },
    /// A requested model is not present in this runtime registry.
    #[error("model `{model_id}` is not registered")]
    UnknownModel {
        /// Missing registry key.
        model_id: lam_core::ModelId,
    },
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
    /// The configured compactor could not produce or install a checkpoint.
    #[error("context compaction failed: {message}")]
    Compaction {
        /// Strategy, validation, or materialization diagnostic.
        message: String,
    },
    /// The provider still rejected the model context as oversized.
    #[error("model context exceeds the provider limit")]
    ContextOverflow,
    /// An embedding explicitly requested compaction while it was disabled.
    #[error("context compaction is disabled")]
    CompactionDisabled,
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
