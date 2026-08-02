use schemars::JsonSchema;
use serde::Serialize;

/// Structured failure from `lam.fs`.
#[derive(Clone, Debug, JsonSchema, Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "camelCase")]
pub(crate) enum FsError {
    /// A path was empty or malformed.
    #[error("invalid path `{path}`: {message}")]
    InvalidPath {
        /// Rejected path.
        path: String,
        /// Validation diagnostic.
        message: String,
    },
    /// A path resolved outside configured readable roots.
    #[error("path `{path}` is outside configured readable roots")]
    OutsideRoots {
        /// Rejected path.
        path: String,
    },
    /// A path could not be resolved or read.
    #[error("path `{path}` is unavailable: {message}")]
    Unavailable {
        /// Requested path.
        path: String,
        /// Filesystem diagnostic.
        message: String,
    },
    /// A file operation received a directory or vice versa.
    #[error("path `{path}` is not a {expected}")]
    WrongKind {
        /// Requested path.
        path: String,
        /// Expected path kind.
        expected: &'static str,
    },
    /// A one-indexed line offset or requested limit was invalid.
    #[error("invalid {field} {value}: {message}")]
    InvalidRange {
        /// Input field name.
        field: &'static str,
        /// Rejected numeric value.
        value: usize,
        /// Validation diagnostic.
        message: String,
    },
    /// One source line could not fit without partial-line truncation.
    #[error("line {line} in `{path}` is {bytes} bytes, exceeding the {max_bytes}-byte limit")]
    LineTooLarge {
        /// Requested path.
        path: String,
        /// One-indexed source line.
        line: usize,
        /// Full encoded line size.
        bytes: usize,
        /// Configured complete-line ceiling.
        max_bytes: usize,
    },
    /// A filesystem operation failed after path validation.
    #[error("could not {operation} `{path}`: {message}")]
    Io {
        /// Short operation description.
        operation: &'static str,
        /// Affected path.
        path: String,
        /// Filesystem diagnostic.
        message: String,
    },
}

/// Structured failure from `lam.edit`.
#[derive(Clone, Debug, JsonSchema, Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "camelCase")]
pub(crate) enum EditError {
    /// Patch syntax was invalid.
    #[error("invalid patch at line {line}: {message}")]
    InvalidPatch {
        /// One-indexed patch line, or zero for a whole-document error.
        line: usize,
        /// Parse diagnostic.
        message: String,
    },
    /// A mutation path was invalid or outside the writable root.
    #[error("invalid mutation path `{path}`: {message}")]
    InvalidPath {
        /// Rejected path.
        path: String,
        /// Validation diagnostic.
        message: String,
    },
    /// Current filesystem contents did not satisfy the proposed operation.
    #[error("patch conflict in `{path}`: {message}")]
    Conflict {
        /// Conflicting path.
        path: String,
        /// Context or existence diagnostic.
        message: String,
    },
    /// A filesystem operation failed before any known mutation completed.
    #[error("could not {operation} `{path}`: {message}")]
    Io {
        /// Short operation description.
        operation: &'static str,
        /// Affected path.
        path: String,
        /// Filesystem diagnostic.
        message: String,
    },
    /// A commit failed after one or more planned changes completed.
    #[error("patch commit failed after modifying {completed:?}: {message}")]
    PartialCommit {
        /// Paths known to have completed before the failure.
        completed: Vec<String>,
        /// Commit diagnostic.
        message: String,
    },
}

/// Structured failure from `lam.shell`.
#[derive(Clone, Debug, JsonSchema, Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "camelCase")]
pub(crate) enum ShellError {
    /// A shell command was empty.
    #[error("command must not be empty")]
    InvalidCommand,
    /// A requested working directory was invalid or outside readable roots.
    #[error("invalid command working directory `{path}`: {message}")]
    InvalidCwd {
        /// Rejected working directory.
        path: String,
        /// Validation diagnostic.
        message: String,
    },
    /// A caller-supplied timeout exceeded host policy.
    #[error("invalid timeout {timeout_ms} ms: must be between 1 and {max_timeout_ms} ms")]
    InvalidTimeout {
        /// Requested timeout.
        timeout_ms: u64,
        /// Host maximum.
        max_timeout_ms: u64,
    },
    /// The injected runner failed before producing a normal command outcome.
    #[error("command runner failed: {message}")]
    Runner {
        /// Runner diagnostic.
        message: String,
    },
}
