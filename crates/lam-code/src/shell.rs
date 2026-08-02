use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lam::Namespace;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::{CaptureConfig, ShellConfig};
use crate::error::ShellError;
use crate::output::capture_stream;
use crate::path::{CodingWorkspace, PathFailure};

/// Input accepted by `lam.shell.run`.
#[derive(Clone, Debug, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ShellRequest {
    /// Shell program text, including pipes and redirections.
    pub command: String,
    /// Optional relative workspace or allowed absolute working directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Optional positive execution timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// One bounded command output stream.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedStream {
    /// Complete text or the bounded tail when truncated.
    pub content: String,
    /// Logical line count observed before truncation.
    pub total_lines: u64,
    /// Raw bytes observed before lossy UTF-8 display decoding.
    pub total_bytes: u64,
    /// Whether the displayed content omits an earlier prefix.
    pub truncated: bool,
    /// Complete raw stream in pack-owned scratch storage when truncated.
    /// The path remains valid only for the lifetime of the capability pack.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

/// Normal model-facing outcome from `lam.shell.run`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ShellOutput {
    /// Conventional process exit code, absent when terminated by a signal.
    pub exit_code: Option<i32>,
    /// Platform signal name when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    /// Whether the configured command timeout killed the process tree.
    pub timed_out: bool,
    /// Captured standard output.
    pub stdout: CapturedStream,
    /// Captured standard error.
    pub stderr: CapturedStream,
    /// End-to-end runner duration in milliseconds.
    pub duration_ms: u64,
}

/// Host-normalized request passed to an injected command runner.
#[derive(Clone, Debug)]
pub struct CommandRequest {
    /// Shell program text.
    pub command: String,
    /// Canonical initial working directory.
    pub cwd: PathBuf,
    /// Effective host-bounded timeout.
    pub timeout: Duration,
    /// Tail and spill thresholds.
    pub capture: CaptureConfig,
    /// Pack-owned directory for complete truncated streams.
    pub scratch_dir: PathBuf,
}

/// Successful command process and capture outcome returned by a runner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    /// Conventional process exit code, absent when terminated by a signal.
    pub exit_code: Option<i32>,
    /// Platform signal name when available.
    pub signal: Option<String>,
    /// Whether the runner's command timeout elapsed.
    pub timed_out: bool,
    /// Captured standard output.
    pub stdout: CapturedStream,
    /// Captured standard error.
    pub stderr: CapturedStream,
    /// End-to-end execution duration.
    pub duration: Duration,
}

/// Infrastructure failure returned by an injected command runner.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct CommandRunnerError {
    message: String,
}

impl CommandRunnerError {
    /// Creates a runner failure with a host diagnostic.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Heap-storable future returned by a dynamic command runner.
pub type CommandFuture =
    Pin<Box<dyn Future<Output = Result<CommandOutput, CommandRunnerError>> + Send + 'static>>;

/// Replaceable host boundary behind `lam.shell.run`.
pub trait CommandRunner: Send + Sync + 'static {
    /// Executes one normalized request. Dropping the future must cancel any
    /// still-running remote or local work. Any `full_output_path` in the result
    /// must point beneath `request.scratch_dir` to remain readable by `lam.fs`.
    fn run(&self, request: CommandRequest) -> CommandFuture;
}

/// Explicit host-authority shell runner supplied by `lam-code`.
#[derive(Clone)]
pub struct LocalCommandRunner {
    program: PathBuf,
    arguments: Vec<OsString>,
}

impl fmt::Debug for LocalCommandRunner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalCommandRunner")
            .field("program", &self.program)
            .field("arguments", &self.arguments)
            .finish()
    }
}

impl Default for LocalCommandRunner {
    fn default() -> Self {
        #[cfg(unix)]
        {
            Self::new("/bin/sh", ["-lc"])
        }
        #[cfg(windows)]
        {
            Self::new("cmd.exe", ["/D", "/S", "/C"])
        }
    }
}

