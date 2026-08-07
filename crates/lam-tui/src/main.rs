//! Interactive terminal application for the Lam coding-agent runtime.

mod app;
mod boot;
mod clipboard;
mod codex;
mod config;
mod diagnostics;
mod runtime;
mod session;
mod ui;
mod xai;

use std::collections::BTreeMap;
use std::env;
use std::io::{self, stdout};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use lam::RunEvent;
use lam_agents::{AgentOutcome, AgentSystemEvent};
use lam_redb::StoreFootprint;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::app::{App, SessionChoice, SessionView};
use crate::boot::{phase, phase_sync};
use crate::config::LoadedConfig;
use crate::diagnostics::DiagnosticLog;
use crate::runtime::{Command, CommandResult, Runtime, RuntimePreferences};
use crate::session::{Session, SessionCatalog};

/// Minimum interval between frames (~60fps). Rendering is decoupled from
/// event arrival: all queued events are applied between frames.
const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// Bound on events applied between frames so a runaway producer cannot
/// starve rendering entirely.
const MAX_EVENTS_PER_FRAME: usize = 1024;

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
    if let Some(command) = args.command {
        return run_command(command).await;
    }

    let boot_started = std::time::Instant::now();
    let cwd = env::current_dir().map_err(AppError::CurrentDirectory)?;
    let cwd = cwd
        .canonicalize()
        .map_err(AppError::CanonicalCurrentDirectory)?;
    let config = phase_sync("config_load", || LoadedConfig::load(args.config.as_deref()))
        .map_err(AppError::Config)?;
    let config_path = config.path.display().to_string();
    let sessions = phase_sync("session_catalog_open", SessionCatalog::open_default)
        .map_err(AppError::Session)?;
    let selection = phase_sync("resume_or_create", || sessions.resume_or_create(&cwd))
        .map_err(AppError::Session)?;
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
    let choices = phase("session_choices", session_choices(&sessions, &cwd)).await?;
    let mut session_lease = selection.lease;
    let (mut runtime, mut app) = phase(
        "open_session",
        open_session(
            &config,
            &cwd,
            selection.session,
            selection.resumed,
            &config_path,
            choices,
            None,
        ),
    )
    .await?;

    let mut terminal = phase_sync("terminal_start", TerminalSession::start)?;
    tracing::info!(
        target: "lam_tui::boot",
        event = "boot.complete",
        total_ms = boot_started.elapsed().as_millis() as u64,
        "boot sequence complete"
    );
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
    let mut next_frame = tokio::time::Instant::now();
    let mut background_compactions: Vec<std::thread::JoinHandle<()>> = Vec::new();
    'main: while !app.should_exit {
        if redraw && tokio::time::Instant::now() >= next_frame {
            terminal
                .terminal
                .draw(|frame| ui::render(frame, &mut app))
                .map_err(AppError::Terminal)?;
            redraw = false;
            next_frame = tokio::time::Instant::now() + FRAME_INTERVAL;
            // Spinners need a steady frame clock even when no agent events
            // arrive. Re-arm immediately so select keeps waking on cadence.
            if app.agents_drawer_animates() {
                redraw = true;
            }
        }
        let interruption_deadline = app.interruption_deadline();
        let toast_deadline = app.toast.as_ref().map(|toast| toast.deadline);
        // Wake for animation frames whenever the agents drawer is spinning,
        // not only when some other event already set `redraw`.
        let want_frame = redraw || app.agents_drawer_animates();
        // Resolve the select into a plain value first so the handlers below
        // can borrow both `app` and `runtime` freely — in particular to fold
        // the journal projectors, which need `&mut runtime`.
        let mut tick = tokio::select! {
            terminal_event = terminal_receiver.recv() => {
                match terminal_event {
                    Some(terminal_event) => Tick::Terminal(terminal_event),
                    None => break,
                }
            }
            event = runtime.events.next() => {
                match event {
                    Some(event) => Tick::Agent(event),
                    None => Tick::Idle,
                }
            }
            result = command_receiver.recv() => {
                match result {
                    Some(result) => Tick::Command(result),
                    None => Tick::Idle,
                }
            }
            () = wait_for_interruption_deadline(interruption_deadline), if interruption_deadline.is_some() => Tick::Deadline,
            () = wait_for_toast_deadline(toast_deadline), if toast_deadline.is_some() => Tick::Toast,
            () = tokio::time::sleep_until(next_frame), if want_frame => Tick::Frame,
        };
        // Apply the awaited event plus everything else already queued, then
        // draw once: a burst costs one frame instead of one frame per event.
        for _ in 0..MAX_EVENTS_PER_FRAME {
            match tick {
                Tick::Terminal(terminal_event) => {
                    match terminal_event.map_err(AppError::Terminal)? {
                        Event::Key(key) => {
                            if let Some(command) = app.handle_key(key) {
                                match command {
                                    session_command @ (Command::New | Command::LoadSession(_)) => {
                                        let preferences = matches!(session_command, Command::New)
                                            .then(|| app.runtime_preferences());
                                        let claimed = match session_command {
                                            Command::New => sessions
                                                .create(&cwd)
                                                .map(|(session, lease)| (session, lease, false)),
                                            Command::LoadSession(id) => sessions
                                                .select(id, &cwd)
                                                .map(|(session, lease)| (session, lease, true)),
                                            _ => {
                                                unreachable!("session commands were matched above")
                                            }
                                        };
                                        let (session, next_lease, resumed) = match claimed {
                                            Ok(claimed) => claimed,
                                            Err(error) => {
                                                app.session_change_failed(error.to_string());
                                                redraw = true;
                                                continue 'main;
                                            }
                                        };
                                        let choices = reconcile_session_choices(
                                            &sessions,
                                            &cwd,
                                            &app.sessions,
                                            (!resumed).then_some(session.id),
                                        )
                                        .await?;
                                        terminal
                                            .terminal
                                            .draw(|frame| ui::render(frame, &mut app))
                                            .map_err(AppError::Terminal)?;
                                        let shutdown = runtime.system.shutdown().await;
                                        runtime.quiesce().await;
                                        shutdown.map_err(|error| {
                                            AppError::Shutdown(error.to_string())
                                        })?;
                                        // Before compaction, so the blobs these
                                        // writes orphan are reclaimed by the
                                        // background pass this switch spawns.
                                        phase(
                                            "session_switch_checkpoints",
                                            runtime.write_teardown_checkpoints(),
                                        )
                                        .await;
                                        spawn_old_session_compaction(
                                            runtime,
                                            &mut background_compactions,
                                        );
                                        drop(session_lease);
                                        session_lease = next_lease;
                                        if let Some(diagnostics) = &diagnostics {
                                            diagnostics
                                                .activate(&session)
                                                .map_err(AppError::Diagnostics)?;
                                        }
                                        (runtime, app) = open_session(
                                            &config,
                                            &cwd,
                                            session,
                                            resumed,
                                            &config_path,
                                            choices,
                                            preferences.as_ref(),
                                        )
                                        .await?;
                                        // The runtime and app were replaced; do
                                        // not keep draining against them.
                                        redraw = true;
                                        continue 'main;
                                    }
                                    Command::RefreshSessions => {
                                        match reconcile_session_choices(
                                            &sessions,
                                            &cwd,
                                            &app.sessions,
                                            None,
                                        )
                                        .await
                                        {
                                            Ok(choices) => app.replace_sessions(choices),
                                            Err(error) => {
                                                app.session_change_failed(error.to_string())
                                            }
                                        }
                                    }
                                    // Deleting a catalog entry never touches the
                                    // open session, so the runtime keeps running.
                                    Command::DeleteSession(id) => match sessions.delete(id, &cwd) {
                                        Ok(()) => {
                                            match reconcile_session_choices(
                                                &sessions,
                                                &cwd,
                                                &app.sessions,
                                                None,
                                            )
                                            .await
                                            {
                                                Ok(choices) => {
                                                    app.replace_sessions(choices);
                                                    app.session_deleted(id);
                                                }
                                                Err(error) => {
                                                    app.session_change_failed(error.to_string())
                                                }
                                            }
                                        }
                                        Err(error) => app.session_change_failed(error.to_string()),
                                    },
                                    // Deleting the open session means replacing
                                    // it: the successor is claimed first, so a
                                    // failure anywhere leaves the user with a
                                    // session, and the old journal is only
                                    // deletable once this runtime and its lease
                                    // have let go of it.
                                    Command::ReplaceSession { delete: old_id } => {
                                        // A replacement starts fresh, so it
                                        // carries the current preferences like
                                        // Command::New does.
                                        let preferences = app.runtime_preferences();
                                        let (session, next_lease) = match sessions.create(&cwd) {
                                            Ok(claimed) => claimed,
                                            Err(error) => {
                                                app.session_change_failed(error.to_string());
                                                redraw = true;
                                                continue 'main;
                                            }
                                        };
                                        terminal
                                            .terminal
                                            .draw(|frame| ui::render(frame, &mut app))
                                            .map_err(AppError::Terminal)?;
                                        let shutdown = runtime.system.shutdown().await;
                                        runtime.quiesce().await;
                                        shutdown.map_err(|error| {
                                            AppError::Shutdown(error.to_string())
                                        })?;
                                        // No teardown checkpoints and no
                                        // compaction: this journal is about to
                                        // be removed, so both would be wasted
                                        // work. Releasing the store is what the
                                        // deletion needs.
                                        drop(runtime.into_store());
                                        drop(session_lease);
                                        session_lease = next_lease;
                                        let deleted = sessions.delete(old_id, &cwd);
                                        // Listed after the deletion, so the
                                        // replaced session is already gone from
                                        // the palette.
                                        let choices = reconcile_session_choices(
                                            &sessions,
                                            &cwd,
                                            &app.sessions,
                                            Some(session.id),
                                        )
                                        .await?;
                                        if let Some(diagnostics) = &diagnostics {
                                            diagnostics
                                                .activate(&session)
                                                .map_err(AppError::Diagnostics)?;
                                        }
                                        (runtime, app) = open_session(
                                            &config,
                                            &cwd,
                                            session,
                                            false,
                                            &config_path,
                                            choices,
                                            Some(&preferences),
                                        )
                                        .await?;
                                        // A failed deletion never aborts the
                                        // switch: the old session survives in
                                        // the catalog and the picker can drop it
                                        // later.
                                        match deleted {
                                            Ok(()) => app.session_replaced(old_id),
                                            Err(error) => {
                                                app.session_change_failed(error.to_string())
                                            }
                                        }
                                        // The runtime and app were replaced; do
                                        // not keep draining against them.
                                        redraw = true;
                                        continue 'main;
                                    }
                                    command => runtime.execute(command, command_results.clone()),
                                }
                            }
                        }
                        Event::Mouse(mouse) => {
                            if let Some(selected) = app.handle_mouse(mouse) {
                                match clipboard::copy_to_clipboard(&selected) {
                                    Ok(()) => app.show_toast("Copied selection".to_owned()),
                                    Err(error) => app.show_toast(format!("Copy failed: {error}")),
                                }
                            }
                        }
                        Event::Paste(text) => app.handle_paste(&text),
                        Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => {}
                    }
                    redraw = true;
                }
                Tick::Agent(event) => {
                    // Fold the owning actor's journal before applying the event:
                    // committed rows always render from the journal, in journal
                    // order, and the event itself carries only streaming deltas
                    // and status.
                    if let Some(address) = fold_address(&event) {
                        redraw |= fold_and_apply(&mut runtime, &mut app, &address).await;
                    }
                    redraw |= app.apply_agent_event(event);
                }
                Tick::Command(result) => {
                    match result {
                        CommandResult::Message(Ok(sent)) => {
                            // Fold first: if the delivery already committed, the
                            // row is rendered and the receipt registers nothing.
                            fold_and_apply(&mut runtime, &mut app, "/root").await;
                            let consumed = runtime.is_consumed("/root", &sent.message_id);
                            app.apply_message_receipt(sent, consumed);
                        }
                        CommandResult::Message(Err(error)) => app.apply_message_error(error),
                        CommandResult::Interrupt(result) => {
                            for address in runtime.projected_addresses() {
                                fold_and_apply(&mut runtime, &mut app, &address).await;
                            }
                            app.apply_interrupt_result(result);
                        }
                        other => app.apply_command_result(other),
                    }
                    redraw = true;
                }
                Tick::Deadline => {
                    redraw |= app.expire_interruption(std::time::Instant::now());
                }
                Tick::Toast => {
                    app.toast = None;
                    redraw = true;
                }
                Tick::Frame => {
                    // Animation tick: request a paint on the next loop top.
                    if app.agents_drawer_animates() {
                        redraw = true;
                    }
                }
                Tick::Idle => {}
            }
            if app.should_exit {
                break;
            }
            // Refill from whatever is already queued, input first so
            // interaction stays ordered ahead of streaming noise.
            tick = if let Ok(terminal_event) = terminal_receiver.try_recv() {
                Tick::Terminal(terminal_event)
            } else if let Ok(result) = command_receiver.try_recv() {
                Tick::Command(result)
            } else if let Some(event) = runtime.events.try_next() {
                Tick::Agent(event)
            } else {
                break;
            };
        }
        // Keep the frame clock alive while the agents drawer (collapsed or
        // expanded) needs a spinner — not gated on keystrokes.
        if app.agents_drawer_animates() {
            redraw = true;
        }
    }

    let shutdown = phase("shutdown_abort", runtime.system.abort()).await;
    phase("shutdown_quiesce", runtime.quiesce()).await;
    shutdown.map_err(|error| AppError::Shutdown(error.to_string()))?;
    // Before compaction, so the blobs these writes orphan are reclaimed by the
    // same teardown rather than lingering as waste.
    phase("shutdown_checkpoints", runtime.write_teardown_checkpoints()).await;
    phase_sync("shutdown_background_compactions", || {
        for handle in background_compactions.drain(..) {
            let _ = handle.join();
        }
    });
    terminal.restore()?;
    phase_sync("shutdown_compact_store", || compact_store(runtime));
    Ok(())
}

