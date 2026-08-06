use lam::Namespace;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::EditError;
use crate::patch::PatchPlan;
use crate::path::{CodingWorkspace, PathFailure};

/// Multi-line text accepted as either one string or an array of lines.
///
/// Models often embed patch bodies in TypeScript template literals. Any
/// backtick inside that body (Markdown code spans, code samples, etc.) closes
/// the template early and fails transpile. Passing an array of ordinary
/// double-quoted strings avoids that entirely.
#[derive(Clone, Debug, JsonSchema, Deserialize)]
#[serde(untagged)]
pub(crate) enum TextBody {
    /// Complete text, including embedded newlines.
    Text(String),
    /// Individual lines joined with `\n` (no trailing newline is added).
    Lines(Vec<String>),
}

impl TextBody {
    fn into_string(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Lines(lines) => lines.join("\n"),
        }
    }
}

/// Input accepted by `lam.edit.apply`.
#[derive(Clone, Debug, JsonSchema, Deserialize)]
pub(crate) struct ApplyPatchRequest {
    /// Complete `*** Begin Patch` file-oriented patch as a string, or as an
    /// array of lines. Prefer the array form whenever the patch body contains
    /// backticks or other characters that break TypeScript template literals.
    pub patch: TextBody,
}

/// One committed file operation.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum FileChange {
    /// A new file was written.
    Added {
        /// Relative patch path.
        path: String,
    },
    /// An existing file was modified in place.
    Updated {
        /// Relative patch path.
        path: String,
    },
    /// An existing file was removed.
    Deleted {
        /// Relative patch path.
        path: String,
    },
    /// An existing file was updated and moved.
    Moved {
        /// Original relative patch path.
        from: String,
        /// New relative patch path.
        to: String,
    },
}

/// Successful result from `lam.edit.apply`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub(crate) struct ApplyPatchOutput {
    /// Changes committed in patch order.
    pub changes: Vec<FileChange>,
}

/// Input accepted by `lam.edit.write`.
#[derive(Clone, Debug, JsonSchema, Deserialize)]
pub(crate) struct WriteRequest {
    /// Relative workspace path or an allowed absolute path.
    pub path: String,
    /// Complete UTF-8 file contents as a string, or as an array of lines.
    /// Prefer the array form when contents contain backticks.
    pub content: TextBody,
}

/// Successful result from `lam.edit.write`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WriteOutput {
    /// Requested path spelling.
    pub path: String,
    /// UTF-8 bytes written.
    pub bytes_written: usize,
    /// Whether the target did not exist before this operation.
    pub created: bool,
}

/// Builds the mutating `lam.edit` namespace for one shared coding workspace.
#[must_use]
pub(crate) fn edit_namespace(workspace: CodingWorkspace) -> Namespace {
    let apply_workspace = workspace.clone();
    let write_workspace = workspace;
    Namespace::new(
        "lam.edit",
        "Applies validated file patches and deliberate complete UTF-8 rewrites.",
    )
    .function(
        "apply",
        "Apply a file-oriented patch enclosed by *** Begin Patch and *** End Patch. Use explicit Add File, Delete File, or Update File sections; updates may include *** Move to, and hunks begin with @@ and space, -, or + prefixes. Context and removal lines match whole lines exactly: a substring within a line does not match. Paths must be relative. Every path and hunk is validated before the first mutation. Pass patch as a string or, when the body contains backticks, as an array of lines so TypeScript template literals are not required.",
        move |_context, request: ApplyPatchRequest| {
            let workspace = apply_workspace.clone();
            async move {
                let _mutation = workspace.mutation().await;
                let patch = request.patch.into_string();
                let plan = PatchPlan::prepare(&workspace, &patch).await?;
                let changes = plan.commit().await?;
                Ok::<_, EditError>(ApplyPatchOutput { changes })
            }
        },
    )
    .function(
        "write",
        "Create or completely rewrite one UTF-8 text file, creating missing parent directories. Prefer lam.edit.apply for targeted changes to an existing file. content may be a string or an array of lines; prefer the array form when contents contain backticks.",
        move |_context, request: WriteRequest| {
            let workspace = write_workspace.clone();
            async move { write_file(&workspace, request).await }
        },
    )
}

async fn write_file(
    workspace: &CodingWorkspace,
    request: WriteRequest,
) -> Result<WriteOutput, EditError> {
    let _mutation = workspace.mutation().await;
    let path = workspace
        .resolve_write(&request.path)
        .await
        .map_err(edit_path_error)?;
    let created = !tokio::fs::try_exists(&path)
        .await
        .map_err(|error| edit_io("inspect", &request.path, error))?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| edit_io("create parent directories for", &request.path, error))?;
    }
    let content = request.content.into_string();
    tokio::fs::write(&path, content.as_bytes())
        .await
        .map_err(|error| edit_io("write", &request.path, error))?;
    Ok(WriteOutput {
        path: request.path,
        bytes_written: content.len(),
        created,
    })
}

pub(crate) fn edit_path_error(error: PathFailure) -> EditError {
    match error {
        PathFailure::Invalid { path, message } | PathFailure::Unavailable { path, message } => {
            EditError::InvalidPath { path, message }
        }
        PathFailure::OutsideRoots { path } => EditError::InvalidPath {
            path,
            message: "path is outside the writable workspace root".to_owned(),
        },
    }
}

fn edit_io(operation: &'static str, path: &str, error: std::io::Error) -> EditError {
    EditError::Io {
        operation,
        path: path.to_owned(),
        message: error.to_string(),
    }
}
