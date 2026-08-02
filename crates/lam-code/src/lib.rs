//! Optional coding-agent capabilities for lam.
//!
//! This crate installs typed filesystem, editing, and shell namespaces without
//! changing lam's one-tool model interface. Local policies are useful
//! guardrails but are not an operating-system sandbox.

mod config;
mod edit;
mod error;
mod fs;
mod output;
mod patch;
mod path;
mod shell;

pub use config::{
    CaptureConfig, CodingPack, CodingPackBuildError, CodingPackBuilder, FilesystemAccess,
    ListConfig, ReadConfig, ShellConfig,
};
pub use shell::{
    CapturedStream, CommandFuture, CommandOutput, CommandRequest, CommandRunner,
    CommandRunnerError, LocalCommandRunner,
};