async fn wait_for_interruption_deadline(deadline: Option<std::time::Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

async fn wait_for_toast_deadline(deadline: Option<tokio::time::Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    }
}

enum Tick {
    Terminal(io::Result<Event>),
    Agent(AgentSystemEvent),
    Command(CommandResult),
    Deadline,
    /// A copy toast's display window elapsed; clear it and repaint.
    Toast,
    /// A throttled redraw came due; the frame renders at the top of the loop.
    Frame,
    Idle,
}

/// Which actor's journal to fold before applying the event. Streaming deltas
/// commit nothing, so they skip the fold; every other event marks a point
/// where the journal may have advanced.
fn fold_address(event: &AgentSystemEvent) -> Option<String> {
    match event {
        AgentSystemEvent::Run {
            event: RunEvent::ModelDelta { .. },
            ..
        } => None,
        AgentSystemEvent::Run { address, .. }
        | AgentSystemEvent::Hosted { address, .. }
        | AgentSystemEvent::Retired { address, .. }
        | AgentSystemEvent::ActorRuntime { address, .. } => Some(address.to_string()),
        AgentSystemEvent::Outcome { outcome } => Some(
            match outcome {
                AgentOutcome::Completed { address, .. }
                | AgentOutcome::Failed { address, .. }
                | AgentOutcome::Cancelled { address, .. } => address,
            }
            .to_string(),
        ),
    }
}

