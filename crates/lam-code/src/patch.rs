use std::path::{Path, PathBuf};

use crate::edit::{FileChange, edit_path_error};
use crate::error::EditError;
use crate::path::CodingWorkspace;

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const END_OF_FILE: &str = "*** End of File";

pub(crate) struct PatchPlan {
    changes: Vec<PlannedChange>,
}

enum PlannedChange {
    Add {
        path: PathBuf,
        display: String,
        content: String,
    },
    Delete {
        path: PathBuf,
        display: String,
    },
    Update {
        source: PathBuf,
        source_display: String,
        destination: Option<(PathBuf, String)>,
        content: String,
    },
}

enum PatchOperation {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<UpdateHunk>,
    },
}

struct UpdateHunk {
    header: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    end_of_file: bool,
}

impl PatchPlan {
    pub(crate) async fn prepare(
        workspace: &CodingWorkspace,
        patch: &str,
    ) -> Result<Self, EditError> {
        let operations = parse_patch(patch)?;
        let mut touched = Vec::new();
        let mut changes = Vec::with_capacity(operations.len());

        for operation in operations {
            match operation {
                PatchOperation::Add { path, content } => {
                    let resolved = resolve_mutation_path(workspace, &path).await?;
                    reserve_path(&mut touched, &resolved, &path)?;
                    if path_exists(&resolved, &path).await? {
                        return Err(EditError::Conflict {
                            path,
                            message: "add-file target already exists".to_owned(),
                        });
                    }
                    changes.push(PlannedChange::Add {
                        path: resolved,
                        display: path,
                        content,
                    });
                }
                PatchOperation::Delete { path } => {
                    let resolved = resolve_mutation_path(workspace, &path).await?;
                    reserve_path(&mut touched, &resolved, &path)?;
                    require_text_file(&resolved, &path).await?;
                    changes.push(PlannedChange::Delete {
                        path: resolved,
                        display: path,
                    });
                }
                PatchOperation::Update {
                    path,
                    move_to,
                    hunks,
                } => {
                    let source = resolve_mutation_path(workspace, &path).await?;
                    reserve_path(&mut touched, &source, &path)?;
                    let original = require_text_file(&source, &path).await?;
                    let content = apply_hunks(&path, &original, &hunks)?;
                    let destination = if let Some(destination) = move_to {
                        let resolved = resolve_mutation_path(workspace, &destination).await?;
                        reserve_path(&mut touched, &resolved, &destination)?;
                        if path_exists(&resolved, &destination).await? {
                            return Err(EditError::Conflict {
                                path: destination,
                                message: "move destination already exists".to_owned(),
                            });
                        }
                        Some((resolved, destination))
                    } else {
                        None
                    };
                    changes.push(PlannedChange::Update {
                        source,
                        source_display: path,
                        destination,
                        content,
                    });
                }
            }
        }

        Ok(Self { changes })
    }

    pub(crate) async fn commit(self) -> Result<Vec<FileChange>, EditError> {
        let mut completed = Vec::new();
        let mut output = Vec::with_capacity(self.changes.len());
        for change in self.changes {
            let result = match change {
                PlannedChange::Add {
                    path,
                    display,
                    content,
                } => write_with_parents(&path, &content).await.map(|()| {
                    completed.push(display.clone());
                    output.push(FileChange::Added { path: display });
                }),
                PlannedChange::Delete { path, display } => {
                    tokio::fs::remove_file(&path).await.map(|()| {
                        completed.push(display.clone());
                        output.push(FileChange::Deleted { path: display });
                    })
                }
                PlannedChange::Update {
                    source,
                    source_display,
                    destination,
                    content,
                } => {
                    if let Some((destination, destination_display)) = destination {
                        match write_with_parents(&destination, &content).await {
                            Ok(()) => {
                                completed.push(destination_display.clone());
                                match tokio::fs::remove_file(&source).await {
                                    Ok(()) => {
                                        output.push(FileChange::Moved {
                                            from: source_display,
                                            to: destination_display,
                                        });
                                        Ok(())
                                    }
                                    Err(error) => Err(error),
                                }
                            }
                            Err(error) => Err(error),
                        }
                    } else {
                        tokio::fs::write(&source, content).await.map(|()| {
                            completed.push(source_display.clone());
                            output.push(FileChange::Updated {
                                path: source_display,
                            });
                        })
                    }
                }
            };

            if let Err(error) = result {
                if completed.is_empty() {
                    return Err(EditError::Io {
                        operation: "commit patch to",
                        path: "workspace".to_owned(),
                        message: error.to_string(),
                    });
                }
                return Err(EditError::PartialCommit {
                    completed,
                    message: error.to_string(),
                });
            }
        }
        Ok(output)
    }
}

