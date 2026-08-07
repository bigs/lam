use lam::Namespace;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::config::{ListConfig, ReadConfig};
use crate::error::FsError;
use crate::path::{CodingWorkspace, PathFailure};

/// Input accepted by `lam.fs.read`.
#[derive(Clone, Debug, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReadRequest {
    /// Path relative to the configured working directory, or an absolute path.
    pub path: String,
    /// One-indexed source line at which to begin; defaults to one.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum complete lines to return; defaults to host configuration.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Numbered, bounded text returned by `lam.fs.read`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReadOutput {
    /// Requested path spelling.
    pub path: String,
    /// First requested one-indexed source line.
    pub start_line: usize,
    /// Last returned source line, absent for an empty result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    /// Tab-separated line numbers and source text.
    pub content: String,
    /// Offset for the next chunk, absent at end of file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
}

/// Input accepted by `lam.fs.list`.
#[derive(Clone, Debug, Default, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListRequest {
    /// Directory to list; defaults to the configured working-directory base.
    #[serde(default)]
    pub path: Option<String>,
    /// Return entries lexically after this previously returned name.
    #[serde(default)]
    pub after: Option<String>,
    /// Maximum direct children to return; defaults to host configuration.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// A direct directory child's filesystem kind.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum FileKind {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Symbolic link, not followed by the listing operation.
    Symlink,
    /// Socket, device, or another filesystem-specific kind.
    Other,
}

/// One structured direct child returned by `lam.fs.list`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListEntry {
    /// File name relative to the listed directory.
    pub name: String,
    /// Filesystem kind without following symbolic links.
    pub kind: FileKind,
    /// Regular-file byte length when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

/// Sorted, paginated direct children returned by `lam.fs.list`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListOutput {
    /// Requested path spelling.
    pub path: String,
    /// Lexically sorted direct children.
    pub entries: Vec<ListEntry>,
    /// Pass this value as `after` to continue, absent on the final page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after: Option<String>,
}

/// Builds the read-only `lam.fs` namespace for one shared coding workspace.
#[must_use]
pub(crate) fn fs_namespace(
    workspace: CodingWorkspace,
    read_config: ReadConfig,
    list_config: ListConfig,
) -> Namespace {
    let read_workspace = workspace.clone();
    let list_workspace = workspace;
    Namespace::new(
        "lam.fs",
        "Reads bounded text chunks and lists direct workspace children without mutation.",
    )
    .function(
        "read",
        "Read UTF-8 text with one-indexed line numbers. Results default to a bounded chunk; pass the returned nextOffset as offset to continue. Complete UTF-8 output files spilled by lam.shell are readable here too.",
        move |_context, request: ReadRequest| {
            let workspace = read_workspace.clone();
            async move { read_text(&workspace, request, read_config).await }
        },
    )
    .function(
        "list",
        "List lexically sorted direct children. This operation is non-recursive; traverse further in TypeScript and pass nextAfter as after to continue a large directory.",
        move |_context, request: ListRequest| {
            let workspace = list_workspace.clone();
            async move { list_directory(&workspace, request, list_config).await }
        },
    )
}