impl LocalCommandRunner {
    /// Configures a shell executable and arguments placed before command text.
    #[must_use]
    pub fn new(
        program: impl Into<PathBuf>,
        arguments: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            program: program.into(),
            arguments: arguments.into_iter().map(Into::into).collect(),
        }
    }
}

impl CommandRunner for LocalCommandRunner {
    fn run(&self, request: CommandRequest) -> CommandFuture {
        let program = self.program.clone();
        let arguments = self.arguments.clone();
        Box::pin(async move { run_local(program, arguments, request).await })
    }
}

/// Builds `lam.shell` around an explicit command runner.
#[must_use]
pub(crate) fn shell_namespace(
    workspace: CodingWorkspace,
    runner: Arc<dyn CommandRunner>,
    config: ShellConfig,
) -> Namespace {
    Namespace::new(
        "lam.shell",
        "Runs explicitly enabled host shell commands with bounded time and model-visible output.",
    )
    .function(
        "run",
        "Run one shell command string. A nonzero exit or command timeout is returned as a normal outcome; policy, spawn, capture, or reap failures reject the Promise. Truncated stdout or stderr includes a fullOutputPath; UTF-8 output is pageable when lam.fs is installed.",
        move |_context, request: ShellRequest| {
            let workspace = workspace.clone();
            let runner = Arc::clone(&runner);
            async move { run_command(&workspace, runner.as_ref(), request, config).await }
        },
    )
}

async fn run_command(
    workspace: &CodingWorkspace,
    runner: &dyn CommandRunner,
    request: ShellRequest,
    config: ShellConfig,
) -> Result<ShellOutput, ShellError> {
    if request.command.trim().is_empty() {
        return Err(ShellError::InvalidCommand);
    }
    let requested_cwd = request.cwd.unwrap_or_else(|| ".".to_owned());
    let cwd = workspace
        .resolve_read(&requested_cwd)
        .await
        .map_err(shell_path_error)?;
    let metadata = tokio::fs::metadata(&cwd)
        .await
        .map_err(|error| ShellError::InvalidCwd {
            path: requested_cwd.clone(),
            message: error.to_string(),
        })?;
    if !metadata.is_dir() {
        return Err(ShellError::InvalidCwd {
            path: requested_cwd,
            message: "path is not a directory".to_owned(),
        });
    }

    let timeout = request
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(config.default_timeout);
    if timeout.is_zero() || timeout > config.max_timeout {
        return Err(ShellError::InvalidTimeout {
            timeout_ms: duration_millis(timeout),
            max_timeout_ms: duration_millis(config.max_timeout),
        });
    }
    let output = runner
        .run(CommandRequest {
            command: request.command,
            cwd,
            timeout,
            capture: config.capture,
            scratch_dir: workspace.scratch().to_path_buf(),
        })
        .await
        .map_err(|error| ShellError::Runner {
            message: error.to_string(),
        })?;
    Ok(ShellOutput {
        exit_code: output.exit_code,
        signal: output.signal,
        timed_out: output.timed_out,
        stdout: output.stdout,
        stderr: output.stderr,
        duration_ms: duration_millis(output.duration),
    })
}