fn parse_patch(patch: &str) -> Result<Vec<PatchOperation>, EditError> {
    let normalized = patch.replace("\r\n", "\n");
    let lines = normalized.trim().split('\n').collect::<Vec<_>>();
    if lines.first().map(|line| line.trim()) != Some(BEGIN) {
        return Err(invalid_patch(1, format!("first line must be `{BEGIN}`")));
    }
    if lines.last().map(|line| line.trim()) != Some(END) {
        return Err(invalid_patch(
            lines.len(),
            format!("last line must be `{END}`"),
        ));
    }

    let mut operations = Vec::new();
    let mut index = 1usize;
    while index + 1 < lines.len() {
        let line = lines[index];
        if let Some(path) = line.strip_prefix(ADD) {
            let path = required_path(path, index + 1)?;
            index += 1;
            let mut added = Vec::new();
            while index + 1 < lines.len() && !is_operation_header(lines[index]) {
                let source = lines[index];
                let Some(content) = source.strip_prefix('+') else {
                    return Err(invalid_patch(
                        index + 1,
                        "add-file content lines must begin with `+`",
                    ));
                };
                added.push(content);
                index += 1;
            }
            let content = if added.is_empty() {
                String::new()
            } else {
                format!("{}\n", added.join("\n"))
            };
            operations.push(PatchOperation::Add { path, content });
        } else if let Some(path) = line.strip_prefix(DELETE) {
            operations.push(PatchOperation::Delete {
                path: required_path(path, index + 1)?,
            });
            index += 1;
        } else if let Some(path) = line.strip_prefix(UPDATE) {
            let path = required_path(path, index + 1)?;
            index += 1;
            let move_to = if index + 1 < lines.len() {
                lines[index]
                    .strip_prefix(MOVE)
                    .map(|path| required_path(path, index + 1))
                    .transpose()?
            } else {
                None
            };
            if move_to.is_some() {
                index += 1;
            }
            let mut hunks = Vec::new();
            while index + 1 < lines.len() && !is_operation_header(lines[index]) {
                let header_line = lines[index];
                let header = if header_line == "@@" {
                    None
                } else if let Some(header) = header_line.strip_prefix("@@ ") {
                    Some(header.to_owned())
                } else {
                    return Err(invalid_patch(
                        index + 1,
                        "update chunks must begin with `@@`",
                    ));
                };
                index += 1;
                let mut old_lines = Vec::new();
                let mut new_lines = Vec::new();
                let mut changed = false;
                let mut end_of_file = false;
                while index + 1 < lines.len()
                    && !is_operation_header(lines[index])
                    && !lines[index].starts_with("@@")
                {
                    let change = lines[index];
                    if change == END_OF_FILE {
                        end_of_file = true;
                        index += 1;
                        break;
                    }
                    let (prefix, content) = change.split_at_checked(1).ok_or_else(|| {
                        invalid_patch(index + 1, "empty update line requires a prefix")
                    })?;
                    match prefix {
                        " " => {
                            old_lines.push(content.to_owned());
                            new_lines.push(content.to_owned());
                        }
                        "-" => {
                            old_lines.push(content.to_owned());
                            changed = true;
                        }
                        "+" => {
                            new_lines.push(content.to_owned());
                            changed = true;
                        }
                        _ => {
                            return Err(invalid_patch(
                                index + 1,
                                "update lines must begin with space, `-`, or `+`",
                            ));
                        }
                    }
                    index += 1;
                }
                if !changed {
                    return Err(invalid_patch(
                        index.max(1),
                        "update chunk contains no addition or removal",
                    ));
                }
                hunks.push(UpdateHunk {
                    header,
                    old_lines,
                    new_lines,
                    end_of_file,
                });
            }
            if hunks.is_empty() && move_to.is_none() {
                return Err(invalid_patch(
                    index + 1,
                    "update operation requires a chunk or move destination",
                ));
            }
            operations.push(PatchOperation::Update {
                path,
                move_to,
                hunks,
            });
        } else {
            return Err(invalid_patch(
                index + 1,
                "expected add, delete, or update file header",
            ));
        }
    }

    if operations.is_empty() {
        return Err(invalid_patch(
            0,
            "patch must contain at least one operation",
        ));
    }
    Ok(operations)
}