async fn fold_and_apply(runtime: &mut Runtime, app: &mut App, address: &str) -> bool {
    match runtime.fold(address).await {
        Ok(outcome) => app.apply_fold(address, outcome),
        Err(error) => {
            app.fold_failed(address, error);
            true
        }
    }
}

/// Compaction of a large journal costs seconds, so teardown runs it only
/// when it can reclaim both a meaningful absolute amount and a meaningful
/// share of the file. Below the thresholds quit stays instant; the waste is
/// reclaimed on a later quit once enough accumulates.
const COMPACTION_MIN_RECLAIMABLE_BYTES: u64 = 64 * 1024 * 1024;

fn worth_compacting(footprint: &StoreFootprint) -> bool {
    footprint.reclaimable_bytes >= COMPACTION_MIN_RECLAIMABLE_BYTES
        && footprint.reclaimable_bytes.saturating_mul(4) >= footprint.file_bytes
}

/// Best-effort journal compaction at quit, gated on measured waste. The
/// terminal is restored first so the user sees the status line and the shell
/// prompt is not delayed behind a frozen alternate screen. Whatever happens,
/// the store must drop before the process exits: a clean close is what lets
/// the next boot open the journal without a full repair.
fn compact_store(runtime: Runtime) {
    let store = runtime.into_store();
    let Ok(mut store) = Arc::try_unwrap(store) else {
        // Another owner still holds the journal; it will close when that
        // reference drops, but no maintenance can run without exclusivity.
        eprintln!("lam: session journal is still referenced at quit; skipping maintenance");
        return;
    };
    let footprint = match store.footprint() {
        Ok(footprint) => footprint,
        Err(error) => {
            eprintln!("lam: could not measure the session journal: {error}");
            return;
        }
    };
    if !worth_compacting(&footprint) {
        return;
    }
    println!(
        "lam: compacting session journal ({} MiB reclaimable)",
        footprint.reclaimable_bytes >> 20
    );
    let started = std::time::Instant::now();
    match store.compact() {
        Ok(_) => println!(
            "lam: session journal compacted in {:.1}s",
            started.elapsed().as_secs_f64()
        ),
        Err(error) => eprintln!("lam: session journal compaction failed: {error}"),
    }
}