async fn run_local(
    program: PathBuf,
    arguments: Vec<OsString>,
    request: CommandRequest,
) -> Result<CommandOutput, CommandRunnerError> {
    let started = Instant::now();
    let mut command = tokio::process::Command::new(program);
    command
        .args(arguments)
        .arg(&request.command)
        .current_dir(&request.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|error| CommandRunnerError::new(format!("could not spawn shell: {error}")))?;
    let process_id = child
        .id()
        .ok_or_else(|| CommandRunnerError::new("spawned shell has no process id"))?;
    let mut process_tree = ProcessTreeGuard::new(process_id);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CommandRunnerError::new("spawned shell has no stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CommandRunnerError::new("spawned shell has no stderr pipe"))?;
    let stdout_scratch = request.scratch_dir.clone();
    let stderr_scratch = request.scratch_dir;
    let capture = request.capture;
    let mut stdout_task =
        tokio::spawn(
            async move { capture_stream(stdout, capture, &stdout_scratch, "stdout").await },
        );
    let mut stderr_task =
        tokio::spawn(
            async move { capture_stream(stderr, capture, &stderr_scratch, "stderr").await },
        );
    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    let execution = async {
        status = Some(child.wait().await.map_err(|error| {
            CommandRunnerError::new(format!("could not wait for shell: {error}"))
        })?);
        stdout = Some(await_capture(&mut stdout_task, "stdout").await?);
        stderr = Some(await_capture(&mut stderr_task, "stderr").await?);
        Ok::<_, CommandRunnerError>(())
    };
    let timed_out = match tokio::time::timeout(request.timeout, execution).await {
        Ok(result) => {
            result?;
            false
        }
        Err(_) => {
            process_tree.kill()?;
            if status.is_none() {
                status = Some(child.wait().await.map_err(|error| {
                    CommandRunnerError::new(format!("could not reap timed-out shell: {error}"))
                })?);
            }
            if stdout.is_none() {
                stdout = Some(await_capture(&mut stdout_task, "stdout").await?);
            }
            if stderr.is_none() {
                stderr = Some(await_capture(&mut stderr_task, "stderr").await?);
            }
            true
        }
    };
    let status = status.ok_or_else(|| CommandRunnerError::new("shell produced no exit status"))?;
    let stdout = stdout.ok_or_else(|| CommandRunnerError::new("shell produced no stdout"))?;
    let stderr = stderr.ok_or_else(|| CommandRunnerError::new("shell produced no stderr"))?;
    process_tree.disarm();
    Ok(CommandOutput {
        exit_code: status.code(),
        signal: exit_signal(&status),
        timed_out,
        stdout,
        stderr,
        duration: started.elapsed(),
    })
}

async fn await_capture(
    task: &mut tokio::task::JoinHandle<std::io::Result<CapturedStream>>,
    label: &str,
) -> Result<CapturedStream, CommandRunnerError> {
    task.await
        .map_err(|error| CommandRunnerError::new(format!("{label} capture task failed: {error}")))?
        .map_err(|error| CommandRunnerError::new(format!("{label} capture failed: {error}")))
}

struct ProcessTreeGuard {
    process_id: u32,
    armed: bool,
}

impl ProcessTreeGuard {
    const fn new(process_id: u32) -> Self {
        Self {
            process_id,
            armed: true,
        }
    }

    fn kill(&self) -> Result<(), CommandRunnerError> {
        kill_process_tree(self.process_id)
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = kill_process_tree(self.process_id);
        }
    }
}

#[cfg(unix)]
fn kill_process_tree(process_id: u32) -> Result<(), CommandRunnerError> {
    use nix::errno::Errno;
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;

    match killpg(Pid::from_raw(process_id as i32), Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(CommandRunnerError::new(format!(
            "could not kill process group {process_id}: {error}"
        ))),
    }
}

#[cfg(windows)]
fn kill_process_tree(process_id: u32) -> Result<(), CommandRunnerError> {
    let status = std::process::Command::new("taskkill")
        .args(["/PID", &process_id.to_string(), "/T", "/F"])
        .status()
        .map_err(|error| CommandRunnerError::new(format!("could not start taskkill: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(CommandRunnerError::new(format!(
            "taskkill failed with status {status}"
        )))
    }
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;

    status.signal().map(|signal| {
        nix::sys::signal::Signal::try_from(signal)
            .map_or_else(|_| signal.to_string(), |signal| format!("{signal:?}"))
    })
}

#[cfg(windows)]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

fn shell_path_error(error: PathFailure) -> ShellError {
    match error {
        PathFailure::Invalid { path, message } | PathFailure::Unavailable { path, message } => {
            ShellError::InvalidCwd { path, message }
        }
        PathFailure::OutsideRoots { path } => ShellError::InvalidCwd {
            path,
            message: "path is outside configured readable roots".to_owned(),
        },
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}