fn apply_hunks(path: &str, original: &str, hunks: &[UpdateHunk]) -> Result<String, EditError> {
    if hunks.is_empty() {
        return Ok(original.to_owned());
    }
    let line_ending = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let normalized = original.replace("\r\n", "\n");
    let trailing_newline = normalized.ends_with('\n');
    let mut lines = if normalized.is_empty() {
        Vec::new()
    } else {
        normalized
            .split('\n')
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    if trailing_newline {
        lines.pop();
    }

    let mut replacements = Vec::with_capacity(hunks.len());
    let mut cursor = 0usize;
    for hunk in hunks {
        if let Some(header) = &hunk.header {
            let index = find_unique(path, &lines, std::slice::from_ref(header), cursor, false)?;
            cursor = index + 1;
        }
        if hunk.old_lines.is_empty() {
            let index = if hunk.end_of_file || hunk.header.is_none() {
                lines.len()
            } else {
                cursor
            };
            replacements.push((index, 0usize, hunk.new_lines.clone()));
            cursor = index;
            continue;
        }
        let index = find_unique(path, &lines, &hunk.old_lines, cursor, hunk.end_of_file)?;
        replacements.push((index, hunk.old_lines.len(), hunk.new_lines.clone()));
        cursor = index + hunk.old_lines.len();
    }

    for (index, old_len, replacement) in replacements.into_iter().rev() {
        lines.splice(index..index + old_len, replacement);
    }
    let mut output = lines.join(line_ending);
    if trailing_newline && !lines.is_empty() {
        output.push_str(line_ending);
    }
    Ok(output)
}

fn find_unique(
    path: &str,
    lines: &[String],
    pattern: &[String],
    start: usize,
    at_end: bool,
) -> Result<usize, EditError> {
    for comparison in [Comparison::Exact, Comparison::TrimEnd, Comparison::Trim] {
        let matches = matching_indices(lines, pattern, start, at_end, comparison);
        match matches.as_slice() {
            [index] => return Ok(*index),
            [] => {}
            _ => {
                return Err(EditError::Conflict {
                    path: path.to_owned(),
                    message: format!("patch context is ambiguous at {} locations", matches.len()),
                });
            }
        }
    }
    Err(EditError::Conflict {
        path: path.to_owned(),
        message: format!("expected context was not found:\n{}", pattern.join("\n")),
    })
}

#[derive(Clone, Copy)]
enum Comparison {
    Exact,
    TrimEnd,
    Trim,
}

fn matching_indices(
    lines: &[String],
    pattern: &[String],
    start: usize,
    at_end: bool,
    comparison: Comparison,
) -> Vec<usize> {
    if pattern.len() > lines.len() || start > lines.len().saturating_sub(pattern.len()) {
        return Vec::new();
    }
    let final_start = lines.len() - pattern.len();
    let candidate_start = if at_end { final_start } else { start };
    (candidate_start..=final_start)
        .filter(|index| {
            lines[*index..*index + pattern.len()]
                .iter()
                .zip(pattern)
                .all(|(actual, expected)| compare(actual, expected, comparison))
        })
        .collect()
}

fn compare(actual: &str, expected: &str, comparison: Comparison) -> bool {
    match comparison {
        Comparison::Exact => actual == expected,
        Comparison::TrimEnd => actual.trim_end() == expected.trim_end(),
        Comparison::Trim => actual.trim() == expected.trim(),
    }
}

fn required_path(path: &str, line: usize) -> Result<String, EditError> {
    let path = path.trim();
    if path.is_empty() {
        Err(invalid_patch(line, "file path must not be empty"))
    } else if Path::new(path).is_absolute() {
        Err(invalid_patch(line, "patch paths must be relative"))
    } else {
        Ok(path.to_owned())
    }
}

fn is_operation_header(line: &str) -> bool {
    line.starts_with(ADD) || line.starts_with(DELETE) || line.starts_with(UPDATE) || line == END
}

fn invalid_patch(line: usize, message: impl Into<String>) -> EditError {
    EditError::InvalidPatch {
        line,
        message: message.into(),
    }
}

async fn resolve_mutation_path(
    workspace: &CodingWorkspace,
    path: &str,
) -> Result<PathBuf, EditError> {
    workspace.resolve_write(path).await.map_err(edit_path_error)
}

fn reserve_path(touched: &mut Vec<PathBuf>, path: &Path, display: &str) -> Result<(), EditError> {
    if touched
        .iter()
        .any(|other| path.starts_with(other) || other.starts_with(path))
    {
        return Err(EditError::Conflict {
            path: display.to_owned(),
            message: "path overlaps another patch operation".to_owned(),
        });
    }
    touched.push(path.to_path_buf());
    Ok(())
}

async fn path_exists(path: &Path, display: &str) -> Result<bool, EditError> {
    tokio::fs::try_exists(path)
        .await
        .map_err(|error| EditError::Io {
            operation: "inspect",
            path: display.to_owned(),
            message: error.to_string(),
        })
}

async fn require_text_file(path: &Path, display: &str) -> Result<String, EditError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| EditError::Conflict {
            path: display.to_owned(),
            message: error.to_string(),
        })?;
    if !metadata.is_file() {
        return Err(EditError::Conflict {
            path: display.to_owned(),
            message: "target is not a regular file".to_owned(),
        });
    }
    tokio::fs::read_to_string(path)
        .await
        .map_err(|error| EditError::Conflict {
            path: display.to_owned(),
            message: format!("file is not readable UTF-8 text: {error}"),
        })
}

async fn write_with_parents(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, content).await
}
