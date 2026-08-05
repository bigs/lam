use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

use crate::session::Session;

const SCHEMA_VERSION: u64 = 1;

/// An opt-in JSONL diagnostic sink whose active file follows the TUI session.
pub(crate) struct DiagnosticLog {
    writer: SessionWriter,
    launch_id: String,
}

impl DiagnosticLog {
    pub(crate) fn install() -> Result<Self, DiagnosticError> {
        let writer = SessionWriter::default();
        let filter = tracing_subscriber::filter::Targets::new()
            .with_target("lam", tracing::Level::TRACE)
            .with_target("lam_agents", tracing::Level::TRACE)
            .with_target("lam_code", tracing::Level::TRACE)
            .with_target("lam_core", tracing::Level::TRACE)
            .with_target("lam_deno", tracing::Level::TRACE)
            .with_target("lam_openai", tracing::Level::TRACE)
            .with_target("lam_redb", tracing::Level::TRACE)
            .with_target("lam_tui", tracing::Level::TRACE);
        let formatter = tracing_subscriber::fmt::layer()
            .json()
            .with_ansi(false)
            .with_current_span(true)
            .with_span_list(true)
            .with_thread_ids(true)
            .with_thread_names(true)
            .with_writer(writer.clone())
            .with_filter(filter);
        let subscriber = tracing_subscriber::registry().with(formatter);
        tracing::subscriber::set_global_default(subscriber)
            .map_err(DiagnosticError::InstallSubscriber)?;
        Ok(Self {
            writer,
            launch_id: launch_id()?,
        })
    }

    pub(crate) fn activate(&self, session: &Session) -> Result<PathBuf, DiagnosticError> {
        let path = session.diagnostic_log_path();
        self.writer.activate(&path)?;
        tracing::info!(
            target: "lam_tui::diagnostics",
            event = "diagnostics.session_activated",
            schema_version = SCHEMA_VERSION,
            launch_id = self.launch_id,
            session_id = session.id,
            session_journal = %session.database_path.display(),
            diagnostic_log = %path.display(),
            "session diagnostics activated"
        );
        Ok(path)
    }
}

#[derive(Clone, Default)]
struct SessionWriter {
    file: Arc<Mutex<Option<File>>>,
}

impl SessionWriter {
    fn activate(&self, path: &Path) -> Result<(), DiagnosticError> {
        let file = open_log(path)?;
        *lock(&self.file) = Some(file);
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for SessionWriter {
    type Writer = SessionWriterGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        SessionWriterGuard {
            file: lock(&self.file),
        }
    }
}

struct SessionWriterGuard<'a> {
    file: MutexGuard<'a, Option<File>>,
}

impl Write for SessionWriterGuard<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let Some(file) = self.file.as_mut() else {
            return Ok(buffer.len());
        };
        file.write_all(buffer)?;
        file.flush()?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().map_or(Ok(()), Write::flush)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn open_log(path: &Path) -> Result<File, DiagnosticError> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|source| DiagnosticError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    restrict_file(path)?;
    Ok(file)
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<(), DiagnosticError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        DiagnosticError::Permissions {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> Result<(), DiagnosticError> {
    Ok(())
}

fn launch_id() -> Result<String, DiagnosticError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(DiagnosticError::Clock)?
        .as_nanos();
    Ok(format!("{:x}-{nanos:x}", std::process::id()))
}

#[derive(Debug, Error)]
pub(crate) enum DiagnosticError {
    #[error("could not install the debug log subscriber: {0}")]
    InstallSubscriber(#[source] tracing::subscriber::SetGlobalDefaultError),
    #[error("could not open diagnostic log `{path}`: {source}")]
    Open { path: PathBuf, source: io::Error },
    #[error("could not restrict diagnostic log `{path}`: {source}")]
    Permissions { path: PathBuf, source: io::Error },
    #[error("system clock is before the Unix epoch: {0}")]
    Clock(#[source] std::time::SystemTimeError),
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;

    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::prelude::*;

    use super::{SessionWriter, lock};

    #[test]
    fn rotates_between_append_only_session_files() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.debug.jsonl");
        let second = temp.path().join("second.debug.jsonl");
        let writer = SessionWriter::default();

        writer.activate(&first).unwrap();
        writer.make_writer().write_all(b"first\n").unwrap();
        writer.activate(&second).unwrap();
        writer.make_writer().write_all(b"second\n").unwrap();
        writer.make_writer().flush().unwrap();

        assert_eq!(fs::read_to_string(first).unwrap(), "first\n");
        assert_eq!(fs::read_to_string(second).unwrap(), "second\n");
        assert!(lock(&writer.file).is_some());
    }

    #[test]
    fn emits_parseable_json_lines() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.debug.jsonl");
        let writer = SessionWriter::default();
        writer.activate(&path).unwrap();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(writer),
        );

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(event = "diagnostics.test", count = 1, "test event");
        });

        let lines = fs::read_to_string(path).unwrap();
        let line = lines.lines().next().expect("one JSONL event");
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(event.pointer("/fields/event").unwrap(), "diagnostics.test");
        assert_eq!(event.pointer("/fields/count").unwrap(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn diagnostic_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.debug.jsonl");
        SessionWriter::default().activate(&path).unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