/// Compacts a torn-down session journal on a background thread so a session
/// switch is not delayed, with the same waste gate as quit. Quit joins every
/// such thread before exiting.
fn spawn_old_session_compaction(
    runtime: Runtime,
    background: &mut Vec<std::thread::JoinHandle<()>>,
) {
    let store = runtime.into_store();
    let Ok(mut store) = Arc::try_unwrap(store) else {
        return;
    };
    if let Ok(handle) = std::thread::Builder::new()
        .name("lam-session-compaction".to_owned())
        .spawn(move || {
            if !store
                .footprint()
                .is_ok_and(|footprint| worth_compacting(&footprint))
            {
                return;
            }
            let started = std::time::Instant::now();
            let relocated = store.compact();
            tracing::info!(
                target: "lam_tui::runtime",
                event = "session.compaction",
                elapsed_ms = started.elapsed().as_millis() as u64,
                relocated = ?relocated.ok(),
                "old session journal compacted in the background"
            );
        })
    {
        background.push(handle);
    }
}

async fn open_session(
    config: &LoadedConfig,
    cwd: &Path,
    session: Session,
    resumed: bool,
    config_path: &str,
    choices: Vec<SessionChoice>,
    preferences: Option<&RuntimePreferences>,
) -> Result<(Runtime, App), AppError> {
    let runtime = Runtime::build(config, cwd.to_path_buf(), &session, preferences)
        .await
        .map_err(AppError::Runtime)?;
    let selected_effort = runtime.selected_effort();
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
        &selected_effort,
        runtime.startup_warnings.clone(),
    );
    Ok((runtime, app))
}

