use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;
use tokio::sync::{Mutex, MutexGuard};

use crate::config::CodingPackBuildError;

struct WorkspaceInner {
    root: PathBuf,
    _scratch: TempDir,
    scratch_root: PathBuf,
    mutation: Mutex<()>,
}

/// Shared workspace, mutation serialization, and command-output scratch state.
#[derive(Clone)]
pub(crate) struct CodingWorkspace {
    inner: Arc<WorkspaceInner>,
}

impl fmt::Debug for CodingWorkspace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodingWorkspace")
            .field("root", &self.inner.root)
            .field("scratch", &self.inner.scratch_root)
            .finish_non_exhaustive()
    }
}

impl CodingWorkspace {
    /// Resolves an existing directory and creates its ephemeral output scratch area.
    pub(crate) fn new(root: impl AsRef<Path>) -> Result<Self, CodingPackBuildError> {
        let requested = root.as_ref();
        let root =
            std::fs::canonicalize(requested).map_err(|error| CodingPackBuildError::Workspace {
                path: requested.to_path_buf(),
                message: error.to_string(),
            })?;
        if !root.is_dir() {
            return Err(CodingPackBuildError::Workspace {
                path: root,
                message: "path is not a directory".to_owned(),
            });
        }
        let scratch = tempfile::Builder::new()
            .prefix("lam-code-")
            .tempdir()
            .map_err(|error| CodingPackBuildError::Scratch {
                message: error.to_string(),
            })?;
        let scratch_root = std::fs::canonicalize(scratch.path()).map_err(|error| {
            CodingPackBuildError::Scratch {
                message: error.to_string(),
            }
        })?;
        Ok(Self {
            inner: Arc::new(WorkspaceInner {
                root,
                _scratch: scratch,
                scratch_root,
                mutation: Mutex::new(()),
            }),
        })
    }

    pub(crate) fn scratch(&self) -> &Path {
        &self.inner.scratch_root
    }

    pub(crate) async fn mutation(&self) -> MutexGuard<'_, ()> {
        self.inner.mutation.lock().await
    }

    pub(crate) async fn resolve_read(&self, raw: &str) -> Result<PathBuf, PathFailure> {
        let candidate = self.candidate(raw)?;
        let resolved = tokio::fs::canonicalize(&candidate).await.map_err(|error| {
            PathFailure::Unavailable {
                path: raw.to_owned(),
                message: error.to_string(),
            }
        })?;
        if self.is_readable(&resolved) {
            Ok(resolved)
        } else {
            Err(PathFailure::OutsideRoots {
                path: raw.to_owned(),
            })
        }
    }

    pub(crate) async fn resolve_write(&self, raw: &str) -> Result<PathBuf, PathFailure> {
        let candidate = self.candidate(raw)?;
        if !candidate.starts_with(&self.inner.root) || candidate == self.inner.root {
            return Err(PathFailure::OutsideRoots {
                path: raw.to_owned(),
            });
        }
        let mut existing = candidate.as_path();
        let existing_metadata = loop {
            match tokio::fs::symlink_metadata(existing).await {
                Ok(metadata) => break metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    existing = existing.parent().ok_or_else(|| PathFailure::Invalid {
                        path: raw.to_owned(),
                        message: "path has no existing ancestor".to_owned(),
                    })?;
                }
                Err(error) => {
                    return Err(PathFailure::Unavailable {
                        path: raw.to_owned(),
                        message: error.to_string(),
                    });
                }
            }
        };
        if existing == candidate && existing_metadata.file_type().is_symlink() {
            return Err(PathFailure::Invalid {
                path: raw.to_owned(),
                message: "symbolic-link mutation is not supported".to_owned(),
            });
        }

        let resolved_ancestor =
            tokio::fs::canonicalize(existing)
                .await
                .map_err(|error| PathFailure::Unavailable {
                    path: raw.to_owned(),
                    message: error.to_string(),
                })?;
        if !resolved_ancestor.starts_with(&self.inner.root) {
            return Err(PathFailure::OutsideRoots {
                path: raw.to_owned(),
            });
        }
        if existing == candidate {
            Ok(resolved_ancestor)
        } else {
            let metadata = tokio::fs::metadata(&resolved_ancestor)
                .await
                .map_err(|error| PathFailure::Unavailable {
                    path: raw.to_owned(),
                    message: error.to_string(),
                })?;
            if !metadata.is_dir() {
                return Err(PathFailure::Invalid {
                    path: raw.to_owned(),
                    message: "nearest existing parent is not a directory".to_owned(),
                });
            }
            let suffix =
                candidate
                    .strip_prefix(existing)
                    .map_err(|error| PathFailure::Invalid {
                        path: raw.to_owned(),
                        message: error.to_string(),
                    })?;
            Ok(resolved_ancestor.join(suffix))
        }
    }

    fn candidate(&self, raw: &str) -> Result<PathBuf, PathFailure> {
        if raw.is_empty() {
            return Err(PathFailure::Invalid {
                path: raw.to_owned(),
                message: "path must not be empty".to_owned(),
            });
        }
        let path = Path::new(raw);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.inner.root.join(path)
        };
        lexical_normalize(&joined).ok_or_else(|| PathFailure::Invalid {
            path: raw.to_owned(),
            message: "path escapes the filesystem root".to_owned(),
        })
    }

    fn is_readable(&self, path: &Path) -> bool {
        path.starts_with(&self.inner.root) || path.starts_with(&self.inner.scratch_root)
    }
}

#[derive(Debug)]
pub(crate) enum PathFailure {
    Invalid { path: String, message: String },
    OutsideRoots { path: String },
    Unavailable { path: String, message: String },
}

fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
        }
    }
    Some(normalized)
}
