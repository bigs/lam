//! Interactive terminal application for the Lam coding-agent runtime.

mod app;
mod config;
mod diagnostics;
mod runtime;
mod session;
mod ui;

use std::env;
use std::io::{self, stdout};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::app::{App, SessionChoice, SessionView};
use crate::config::LoadedConfig;
use crate::diagnostics::DiagnosticLog;
use crate::runtime::{Command, CommandResult, Runtime};
use crate::session::{Session, SessionCatalog};

fn main() -> ExitCode {
    match tokio_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lam: {error}");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn tokio_main() -> Result<(), AppError> {
    let args = Args::parse()?;
    if args.help {
        print_help();
        return Ok(());
    }
    if args.version {
        println!("lam {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let cwd = env::current_dir().map_err(AppError::CurrentDirectory)?;
    let cwd = cwd
        .canonicalize()
        .map_err(AppError::CanonicalCurrentDirectory)?;
    let config = LoadedConfig::load(args.config.as_deref()).map_err(AppError::Config)?;
    let config_path = config.path.display().to_string();
    let sessions = SessionCatalog::open_default().map_err(AppError::Session)?;
    let selection = sessions.resume_or_create(&cwd).map_err(AppError::Session)?;
    let diagnostics = args
        .debug_log
        .then(DiagnosticLog::install)
        .transpose()
        .map_err(AppError::Diagnostics)?;
    if let Some(diagnostics) = &diagnostics {
        diagnostics
            .activate(&selection.session)
            .map_err(AppError::Diagnostics)?;
    }
    let (mut runtime, mut app) = open_session(
        &config,
        &sessions,
        &cwd,
        selection.session,
        selection.resumed,
        &config_path,
    )
    .await?;

    let mut terminal = TerminalSession::start()?;
    let (terminal_events, mut terminal_receiver) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("lam-terminal-events".to_owned())
        .spawn(move || {
            loop {
                if terminal_events.send(crossterm::event::read()).is_err() {
                    break;
                }
            }
        })
        .map_err(AppError::EventThread)?;
    let (command_results, mut command_receiver) = mpsc::unbounded_channel::<CommandResult>();

    let mut redraw = true;
    while !app.should_exit {
        if redraw {
            terminal
                .terminal
                .draw(|frame| ui::render(frame, &mut app))
                .map_err(AppError::Terminal)?;
            redraw = false;
        }
        let interruption_deadline = app.interruption_deadline();
        tokio::select! {
            terminal_event = terminal_receiver.recv() => {
                let Some(terminal_event) = terminal_event else {
                    break;
                };
                match terminal_event.map_err(AppError::Terminal)? {
                    Event::Key(key) => {
                        if let Some(command) = app.handle_key(key) {
                            match command {
                                session_command @ (Command::New | Command::LoadSession(_)) => {
                                runtime
                                    .system
                                    .shutdown()
                                    .await
                                    .map_err(|error| AppError::Shutdown(error.to_string()))?;
                                let (session, resumed) = match session_command {
                                    Command::New => (
                                        sessions.create(&cwd).map_err(AppError::Session)?,
                                        false,
                                    ),
                                    Command::LoadSession(id) => (
                                        sessions.select(id, &cwd).map_err(AppError::Session)?,
                                        true,
                                    ),
                                    _ => unreachable!("session commands were matched above"),
                                };
                                drop(runtime);
                                if let Some(diagnostics) = &diagnostics {
                                    diagnostics
                                        .activate(&session)
                                        .map_err(AppError::Diagnostics)?;
                                }
                                (runtime, app) = open_session(
                                    &config,
                                    &sessions,
                                    &cwd,
                                    session,
                                    resumed,
                                    &config_path,
                                )
                                .await?;
                                }
                                command => runtime.execute(command, command_results.clone()),
                            }
                        }
                    }
                    Event::Mouse(mouse) => app.handle_mouse(mouse),
                    Event::Paste(text) => app.handle_paste(&text),
                    Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => {}
                }
                redraw = true;
            }
            event = runtime.events.next() => {
                if let Some(event) = event {
                    redraw |= app.apply_agent_event(event);
                }
            }
            result = command_receiver.recv() => {
                if let Some(result) = result {
                    app.apply_command_result(result);
                    redraw = true;
                }
            }
            () = wait_for_interruption_deadline(interruption_deadline), if interruption_deadline.is_some() => {
                redraw |= app.expire_interruption(std::time::Instant::now());
            }
        }
    }

    runtime
        .system
        .abort()
        .await
        .map_err(|error| AppError::Shutdown(error.to_string()))?;
    terminal.restore()?;
    Ok(())
}

async fn wait_for_interruption_deadline(deadline: Option<std::time::Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

async fn open_session(
    config: &LoadedConfig,
    catalog: &SessionCatalog,
    cwd: &Path,
    session: Session,
    resumed: bool,
    config_path: &str,
) -> Result<(Runtime, App), AppError> {
    let choices = session_choices(catalog, cwd).await?;
    let runtime = Runtime::build(config, cwd.to_path_buf(), &session)
        .await
        .map_err(AppError::Runtime)?;
    let app = App::new(
        cwd.display().to_string(),
        config_path.to_owned(),
        SessionView {
            id: session.id,
            journal_path: session.database_path.display().to_string(),
            resumed,
            agents: runtime.agents.clone(),
            choices,
        },
        runtime.models.clone(),
        runtime.selected_model,
    );
    Ok((runtime, app))
}

async fn session_choices(
    catalog: &SessionCatalog,
    cwd: &Path,
) -> Result<Vec<SessionChoice>, AppError> {
    let sessions = catalog.list(cwd).map_err(AppError::Session)?;
    let mut choices = Vec::with_capacity(sessions.len());
    for session in sessions {
        let preview = match runtime::first_user_message(&session).await {
            Ok(preview) => preview,
            Err(error) => Some(format!("Preview unavailable: {error}")),
        };
        choices.push(SessionChoice {
            id: session.id,
            preview,
        });
    }
    Ok(choices)
}

struct Args {
    config: Option<PathBuf>,
    debug_log: bool,
    help: bool,
    version: bool,
}

impl Args {
    fn parse() -> Result<Self, AppError> {
        let mut config = None;
        let mut debug_log = false;
        let mut help = false;
        let mut version = false;
        let mut arguments = env::args_os().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.to_str() {
                Some("--config") => {
                    let path = arguments.next().ok_or_else(|| {
                        AppError::Arguments("--config requires a path".to_owned())
                    })?;
                    config = Some(PathBuf::from(path));
                }
                Some("--debug-log") => debug_log = true,
                Some("--help" | "-h") => help = true,
                Some("--version" | "-V") => version = true,
                Some(value) => {
                    return Err(AppError::Arguments(format!(
                        "unknown argument `{value}`; try --help"
                    )));
                }
                None => {
                    return Err(AppError::Arguments(
                        "arguments must be valid UTF-8".to_owned(),
                    ));
                }
            }
        }
        Ok(Self {
            config,
            debug_log,
            help,
            version,
        })
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    restored: bool,
}

impl TerminalSession {
    fn start() -> Result<Self, AppError> {
        enable_raw_mode().map_err(AppError::Terminal)?;
        execute!(
            stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )
        .map_err(AppError::Terminal)?;
        let backend = CrosstermBackend::new(stdout());
        let mut terminal = Terminal::new(backend).map_err(AppError::Terminal)?;
        terminal.clear().map_err(AppError::Terminal)?;
        Ok(Self {
            terminal,
            restored: false,
        })
    }

    fn restore(&mut self) -> Result<(), AppError> {
        if self.restored {
            return Ok(());
        }
        disable_raw_mode().map_err(AppError::Terminal)?;
        execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        )
        .map_err(AppError::Terminal)?;
        self.terminal.show_cursor().map_err(AppError::Terminal)?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if !self.restored {
            let _ = disable_raw_mode();
            let _ = execute!(
                self.terminal.backend_mut(),
                DisableBracketedPaste,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            let _ = self.terminal.show_cursor();
        }
    }
}

fn print_help() {
    println!(
        "lam — a minimal TypeScript coding agent\n\nUSAGE:\n    lam [--config PATH] [--debug-log]\n\nOPTIONS:\n    --config PATH  Read providers from PATH instead of ~/.lam/providers.toml\n    --debug-log    Append metadata-only diagnostics beside the session journal\n    -h, --help     Show this help\n    -V, --version  Show the version\n\nLam resumes the latest durable session for the current directory. Inside the TUI,\ntype / for commands, including /new for a fresh session and /session to restore\nan earlier one. Tab switches focus between the input shelf and conversation;\narrows select transcript rows; Enter expands the selected row. While the root is\nworking, press Escape twice within 1.5 seconds to stop its complete agent tree."
    );
}

#[derive(Debug, Error)]
enum AppError {
    #[error("{0}")]
    Arguments(String),
    #[error("could not read the current directory: {0}")]
    CurrentDirectory(std::io::Error),
    #[error("could not resolve the current directory: {0}")]
    CanonicalCurrentDirectory(std::io::Error),
    #[error(
        "{0}\n\nCreate the file or pass --config PATH. See crates/lam-tui/README.md for an example."
    )]
    Config(crate::config::ConfigError),
    #[error(transparent)]
    Session(crate::session::SessionError),
    #[error(transparent)]
    Diagnostics(crate::diagnostics::DiagnosticError),
    #[error(transparent)]
    Runtime(crate::runtime::RuntimeError),
    #[error("terminal operation failed: {0}")]
    Terminal(std::io::Error),
    #[error("could not start terminal event reader: {0}")]
    EventThread(std::io::Error),
    #[error("could not stop the agent runtime: {0}")]
    Shutdown(String),
}
