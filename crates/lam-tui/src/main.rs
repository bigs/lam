//! Interactive terminal application for the Lam coding-agent runtime.

mod app;
mod config;
mod runtime;
mod ui;

use std::env;
use std::io::{self, stdout};
use std::path::PathBuf;
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

use crate::app::App;
use crate::config::LoadedConfig;
use crate::runtime::{CommandResult, Runtime};

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
    let config = LoadedConfig::load(args.config.as_deref()).map_err(AppError::Config)?;
    let config_path = config.path.display().to_string();
    let mut runtime = Runtime::build(config, cwd.clone())
        .await
        .map_err(AppError::Runtime)?;
    let mut app = App::new(
        cwd.display().to_string(),
        config_path,
        runtime.models.clone(),
        runtime.selected_model,
    );

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

    while !app.should_exit {
        terminal
            .terminal
            .draw(|frame| ui::render(frame, &mut app))
            .map_err(AppError::Terminal)?;
        tokio::select! {
            terminal_event = terminal_receiver.recv() => {
                let Some(terminal_event) = terminal_event else {
                    break;
                };
                match terminal_event.map_err(AppError::Terminal)? {
                    Event::Key(key) => {
                        if let Some(command) = app.handle_key(key) {
                            runtime.execute(command, command_results.clone());
                        }
                    }
                    Event::Mouse(mouse) => app.handle_mouse(mouse),
                    Event::Paste(text) => app.handle_paste(&text),
                    Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => {}
                }
            }
            event = runtime.events.next() => {
                if let Some(event) = event {
                    app.apply_agent_event(event);
                }
            }
            result = command_receiver.recv() => {
                if let Some(result) = result {
                    app.apply_command_result(result);
                }
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

struct Args {
    config: Option<PathBuf>,
    help: bool,
    version: bool,
}

impl Args {
    fn parse() -> Result<Self, AppError> {
        let mut config = None;
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
        "lam — a minimal TypeScript coding agent\n\nUSAGE:\n    lam [--config PATH]\n\nOPTIONS:\n    --config PATH  Read providers from PATH instead of ~/.lam/providers.toml\n    -h, --help     Show this help\n    -V, --version  Show the version\n\nInside the TUI, type / for commands. Tab switches focus between the input shelf\nand conversation; arrows select transcript rows; Enter expands the selected row."
    );
}

#[derive(Debug, Error)]
enum AppError {
    #[error("{0}")]
    Arguments(String),
    #[error("could not read the current directory: {0}")]
    CurrentDirectory(std::io::Error),
    #[error(
        "{0}\n\nCreate the file or pass --config PATH. See crates/lam-tui/README.md for an example."
    )]
    Config(crate::config::ConfigError),
    #[error(transparent)]
    Runtime(crate::runtime::RuntimeError),
    #[error("terminal operation failed: {0}")]
    Terminal(std::io::Error),
    #[error("could not start terminal event reader: {0}")]
    EventThread(std::io::Error),
    #[error("could not stop the agent runtime: {0}")]
    Shutdown(String),
}