async fn session_choices(
    catalog: &SessionCatalog,
    cwd: &Path,
) -> Result<Vec<SessionChoice>, AppError> {
    reconcile_session_choices(catalog, cwd, &[], None).await
}

async fn reconcile_session_choices(
    catalog: &SessionCatalog,
    cwd: &Path,
    cached: &[SessionChoice],
    fresh_session: Option<u64>,
) -> Result<Vec<SessionChoice>, AppError> {
    let listings = catalog.list(cwd).map_err(AppError::Session)?;
    let mut cached = cached
        .iter()
        .cloned()
        .map(|choice| (choice.id, choice))
        .collect::<BTreeMap<_, _>>();
    let mut choices = Vec::with_capacity(listings.len());
    for listing in listings {
        let session = listing.session;
        if fresh_session == Some(session.id) {
            choices.push(SessionChoice {
                id: session.id,
                preview: None,
            });
            continue;
        }
        // The catalog caches each session's first user message, so listing
        // sessions normally never opens their journals.
        if let Some(preview) = listing.preview {
            choices.push(SessionChoice {
                id: session.id,
                preview: Some(preview),
            });
            continue;
        }
        if let Some(choice) = cached.remove(&session.id)
            && !choice
                .preview
                .as_deref()
                .is_some_and(|preview| preview.starts_with("Preview unavailable:"))
        {
            choices.push(choice);
            continue;
        }
        let preview = match phase(
            &format!("session_preview_{}", session.id),
            runtime::first_user_message(&session),
        )
        .await
        {
            Ok(preview) => {
                // Backfill the catalog so this journal scan happens once per
                // session, ever. Best-effort: a failed write only costs a
                // rescan on the next boot.
                if let Some(text) = &preview
                    && let Err(error) = catalog.store_preview(session.id, text)
                {
                    tracing::warn!(
                        target: "lam_tui::session",
                        session_id = session.id,
                        %error,
                        "session preview could not be cached in the catalog"
                    );
                }
                preview
            }
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
    command: Option<CliCommand>,
}

enum CliCommand {
    Login {
        provider: LoginProvider,
        no_browser: bool,
        force: bool,
    },
    Logout {
        provider: LoginProvider,
    },
}

#[derive(Clone, Copy)]
enum LoginProvider {
    OpenAi,
    Xai,
}

impl Args {
    fn parse() -> Result<Self, AppError> {
        let mut config = None;
        let mut debug_log = false;
        let mut help = false;
        let mut version = false;
        let mut command = None;
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
                Some("login") => {
                    let provider = arguments
                        .next()
                        .and_then(|value| value.to_str().map(str::to_owned))
                        .ok_or_else(|| {
                            AppError::Arguments(
                                "login requires a provider; try `lam-agent login openai` or `lam-agent login xai`".to_owned(),
                            )
                        })?;
                    let mut no_browser = false;
                    let mut force = false;
                    for flag in arguments.by_ref() {
                        match flag.to_str() {
                            Some("--no-browser") => no_browser = true,
                            Some("--force") => force = true,
                            Some(value) => {
                                return Err(AppError::Arguments(format!(
                                    "unknown login flag `{value}`; try --help"
                                )));
                            }
                            None => {
                                return Err(AppError::Arguments(
                                    "arguments must be valid UTF-8".to_owned(),
                                ));
                            }
                        }
                    }
                    command = Some(CliCommand::Login {
                        provider: parse_login_provider(&provider)?,
                        no_browser,
                        force,
                    });
                }
                Some("logout") => {
                    let provider = arguments
                        .next()
                        .and_then(|value| value.to_str().map(str::to_owned))
                        .ok_or_else(|| {
                            AppError::Arguments(
                                "logout requires a provider; try `lam-agent logout openai` or `lam-agent logout xai`".to_owned(),
                            )
                        })?;
                    command = Some(CliCommand::Logout {
                        provider: parse_login_provider(&provider)?,
                    });
                }
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
            command,
        })
    }
}