async fn read_text(
    workspace: &CodingWorkspace,
    request: ReadRequest,
    config: ReadConfig,
) -> Result<ReadOutput, FsError> {
    let offset = request.offset.unwrap_or(1);
    if offset == 0 {
        return Err(FsError::InvalidRange {
            field: "offset",
            value: offset,
            message: "line offsets are one-indexed".to_owned(),
        });
    }
    let limit = request.limit.unwrap_or(config.default_lines);
    if limit == 0 || limit > config.max_lines {
        return Err(FsError::InvalidRange {
            field: "limit",
            value: limit,
            message: format!("must be between 1 and {}", config.max_lines),
        });
    }

    let path = workspace
        .resolve_read(&request.path)
        .await
        .map_err(fs_path_error)?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| fs_io("inspect", &request.path, error))?;
    if !metadata.is_file() {
        return Err(FsError::WrongKind {
            path: request.path,
            expected: "file",
        });
    }

    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|error| fs_io("open", &request.path, error))?;
    let mut reader = BufReader::new(file);
    let mut source_line = String::new();
    let mut line_number = 0usize;
    let mut returned = 0usize;
    let mut rendered_bytes = 0usize;
    let mut content = String::new();
    let mut end_line = None;
    let mut next_offset = None;

    loop {
        source_line.clear();
        let read = reader
            .read_line(&mut source_line)
            .await
            .map_err(|error| fs_io("read", &request.path, error))?;
        if read == 0 {
            break;
        }
        line_number += 1;
        if line_number < offset {
            continue;
        }
        if returned == limit {
            next_offset = Some(line_number);
            break;
        }

        trim_line_ending(&mut source_line);
        let rendered = format!("{line_number}\t{source_line}");
        let separator_bytes = usize::from(!content.is_empty());
        let candidate_bytes = rendered_bytes + separator_bytes + rendered.len();
        if candidate_bytes > config.max_bytes {
            if returned == 0 {
                return Err(FsError::LineTooLarge {
                    path: request.path,
                    line: line_number,
                    bytes: rendered.len(),
                    max_bytes: config.max_bytes,
                });
            }
            next_offset = Some(line_number);
            break;
        }
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&rendered);
        rendered_bytes = candidate_bytes;
        returned += 1;
        end_line = Some(line_number);
    }

    if line_number < offset && !(line_number == 0 && offset == 1) {
        return Err(FsError::InvalidRange {
            field: "offset",
            value: offset,
            message: format!("is beyond the end of the {line_number}-line file"),
        });
    }

    Ok(ReadOutput {
        path: request.path,
        start_line: offset,
        end_line,
        content,
        next_offset,
    })
}

async fn list_directory(
    workspace: &CodingWorkspace,
    request: ListRequest,
    config: ListConfig,
) -> Result<ListOutput, FsError> {
    let requested_path = request.path.unwrap_or_else(|| ".".to_owned());
    let limit = request.limit.unwrap_or(config.default_entries);
    if limit == 0 || limit > config.max_entries {
        return Err(FsError::InvalidRange {
            field: "limit",
            value: limit,
            message: format!("must be between 1 and {}", config.max_entries),
        });
    }
    let path = workspace
        .resolve_read(&requested_path)
        .await
        .map_err(fs_path_error)?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| fs_io("inspect", &requested_path, error))?;
    if !metadata.is_dir() {
        return Err(FsError::WrongKind {
            path: requested_path,
            expected: "directory",
        });
    }

    let mut directory = tokio::fs::read_dir(&path)
        .await
        .map_err(|error| fs_io("list", &requested_path, error))?;
    let mut entries = Vec::new();
    while let Some(entry) = directory
        .next_entry()
        .await
        .map_err(|error| fs_io("list", &requested_path, error))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry
            .file_type()
            .await
            .map_err(|error| fs_io("inspect", &entry.path().display().to_string(), error))?;
        let kind = if file_type.is_file() {
            FileKind::File
        } else if file_type.is_dir() {
            FileKind::Directory
        } else if file_type.is_symlink() {
            FileKind::Symlink
        } else {
            FileKind::Other
        };
        let size_bytes = if kind == FileKind::File {
            Some(
                entry
                    .metadata()
                    .await
                    .map_err(|error| fs_io("inspect", &entry.path().display().to_string(), error))?
                    .len(),
            )
        } else {
            None
        };
        entries.push(ListEntry {
            name,
            kind,
            size_bytes,
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    if let Some(after) = &request.after {
        entries.retain(|entry| entry.name.as_str() > after.as_str());
    }

    let has_more = entries.len() > limit;
    entries.truncate(limit);
    let next_after = has_more.then(|| entries.last().expect("nonzero limit").name.clone());
    Ok(ListOutput {
        path: requested_path,
        entries,
        next_after,
    })
}

fn trim_line_ending(line: &mut String) {
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
}

fn fs_path_error(error: PathFailure) -> FsError {
    match error {
        PathFailure::Invalid { path, message } => FsError::InvalidPath { path, message },
        PathFailure::Unavailable { path, message } => FsError::Unavailable { path, message },
    }
}

fn fs_io(operation: &'static str, path: &str, error: std::io::Error) -> FsError {
    FsError::Io {
        operation,
        path: path.to_owned(),
        message: error.to_string(),
    }
}
