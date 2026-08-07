use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lam::Namespace;
use thiserror::Error;

use crate::edit::edit_namespace;
use crate::fs::fs_namespace;
use crate::path::CodingWorkspace;
use crate::shell::{CommandRunner, shell_namespace};

/// Filesystem namespaces installed by a coding capability pack.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FilesystemAccess {
    /// Install neither `lam.fs` nor `lam.edit`.
    Disabled,
    /// Install `lam.fs` without mutation functions.
    ReadOnly,
    /// Install both `lam.fs` and `lam.edit`.
    #[default]
    ReadWrite,
}

/// Limits for one `lam.fs.read` result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadConfig {
    /// Lines returned when the caller omits `limit`.
    pub default_lines: usize,
    /// Largest line limit a caller may request.
    pub max_lines: usize,
    /// Largest complete-line payload returned in one call.
    pub max_bytes: usize,
}

impl Default for ReadConfig {
    fn default() -> Self {
        Self {
            default_lines: 200,
            max_lines: 2_000,
            max_bytes: 64 * 1024,
        }
    }
}

/// Limits for one `lam.fs.list` result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ListConfig {
    /// Entries returned when the caller omits `limit`.
    pub default_entries: usize,
    /// Largest entry limit a caller may request.
    pub max_entries: usize,
}

impl Default for ListConfig {
    fn default() -> Self {
        Self {
            default_entries: 200,
            max_entries: 2_000,
        }
    }
}

/// In-memory tail and full-output spill thresholds for a command stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureConfig {
    /// Maximum complete lines retained in the model-visible tail.
    pub max_lines: usize,
    /// Maximum bytes retained in the model-visible tail.
    pub max_bytes: usize,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            max_lines: 2_000,
            max_bytes: 64 * 1024,
        }
    }
}

/// Host bounds applied to `lam.shell.run` before invoking a runner.
///
/// The isolate's eval timeout remains an outer bound. Configure it above the
/// longest shell timeout when timeouts should return a normal shell outcome
/// rather than interrupting and replacing the isolate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShellConfig {
    /// Timeout used when the TypeScript request omits one.
    pub default_timeout: Duration,
    /// Largest timeout TypeScript may request.
    pub max_timeout: Duration,
    /// Output capture limits passed to the runner.
    pub capture: CaptureConfig,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(20),
            max_timeout: Duration::from_secs(10 * 60),
            capture: CaptureConfig::default(),
        }
    }
}

/// Invalid coding-pack configuration or workspace setup.
#[derive(Debug, Error)]
pub enum CodingPackBuildError {
    /// The configured workspace could not be resolved.
    #[error("workspace `{path}` is unavailable: {message}")]
    Workspace {
        /// Rejected workspace path.
        path: PathBuf,
        /// Filesystem diagnostic.
        message: String,
    },
    /// A line, entry, byte, or timeout bound was zero or internally inconsistent.
    #[error("invalid {field}: {message}")]
    InvalidLimit {
        /// Configuration field name.
        field: &'static str,
        /// Validation diagnostic.
        message: String,
    },
    /// The pack-owned scratch directory could not be created.
    #[error("could not create command-output scratch storage: {message}")]
    Scratch {
        /// Filesystem diagnostic.
        message: String,
    },
}

/// A configured collection of optional coding namespaces.
pub struct CodingPack {
    workspace: CodingWorkspace,
    namespaces: Vec<Namespace>,
}

impl fmt::Debug for CodingPack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodingPack")
            .field("workspace", &self.workspace)
            .field(
                "namespaces",
                &self
                    .namespaces
                    .iter()
                    .map(Namespace::path)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl CodingPack {
    /// Starts a pack using one directory as the base for relative paths.
    #[must_use]
    pub fn builder(root: impl Into<PathBuf>) -> CodingPackBuilder {
        CodingPackBuilder {
            root: root.into(),
            filesystem_access: FilesystemAccess::default(),
            read: ReadConfig::default(),
            list: ListConfig::default(),
            shell_config: ShellConfig::default(),
            runner: None,
        }
    }

    /// Clones the cheaply shared namespaces for registration with a lam builder.
    pub fn namespaces(&self) -> impl Iterator<Item = Namespace> + '_ {
        self.namespaces.iter().cloned()
    }
}

impl<'a> IntoIterator for &'a CodingPack {
    type Item = Namespace;
    type IntoIter = std::iter::Cloned<std::slice::Iter<'a, Namespace>>;

    fn into_iter(self) -> Self::IntoIter {
        self.namespaces.iter().cloned()
    }
}