fn parse_login_provider(value: &str) -> Result<LoginProvider, AppError> {
    match value {
        "openai" | "codex" | "chatgpt" => Ok(LoginProvider::OpenAi),
        "xai" | "supergrok" | "grok" => Ok(LoginProvider::Xai),
        other => Err(AppError::Arguments(format!(
            "unknown auth provider `{other}`; supported: openai, xai"
        ))),
    }
}

async fn run_command(command: CliCommand) -> Result<(), AppError> {
    match command {
        CliCommand::Login {
            provider: LoginProvider::OpenAi,
            no_browser,
            force,
        } => {
            codex::login(no_browser, force)
                .await
                .map_err(AppError::CodexAuth)?;
            Ok(())
        }
        CliCommand::Login {
            provider: LoginProvider::Xai,
            no_browser,
            force: _,
        } => {
            let store = xai::XaiCredentialStore::default_store().map_err(AppError::Auth)?;
            xai::device_login(&store, !no_browser)
                .await
                .map_err(AppError::OAuth)?;
            Ok(())
        }
        CliCommand::Logout {
            provider: LoginProvider::OpenAi,
        } => {
            codex::logout().map_err(AppError::CodexAuth)?;
            println!(
                "Removed shared Codex login credentials; the official Codex CLI is signed out too (the cache is shared)."
            );
            Ok(())
        }
        CliCommand::Logout {
            provider: LoginProvider::Xai,
        } => {
            let store = xai::XaiCredentialStore::default_store().map_err(AppError::Auth)?;
            store.clear().map_err(AppError::Auth)?;
            println!(
                "Removed SuperGrok credentials from {}.",
                store.path().display()
            );
            Ok(())
        }
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
        "lam-agent — a minimal TypeScript coding agent\n\nUSAGE:\n    lam-agent [--config PATH] [--debug-log]\n    lam-agent login openai [--no-browser] [--force]\n    lam-agent login xai [--no-browser]\n    lam-agent logout openai\n    lam-agent logout xai\n\nOPTIONS:\n    --config PATH  Read providers from PATH instead of ~/.lam/providers.toml\n    --debug-log    Append metadata-only diagnostics beside the session journal\n    -h, --help     Show this help\n    -V, --version  Show the version\n\nCOMMANDS:\n    login openai   Sign in with ChatGPT via Codex OAuth; --force replaces an existing login\n    login xai      Sign in with SuperGrok / X Premium via device-code OAuth\n    logout openai  Remove the shared Codex login (also signs out the official Codex CLI)\n    logout xai     Remove stored SuperGrok credentials from ~/.lam/auth/xai.json\n\nLam resumes the latest durable session for the current directory. Inside the TUI,\ntype / for commands, including /new for a fresh session and /session to restore\nan earlier one; in that picker, Ctrl+D twice deletes the highlighted session.\nPress Alt+M to select a provider and model. Tab switches focus between the input\nshelf and conversation; arrows select transcript rows; Enter expands the selected\nrow. While the root is working, a submitted message is queued above the input as\na pending steer and is delivered at the next model boundary. Press Escape twice\nwithin 1.5 seconds to stop its complete agent tree."
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
        "{0}\n\nRun `lam-agent login openai` to use the built-in Codex catalog, or create the file. See crates/lam-tui/README.md for details."
    )]
    Config(crate::config::ConfigError),
    #[error(transparent)]
    Session(crate::session::SessionError),
    #[error(transparent)]
    Diagnostics(crate::diagnostics::DiagnosticError),
    #[error(transparent)]
    Runtime(crate::runtime::RuntimeError),
    #[error(transparent)]
    Auth(crate::xai::AuthError),
    #[error(transparent)]
    OAuth(crate::xai::OAuthError),
    #[error(transparent)]
    CodexAuth(crate::codex::CodexAuthError),
    #[error("terminal operation failed: {0}")]
    Terminal(std::io::Error),
    #[error("could not start terminal event reader: {0}")]
    EventThread(std::io::Error),
    #[error("could not stop the agent runtime: {0}")]
    Shutdown(String),
}