/// Builder for one optional coding capability pack.
pub struct CodingPackBuilder {
    root: PathBuf,
    filesystem_access: FilesystemAccess,
    read: ReadConfig,
    list: ListConfig,
    shell_config: ShellConfig,
    runner: Option<Arc<dyn CommandRunner>>,
}

impl fmt::Debug for CodingPackBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodingPackBuilder")
            .field("root", &self.root)
            .field("filesystem_access", &self.filesystem_access)
            .field("read", &self.read)
            .field("list", &self.list)
            .field("shell_config", &self.shell_config)
            .field("has_runner", &self.runner.is_some())
            .finish()
    }
}

impl CodingPackBuilder {
    /// Selects which filesystem namespaces the pack installs.
    #[must_use]
    pub const fn filesystem_access(mut self, access: FilesystemAccess) -> Self {
        self.filesystem_access = access;
        self
    }

    /// Replaces numbered file-read limits.
    #[must_use]
    pub const fn read_config(mut self, config: ReadConfig) -> Self {
        self.read = config;
        self
    }

    /// Replaces direct-directory-listing limits.
    #[must_use]
    pub const fn list_config(mut self, config: ListConfig) -> Self {
        self.list = config;
        self
    }

    /// Replaces shell timeout and output-capture bounds.
    #[must_use]
    pub const fn shell_config(mut self, config: ShellConfig) -> Self {
        self.shell_config = config;
        self
    }

    /// Installs `lam.shell` using an explicit host command runner.
    #[must_use]
    pub fn shell(mut self, runner: impl CommandRunner) -> Self {
        self.runner = Some(Arc::new(runner));
        self
    }

    /// Validates the policy, creates scratch storage, and materializes namespaces.
    pub fn build(self) -> Result<CodingPack, CodingPackBuildError> {
        if self.filesystem_access != FilesystemAccess::Disabled {
            validate_read(self.read)?;
            validate_list(self.list)?;
        }
        if self.runner.is_some() {
            validate_shell(self.shell_config)?;
        }

        let workspace = CodingWorkspace::new(&self.root)?;
        let mut namespaces = Vec::new();
        if self.filesystem_access != FilesystemAccess::Disabled {
            namespaces.push(fs_namespace(workspace.clone(), self.read, self.list));
        }
        if self.filesystem_access == FilesystemAccess::ReadWrite {
            namespaces.push(edit_namespace(workspace.clone()));
        }
        if let Some(runner) = self.runner {
            namespaces.push(shell_namespace(
                workspace.clone(),
                runner,
                self.shell_config,
            ));
        }

        Ok(CodingPack {
            workspace,
            namespaces,
        })
    }
}

fn validate_read(config: ReadConfig) -> Result<(), CodingPackBuildError> {
    nonzero("read.default_lines", config.default_lines)?;
    nonzero("read.max_lines", config.max_lines)?;
    nonzero("read.max_bytes", config.max_bytes)?;
    if config.default_lines > config.max_lines {
        return Err(CodingPackBuildError::InvalidLimit {
            field: "read.default_lines",
            message: "must not exceed read.max_lines".to_owned(),
        });
    }
    Ok(())
}

fn validate_list(config: ListConfig) -> Result<(), CodingPackBuildError> {
    nonzero("list.default_entries", config.default_entries)?;
    nonzero("list.max_entries", config.max_entries)?;
    if config.default_entries > config.max_entries {
        return Err(CodingPackBuildError::InvalidLimit {
            field: "list.default_entries",
            message: "must not exceed list.max_entries".to_owned(),
        });
    }
    Ok(())
}

fn validate_shell(config: ShellConfig) -> Result<(), CodingPackBuildError> {
    nonzero("shell.capture.max_lines", config.capture.max_lines)?;
    nonzero("shell.capture.max_bytes", config.capture.max_bytes)?;
    if config.default_timeout.is_zero() {
        return Err(CodingPackBuildError::InvalidLimit {
            field: "shell.default_timeout",
            message: "must be greater than zero".to_owned(),
        });
    }
    if config.max_timeout.is_zero() || config.default_timeout > config.max_timeout {
        return Err(CodingPackBuildError::InvalidLimit {
            field: "shell.max_timeout",
            message: "must be nonzero and at least shell.default_timeout".to_owned(),
        });
    }
    Ok(())
}

fn nonzero(field: &'static str, value: usize) -> Result<(), CodingPackBuildError> {
    if value == 0 {
        Err(CodingPackBuildError::InvalidLimit {
            field,
            message: "must be greater than zero".to_owned(),
        })
    } else {
        Ok(())
    }
}
