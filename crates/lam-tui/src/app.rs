use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use tokio::time::Instant as TokioInstant;

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use lam::{ModelDelta, RunEvent, RunId, RuntimeEvent, ToolCallDelta};
use lam_agents::{AgentOutcome, AgentSystemEvent, StopReason};
use ratatui::layout::Rect;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::ModelChoice;
use crate::runtime::{
    AgentHistory, Command, CommandResult, CommittedRow, FoldOutcome, HistoryKind,
    RuntimePreferences, SentMessage,
};

const MOUSE_SCROLL_LINES: usize = 2;
const TOAST_DURATION: Duration = Duration::from_millis(2_000);
const INTERRUPTION_ARM_WINDOW: Duration = Duration::from_millis(1_500);
const INTERRUPTION_WARNING: &str = "Press Esc again to stop the current run";
const SESSION_PICKER_HINT: &str = "ctrl+d delete the highlighted session";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Focus {
    Conversation,
    Input,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryKind {
    User,
    Assistant,
    Reasoning,
    ToolCall,
    ToolResult,
    System,
    Error,
}

#[derive(Clone, Debug)]
pub(crate) struct ConversationEntry {
    pub(crate) kind: EntryKind,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) expanded: bool,
    pub(crate) pending_tool: bool,
    pub(crate) streaming: bool,
    model_owner: Option<String>,
    model_run: Option<String>,
    tool_owner: Option<String>,
    tool_run: Option<String>,
    tool_index: Option<usize>,
    tool_name: String,
    /// Which streaming turn produced this overlay row. `None` for committed
    /// rows. A committed model turn replaces the oldest overlay turn of its
    /// run, so segmentation must survive fold lag.
    overlay_turn: Option<u64>,
    /// Cached visual layout, rebuilt only when [`LayoutKey`] changes. Bodies
    /// and titles are append-only, so their lengths are sound dirty signals.
    pub(crate) layout: Option<EntryLayout>,
}

/// One row's laid-out lines, valid while its key matches the row's state.
#[derive(Clone, Debug)]
pub(crate) struct EntryLayout {
    pub(crate) key: LayoutKey,
    pub(crate) lines: Vec<ratatui::text::Line<'static>>,
    /// Presentation-padding cell width of each line: the alignment
    /// indentation and header furniture the renderer adds, which copied
    /// text should not include.
    pub(crate) pads: Vec<usize>,
}

/// Everything the visual layout of a row depends on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LayoutKey {
    pub(crate) width: usize,
    pub(crate) expanded: bool,
    pub(crate) selected: bool,
    pub(crate) dimmed: bool,
    pub(crate) title_len: usize,
    pub(crate) body_len: usize,
}

/// One entry's visible click target in terminal rows: the row span the mouse
/// can hit, the header row that toggles expand/collapse (the entry's first
/// layout line, `None` when scrolled out of the viewport), and the entry
/// index the click selects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Hitbox {
    pub(crate) top: u16,
    pub(crate) bottom: u16,
    pub(crate) header: Option<u16>,
    pub(crate) entry: usize,
}

impl ConversationEntry {
    fn run_id(&self) -> Option<&str> {
        self.model_run.as_deref().or(self.tool_run.as_deref())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Suggestion {
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) replacement: String,
    pub(crate) provider: Option<String>,
    /// True when this row is an agent with a live harness run.
    pub(crate) running: bool,
}

/// One user message admitted as a durable steer of the active root run.
#[derive(Clone, Debug)]
pub(crate) struct PendingSteer {
    /// Durable identity used to match the message against delivery events.
    /// None while the message awaits its admission receipt: the optimistic
    /// row shown the moment the user submits.
    pub(crate) message_id: Option<String>,
    /// The user's message text.
    pub(crate) text: String,
}

pub(crate) struct SessionView {
    pub(crate) id: u64,
    pub(crate) journal_path: String,
    pub(crate) resumed: bool,
    pub(crate) agents: Vec<AgentHistory>,
    pub(crate) choices: Vec<SessionChoice>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionChoice {
    pub(crate) id: u64,
    pub(crate) preview: Option<String>,
}

/// The one advisory line under the palette: a pending destructive
/// confirmation, or the picker's key hint when nothing is armed.
pub(crate) struct Notice<'a> {
    pub(crate) text: &'a str,
    pub(crate) kind: NoticeKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NoticeKind {
    Warning,
    Hint,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct InputBuffer {
    pub(crate) text: String,
    pub(crate) cursor: usize,
    preferred_column: Option<usize>,
}

struct InputHistory {
    messages: Vec<String>,
    position: usize,
    draft: InputBuffer,
}

struct InputLayout {
    rows: Vec<String>,
    cursor_positions: Vec<(usize, usize)>,
}

/// A cell position in the conversation viewport: a layout row and column
/// relative to the conversation pane's top-left corner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CellPos {
    pub(crate) row: usize,
    pub(crate) col: usize,
}

/// An in-progress drag selection over the conversation viewport. The anchor
/// is where the drag started and the head follows the cursor; a completed
/// selection is copied to the clipboard and cleared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TextSelection {
    pub(crate) anchor: CellPos,
    pub(crate) head: CellPos,
}

impl TextSelection {
    /// The selection as an ordered (start, end) pair for rendering and copy.
    pub(crate) fn normalized(&self) -> (CellPos, CellPos) {
        if (self.anchor.row, self.anchor.col) <= (self.head.row, self.head.col) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// A transient status toast drawn over the conversation pane until its
/// deadline, then cleared by the main loop.
pub(crate) struct Toast {
    pub(crate) text: String,
    pub(crate) deadline: TokioInstant,
}

/// One visible conversation viewport row, plus how many leading cells are
/// presentation padding (alignment indentation and header furniture) added
/// purely for layout. Copying skips those cells so pasted text keeps only
/// the row's content, including any content-level indentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CopyRow {
    pub(crate) pad: usize,
    pub(crate) text: String,
}

/// Maps a terminal row/column onto a viewport cell, clamped to the pane.
fn cell_pos(row: u16, column: u16, area: Rect) -> CellPos {
    CellPos {
        row: usize::from(row.saturating_sub(area.y))
            .min(usize::from(area.height.saturating_sub(1))),
        col: usize::from(column.saturating_sub(area.x))
            .min(usize::from(area.width.saturating_sub(1))),
    }
}

/// The copied text for a selection over the viewport's visible rows: the
/// selected content cells of each row in order, joined with newlines, with
/// trailing whitespace trimmed per row. Each row's presentation padding is
/// skipped, so alignment indentation and header furniture never reach the
/// clipboard. The selection is normalized, so the anchor may be below or
/// to the right of the head.
pub(crate) fn selected_text(rows: &[CopyRow], anchor: CellPos, head: CellPos) -> String {
    let (start, end) = if (anchor.row, anchor.col) <= (head.row, head.col) {
        (anchor, head)
    } else {
        (head, anchor)
    };
    if rows.is_empty() || start.row >= rows.len() {
        return String::new();
    }
    let last = rows.len() - 1;
    let mut parts = Vec::new();
    let last_row = end.row.min(last);
    for (row, copy_row) in rows.iter().enumerate().take(last_row + 1).skip(start.row) {
        let from_col = if row == start.row {
            start.col.max(copy_row.pad)
        } else {
            copy_row.pad
        };
        let to_col = if row == end.row { end.col } else { usize::MAX };
        parts.push(
            slice_cells(&copy_row.text, from_col, to_col)
                .trim_end()
                .to_owned(),
        );
    }
    parts.join("\n")
}

/// Yields each grapheme cluster in `text` with its half-open terminal-cell
/// range, beginning at `start_cell`.
pub(crate) fn grapheme_cells(
    text: &str,
    start_cell: usize,
) -> impl Iterator<Item = (&str, usize, usize)> {
    let mut cell = start_cell;
    text.graphemes(true).map(move |grapheme| {
        let start = cell;
        cell = cell.saturating_add(UnicodeWidthStr::width(grapheme));
        (grapheme, start, cell)
    })
}

/// The substring of `text` occupying terminal cells `from_cell..=to_cell`.
/// Cells are counted by display width, and every grapheme that overlaps the
/// selection is kept whole.
pub(crate) fn slice_cells(text: &str, from_cell: usize, to_cell: usize) -> String {
    let mut out = String::new();
    for (grapheme, start, end) in grapheme_cells(text, 0) {
        if start > to_cell {
            break;
        }
        if start <= to_cell && end > from_cell {
            out.push_str(grapheme);
        }
    }
    out
}

pub(crate) struct App {
    pub(crate) cwd: String,
    pub(crate) session_id: u64,
    pub(crate) sessions: Vec<SessionChoice>,
    pub(crate) models: Vec<ModelChoice>,
    pub(crate) selected_model: usize,
    selected_efforts: Vec<usize>,
    pub(crate) focus: Focus,
    pub(crate) input: InputBuffer,
    pub(crate) input_width: usize,
    /// Terminal geometry of the input box from the last rendered frame, so
    /// mouse clicks can be mapped back to character positions.
    pub(crate) input_area: Option<Rect>,
    pub(crate) entries: Vec<ConversationEntry>,
    pub(crate) selected_entry: Option<usize>,
    pub(crate) conversation_offset: usize,
    pub(crate) follow_conversation_tail: bool,
    pub(crate) selection_drives_viewport: bool,
    /// One-shot: next paint aligns this entry near the top of the viewport
    /// (keyboard expand only). Cleared after use.
    pub(crate) reveal_entry_top: Option<usize>,
    pub(crate) conversation_ranges: Vec<(usize, usize)>,
    pub(crate) conversation_viewport_height: usize,
    pub(crate) conversation_total_lines: usize,
    pub(crate) suggestion_index: usize,
    pub(crate) busy: bool,
    pub(crate) status: String,
    /// Provider-reported context consumption (total_tokens) of the newest
    /// completed model response for the current agent, when one exists.
    pub(crate) context_tokens: Option<u64>,
    pub(crate) should_exit: bool,
    /// Visible click targets for the conversation, one per entry with at
    /// least one row in the viewport.
    pub(crate) hitboxes: Vec<Hitbox>,
    /// A drag in progress over the conversation viewport, if any.
    pub(crate) text_selection: Option<TextSelection>,
    /// Visible text and presentation padding of each conversation viewport
    /// row from the last frame, so mouse-up can slice the selected cells
    /// into copied text without the layout indentation.
    pub(crate) conversation_rows: Vec<CopyRow>,
    /// Terminal geometry of the conversation pane from the last frame, so
    /// mouse events can be mapped to viewport cell positions.
    pub(crate) conversation_area: Option<Rect>,
    pub(crate) toast: Option<Toast>,
    pub(crate) current_agent: String,
    /// Durably admitted user messages not yet consumed into model-visible
    /// context, shown above the input bar until the runner delivers them at
    /// a model boundary.
    pub(crate) pending_steers: Vec<PendingSteer>,
    input_history: Option<InputHistory>,
    current_parent: Option<String>,
    current_model: Option<String>,
    inactive_agents: BTreeMap<String, AgentConversation>,
    /// Boundary between pinned rows (committed journal rows and local system
    /// rows, in arrival order) and the streaming overlay at the tail.
    committed_len: usize,
    /// Counts streaming turns for the current agent; overlay rows are tagged
    /// with the turn that produced them.
    overlay_turn: u64,
    /// Runs that reached a terminal, failed, or interrupted entry. Late
    /// streaming events for these runs are stale and are ignored.
    dead_runs: BTreeSet<String>,
    root_run_active: bool,
    /// Whether the currently viewed agent has a live harness run. For
    /// \`/root\` this mirrors \`root_run_active\`; for children it is independent.
    current_run_active: bool,
    /// Create order of the currently viewed agent (0 for `/root`).
    current_created_ord: u64,
    /// Next create-order value to assign to a newly observed agent.
    next_agent_ord: u64,
    /// A blocking command (compact, model switch, session change) awaits its
    /// result. Messages are never blocking; they queue as steers.
    command_in_flight: bool,
    interruption_deadline: Option<Instant>,
    interruption_in_progress: bool,
    /// Session armed for deletion by a first ctrl+d in the session palette.
    /// Sticky until the next keystroke, whatever it is.
    armed_session: Option<u64>,
    /// What the session palette has to say about deletion: the armed
    /// confirmation, worded differently for the open session because deleting
    /// it replaces it. Cleared with the arming.
    session_notice: Option<String>,
}

struct AgentConversation {
    entries: Vec<ConversationEntry>,
    selected_entry: Option<usize>,
    conversation_offset: usize,
    follow_conversation_tail: bool,
    selection_drives_viewport: bool,
    reveal_entry_top: Option<usize>,
    status: String,
    context_tokens: Option<u64>,
    parent: Option<String>,
    model: Option<String>,
    /// Whether this agent currently has a live harness run.
    run_active: bool,
    /// Monotonic create order for this session view (higher = newer).
    /// Assigned when the agent is first observed (Hosted / ensure / restore).
    created_ord: u64,
    committed_len: usize,
    overlay_turn: u64,
    dead_runs: BTreeSet<String>,
}

impl App {
    pub(crate) fn new(
        cwd: String,
        config_path: String,
        session: SessionView,
        models: Vec<ModelChoice>,
        selected_model: usize,
        selected_effort: &str,
        startup_warnings: Vec<String>,
    ) -> Self {
        let action = if session.resumed {
            "Resumed"
        } else {
            "Started"
        };
        let session_id = session.id;
        let session_path = session.journal_path;
        let sessions = session.choices;
        let ready_message = format!(
            "{action} durable session #{session_id}. Coding and multi-agent capabilities are active. Using {} ({} token context). Journal: {session_path}. Configuration: {config_path}.",
            models[selected_model].display_name, models[selected_model].context_window
        );
        let mut agents = session.agents;
        let root_index = agents
            .iter()
            .position(|agent| agent.address == "/root")
            .unwrap_or_else(|| {
                agents.push(AgentHistory::root(Vec::new()));
                agents.len() - 1
            });
        let root = agents.remove(root_index);
        let root_run_active = !root.run_completed;
        let mut entries = conversation_from_rows(root.history);
        entries.push(pinned_entry(
            EntryKind::System,
            "Ready".to_owned(),
            ready_message,
        ));
        // Host-only warnings (skipped providers, default fallback). Not folded
        // from the journal — same class of local chrome as the Ready line.
        for warning in startup_warnings {
            entries.push(pinned_entry(
                EntryKind::System,
                "Provider unavailable".to_owned(),
                warning,
            ));
        }
        let committed_len = entries.len();
        let selected_entry = entries.len().checked_sub(1);
        let mut selected_efforts = models
            .iter()
            .map(|model| model.efforts.len() - 1)
            .collect::<Vec<_>>();
        selected_efforts[selected_model] = models[selected_model]
            .efforts
            .iter()
            .position(|effort| effort == selected_effort)
            .unwrap_or(selected_efforts[selected_model]);
        // Restored agents have no durable create clock; assign increasing
        // ords in load order so sibling ordering stays stable within a boot.
        let mut next_agent_ord = 1u64;
        let inactive_agents = agents
            .into_iter()
            .map(|agent| {
                let address = agent.address.clone();
                let created_ord = next_agent_ord;
                next_agent_ord = next_agent_ord.saturating_add(1);
                (address, AgentConversation::from_history(agent, created_ord))
            })
            .collect();
        Self {
            cwd,
            session_id,
            sessions,
            models,
            selected_model,
            selected_efforts,
            focus: Focus::Input,
            input: InputBuffer::default(),
            input_width: 80,
            input_area: None,
            entries,
            selected_entry,
            conversation_offset: 0,
            follow_conversation_tail: true,
            selection_drives_viewport: true,
            reveal_entry_top: None,
            conversation_ranges: Vec::new(),
            conversation_viewport_height: 1,
            conversation_total_lines: 0,
            suggestion_index: 0,
            busy: root_run_active,
            status: root.status,
            context_tokens: root.context_tokens,
            should_exit: false,
            hitboxes: Vec::new(),
            text_selection: None,
            conversation_rows: Vec::new(),
            conversation_area: None,
            toast: None,
            current_agent: root.address,
            pending_steers: Vec::new(),
            input_history: None,
            current_parent: root.parent,
            current_model: root.model,
            inactive_agents,
            committed_len,
            overlay_turn: 0,
            dead_runs: BTreeSet::new(),
            root_run_active,
            current_run_active: root_run_active,
            current_created_ord: 0,
            next_agent_ord,
            command_in_flight: false,
            interruption_deadline: None,
            interruption_in_progress: false,
            armed_session: None,
            session_notice: None,
        }
    }

    pub(crate) fn selected_model(&self) -> &ModelChoice {
        &self.models[self.selected_model]
    }

    pub(crate) fn current_agent_model(&self) -> Option<&str> {
        if self.current_agent == "/root" {
            Some(self.selected_model().registry_id.as_str())
        } else {
            self.current_model.as_deref()
        }
    }

    pub(crate) fn current_agent_effort(&self) -> Option<&str> {
        let model_index = if self.current_agent == "/root" {
            self.selected_model
        } else {
            let model = self.current_model.as_deref()?;
            self.models
                .iter()
                .position(|choice| choice.registry_id == model)?
        };
        self.models[model_index]
            .efforts
            .get(self.selected_efforts[model_index])
            .map(String::as_str)
    }

    /// Context window of the model driving the current agent, when known.
    pub(crate) fn context_window_tokens(&self) -> Option<u64> {
        let model = self
            .current_agent_model()
            .and_then(|id| self.models.iter().find(|model| model.registry_id == id))
            .unwrap_or_else(|| self.selected_model());
        (model.context_window > 0).then_some(model.context_window)
    }

    pub(crate) fn runtime_preferences(&self) -> RuntimePreferences {
        RuntimePreferences {
            model_id: self.models[self.selected_model].registry_id.clone(),
            effort: self.models[self.selected_model].efforts
                [self.selected_efforts[self.selected_model]]
                .clone(),
        }
    }

    pub(crate) fn replace_sessions(&mut self, sessions: Vec<SessionChoice>) {
        self.sessions = sessions;
        // A deletion can shrink the palette under the highlight.
        self.suggestion_index = self
            .suggestion_index
            .min(self.suggestions().len().saturating_sub(1));
    }

    pub(crate) fn session_change_failed(&mut self, error: impl Into<String>) {
        self.finish_command();
        self.push_error("Session change failed", error.into());
    }

    /// Confirms a session dropped from the catalog while another one stays
    /// open, so nothing about the running session changes.
    pub(crate) fn session_deleted(&mut self, id: u64) {
        self.status = format!("Deleted session #{id}");
    }

    /// Confirms a replaced session: this app already renders the successor, so
    /// the status names both halves of the exchange.
    pub(crate) fn session_replaced(&mut self, deleted: u64) {
        self.status = format!("Deleted session #{deleted}; started #{}", self.session_id);
    }

    pub(crate) fn interruption_warning(&self) -> Option<&'static str> {
        self.interruption_deadline.map(|_| INTERRUPTION_WARNING)
    }

    /// The advisory line under the palette. Warnings win: an armed deletion or
    /// interruption always displaces the picker's key hint.
    pub(crate) fn notice(&self) -> Option<Notice<'_>> {
        if let Some(warning) = self.interruption_warning() {
            return Some(Notice {
                text: warning,
                kind: NoticeKind::Warning,
            });
        }
        if let Some(notice) = self.session_notice.as_deref() {
            return Some(Notice {
                text: notice,
                kind: NoticeKind::Warning,
            });
        }
        self.session_palette_open().then_some(Notice {
            text: SESSION_PICKER_HINT,
            kind: NoticeKind::Hint,
        })
    }

    pub(crate) const fn interruption_deadline(&self) -> Option<Instant> {
        self.interruption_deadline
    }

    pub(crate) fn expire_interruption(&mut self, now: Instant) -> bool {
        if self
            .interruption_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.interruption_deadline = None;
            return true;
        }
        false
    }

    pub(crate) fn suggestions(&self) -> Vec<Suggestion> {
        let input = self.input.text.trim_start();
        if let Some(query) = input.strip_prefix("/model ") {
            let query = query.trim().to_lowercase();
            return self
                .models
                .iter()
                .filter(|model| {
                    query.is_empty()
                        || model.registry_id.to_lowercase().contains(&query)
                        || model.display_name.to_lowercase().contains(&query)
                })
                .map(|model| Suggestion {
                    label: format!("{} / {}", model.provider, model.display_name),
                    detail: format!("{} · {}k", model.model, model.context_window / 1_000),
                    replacement: format!("/model {}", model.registry_id),
                    provider: Some(model.provider.clone()),
                    running: false,
                })
                .collect();
        }
        if let Some(query) = input.strip_prefix("/effort ") {
            let query = query.trim().to_lowercase();
            return self
                .selected_model()
                .efforts
                .iter()
                .filter(|effort| query.is_empty() || effort.to_lowercase().contains(&query))
                .map(|effort| Suggestion {
                    label: effort.clone(),
                    detail: format!("{} reasoning effort", self.selected_model().display_name),
                    replacement: format!("/effort {effort}"),
                    provider: None,
                    running: false,
                })
                .collect();
        }
        if self.agents_palette_open() {
            let query = input
                .strip_prefix("/agents")
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            return self
                .agent_addresses()
                .into_iter()
                .filter(|address| query.is_empty() || address.to_lowercase().contains(&query))
                .map(|address| {
                    let current = address == self.current_agent;
                    let running = self.agent_is_running(&address);
                    Suggestion {
                        label: agent_tree_label(&address, current, running),
                        detail: self.agent_detail(&address),
                        replacement: format!("/agents {address}"),
                        provider: None,
                        running,
                    }
                })
                .collect();
        }
        if let Some(sessions) = self.session_matches() {
            return sessions
                .into_iter()
                .map(|session| Suggestion {
                    label: format!("#{}", session.id),
                    detail: session
                        .preview
                        .as_deref()
                        .map(one_line_preview)
                        .unwrap_or_else(|| "No user message yet".to_owned()),
                    replacement: format!("/session {}", session.id),
                    provider: None,
                    running: false,
                })
                .collect();
        }
        if !input.starts_with('/') || input.contains(' ') {
            return Vec::new();
        }
        let query = input.trim_start_matches('/').to_lowercase();
        [
            ("compact", "Compact the current context", "/compact"),
            ("agents", "Switch to an agent conversation", "/agents "),
            ("effort", "Choose the reasoning effort", "/effort "),
            ("model", "Choose a provider and model", "/model "),
            ("new", "Start a new session in this directory", "/new"),
            ("session", "Load a session from this directory", "/session "),
            ("exit", "Close Lam", "/exit"),
            ("quit", "Close Lam", "/quit"),
        ]
        .into_iter()
        .filter(|(command, _, _)| command.starts_with(&query))
        .map(|(command, detail, replacement)| Suggestion {
            label: format!("/{command}"),
            detail: detail.to_owned(),
            replacement: replacement.to_owned(),
            provider: None,
            running: false,
        })
        .collect()
    }

    fn session_palette_open(&self) -> bool {
        let input = self.input.text.trim_start();
        input == "/session" || input.starts_with("/session ")
    }

    /// Sessions offered by the `/session` palette, newest first, or `None`
    /// when that palette is not open. Shared by the suggestion list and the
    /// deletion binding so both agree on which row is highlighted.
    fn session_matches(&self) -> Option<Vec<&SessionChoice>> {
        if !self.session_palette_open() {
            return None;
        }
        let query = self
            .input
            .text
            .trim_start()
            .strip_prefix("/session")
            .unwrap_or_default()
            .trim()
            .trim_start_matches('#')
            .to_lowercase();
        Some(
            self.sessions
                .iter()
                .filter(|session| {
                    query.is_empty()
                        || session.id.to_string().contains(&query)
                        || session
                            .preview
                            .as_deref()
                            .is_some_and(|preview| preview.to_lowercase().contains(&query))
                })
                .collect(),
        )
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<Command> {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return None;
        }
        // Any keystroke dismisses an in-progress drag selection.
        self.text_selection = None;
        let deleting =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d');
        if !deleting {
            // Arming survives until the next keystroke, including Esc and
            // selection movement, so it is resolved before anything else runs.
            self.disarm_session_deletion();
        }
        if key.code == KeyCode::Esc {
            return (key.kind == KeyEventKind::Press)
                .then(|| self.handle_escape(Instant::now()))
                .flatten();
        }
        self.disarm_interruption();
        if deleting {
            // As with Esc, only a physical press arms or confirms, so holding
            // the key down cannot delete a session.
            return (key.kind == KeyEventKind::Press)
                .then(|| self.handle_session_deletion())
                .flatten();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.input.text.is_empty() {
                self.should_exit = true;
            } else {
                self.input = InputBuffer::default();
                self.input_history = None;
                self.suggestion_index = 0;
            }
            return None;
        }
        // Alt+A opens the agents drawer: populate `/agents ` and focus input.
        if key.kind == KeyEventKind::Press
            && key.modifiers.contains(KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A'))
        {
            self.open_agents_drawer();
            return None;
        }

        let suggestions = self.suggestions();
        if !suggestions.is_empty() {
            self.suggestion_index = self.suggestion_index.min(suggestions.len() - 1);
        } else {
            self.suggestion_index = 0;
        }
        match self.focus {
            Focus::Input => self.handle_input_key(key, &suggestions),
            Focus::Conversation => {
                self.handle_conversation_key(key);
                None
            }
        }
    }

    fn handle_escape(&mut self, now: Instant) -> Option<Command> {
        // Dismiss the agents drawer before interruption arming so Esc is a
        // reliable way out of /agents even while root is busy.
        if self.agents_palette_open() {
            self.input = InputBuffer::default();
            self.input_history = None;
            self.suggestion_index = 0;
            self.disarm_interruption();
            return None;
        }
        if !self.root_run_active || self.interruption_in_progress {
            self.disarm_interruption();
            return None;
        }
        if self
            .interruption_deadline
            .is_some_and(|deadline| now <= deadline)
        {
            self.interruption_deadline = None;
            self.interruption_in_progress = true;
            self.with_agent("/root", |app| {
                app.status = "Stopping current run…".to_owned();
            });
            return Some(Command::Interrupt);
        }
        self.interruption_deadline = Some(now + INTERRUPTION_ARM_WINDOW);
        None
    }

    fn disarm_interruption(&mut self) {
        self.interruption_deadline = None;
    }

    /// ctrl+d in the session palette: the first press arms the highlighted
    /// session, a second press on the same session deletes it, and anything
    /// else disarms. Deleting the open session replaces it — a fresh session
    /// takes over before the old journal goes away — so it arms like any other
    /// row, only with wording that says so.
    fn handle_session_deletion(&mut self) -> Option<Command> {
        // A highlight left past the end of a shrunken palette arms nothing;
        // the next frame clamps it.
        let id = self.session_matches()?.get(self.suggestion_index)?.id;
        let open = id == self.session_id;
        if self.armed_session == Some(id) {
            self.disarm_session_deletion();
            return Some(if open {
                Command::ReplaceSession { delete: id }
            } else {
                Command::DeleteSession(id)
            });
        }
        self.armed_session = Some(id);
        self.session_notice = Some(if open {
            format!("Press ctrl+d again to delete the open session #{id} and start a fresh one")
        } else {
            format!("Press ctrl+d again to delete session #{id}")
        });
        None
    }

    fn disarm_session_deletion(&mut self) {
        self.armed_session = None;
        self.session_notice = None;
    }

    fn handle_input_key(&mut self, key: KeyEvent, suggestions: &[Suggestion]) -> Option<Command> {
        match key.code {
            KeyCode::Tab if !suggestions.is_empty() => {
                self.apply_suggestion(suggestions);
                None
            }
            KeyCode::BackTab | KeyCode::Tab => {
                self.focus = Focus::Conversation;
                self.selected_entry = self.entries.len().checked_sub(1);
                self.follow_conversation_tail = true;
                self.selection_drives_viewport = true;
                None
            }
            KeyCode::Up if !suggestions.is_empty() => {
                self.suggestion_index = self
                    .suggestion_index
                    .checked_sub(1)
                    .unwrap_or(suggestions.len() - 1);
                None
            }
            KeyCode::Down if !suggestions.is_empty() => {
                self.suggestion_index = (self.suggestion_index + 1) % suggestions.len();
                None
            }
            // Ctrl+J is \n (0x0A): in raw mode crossterm reports it as
            // Char('j') + CONTROL, so it can insert a literal newline while
            // Enter keeps submitting.
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input_history = None;
                self.input.insert('\n');
                self.suggestion_index = 0;
                None
            }
            KeyCode::Enter if !suggestions.is_empty() => {
                let selected = &suggestions[self.suggestion_index];
                let agents_pick = self.agents_palette_open();
                if self.input.text.trim_end() != selected.replacement.trim_end()
                    || selected.replacement.ends_with(' ')
                {
                    self.apply_suggestion(suggestions);
                    // Agent picks switch immediately; other palettes still
                    // require a second Enter so multi-step commands stay safe.
                    if agents_pick { self.submit() } else { None }
                } else {
                    self.submit()
                }
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Up => {
                if !self.input.move_vertical(-1, self.input_width) {
                    self.recall_older_input();
                }
                None
            }
            KeyCode::Down => {
                if !self.input.move_vertical(1, self.input_width) {
                    self.recall_newer_input();
                }
                None
            }
            KeyCode::Left => {
                self.input.cursor = self.input.cursor.saturating_sub(1);
                self.input.preferred_column = None;
                None
            }
            KeyCode::Right => {
                self.input.cursor = (self.input.cursor + 1).min(self.input.char_count());
                self.input.preferred_column = None;
                None
            }
            KeyCode::Home => {
                self.input.cursor = 0;
                self.input.preferred_column = None;
                None
            }
            KeyCode::End => {
                self.input.cursor = self.input.char_count();
                self.input.preferred_column = None;
                None
            }
            KeyCode::Backspace => {
                self.input_history = None;
                self.input.backspace();
                self.suggestion_index = 0;
                None
            }
            KeyCode::Delete => {
                self.input_history = None;
                self.input.delete();
                self.suggestion_index = 0;
                None
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.move_to_line_start();
                None
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.move_to_line_end();
                None
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input_history = None;
                self.input.delete_word_backward();
                self.suggestion_index = 0;
                None
            }
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.input_history = None;
                self.input.insert(character);
                self.suggestion_index = 0;
                None
            }
            _ => None,
        }
    }

    fn handle_conversation_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => self.focus = Focus::Input,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-8),
            KeyCode::PageDown => self.move_selection(8),
            KeyCode::Home => {
                self.selected_entry = (!self.entries.is_empty()).then_some(0);
                self.follow_conversation_tail = false;
                self.selection_drives_viewport = true;
            }
            KeyCode::End => {
                self.selected_entry = self.entries.len().checked_sub(1);
                self.follow_conversation_tail = true;
                self.selection_drives_viewport = true;
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_selected_from_keyboard(),
            _ => {}
        }
    }

    fn submit(&mut self) -> Option<Command> {
        let input = self.input.text.trim().to_owned();
        if input.is_empty() {
            return None;
        }
        if matches!(input.as_str(), "/exit" | "/quit") {
            self.should_exit = true;
            return None;
        }
        if input == "/agents" {
            self.open_agents_drawer();
            return None;
        }
        if let Some(address) = input.strip_prefix("/agents ").map(str::trim) {
            self.input = InputBuffer::default();
            self.input_history = None;
            self.suggestion_index = 0;
            if self.switch_agent(address) {
                self.status = self.agent_status(address);
            } else {
                self.push_error(
                    "Unknown agent",
                    format!("No agent conversation matches `{address}`."),
                );
            }
            return None;
        }
        if !input.starts_with('/') {
            // A plain message is never blocking: it is durably admitted as a
            // steer, delivered at the next model boundary if a run is active,
            // and starts a new run otherwise. Its row renders when the
            // journal commits the delivery.
            if self.interruption_in_progress {
                self.status = "Wait for the interruption to finish".to_owned();
                return None;
            }
            self.input = InputBuffer::default();
            self.input_history = None;
            self.suggestion_index = 0;
            if let Some(session) = self
                .sessions
                .iter_mut()
                .find(|session| session.id == self.session_id && session.preview.is_none())
            {
                session.preview = Some(input.clone());
            }
            self.with_agent("/root", |app| {
                app.status = "Sending…".to_owned();
            });
            // Optimistically show the steer the moment it is submitted; the
            // receipt attaches the durable identity (or the row is removed on
            // failure) so the steering area never waits on the journal write.
            self.pending_steers.push(PendingSteer {
                message_id: None,
                text: input.clone(),
            });
            return Some(Command::Message(input));
        }
        if self.busy {
            self.status = "Wait for the current operation to finish".to_owned();
            return None;
        }
        self.input = InputBuffer::default();
        self.input_history = None;
        self.suggestion_index = 0;
        self.switch_agent("/root");
        match input.as_str() {
            "/compact" => {
                self.begin_command("Compacting context…");
                Some(Command::Compact)
            }
            "/new" => {
                self.begin_command("Starting a new session…");
                Some(Command::New)
            }
            "/model" => {
                self.input = InputBuffer::at_end("/model ".to_owned());
                None
            }
            "/effort" => {
                self.input = InputBuffer::at_end("/effort ".to_owned());
                None
            }
            "/session" => {
                self.input = InputBuffer::at_end("/session ".to_owned());
                Some(Command::RefreshSessions)
            }
            _ if input.starts_with("/session ") => {
                let id = input
                    .trim_start_matches("/session ")
                    .trim()
                    .trim_start_matches('#');
                let Ok(id) = id.parse::<u64>() else {
                    self.push_error(
                        "Unknown session",
                        format!("No session in this directory matches `{id}`."),
                    );
                    return None;
                };
                if !self.sessions.iter().any(|session| session.id == id) {
                    self.push_error(
                        "Unknown session",
                        format!("No session in this directory matches `#{id}`."),
                    );
                    return None;
                }
                if id == self.session_id {
                    self.status = format!("Already using session #{id}");
                    return None;
                }
                self.begin_command(format!("Loading session #{id}…"));
                Some(Command::LoadSession(id))
            }
            _ if input.starts_with("/effort ") => {
                let effort = input.trim_start_matches("/effort ").trim();
                let model_index = self.selected_model;
                let Some(effort_index) = self.models[model_index]
                    .efforts
                    .iter()
                    .position(|configured| configured == effort)
                else {
                    self.push_error(
                        "Unknown effort",
                        format!(
                            "`{effort}` is not supported by {}.",
                            self.models[model_index].display_name
                        ),
                    );
                    return None;
                };
                if self.selected_efforts[model_index] == effort_index {
                    self.status = format!("Already using {effort} reasoning effort");
                    return None;
                }
                self.begin_command(format!("Switching reasoning effort to {effort}…"));
                Some(Command::SetEffort {
                    index: model_index,
                    effort: effort.to_owned(),
                })
            }
            _ if input.starts_with("/model ") => {
                let id = input.trim_start_matches("/model ").trim();
                let Some(index) = self.models.iter().position(|model| model.registry_id == id)
                else {
                    self.push_error(
                        "Unknown model",
                        format!("No configured model matches `{id}`."),
                    );
                    return None;
                };
                if index == self.selected_model {
                    self.status = format!("Already using {}", self.models[index].display_name);
                    return None;
                }
                self.begin_command(format!("Switching to {}…", self.models[index].display_name));
                Some(Command::SwitchModel {
                    index,
                    registry_id: self.models[index].registry_id.clone(),
                })
            }
            _ => {
                self.push_error(
                    "Unknown command",
                    format!("`{input}` is not a Lam command."),
                );
                None
            }
        }
    }

    fn begin_command(&mut self, status: impl Into<String>) {
        self.command_in_flight = true;
        self.status = status.into();
        self.recompute_busy();
    }

    fn finish_command(&mut self) {
        self.command_in_flight = false;
        self.recompute_busy();
    }

    /// `busy` gates blocking commands and drives the "working" title. It is
    /// event-driven but corrected by every journal fold, so a dropped event
    /// can only leave it stale until the next fold.
    fn recompute_busy(&mut self) {
        self.busy = self.command_in_flight || self.root_run_active || self.interruption_in_progress;
    }

    fn apply_suggestion(&mut self, suggestions: &[Suggestion]) {
        let replacement = suggestions[self.suggestion_index].replacement.clone();
        self.input = InputBuffer::at_end(replacement);
        self.input_history = None;
        self.suggestion_index = 0;
    }

    fn recall_older_input(&mut self) {
        if let Some(history) = self.input_history.as_mut() {
            if history.position > 0 {
                history.position -= 1;
                self.input = InputBuffer::at_end(history.messages[history.position].clone());
            }
            return;
        }

        let messages = self
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::User)
            .map(|entry| entry.body.clone())
            .collect::<Vec<_>>();
        let Some(position) = messages.len().checked_sub(1) else {
            return;
        };
        let draft = self.input.clone();
        self.input = InputBuffer::at_end(messages[position].clone());
        self.input_history = Some(InputHistory {
            messages,
            position,
            draft,
        });
    }

    fn recall_newer_input(&mut self) {
        let Some(mut history) = self.input_history.take() else {
            return;
        };
        if history.position + 1 < history.messages.len() {
            history.position += 1;
            self.input = InputBuffer::at_end(history.messages[history.position].clone());
            self.input_history = Some(history);
        } else {
            self.input = history.draft;
        }
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<String> {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.text_selection = None;
                self.scroll_conversation(-1);
                None
            }
            MouseEventKind::ScrollDown => {
                self.text_selection = None;
                self.scroll_conversation(1);
                None
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(area) = self.input_area
                    && mouse.row >= area.y
                    && mouse.row < area.y.saturating_add(area.height)
                    && mouse.column >= area.x
                {
                    self.text_selection = None;
                    self.focus = Focus::Input;
                    let row = usize::from(mouse.row - area.y);
                    let column = usize::from(mouse.column - area.x).saturating_sub(3);
                    self.input.cursor = self.input.cursor_at(self.input_width, row, column);
                    self.input.preferred_column = None;
                    self.suggestion_index = 0;
                    return None;
                }
                if let Some(hitbox) = self
                    .hitboxes
                    .iter()
                    .find(|hitbox| mouse.row >= hitbox.top && mouse.row <= hitbox.bottom)
                {
                    self.focus = Focus::Conversation;
                    self.selected_entry = Some(hitbox.entry);
                    // Only the header line toggles expand/collapse; clicking
                    // a body row selects the entry and arms text selection.
                    // Body clicks must not steal the scroll position: long
                    // expanded messages are often taller than the viewport,
                    // and follow-tail / selection-driven scrolling would jump
                    // to the end and make upward copy selections impossible.
                    if hitbox.header == Some(mouse.row) {
                        self.follow_conversation_tail = false;
                        self.selection_drives_viewport = true;
                        self.toggle_selected();
                        return None;
                    }
                    self.follow_conversation_tail = false;
                    self.selection_drives_viewport = false;
                }
                self.begin_selection(mouse.row, mouse.column);
                None
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(selection) = &mut self.text_selection
                    && let Some(area) = self.conversation_area
                {
                    selection.head = cell_pos(mouse.row, mouse.column, area);
                }
                None
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(selection) = self.text_selection.take()
                    && selection.anchor != selection.head
                {
                    return Some(selected_text(
                        &self.conversation_rows,
                        selection.anchor,
                        selection.head,
                    ));
                }
                None
            }
            _ => None,
        }
    }

    /// Arms a drag selection at the pressed cell, or does nothing when the
    /// press landed outside the conversation pane.
    fn begin_selection(&mut self, row: u16, column: u16) {
        let Some(area) = self.conversation_area else {
            return;
        };
        if row < area.y || row >= area.y.saturating_add(area.height) || column < area.x {
            return;
        }
        self.focus = Focus::Conversation;
        let position = cell_pos(row, column, area);
        self.text_selection = Some(TextSelection {
            anchor: position,
            head: position,
        });
    }

    /// Shows a transient status toast in the conversation pane corner.
    pub(crate) fn show_toast(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast {
            text: text.into(),
            deadline: TokioInstant::now() + TOAST_DURATION,
        });
    }

    pub(crate) fn handle_paste(&mut self, text: &str) {
        if self.focus == Focus::Input {
            self.input_history = None;
            self.input.insert_text(text);
            self.suggestion_index = 0;
        }
    }

    pub(crate) fn apply_agent_event(&mut self, event: AgentSystemEvent) -> bool {
        let picker_open = self.agents_palette_open();
        let before_other_running = self.other_running_agents();
        let affected = match event {
            AgentSystemEvent::Hosted { address, parent } => {
                let address = address.to_string();
                self.ensure_agent(
                    &address,
                    parent.map(|parent| parent.to_string()),
                    "Starting…",
                );
                self.with_agent(&address, |app| app.status = "Ready".to_owned());
                address
            }
            AgentSystemEvent::Retired { address, reason } => {
                let address = address.to_string();
                self.ensure_agent(&address, parent_address(&address), "Stopped");
                self.with_agent(&address, |app| {
                    app.set_current_run_active(false);
                    if reason == StopReason::Interrupted {
                        app.status = "Interrupted".to_owned();
                    } else {
                        app.push_entry(
                            EntryKind::System,
                            "Agent stopped",
                            format!("{address}: {reason:?}"),
                        );
                        app.status = "Stopped".to_owned();
                    }
                });
                address
            }
            AgentSystemEvent::Outcome { outcome } => {
                let address = outcome_address(&outcome).to_owned();
                self.ensure_agent(&address, parent_address(&address), "Finishing…");
                self.with_agent(&address, move |app| {
                    app.set_current_run_active(false);
                    app.apply_outcome(outcome);
                });
                address
            }
            AgentSystemEvent::ActorRuntime { address, event } => {
                let address = address.to_string();
                self.ensure_agent(&address, parent_address(&address), "Working…");
                let target = address.clone();
                self.with_agent(&target, move |app| {
                    app.apply_runtime_event(&address, event);
                });
                target
            }
            AgentSystemEvent::Run { address, event } => {
                let address = address.to_string();
                self.ensure_agent(&address, parent_address(&address), "Working…");
                let target = address.clone();
                self.with_agent(&target, move |app| app.apply_run_event(&address, event));
                target
            }
        };
        let after_other_running = self.other_running_agents();
        picker_open
            || affected == self.current_agent
            || before_other_running != after_other_running
            || self.agents_collapsed_visible()
    }

    fn apply_run_event(&mut self, address: &str, event: RunEvent) {
        if is_streaming_event(&event)
            && run_event_id(&event).is_some_and(|run_id| self.dead_runs.contains(run_id.as_str()))
        {
            return;
        }
        match event {
            RunEvent::Started { .. } => {
                self.status = format!("{address} is working…");
                self.set_current_run_active(true);
                if address == "/root" {
                    self.disarm_interruption();
                }
            }
            RunEvent::MessagesDelivered { .. } => {
                // The delivered rows render from the journal fold that
                // preceded this event; only the status is event-driven.
                self.status = format!("{address} is working…");
            }
            RunEvent::ModelStarted { .. } => {
                self.overlay_turn += 1;
                self.status = format!("{address} is thinking…");
            }
            RunEvent::ModelDelta { run_id, delta } => match delta {
                ModelDelta::Text(text) if text.is_empty() => {}
                ModelDelta::Text(text) => {
                    self.append_delta(EntryKind::Assistant, address, &run_id, text);
                    self.status = format!("{address} is responding…");
                }
                ModelDelta::Reasoning(text) if text.is_empty() => {}
                ModelDelta::Reasoning(text) => {
                    self.append_delta(EntryKind::Reasoning, address, &run_id, text);
                    self.status = format!("{address} is reasoning…");
                }
                ModelDelta::ToolCall(delta) => {
                    self.append_tool_delta(address, &run_id, delta);
                    self.status = format!("{address} is preparing a tool call…");
                }
            },
            RunEvent::ModelCompleted { metadata, .. } => {
                self.context_tokens = metadata.usage.map(|usage| usage.total_tokens);
            }
            RunEvent::EvalStarted { .. } => {
                // The model turn that requested this eval committed before
                // the event was emitted, so the fold that preceded this event
                // already rendered the committed tool-call row.
                self.status = format!("{address} is evaluating TypeScript…");
            }
            RunEvent::EvalCompleted { .. } => {
                // The durable eval outcome commits before this event is
                // emitted, so the fold that preceded it has already rendered
                // the committed result row.
                self.status = format!("{address} finished eval");
            }
            RunEvent::CompactionStarted { .. } => {
                self.status = format!("{address} is compacting context…");
            }
            RunEvent::CompactionCompleted { .. } => {
                // The compaction row renders from the journal fold.
                self.status = format!("{address} compacted context");
            }
            RunEvent::CompactionFailed { message, .. } => {
                self.push_error("Compaction failed", message);
            }
            RunEvent::Completed { run_id } => {
                self.clear_streaming(&run_id);
                self.status = if address == "/root" {
                    "Ready".to_owned()
                } else {
                    "Complete".to_owned()
                };
                self.set_current_run_active(false);
                if address == "/root" {
                    self.disarm_interruption();
                }
            }
            RunEvent::Failed { message } => {
                tracing::error!(
                    target: "lam_tui::runtime",
                    event = "tui.run_failed",
                    actor_id = address,
                    "agent run failed"
                );
                self.push_error("Run failed", message);
                self.set_current_run_active(false);
                if address == "/root" {
                    self.disarm_interruption();
                }
            }
        }
    }

    fn apply_runtime_event(&mut self, address: &str, event: RuntimeEvent) {
        match event {
            RuntimeEvent::RuntimeResumed { .. } => self.push_entry(
                EntryKind::System,
                "Runtime resumed",
                format!("{address} resumed with a fresh TypeScript isolate."),
            ),
            RuntimeEvent::CompactionFailed { message, .. } => {
                self.push_error("Compaction failed", message);
            }
            RuntimeEvent::CompactionStarted { .. } | RuntimeEvent::CompactionCompleted { .. } => {}
        }
    }

    fn apply_outcome(&mut self, outcome: AgentOutcome) {
        match outcome {
            AgentOutcome::Completed { .. } => {
                // The terminal output renders from the journal fold.
                self.status = "Complete".to_owned();
            }
            AgentOutcome::Failed { address, error, .. } => {
                self.push_error(format!("{address} failed"), error);
            }
            AgentOutcome::Cancelled {
                address, reason, ..
            } => {
                if reason.as_deref() == Some("the actor run was interrupted") {
                    self.status = "Interrupted".to_owned();
                } else {
                    self.push_error(
                        format!("{address} cancelled"),
                        reason.unwrap_or_else(|| "No reason was reported.".to_owned()),
                    );
                }
            }
        }
    }

    /// Applies one incremental journal fold to the owning agent's view. The
    /// fold's rows are the only path by which transcript content enters the
    /// view; events and command results carry state transitions only.
    pub(crate) fn apply_fold(&mut self, address: &str, outcome: FoldOutcome) -> bool {
        let before_other_running = self.other_running_agents();
        let affected = !outcome.rows.is_empty() && address == self.current_agent;
        if address == "/root" && !outcome.consumed_message_ids.is_empty() {
            self.pending_steers.retain(|steer| {
                !outcome.consumed_message_ids.iter().any(|consumed| {
                    steer
                        .message_id
                        .as_ref()
                        .is_some_and(|message_id| message_id == consumed)
                })
            });
        }
        self.ensure_agent(address, parent_address(address), "Working…");
        let owner = address.to_owned();
        let context_tokens = outcome.context_tokens;
        let run_active = outcome.active_run.is_some();
        self.with_agent(address, move |app| {
            app.set_current_run_active(run_active);
            // A fold with no new model response reports `None`; keep the
            // last known provider usage (e.g. the value seeded from the
            // journal at startup) rather than clearing it.
            if let Some(context_tokens) = context_tokens {
                app.context_tokens = Some(context_tokens);
            }
            if let Some(model) = outcome.selected_model {
                app.current_model = Some(model);
            }
            let committed_runs = outcome
                .rows
                .iter()
                .filter_map(|row| row.run_id.clone())
                .collect::<BTreeSet<_>>();
            app.apply_committed_rows(&owner, outcome.rows);
            for run_id in &outcome.model_turns {
                app.pop_overlay_model_segment(run_id);
            }
            for run_id in outcome.dead_runs {
                app.purge_overlay_run(&run_id);
                app.clear_streaming_run(&run_id);
                app.dead_runs.insert(run_id);
            }
            // Keep the newest committed row of the still-active run at full
            // intensity, exactly as if it were streaming: the run's cursor
            // is there.
            if let Some(active) = &outcome.active_run
                && committed_runs.contains(active)
            {
                app.clear_streaming_run(active);
                if let Some(entry) = app.entries[..app.committed_len]
                    .iter_mut()
                    .rev()
                    .find(|entry| entry.run_id() == Some(active.as_str()))
                {
                    entry.streaming = true;
                }
            }
            if outcome.interrupted {
                app.status = "Interrupted".to_owned();
            }
        });
        let after_other_running = self.other_running_agents();
        affected
            || self.agents_palette_open()
            || before_other_running != after_other_running
            || self.agents_collapsed_visible()
    }

    /// Reports a failed transcript fold. The view keeps rendering; the next
    /// successful fold catches up from the projector's revision.
    pub(crate) fn fold_failed(&mut self, address: &str, error: String) {
        self.push_error(format!("Transcript sync failed for {address}"), error);
    }

    /// Registers the durable receipt for a sent message. `consumed` reports
    /// whether the projector has already folded the message into context —
    /// in that case its row is already rendered and nothing is pending.
    pub(crate) fn apply_message_receipt(&mut self, sent: SentMessage, consumed: bool) {
        // Attach the durable identity to the optimistic row submitted above,
        // or drop the row when the delivery already committed into context.
        if let Some(index) = self
            .pending_steers
            .iter()
            .position(|steer| steer.message_id.is_none() && steer.text == sent.text)
        {
            if consumed {
                self.pending_steers.remove(index);
            } else {
                self.pending_steers[index].message_id = Some(sent.message_id);
            }
        } else if !consumed {
            // No optimistic row (e.g. a receipt without a prior submit);
            // register the pending steer directly.
            self.pending_steers.push(PendingSteer {
                message_id: Some(sent.message_id),
                text: sent.text,
            });
        }
        self.with_agent("/root", |app| {
            if !app.root_run_active {
                return;
            }
            app.status = "Message queued — delivered at the next boundary".to_owned();
        });
    }

    pub(crate) fn apply_message_error(&mut self, error: String) {
        // A rejected send removes the optimistic row and restores the draft
        // so the user's text is never lost.
        if let Some(index) = self
            .pending_steers
            .iter()
            .position(|steer| steer.message_id.is_none())
        {
            let steer = self.pending_steers.remove(index);
            self.input = InputBuffer::at_end(steer.text);
            self.input_history = None;
            self.suggestion_index = 0;
        }
        self.push_error("Could not send message", error);
    }

    pub(crate) fn apply_interrupt_result(&mut self, result: Result<bool, String>) {
        self.interruption_in_progress = false;
        self.disarm_interruption();
        self.recompute_busy();
        match result {
            Ok(_) => {
                self.with_agent("/root", |app| {
                    app.status = if app.root_run_active {
                        "/root is working…".to_owned()
                    } else {
                        "Ready".to_owned()
                    };
                });
            }
            Err(error) => self.push_error("Could not stop run", error),
        }
    }

    pub(crate) fn apply_command_result(&mut self, result: CommandResult) {
        let current = self.current_agent.clone();
        self.switch_agent("/root");
        match result {
            CommandResult::Message(Ok(sent)) => {
                // Main folds the root projector before routing here, so a
                // missing consumed check can only under-report; the pending
                // row is then cleared by the delivery fold.
                self.apply_message_receipt(sent, false);
            }
            CommandResult::Message(Err(error)) => self.apply_message_error(error),
            CommandResult::Interrupt(result) => self.apply_interrupt_result(result),
            CommandResult::Compact(Ok(message)) => {
                self.finish_command();
                self.push_entry(EntryKind::System, "Compact", message);
                self.status = "Ready".to_owned();
            }
            CommandResult::Compact(Err(error)) => {
                self.finish_command();
                self.push_error("Compaction failed", error);
            }
            CommandResult::SwitchModel {
                index,
                result: Ok(message),
            } => {
                self.finish_command();
                self.selected_model = index;
                self.current_model = Some(self.models[index].registry_id.clone());
                self.push_entry(EntryKind::System, "Model", message);
                self.status = "Ready".to_owned();
            }
            CommandResult::SwitchModel {
                result: Err(error), ..
            } => {
                self.finish_command();
                self.push_error("Model switch failed", error);
            }
            CommandResult::SetEffort {
                index,
                effort,
                result: Ok(message),
            } => {
                self.finish_command();
                let effort_index = self.models[index]
                    .efforts
                    .iter()
                    .position(|configured| configured == &effort)
                    .expect("runtime returns the requested configured effort");
                self.selected_efforts[index] = effort_index;
                self.push_entry(EntryKind::System, "Effort", message);
                self.status = "Ready".to_owned();
            }
            CommandResult::SetEffort {
                result: Err(error), ..
            } => {
                self.finish_command();
                self.push_error("Effort switch failed", error);
            }
        }
        if current != "/root" {
            self.switch_agent(&current);
        }
    }

    /// Inserts committed rows at the pinned boundary, pairing each committed
    /// eval result with the first pending committed tool call of its run.
    fn apply_committed_rows(&mut self, owner: &str, rows: Vec<CommittedRow>) {
        for row in rows {
            let mut entry = committed_entry(row);
            match entry.kind {
                EntryKind::Assistant | EntryKind::Reasoning => {
                    entry.model_owner = Some(owner.to_owned());
                }
                EntryKind::ToolCall | EntryKind::ToolResult => {
                    entry.tool_owner = Some(owner.to_owned());
                }
                EntryKind::User | EntryKind::System | EntryKind::Error => {}
            }
            if entry.kind == EntryKind::ToolResult
                && let Some(run_id) = entry.tool_run.clone()
                && let Some(pending) = self.entries[..self.committed_len].iter_mut().find(|entry| {
                    entry.kind == EntryKind::ToolCall
                        && entry.pending_tool
                        && entry.tool_run.as_deref() == Some(run_id.as_str())
                })
            {
                pending.pending_tool = false;
            }
            self.insert_pinned(entry);
        }
    }

    /// Removes the oldest streaming overlay segment for the run: a committed
    /// model turn has replaced it with authoritative rows.
    fn pop_overlay_model_segment(&mut self, run_id: &str) {
        let oldest_turn = self.entries[self.committed_len..]
            .iter()
            .filter(|entry| entry.run_id() == Some(run_id) && entry.kind != EntryKind::ToolResult)
            .filter_map(|entry| entry.overlay_turn)
            .min();
        let Some(turn) = oldest_turn else {
            return;
        };
        self.remove_overlay_rows(|entry| {
            entry.run_id() == Some(run_id)
                && entry.kind != EntryKind::ToolResult
                && entry.overlay_turn == Some(turn)
        });
    }

    /// Drops every overlay row for a run that reached a terminal or
    /// interrupted entry.
    fn purge_overlay_run(&mut self, run_id: &str) {
        self.remove_overlay_rows(|entry| entry.run_id() == Some(run_id));
    }

    fn remove_overlay_rows(&mut self, mut retire: impl FnMut(&ConversationEntry) -> bool) {
        let mut index = self.committed_len;
        while index < self.entries.len() {
            if retire(&self.entries[index]) {
                self.remove_entry(index);
            } else {
                index += 1;
            }
        }
    }

    fn remove_entry(&mut self, index: usize) {
        self.entries.remove(index);
        if index < self.committed_len {
            self.committed_len -= 1;
        }
        if let Some(selected) = self.selected_entry {
            self.selected_entry = if self.entries.is_empty() {
                None
            } else if selected > index {
                Some(selected - 1)
            } else {
                Some(selected.min(self.entries.len() - 1))
            };
        }
    }

    /// Inserts a row at the pinned boundary, before any streaming overlay
    /// rows, keeping pinned rows in arrival order.
    fn insert_pinned(&mut self, entry: ConversationEntry) {
        let index = self.committed_len;
        self.entries.insert(index, entry);
        self.committed_len += 1;
        if let Some(selected) = self.selected_entry
            && selected >= index
        {
            self.selected_entry = Some(selected + 1);
        }
        if self.focus == Focus::Input || self.follow_conversation_tail {
            self.selected_entry = self.entries.len().checked_sub(1);
        }
    }

    fn append_delta(&mut self, kind: EntryKind, address: &str, run_id: &RunId, delta: String) {
        let title = if kind == EntryKind::Reasoning {
            format!("{address} · reasoning")
        } else {
            address.to_owned()
        };
        let turn = self.overlay_turn;
        if let Some(entry) = self.entries.last_mut()
            && entry.kind == kind
            && entry.title == title
            && entry.model_owner.as_deref() == Some(address)
            && entry.model_run.as_deref() == Some(run_id.as_str())
            && entry.overlay_turn == Some(turn)
        {
            entry.body.push_str(&delta);
            if kind == EntryKind::Assistant {
                entry.expanded = true;
            }
            entry.streaming = true;
        } else {
            self.push_overlay_entry_with_expansion(
                kind,
                title,
                delta,
                kind == EntryKind::Assistant,
            );
            if let Some(entry) = self.entries.last_mut() {
                entry.model_owner = Some(address.to_owned());
                entry.model_run = Some(run_id.to_string());
                entry.overlay_turn = Some(turn);
            }
            self.mark_last_streaming(run_id);
        }
    }

    /// Clears the streaming marker from every row owned by the run. Each
    /// run keeps at most one streaming row: the row it is actively writing.
    fn clear_streaming(&mut self, run_id: &RunId) {
        self.clear_streaming_run(run_id.as_str());
    }

    fn clear_streaming_run(&mut self, run_id: &str) {
        for entry in &mut self.entries {
            if entry.model_run.as_deref() == Some(run_id)
                || entry.tool_run.as_deref() == Some(run_id)
            {
                entry.streaming = false;
            }
        }
    }

    /// Moves the run's streaming marker to its newest row, dimming the row
    /// the cursor just left.
    fn mark_last_streaming(&mut self, run_id: &RunId) {
        self.clear_streaming(run_id);
        if let Some(entry) = self.entries.last_mut() {
            entry.streaming = true;
        }
    }

    fn append_tool_delta(&mut self, address: &str, run_id: &RunId, delta: ToolCallDelta) {
        let turn = self.overlay_turn;
        let existing = self
            .entries
            .iter()
            .enumerate()
            .skip(self.committed_len)
            .rev()
            .find(|(_, entry)| {
                entry.kind == EntryKind::ToolCall
                    && entry.pending_tool
                    && entry.tool_owner.as_deref() == Some(address)
                    && entry.tool_run.as_deref() == Some(run_id.as_str())
                    && entry.tool_index == Some(delta.index)
                    && entry.overlay_turn == Some(turn)
            })
            .map(|(index, _)| index);
        if let Some(index) = existing {
            self.clear_streaming(run_id);
            let entry = &mut self.entries[index];
            entry.streaming = true;
            if let Some(name) = delta.name {
                entry.tool_name.push_str(&name);
                entry.title = format!("{address} · {}", entry.tool_name);
            }
            entry.body.push_str(&delta.arguments);
            update_streamed_eval_title(entry, address);
            return;
        }

        let name = delta.name.unwrap_or_default();
        let title = if name.is_empty() {
            format!("{address} · tool call {}", delta.index + 1)
        } else {
            format!("{address} · {name}")
        };
        let turn = self.overlay_turn;
        self.push_overlay_entry(EntryKind::ToolCall, title, delta.arguments);
        if let Some(entry) = self.entries.last_mut() {
            entry.pending_tool = true;
            entry.tool_owner = Some(address.to_owned());
            entry.tool_run = Some(run_id.to_string());
            entry.tool_index = Some(delta.index);
            entry.tool_name = name;
            entry.overlay_turn = Some(turn);
            update_streamed_eval_title(entry, address);
        }
        self.mark_last_streaming(run_id);
    }

    fn push_error(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.push_entry(EntryKind::Error, title, body);
        self.status = "Error".to_owned();
    }

    /// Pins a locally produced row (system notice, error) at the committed
    /// boundary so it keeps its chronological position as later journal rows
    /// and streaming rows arrive.
    fn push_entry(&mut self, kind: EntryKind, title: impl Into<String>, body: impl Into<String>) {
        self.insert_pinned(pinned_entry(kind, title.into(), body.into()));
    }

    fn push_overlay_entry(
        &mut self,
        kind: EntryKind,
        title: impl Into<String>,
        body: impl Into<String>,
    ) {
        self.push_overlay_entry_with_expansion(kind, title, body, false);
    }

    fn push_overlay_entry_with_expansion(
        &mut self,
        kind: EntryKind,
        title: impl Into<String>,
        body: impl Into<String>,
        expanded: bool,
    ) {
        let mut entry = pinned_entry(kind, title.into(), body.into());
        entry.expanded = expanded;
        self.entries.push(entry);
        if self.focus == Focus::Input || self.follow_conversation_tail {
            self.selected_entry = self.entries.len().checked_sub(1);
        }
    }

    fn move_selection(&mut self, amount: isize) {
        self.selection_drives_viewport = true;
        if self.entries.is_empty() {
            self.selected_entry = None;
            return;
        }
        let current = self.selected_entry.unwrap_or(self.entries.len() - 1);
        let selected = current
            .saturating_add_signed(amount)
            .min(self.entries.len() - 1);
        self.selected_entry = Some(selected);
        self.follow_conversation_tail = selected + 1 == self.entries.len() && amount > 0;
    }

    fn scroll_conversation(&mut self, direction: isize) {
        self.focus = Focus::Conversation;
        self.selection_drives_viewport = false;
        let viewport = self.conversation_viewport_height.max(1);
        let maximum = self.conversation_total_lines.saturating_sub(viewport);
        self.conversation_offset = if direction < 0 {
            self.conversation_offset.saturating_sub(MOUSE_SCROLL_LINES)
        } else {
            self.conversation_offset
                .saturating_add(MOUSE_SCROLL_LINES)
                .min(maximum)
        };
        self.follow_conversation_tail = direction > 0 && self.conversation_offset == maximum;
        self.keep_selection_in_view(direction, viewport);
    }

    fn keep_selection_in_view(&mut self, direction: isize, viewport: usize) {
        let visible_start = self.conversation_offset;
        let visible_end = visible_start.saturating_add(viewport);
        let is_visible = |index: usize| {
            self.conversation_ranges
                .get(index)
                .is_some_and(|(start, end)| *end >= visible_start && *start < visible_end)
        };
        if self.selected_entry.is_some_and(is_visible) {
            return;
        }
        let mut visible = self
            .conversation_ranges
            .iter()
            .enumerate()
            .filter(|(_, (start, end))| *end >= visible_start && *start < visible_end)
            .map(|(index, _)| index);
        self.selected_entry = if direction < 0 {
            visible.next_back()
        } else {
            visible.next()
        };
    }

    fn toggle_selected(&mut self) {
        self.follow_conversation_tail = false;
        if let Some(index) = self.selected_entry
            && let Some(entry) = self.entries.get_mut(index)
        {
            entry.expanded = !entry.expanded;
        }
    }

    /// Keyboard expand/collapse. Expanding reveals the entry for reading:
    /// the whole entry if it fits, otherwise the header near the top of the
    /// viewport. Mouse expand leaves scroll alone.
    fn toggle_selected_from_keyboard(&mut self) {
        self.follow_conversation_tail = false;
        self.selection_drives_viewport = false;
        let Some(index) = self.selected_entry else {
            return;
        };
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        let expanding = !entry.expanded;
        entry.expanded = !entry.expanded;
        if expanding {
            self.reveal_entry_top = Some(index);
        }
    }

    /// Hierarchical agent list for the drawer: DFS from `/root`, with siblings
    /// ordered newest-created first (session create order, not last activity).
    fn agent_addresses(&self) -> Vec<String> {
        let mut nodes: BTreeMap<String, (Option<String>, u64)> = BTreeMap::new();
        for (address, agent) in &self.inactive_agents {
            nodes.insert(address.clone(), (agent.parent.clone(), agent.created_ord));
        }
        nodes.insert(
            self.current_agent.clone(),
            (self.current_parent.clone(), self.current_created_ord),
        );

        let mut children: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
        let mut roots = Vec::new();
        for (address, (parent, ord)) in &nodes {
            match parent {
                Some(parent) if nodes.contains_key(parent) => {
                    children
                        .entry(parent.clone())
                        .or_default()
                        .push((*ord, address.clone()));
                }
                _ => roots.push((*ord, address.clone())),
            }
        }
        for siblings in children.values_mut() {
            siblings.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        }
        roots.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));

        fn dfs(
            address: &str,
            children: &BTreeMap<String, Vec<(u64, String)>>,
            ordered: &mut Vec<String>,
        ) {
            ordered.push(address.to_owned());
            if let Some(kids) = children.get(address) {
                for (_, child) in kids {
                    dfs(child, children, ordered);
                }
            }
        }

        let mut ordered = Vec::with_capacity(nodes.len());
        if nodes.contains_key("/root") {
            dfs("/root", &children, &mut ordered);
        }
        for (_, address) in roots {
            if !ordered.iter().any(|seen| seen == &address) {
                dfs(&address, &children, &mut ordered);
            }
        }
        ordered
    }

    /// Whether the input bar is currently driving the expanded agents drawer.
    pub(crate) fn agents_palette_open(&self) -> bool {
        let input = self.input.text.trim_start();
        input == "/agents" || input.starts_with("/agents ")
    }

    /// Focus the input and open the expanded agents drawer (`/agents `).
    pub(crate) fn open_agents_drawer(&mut self) {
        self.focus = Focus::Input;
        self.input = InputBuffer::at_end("/agents ".to_owned());
        self.input_history = None;
        self.suggestion_index = 0;
    }

    /// Ground-truth live-run flag for one agent in the local session tree.
    pub(crate) fn agent_is_running(&self, address: &str) -> bool {
        if address == "/root" {
            // Always authoritative for busy gating and interruption.
            return self.root_run_active;
        }
        if address == self.current_agent {
            self.current_run_active
        } else {
            self.inactive_agents
                .get(address)
                .is_some_and(|agent| agent.run_active)
        }
    }

    /// Agents with a live run, in lexical address order.
    pub(crate) fn running_agent_addresses(&self) -> Vec<String> {
        self.agent_addresses()
            .into_iter()
            .filter(|address| self.agent_is_running(address))
            .collect()
    }

    /// Live runs other than the agent currently being viewed — the collapsed
    /// ambient strip only cares about work happening "elsewhere".
    pub(crate) fn other_running_agents(&self) -> Vec<String> {
        self.running_agent_addresses()
            .into_iter()
            .filter(|address| address != &self.current_agent)
            .collect()
    }

    /// Collapsed one-line agents strip above the message shelf.
    pub(crate) fn agents_collapsed_visible(&self) -> bool {
        !self.agents_palette_open() && !self.other_running_agents().is_empty()
    }

    /// Any agents surface currently painted above the message shelf.
    pub(crate) fn agents_drawer_visible(&self) -> bool {
        self.agents_collapsed_visible() || self.agents_palette_open()
    }

    /// Keep the frame clock alive while a live agents surface needs a spinner.
    /// Independent of keystrokes: collapsed strip and expanded /agents both pulse.
    pub(crate) fn agents_drawer_animates(&self) -> bool {
        self.agents_drawer_visible()
            && self
                .agent_addresses()
                .iter()
                .any(|address| self.agent_is_running(address))
    }

    /// One-line summary for the collapsed ambient strip.
    pub(crate) fn agents_collapsed_summary(&self, width: usize) -> String {
        let running = self.other_running_agents();
        let count = running.len();
        let noun = if count == 1 { "agent" } else { "agents" };
        let prefix = format!("{count} {noun} running");
        if width <= prefix.len() {
            return elide_end_plain(&prefix, width);
        }
        if running.is_empty() {
            return prefix;
        }
        // Fit as many short names as possible after the count.
        let mut line = prefix;
        line.push_str(" · ");
        let mut first = true;
        for address in &running {
            let name = address.rsplit('/').next().unwrap_or(address.as_str());
            let piece = if first {
                name.to_owned()
            } else {
                format!(", {name}")
            };
            if line.len() + piece.len() > width.saturating_sub(1) {
                if !line.ends_with('…') {
                    // Prefer an ellipsis over a hard clip mid-name.
                    while line.len() >= width {
                        line.pop();
                    }
                    if !line.ends_with('…') {
                        if line.len() < width {
                            line.push('…');
                        } else if !line.is_empty() {
                            line.pop();
                            line.push('…');
                        }
                    }
                }
                break;
            }
            line.push_str(&piece);
            first = false;
        }
        line
    }

    fn set_current_run_active(&mut self, active: bool) {
        self.current_run_active = active;
        if self.current_agent == "/root" {
            self.root_run_active = active;
            self.recompute_busy();
        }
    }

    fn agent_detail(&self, address: &str) -> String {
        let (status, model) = if address == "/root" {
            let status = if address == self.current_agent {
                &self.status
            } else if let Some(root) = self.inactive_agents.get(address) {
                &root.status
            } else {
                return "unknown".to_owned();
            };
            (status, Some(self.selected_model().registry_id.as_str()))
        } else if address == self.current_agent {
            (&self.status, self.current_model.as_deref())
        } else if let Some(agent) = self.inactive_agents.get(address) {
            (&agent.status, agent.model.as_deref())
        } else {
            return "unknown".to_owned();
        };
        model.map_or_else(|| status.clone(), |model| format!("{status} · {model}"))
    }

    fn agent_status(&self, address: &str) -> String {
        if address == self.current_agent {
            self.status.clone()
        } else {
            self.inactive_agents
                .get(address)
                .map_or_else(|| "Unknown".to_owned(), |agent| agent.status.clone())
        }
    }

    fn ensure_agent(&mut self, address: &str, parent: Option<String>, status: &str) {
        if address == self.current_agent {
            if self.current_parent.is_none() {
                self.current_parent = parent;
            }
            return;
        }
        if self.inactive_agents.contains_key(address) {
            return;
        }
        let created_ord = self.alloc_created_ord();
        self.inactive_agents.insert(
            address.to_owned(),
            AgentConversation::empty(parent, status, created_ord),
        );
    }

    fn alloc_created_ord(&mut self) -> u64 {
        let ord = self.next_agent_ord;
        self.next_agent_ord = self.next_agent_ord.saturating_add(1);
        ord
    }

    fn switch_agent(&mut self, address: &str) -> bool {
        if address == self.current_agent {
            return true;
        }
        let Some(next) = self.inactive_agents.remove(address) else {
            return false;
        };
        let previous_run_active = if self.current_agent == "/root" {
            self.root_run_active
        } else {
            self.current_run_active
        };
        let previous = AgentConversation {
            entries: std::mem::take(&mut self.entries),
            selected_entry: self.selected_entry,
            conversation_offset: self.conversation_offset,
            follow_conversation_tail: self.follow_conversation_tail,
            selection_drives_viewport: self.selection_drives_viewport,
            reveal_entry_top: self.reveal_entry_top.take(),
            status: std::mem::take(&mut self.status),
            context_tokens: self.context_tokens,
            parent: self.current_parent.take(),
            model: self.current_model.take(),
            run_active: previous_run_active,
            created_ord: self.current_created_ord,
            committed_len: self.committed_len,
            overlay_turn: self.overlay_turn,
            dead_runs: std::mem::take(&mut self.dead_runs),
        };
        let previous_address = std::mem::replace(&mut self.current_agent, address.to_owned());
        self.inactive_agents.insert(previous_address, previous);
        self.entries = next.entries;
        self.selected_entry = next.selected_entry;
        self.conversation_offset = next.conversation_offset;
        self.follow_conversation_tail = next.follow_conversation_tail;
        self.selection_drives_viewport = next.selection_drives_viewport;
        self.reveal_entry_top = next.reveal_entry_top;
        self.status = next.status;
        self.context_tokens = next.context_tokens;
        self.current_parent = next.parent;
        self.current_model = next.model;
        self.current_run_active = next.run_active;
        self.current_created_ord = next.created_ord;
        if address == "/root" {
            self.root_run_active = next.run_active;
            self.recompute_busy();
        }
        self.committed_len = next.committed_len;
        self.overlay_turn = next.overlay_turn;
        self.dead_runs = next.dead_runs;
        true
    }

    fn with_agent<T>(&mut self, address: &str, update: impl FnOnce(&mut Self) -> T) -> T {
        if address == self.current_agent {
            return update(self);
        }
        let previous = self.current_agent.clone();
        let switched = self.switch_agent(address);
        debug_assert!(switched, "agent must be ensured before routing an event");
        let result = update(self);
        let restored = self.switch_agent(&previous);
        debug_assert!(
            restored,
            "the previously selected agent must remain available"
        );
        result
    }
}

fn one_line_preview(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn elide_end_plain(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let mut result: String = text.chars().take(width.saturating_sub(1)).collect();
    result.push('…');
    result
}

impl AgentConversation {
    fn empty(parent: Option<String>, status: &str, created_ord: u64) -> Self {
        Self {
            entries: Vec::new(),
            selected_entry: None,
            conversation_offset: 0,
            follow_conversation_tail: true,
            selection_drives_viewport: true,
            reveal_entry_top: None,
            status: status.to_owned(),
            context_tokens: None,
            parent,
            model: None,
            run_active: false,
            created_ord,
            committed_len: 0,
            overlay_turn: 0,
            dead_runs: BTreeSet::new(),
        }
    }

    fn from_history(history: AgentHistory, created_ord: u64) -> Self {
        let entries = conversation_from_rows(history.history);
        Self {
            selected_entry: entries.len().checked_sub(1),
            committed_len: entries.len(),
            entries,
            conversation_offset: 0,
            follow_conversation_tail: true,
            selection_drives_viewport: true,
            reveal_entry_top: None,
            status: history.status,
            context_tokens: history.context_tokens,
            parent: history.parent,
            model: history.model,
            run_active: !history.run_completed,
            created_ord,
            overlay_turn: 0,
            dead_runs: BTreeSet::new(),
        }
    }
}

/// Renders committed rows into conversation entries, pairing each committed
/// eval result with the first pending committed tool call of its run.
fn conversation_from_rows(rows: Vec<CommittedRow>) -> Vec<ConversationEntry> {
    let mut entries: Vec<ConversationEntry> = Vec::with_capacity(rows.len());
    for row in rows {
        let entry = committed_entry(row);
        if entry.kind == EntryKind::ToolResult
            && let Some(run_id) = entry.tool_run.as_deref()
            && let Some(pending) = entries.iter_mut().find(|entry| {
                entry.kind == EntryKind::ToolCall
                    && entry.pending_tool
                    && entry.tool_run.as_deref() == Some(run_id)
            })
        {
            pending.pending_tool = false;
        }
        entries.push(entry);
    }
    entries
}

fn parent_address(address: &str) -> Option<String> {
    address
        .rfind('/')
        .filter(|separator| *separator > 0)
        .map(|separator| address[..separator].to_owned())
}

fn outcome_address(outcome: &AgentOutcome) -> &str {
    match outcome {
        AgentOutcome::Completed { address, .. }
        | AgentOutcome::Failed { address, .. }
        | AgentOutcome::Cancelled { address, .. } => address.as_str(),
    }
}

fn run_event_id(event: &RunEvent) -> Option<&RunId> {
    match event {
        RunEvent::Started { run_id }
        | RunEvent::MessagesDelivered { run_id, .. }
        | RunEvent::ModelStarted { run_id }
        | RunEvent::ModelDelta { run_id, .. }
        | RunEvent::ModelCompleted { run_id, .. }
        | RunEvent::CompactionStarted { run_id, .. }
        | RunEvent::CompactionCompleted { run_id, .. }
        | RunEvent::CompactionFailed { run_id, .. }
        | RunEvent::EvalStarted { run_id, .. }
        | RunEvent::EvalCompleted { run_id, .. }
        | RunEvent::Completed { run_id } => Some(run_id),
        RunEvent::Failed { .. } => None,
    }
}

fn agent_tree_label(address: &str, current: bool, running: bool) -> String {
    let depth = address.matches('/').count().saturating_sub(1);
    let name = address.rsplit('/').next().unwrap_or(address);
    let name = if current {
        format!("[{name}]")
    } else {
        name.to_owned()
    };
    // Running agents get a live marker; the palette may animate it.
    let glyph = if running { "●" } else { " " };
    if depth == 0 {
        return format!("◉{glyph}{name}");
    }
    format!("{}└─{glyph}{name}", "  ".repeat(depth))
}

fn pinned_entry(kind: EntryKind, title: String, body: String) -> ConversationEntry {
    ConversationEntry {
        kind,
        title,
        body,
        expanded: false,
        pending_tool: false,
        streaming: false,
        model_owner: None,
        model_run: None,
        tool_owner: None,
        tool_run: None,
        tool_index: None,
        tool_name: String::new(),
        overlay_turn: None,
        layout: None,
    }
}

fn committed_entry(row: CommittedRow) -> ConversationEntry {
    let CommittedRow { entry, run_id } = row;
    let kind = match entry.kind {
        HistoryKind::User => EntryKind::User,
        HistoryKind::Assistant => EntryKind::Assistant,
        HistoryKind::Reasoning => EntryKind::Reasoning,
        HistoryKind::ToolCall => EntryKind::ToolCall,
        HistoryKind::ToolResult => EntryKind::ToolResult,
        HistoryKind::System => EntryKind::System,
    };
    let mut converted = pinned_entry(kind, entry.title, entry.body);
    converted.expanded = matches!(kind, EntryKind::User | EntryKind::Assistant);
    match kind {
        EntryKind::Assistant | EntryKind::Reasoning => converted.model_run = run_id,
        EntryKind::ToolCall => {
            converted.pending_tool = true;
            converted.tool_run = run_id;
        }
        EntryKind::ToolResult => converted.tool_run = run_id,
        EntryKind::User | EntryKind::System | EntryKind::Error => {}
    }
    converted
}

fn is_streaming_event(event: &RunEvent) -> bool {
    !matches!(event, RunEvent::Completed { .. } | RunEvent::Failed { .. })
}

const STREAMED_INTENT_SCAN_LIMIT: usize = 16 * 1024;
const STREAMED_INTENT_MAX_CHARS: usize = 120;

struct StreamedIntent {
    text: String,
    complete: bool,
}

fn update_streamed_eval_title(entry: &mut ConversationEntry, address: &str) {
    if entry.tool_name != "eval" {
        return;
    }
    let Some(intent) = partial_eval_intent(&entry.body) else {
        return;
    };
    let raw = if intent.complete {
        intent.text.trim()
    } else {
        intent.text.trim_start()
    };
    let text = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(STREAMED_INTENT_MAX_CHARS)
        .collect::<String>();
    if !text.is_empty() {
        entry.title = format!("{address} · {text}");
    }
}

fn partial_eval_intent(arguments: &str) -> Option<StreamedIntent> {
    if arguments.len() > STREAMED_INTENT_SCAN_LIMIT {
        return None;
    }
    let bytes = arguments.as_bytes();
    let mut cursor = skip_json_whitespace(bytes, 0);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    cursor += 1;

    loop {
        cursor = skip_json_whitespace(bytes, cursor);
        if bytes.get(cursor) == Some(&b',') {
            cursor += 1;
            cursor = skip_json_whitespace(bytes, cursor);
        }
        if bytes.get(cursor) == Some(&b'}') {
            return None;
        }
        if bytes.get(cursor) != Some(&b'"') {
            return None;
        }
        let key_end = complete_json_string_end(bytes, cursor)?;
        let key = serde_json::from_str::<String>(&arguments[cursor..=key_end]).ok()?;
        cursor = skip_json_whitespace(bytes, key_end + 1);
        if bytes.get(cursor) != Some(&b':') {
            return None;
        }
        cursor = skip_json_whitespace(bytes, cursor + 1);
        if key == "intent" {
            return partial_json_string(arguments, cursor);
        }
        cursor = skip_complete_json_value(arguments, cursor)?;
    }
}

fn skip_json_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        cursor += 1;
    }
    cursor
}

fn complete_json_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut cursor = start + 1;
    let mut escaped = false;
    while let Some(byte) = bytes.get(cursor) {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            return Some(cursor);
        }
        cursor += 1;
    }
    None
}

fn skip_complete_json_value(arguments: &str, start: usize) -> Option<usize> {
    let mut values = serde_json::Deserializer::from_str(arguments.get(start..)?)
        .into_iter::<serde_json::Value>();
    values.next()?.ok()?;
    Some(start + values.byte_offset())
}

fn partial_json_string(arguments: &str, start: usize) -> Option<StreamedIntent> {
    let bytes = arguments.as_bytes();
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let content_start = start + 1;
    let mut cursor = content_start;
    let mut safe_ends = vec![content_start];
    while let Some(byte) = bytes.get(cursor) {
        match *byte {
            b'"' => {
                let text = serde_json::from_str::<String>(&arguments[start..=cursor]).ok()?;
                return Some(StreamedIntent {
                    text,
                    complete: true,
                });
            }
            b'\\' => {
                let Some(escape) = bytes.get(cursor + 1).copied() else {
                    break;
                };
                if escape == b'u' {
                    let end = cursor + 6;
                    let Some(digits) = bytes.get(cursor + 2..end) else {
                        break;
                    };
                    if !digits.iter().all(u8::is_ascii_hexdigit) {
                        return None;
                    }
                    cursor = end;
                } else if matches!(
                    escape,
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
                ) {
                    cursor += 2;
                } else {
                    return None;
                }
            }
            0x00..=0x1f => return None,
            _ => {
                let character = arguments.get(cursor..)?.chars().next()?;
                cursor += character.len_utf8();
            }
        }
        safe_ends.push(cursor);
    }

    safe_ends.into_iter().rev().find_map(|end| {
        let candidate = format!("\"{}\"", arguments.get(content_start..end)?);
        serde_json::from_str::<String>(&candidate)
            .ok()
            .map(|text| StreamedIntent {
                text,
                complete: false,
            })
    })
}

impl InputBuffer {
    fn at_end(text: String) -> Self {
        let cursor = text.chars().count();
        Self {
            text,
            cursor,
            preferred_column: None,
        }
    }

    pub(crate) fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    pub(crate) fn rows(&self, width: usize) -> Vec<String> {
        self.layout(width).rows
    }

    pub(crate) fn cursor_position(&self, width: usize) -> (usize, usize) {
        let layout = self.layout(width);
        layout.cursor_positions[self.cursor.min(self.char_count())]
    }

    fn move_vertical(&mut self, direction: isize, width: usize) -> bool {
        let layout = self.layout(width);
        let cursor = self.cursor.min(self.char_count());
        let (row, column) = layout.cursor_positions[cursor];
        let target_row = if direction < 0 {
            let Some(row) = row.checked_sub(1) else {
                return false;
            };
            row
        } else {
            if row + 1 >= layout.rows.len() {
                return false;
            }
            row + 1
        };
        let preferred = self.preferred_column.unwrap_or(column);
        let Some((target, _)) = layout
            .cursor_positions
            .iter()
            .enumerate()
            .filter(|(_, (candidate_row, _))| *candidate_row == target_row)
            .min_by_key(|(index, (_, candidate_column))| {
                (candidate_column.abs_diff(preferred), usize::MAX - *index)
            })
        else {
            return false;
        };
        self.cursor = target;
        self.preferred_column = Some(preferred);
        true
    }

    fn layout(&self, width: usize) -> InputLayout {
        let width = width.max(1);
        let mut rows = vec![String::new()];
        let mut cursor_positions = Vec::with_capacity(self.char_count() + 1);
        let mut row = 0;
        let mut column = 0;
        cursor_positions.push((row, column));
        for character in self.text.chars() {
            if character == '\n' {
                rows.push(String::new());
                row += 1;
                column = 0;
                cursor_positions.push((row, column));
                continue;
            }
            let character_width = character.width().unwrap_or(0);
            if column > 0 && column + character_width > width {
                rows.push(String::new());
                row += 1;
                column = 0;
            }
            rows[row].push(character);
            column += character_width;
            cursor_positions.push((row, column));
        }
        InputLayout {
            rows,
            cursor_positions,
        }
    }

    fn insert(&mut self, character: char) {
        let byte = byte_index(&self.text, self.cursor);
        self.text.insert(byte, character);
        self.cursor += 1;
        self.preferred_column = None;
    }

    fn insert_text(&mut self, text: &str) {
        let byte = byte_index(&self.text, self.cursor);
        self.text.insert_str(byte, text);
        self.cursor += text.chars().count();
        self.preferred_column = None;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = byte_index(&self.text, self.cursor - 1);
        let end = byte_index(&self.text, self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
        self.preferred_column = None;
    }

    fn delete(&mut self) {
        if self.cursor == self.char_count() {
            return;
        }
        let start = byte_index(&self.text, self.cursor);
        let end = byte_index(&self.text, self.cursor + 1);
        self.text.replace_range(start..end, "");
        self.preferred_column = None;
    }

    /// Character index of the start of the logical line containing the cursor.
    fn line_start(&self) -> usize {
        let byte = byte_index(&self.text, self.cursor);
        self.text[..byte]
            .rfind('\n')
            .map_or(0, |newline| self.text[..newline].chars().count() + 1)
    }

    /// Character index just past the end of the logical line containing the cursor.
    fn line_end(&self) -> usize {
        let byte = byte_index(&self.text, self.cursor);
        self.text[byte..].find('\n').map_or_else(
            || self.char_count(),
            |newline| self.cursor + self.text[byte..byte + newline].chars().count(),
        )
    }

    /// Moves the cursor to the start of its logical line (Ctrl-A).
    fn move_to_line_start(&mut self) {
        self.cursor = self.line_start();
        self.preferred_column = None;
    }

    /// Moves the cursor to the end of its logical line (Ctrl-E).
    fn move_to_line_end(&mut self) {
        self.cursor = self.line_end();
        self.preferred_column = None;
    }

    /// Removes the whitespace-delimited word immediately before the cursor
    /// (terminal-style Ctrl-W / unix word rubout).
    fn delete_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let byte = byte_index(&self.text, self.cursor);
        let before = &self.text[..byte];
        let trimmed = before.trim_end_matches(char::is_whitespace);
        let start = trimmed
            .trim_end_matches(|character: char| !character.is_whitespace())
            .len();
        self.text.replace_range(start..byte, "");
        self.cursor = self.text[..start].chars().count();
        self.preferred_column = None;
    }

    /// Character index of the cursor position nearest the clicked visual
    /// (row, column) within the wrapped layout. Rows below the text place the
    /// cursor at the end of the input.
    pub(crate) fn cursor_at(&self, width: usize, row: usize, column: usize) -> usize {
        let layout = self.layout(width);
        if row >= layout.rows.len() {
            return self.char_count();
        }
        layout
            .cursor_positions
            .iter()
            .enumerate()
            .filter(|(_, (candidate_row, _))| *candidate_row == row)
            .max_by_key(|(_, (_, candidate_column))| {
                (*candidate_column <= column, *candidate_column)
            })
            .map(|(index, _)| index)
            .unwrap_or(0)
    }
}

fn byte_index(text: &str, character: usize) -> usize {
    text.char_indices()
        .nth(character)
        .map_or(text.len(), |(index, _)| index)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use lam::{
        EvalOutcome, EvalOutput, EvalValue, ModelDelta, ModelResponseMetadata, RunEvent, RunId,
        TokenUsage, ToolCallDelta,
    };
    use lam_agents::{ActorAddress, AgentOutcome, AgentSystemEvent};
    use ratatui::layout::Rect;

    use super::{
        App, CellPos, CopyRow, EntryKind, Focus, Hitbox, InputBuffer, PendingSteer,
        SESSION_PICKER_HINT, SessionChoice, SessionView, TextSelection, partial_eval_intent,
        selected_text,
    };
    use crate::config::ModelChoice;
    use crate::runtime::{
        AgentHistory, Command, CommandResult, CommittedRow, FoldOutcome, HistoryEntry, HistoryKind,
        SentMessage,
    };

    fn committed(kind: HistoryKind, title: &str, body: &str, run_id: Option<&str>) -> CommittedRow {
        CommittedRow {
            entry: HistoryEntry {
                kind,
                title: title.to_owned(),
                body: body.to_owned(),
            },
            run_id: run_id.map(str::to_owned),
        }
    }

    fn fold_with_rows(rows: Vec<CommittedRow>, active_run: Option<&str>) -> FoldOutcome {
        FoldOutcome {
            rows,
            active_run: active_run.map(str::to_owned),
            ..FoldOutcome::default()
        }
    }

    fn app() -> App {
        App::new(
            "/tmp/project".to_owned(),
            "/tmp/providers.toml".to_owned(),
            SessionView {
                id: 7,
                journal_path: "/tmp/session-00000007.redb".to_owned(),
                resumed: false,
                agents: vec![AgentHistory::root(Vec::new())],
                choices: vec![SessionChoice {
                    id: 7,
                    preview: None,
                }],
            },
            vec![ModelChoice {
                registry_id: "openai/gpt-5".to_owned(),
                provider: "openai".to_owned(),
                model: "gpt-5".to_owned(),
                display_name: "GPT-5".to_owned(),
                context_window: 400_000,
                efforts: vec!["low".to_owned(), "high".to_owned()],
            }],
            0,
            "high",
            Vec::new(),
        )
    }

    /// An app whose directory holds the open session plus two older ones.
    fn session_picker() -> App {
        let mut app = app();
        app.sessions.push(SessionChoice {
            id: 4,
            preview: None,
        });
        app.sessions.push(SessionChoice {
            id: 3,
            preview: None,
        });
        app
    }

    fn control_d() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
    }

    #[test]
    fn submit_registers_an_optimistic_pending_steer() {
        let mut app = app();
        app.input = InputBuffer::at_end("hello".to_owned());
        let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(command, Some(Command::Message(_))));
        assert_eq!(app.pending_steers.len(), 1);
        assert!(app.pending_steers[0].message_id.is_none());
        assert_eq!(app.pending_steers[0].text, "hello");
        assert!(app.input.text.is_empty());
    }

    #[test]
    fn receipt_attaches_the_durable_id_to_the_optimistic_steer() {
        let mut app = app();
        app.pending_steers.push(PendingSteer {
            message_id: None,
            text: "hello".to_owned(),
        });
        app.apply_message_receipt(
            SentMessage {
                message_id: "m1".to_owned(),
                text: "hello".to_owned(),
            },
            false,
        );
        assert_eq!(app.pending_steers.len(), 1);
        assert_eq!(app.pending_steers[0].message_id.as_deref(), Some("m1"));
    }

    #[test]
    fn consumed_receipt_removes_the_optimistic_steer() {
        let mut app = app();
        app.pending_steers.push(PendingSteer {
            message_id: None,
            text: "hello".to_owned(),
        });
        app.apply_message_receipt(
            SentMessage {
                message_id: "m1".to_owned(),
                text: "hello".to_owned(),
            },
            true,
        );
        assert!(app.pending_steers.is_empty());
    }

    #[test]
    fn failed_send_removes_the_optimistic_steer_and_restores_the_draft() {
        let mut app = app();
        app.input = InputBuffer::at_end("hello".to_owned());
        let _ = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.pending_steers.len(), 1);
        assert!(app.input.text.is_empty());
        app.apply_message_error("the journal rejected the message".to_owned());
        assert!(app.pending_steers.is_empty());
        assert_eq!(app.input.text, "hello");
    }

    #[test]
    fn delivered_steer_is_cleared_by_the_consume_fold() {
        let mut app = app();
        app.pending_steers.push(PendingSteer {
            message_id: Some("m1".to_owned()),
            text: "hello".to_owned(),
        });
        let outcome = FoldOutcome {
            consumed_message_ids: vec!["m1".to_owned()],
            ..FoldOutcome::default()
        };
        app.apply_fold("/root", outcome);
        assert!(app.pending_steers.is_empty());
    }

    #[test]
    fn edits_unicode_by_character_position() {
        let mut input = InputBuffer {
            text: "aλ".to_owned(),
            cursor: 2,
            preferred_column: None,
        };
        input.backspace();
        input.insert('界');
        assert_eq!(input.text, "a界");
        assert_eq!(input.cursor, 2);
    }

    #[test]
    fn cursor_at_maps_clicked_columns_to_character_positions() {
        let input = InputBuffer::at_end("ab界d".to_owned());
        assert_eq!(input.cursor_at(10, 0, 0), 0);
        assert_eq!(
            input.cursor_at(10, 0, 1),
            1,
            "clicking a narrow char puts the cursor before it"
        );
        assert_eq!(
            input.cursor_at(10, 0, 2),
            2,
            "left half of a wide char stays before it"
        );
        assert_eq!(
            input.cursor_at(10, 0, 3),
            2,
            "right half of a wide char stays before it"
        );
        assert_eq!(
            input.cursor_at(10, 0, 4),
            3,
            "the column right after a wide char lands after it"
        );
        assert_eq!(
            input.cursor_at(10, 0, 5),
            4,
            "past the end clamps to the end"
        );
        assert_eq!(input.cursor_at(10, 0, 99), 4);
    }

    #[test]
    fn cursor_at_maps_multiline_and_wrapped_rows() {
        let input = InputBuffer::at_end("first\nsecond".to_owned());
        assert_eq!(input.cursor_at(10, 0, 2), 2);
        assert_eq!(input.cursor_at(10, 0, 99), 5, "end of the first line");
        assert_eq!(input.cursor_at(10, 1, 0), 6, "start of the second line");
        assert_eq!(input.cursor_at(10, 1, 3), 9);
        assert_eq!(input.cursor_at(10, 1, 99), 12, "end of the second line");
        assert_eq!(
            input.cursor_at(10, 5, 0),
            12,
            "rows below the text land at the end"
        );
    }

    #[test]
    fn delete_word_backward_removes_whitespace_delimited_chunks() {
        let mut input = InputBuffer::at_end("inspect the workspace".to_owned());
        input.delete_word_backward();
        assert_eq!(input.text, "inspect the ");
        assert_eq!(input.cursor, 12);

        let mut input = InputBuffer::at_end("hello".to_owned());
        input.cursor = 3;
        input.delete_word_backward();
        assert_eq!(input.text, "lo");
        assert_eq!(input.cursor, 0);

        let mut input = InputBuffer::at_end("αβ γδ".to_owned());
        input.delete_word_backward();
        assert_eq!(input.text, "αβ ");
        assert_eq!(input.cursor, 3);

        let mut input = InputBuffer::default();
        input.delete_word_backward();
        assert_eq!(input.text, "");
    }

    #[test]
    fn control_a_and_e_jump_to_logical_line_bounds() {
        let mut app = app();
        app.input = InputBuffer::at_end("first\nsecond".to_owned());
        app.input.cursor = 9;
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(app.input.cursor, 6);
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(app.input.cursor, 12);
        // Ctrl-A is line-relative: repeating it stays at the line start.
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(app.input.cursor, 6);
    }

    #[test]
    fn control_w_deletes_the_word_before_the_cursor() {
        let mut app = app();
        app.input = InputBuffer::at_end("inspect the workspace".to_owned());
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.input.text, "inspect the ");
        assert_eq!(app.input.cursor, 12);
    }

    #[test]
    fn control_j_inserts_a_newline_instead_of_submitting() {
        let mut app = app();
        app.input = InputBuffer::at_end("first".to_owned());
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(app.input.text, "first\n");
        assert_eq!(app.input.cursor, 6);
        assert!(app.pending_steers.is_empty(), "Ctrl+J must not submit");

        // The draft stays editable across the newline, and Enter still submits.
        app.input.insert('s');
        let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(command, Some(Command::Message(_))));
        assert_eq!(app.pending_steers[0].text, "first\ns");
    }

    #[test]
    fn control_j_splits_the_line_at_the_cursor() {
        let mut app = app();
        app.input = InputBuffer::at_end("aλb".to_owned());
        app.input.cursor = 2;
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(app.input.text, "aλ\nb");
        assert_eq!(
            app.input.cursor, 3,
            "cursor lands at the start of the new line"
        );

        // The new logical line is navigable: Ctrl-A and Ctrl-E stay within it.
        app.input.cursor = 4;
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(app.input.cursor, 3);
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(app.input.cursor, 4);
    }

    #[test]
    fn clicking_the_input_focuses_and_moves_the_cursor() {
        let mut app = app();
        app.input = InputBuffer::at_end("hello".to_owned());
        app.focus = Focus::Conversation;
        app.input_area = Some(Rect {
            x: 2,
            y: 10,
            width: 60,
            height: 1,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2 + 3 + 2,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focus, Focus::Input);
        assert_eq!(app.input.cursor, 2);
    }

    #[test]
    fn clicking_the_input_prefix_moves_the_cursor_to_the_start() {
        let mut app = app();
        app.input = InputBuffer::at_end("hello".to_owned());
        app.focus = Focus::Conversation;
        app.input_area = Some(Rect {
            x: 2,
            y: 10,
            width: 60,
            height: 1,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focus, Focus::Input);
        assert_eq!(app.input.cursor, 0);
    }

    #[test]
    fn clicking_below_the_input_puts_the_cursor_at_the_end() {
        let mut app = app();
        app.input = InputBuffer::at_end("hi".to_owned());
        app.focus = Focus::Conversation;
        app.input_area = Some(Rect {
            x: 0,
            y: 10,
            width: 80,
            height: 3,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 12,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focus, Focus::Input);
        assert_eq!(app.input.cursor, 2);
    }

    #[test]
    fn bracketed_paste_preserves_multiline_prompts() {
        let mut app = app();
        app.handle_paste("first line\nsecond line");
        assert_eq!(app.input.text, "first line\nsecond line");
        assert_eq!(app.input.cursor, 22);
    }

    #[test]
    fn cell_slicing_keeps_graphemes_that_overlap_the_selection() {
        assert_eq!(super::slice_cells("界x", 1, 1), "界");
        assert_eq!(super::slice_cells("界x", 2, 2), "x");
        assert_eq!(super::slice_cells("e\u{301}x", 0, 0), "e\u{301}");
    }

    #[test]
    fn successful_model_switch_updates_the_root_display_model() {
        let mut app = app();
        app.current_model = Some(app.models[0].registry_id.clone());
        app.models.push(ModelChoice {
            registry_id: "fireworks/deepseek-v4-flash".to_owned(),
            provider: "fireworks".to_owned(),
            model: "deepseek-v4-flash".to_owned(),
            display_name: "DeepSeek V4 Flash".to_owned(),
            context_window: 128_000,
            efforts: vec!["none".to_owned(), "high".to_owned(), "max".to_owned()],
        });
        app.selected_efforts.push(2);

        app.apply_command_result(CommandResult::SwitchModel {
            index: 1,
            result: Ok("Switched to fireworks/deepseek-v4-flash.".to_owned()),
        });

        assert_eq!(app.selected_model, 1);
        assert_eq!(
            app.current_agent_model(),
            Some("fireworks/deepseek-v4-flash")
        );
        assert!(
            app.agent_detail("/root")
                .contains("fireworks/deepseek-v4-flash")
        );
    }

    #[test]
    fn slash_completion_populates_model_arguments() {
        let mut app = app();
        app.input.text = "/".to_owned();
        app.input.cursor = 1;
        app.suggestion_index = 3;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input.text, "/model ");
        let suggestions = app.suggestions();
        assert_eq!(suggestions[0].label, "openai / GPT-5");
        assert_eq!(suggestions[0].replacement, "/model openai/gpt-5");
    }

    #[test]
    fn effort_defaults_to_maximum_and_can_be_changed() {
        let mut app = app();
        assert_eq!(app.current_agent_effort(), Some("high"));
        app.input = InputBuffer::at_end("/effort low".to_owned());

        let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            command,
            Some(Command::SetEffort { index: 0, effort }) if effort == "low"
        ));
        app.apply_command_result(CommandResult::SetEffort {
            index: 0,
            effort: "low".to_owned(),
            result: Ok("Set reasoning effort to low.".to_owned()),
        });
        assert_eq!(app.current_agent_effort(), Some("low"));
        assert_eq!(
            app.runtime_preferences(),
            crate::runtime::RuntimePreferences {
                model_id: "openai/gpt-5".to_owned(),
                effort: "low".to_owned(),
            }
        );
    }

    #[test]
    fn session_picker_requests_an_authoritative_catalog_refresh() {
        let mut app = app();
        app.input = InputBuffer::at_end("/session".to_owned());

        let command = app.submit();

        assert!(matches!(command, Some(Command::RefreshSessions)));
        assert_eq!(app.input.text, "/session ");
    }

    #[test]
    fn session_picker_previews_first_user_messages_and_loads_the_selection() {
        let mut app = app();
        app.sessions.push(SessionChoice {
            id: 4,
            preview: Some("Inspect the workspace\nand summarize it".to_owned()),
        });
        app.input = InputBuffer::at_end("/session ".to_owned());

        let suggestions = app.suggestions();

        assert_eq!(suggestions[0].label, "#7");
        assert_eq!(suggestions[0].detail, "No user message yet");
        assert_eq!(suggestions[1].label, "#4");
        assert_eq!(
            suggestions[1].detail,
            "Inspect the workspace and summarize it"
        );
        assert_eq!(suggestions[1].replacement, "/session 4");

        app.input = InputBuffer::at_end("/session 4".to_owned());
        let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(command, Some(Command::LoadSession(4))));
        assert_eq!(app.status, "Loading session #4…");
    }

    #[test]
    fn session_deletion_needs_a_second_control_d_on_the_same_session() {
        let mut app = session_picker();

        assert!(
            app.handle_key(control_d()).is_none(),
            "ctrl+d is inert outside the session palette"
        );
        assert!(app.notice().is_none());

        app.input = InputBuffer::at_end("/session ".to_owned());
        // The palette lists #7 (the open session), #4, then #3.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

        assert!(app.handle_key(control_d()).is_none());
        assert_eq!(
            app.notice().map(|notice| notice.text),
            Some("Press ctrl+d again to delete session #4")
        );

        assert!(
            app.handle_key(KeyEvent::new_with_kind(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
                KeyEventKind::Repeat,
            ))
            .is_none(),
            "a held key never confirms a deletion"
        );
        assert_eq!(app.armed_session, Some(4));

        let command = app.handle_key(control_d());

        assert!(matches!(command, Some(Command::DeleteSession(4))));
        assert!(app.armed_session.is_none());
        assert_eq!(
            app.notice().map(|notice| notice.text),
            Some(SESSION_PICKER_HINT)
        );

        // The catalog answers with the surviving sessions, so the highlight
        // must not outrun the shortened palette.
        app.replace_sessions(vec![SessionChoice {
            id: 7,
            preview: None,
        }]);
        app.session_deleted(4);
        assert_eq!(app.suggestion_index, 0);
        assert_eq!(app.status, "Deleted session #4");
    }

    #[test]
    fn session_deletion_disarms_on_movement_and_other_keys() {
        let mut app = session_picker();
        app.input = InputBuffer::at_end("/session ".to_owned());
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(app.handle_key(control_d()).is_none());

        // Moving to another session disarms, so the next ctrl+d arms that one
        // instead of deleting anything.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(app.armed_session.is_none());
        assert_eq!(
            app.notice().map(|notice| notice.text),
            Some(SESSION_PICKER_HINT)
        );
        assert!(app.handle_key(control_d()).is_none());
        assert_eq!(app.armed_session, Some(3));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.armed_session.is_none());
        assert!(app.session_notice.is_none());

        assert!(app.handle_key(control_d()).is_none());
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.armed_session.is_none());
    }

    #[test]
    fn deleting_the_open_session_asks_for_a_replacement() {
        let mut app = session_picker();
        app.input = InputBuffer::at_end("/session ".to_owned());

        // The palette opens on #7, the session this app has open.
        assert!(app.handle_key(control_d()).is_none());
        assert_eq!(app.armed_session, Some(7));
        assert_eq!(
            app.notice().map(|notice| notice.text),
            Some("Press ctrl+d again to delete the open session #7 and start a fresh one")
        );

        assert!(
            app.handle_key(KeyEvent::new_with_kind(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
                KeyEventKind::Repeat,
            ))
            .is_none(),
            "a held key never confirms a replacement"
        );
        assert_eq!(app.armed_session, Some(7));

        let command = app.handle_key(control_d());

        assert!(matches!(
            command,
            Some(Command::ReplaceSession { delete: 7 })
        ));
        assert!(app.armed_session.is_none());
        assert_eq!(
            app.notice().map(|notice| notice.text),
            Some(SESSION_PICKER_HINT)
        );

        // The successor is a whole new app; it reports both halves of the swap.
        let mut successor = session_picker();
        successor.session_id = 9;
        successor.session_replaced(7);
        assert_eq!(successor.status, "Deleted session #7; started #9");
    }

    #[test]
    fn arming_the_open_session_disarms_like_any_other_row() {
        let mut app = session_picker();
        app.input = InputBuffer::at_end("/session ".to_owned());
        assert!(app.handle_key(control_d()).is_none());
        assert_eq!(app.armed_session, Some(7));

        // Moving off the open session arms the next one from scratch rather
        // than confirming anything.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(app.armed_session.is_none());
        assert!(app.handle_key(control_d()).is_none());
        assert_eq!(app.armed_session, Some(4));

        // Coming back needs its own confirmation for the replacement.
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(app.handle_key(control_d()).is_none());
        assert_eq!(app.armed_session, Some(7));

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.armed_session.is_none());
        assert!(app.session_notice.is_none());
    }

    #[test]
    fn tab_moves_focus_when_no_completion_is_open() {
        let mut app = app();
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Conversation);
        assert!(app.follow_conversation_tail);
    }

    #[test]
    fn escape_requires_two_physical_presses_and_preserves_the_draft() {
        let mut app = app();
        let run_id = RunId::new("run-escape").unwrap();
        app.apply_agent_event(AgentSystemEvent::Run {
            address: ActorAddress::new("/root").unwrap(),
            event: RunEvent::Started {
                run_id: run_id.clone(),
            },
        });
        app.input = InputBuffer::at_end("keep this draft".to_owned());

        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(app.input.text, "keep this draft");
        assert_eq!(
            app.interruption_warning(),
            Some("Press Esc again to stop the current run")
        );
        assert!(
            app.handle_key(KeyEvent::new_with_kind(
                KeyCode::Esc,
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            ))
            .is_none()
        );
        assert!(app.interruption_warning().is_some());

        let command = app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(command, Some(Command::Interrupt)));
        assert!(app.interruption_warning().is_none());
        assert!(app.interruption_in_progress);
        assert_eq!(app.input.text, "keep this draft");
    }

    #[test]
    fn interruption_warning_disarms_on_other_input_timeout_and_run_completion() {
        let mut app = app();
        let run_id = RunId::new("run-disarm").unwrap();
        app.apply_agent_event(AgentSystemEvent::Run {
            address: ActorAddress::new("/root").unwrap(),
            event: RunEvent::Started {
                run_id: run_id.clone(),
            },
        });

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.interruption_warning().is_none());

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let deadline = app.interruption_deadline().unwrap();
        assert!(app.expire_interruption(deadline));
        assert!(app.interruption_warning().is_none());

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.apply_agent_event(AgentSystemEvent::Run {
            address: ActorAddress::new("/root").unwrap(),
            event: RunEvent::Completed { run_id },
        });
        assert!(app.interruption_warning().is_none());
    }

    #[test]
    fn durable_interruption_replaces_transient_rows_and_ignores_late_deltas() {
        let mut app = app();
        let run_id = RunId::new("run-reconcile").unwrap();
        let root = ActorAddress::new("/root").unwrap();
        app.apply_agent_event(AgentSystemEvent::Run {
            address: root.clone(),
            event: RunEvent::Started {
                run_id: run_id.clone(),
            },
        });
        app.apply_agent_event(AgentSystemEvent::Run {
            address: root.clone(),
            event: RunEvent::ModelDelta {
                run_id: run_id.clone(),
                delta: ModelDelta::Text("partial assistant output".to_owned()),
            },
        });
        app.interruption_in_progress = true;

        // The interruption boundary commits: the fold renders the consumed
        // message and the notice, and reports the run dead.
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![
                    committed(HistoryKind::User, "You", "Do some work", None),
                    committed(
                        HistoryKind::System,
                        "Run interrupted",
                        "The run was stopped.",
                        None,
                    ),
                ],
                dead_runs: vec![run_id.to_string()],
                interrupted: true,
                ..FoldOutcome::default()
            },
        );
        app.apply_interrupt_result(Ok(true));

        assert!(!app.busy);
        assert!(
            app.entries
                .iter()
                .all(|entry| !entry.body.contains("partial assistant"))
        );
        assert!(
            app.entries
                .iter()
                .any(|entry| entry.title == "Run interrupted")
        );
        let entry_count = app.entries.len();
        app.apply_agent_event(AgentSystemEvent::Run {
            address: root,
            event: RunEvent::ModelDelta {
                run_id,
                delta: ModelDelta::Text("late buffered delta".to_owned()),
            },
        });
        assert_eq!(app.entries.len(), entry_count);
        assert!(
            app.entries
                .iter()
                .all(|entry| !entry.body.contains("late buffered"))
        );
    }

    #[test]
    fn submission_during_active_run_queues_a_pending_message() {
        let mut app = app();
        app.root_run_active = true;
        app.busy = true;
        app.input.text = "one more thing".to_owned();
        app.input.cursor = app.input.char_count();

        let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            command,
            Some(Command::Message(input)) if input == "one more thing"
        ));
        assert!(app.input.text.is_empty());
        assert!(app.busy);
        assert!(
            !app.entries
                .iter()
                .any(|entry| entry.body == "one more thing")
        );

        // The receipt registers a pending steer, not a conversation row.
        app.apply_message_receipt(
            SentMessage {
                message_id: "steer-1".to_owned(),
                text: "one more thing".to_owned(),
            },
            false,
        );
        assert_eq!(app.pending_steers.len(), 1);
        assert_eq!(app.pending_steers[0].message_id.as_deref(), Some("steer-1"));
        assert!(
            app.entries
                .iter()
                .all(|entry| entry.body != "one more thing")
        );
    }

    #[test]
    fn messages_submit_even_while_a_command_is_in_flight() {
        let mut app = app();
        app.command_in_flight = true;
        app.busy = true;
        app.input.text = "keep going".to_owned();
        app.input.cursor = app.input.char_count();

        let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            command,
            Some(Command::Message(input)) if input == "keep going"
        ));
    }

    #[test]
    fn commands_remain_blocked_while_the_root_is_busy() {
        let mut app = app();
        app.root_run_active = true;
        app.busy = true;
        app.input.text = "/compact".to_owned();
        app.input.cursor = app.input.char_count();

        let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(command.is_none());
        assert_eq!(app.status, "Wait for the current operation to finish");
        assert!(app.pending_steers.is_empty());
    }

    #[test]
    fn messages_are_rejected_during_an_interruption() {
        let mut app = app();
        app.interruption_in_progress = true;
        app.busy = true;
        app.input.text = "too soon".to_owned();
        app.input.cursor = app.input.char_count();

        let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(command.is_none());
        assert_eq!(app.status, "Wait for the interruption to finish");
    }

    #[test]
    fn delivery_fold_moves_pending_messages_into_the_conversation() {
        let mut app = app();
        app.root_run_active = true;
        app.busy = true;
        app.apply_message_receipt(
            SentMessage {
                message_id: "steer-a".to_owned(),
                text: "first steer".to_owned(),
            },
            false,
        );
        app.apply_message_receipt(
            SentMessage {
                message_id: "steer-b".to_owned(),
                text: "second steer".to_owned(),
            },
            false,
        );
        assert_eq!(app.pending_steers.len(), 2);

        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![
                    committed(HistoryKind::User, "You", "first steer", None),
                    committed(HistoryKind::User, "You", "second steer", None),
                ],
                consumed_message_ids: vec!["steer-a".to_owned(), "steer-b".to_owned()],
                active_run: Some("run-steer".to_owned()),
                ..FoldOutcome::default()
            },
        );

        assert!(app.pending_steers.is_empty());
        let users = app
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::User)
            .map(|entry| entry.body.as_str())
            .collect::<Vec<_>>();
        assert_eq!(users, ["first steer", "second steer"]);
    }

    #[test]
    fn receipt_after_delivery_fold_registers_nothing() {
        let mut app = app();
        app.root_run_active = true;
        app.busy = true;

        // The delivery fold won the race against the send receipt: the row
        // is already rendered and the projector reports the message consumed.
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![committed(HistoryKind::User, "You", "raced message", None)],
                consumed_message_ids: vec!["steer-race".to_owned()],
                active_run: Some("run-steer-race".to_owned()),
                ..FoldOutcome::default()
            },
        );
        assert!(app.pending_steers.is_empty());

        app.apply_message_receipt(
            SentMessage {
                message_id: "steer-race".to_owned(),
                text: "raced message".to_owned(),
            },
            true,
        );

        assert!(app.pending_steers.is_empty());
        let raced = app
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::User && entry.body == "raced message")
            .count();
        assert_eq!(
            raced, 1,
            "the message renders exactly once and never vanishes"
        );
    }

    #[test]
    fn interruption_fold_drops_consumed_steers_and_keeps_queued_ones() {
        let mut app = app();
        app.root_run_active = true;
        app.busy = true;
        app.apply_message_receipt(
            SentMessage {
                message_id: "steer-kept".to_owned(),
                text: "still queued".to_owned(),
            },
            false,
        );
        app.apply_message_receipt(
            SentMessage {
                message_id: "steer-consumed".to_owned(),
                text: "consumed by boundary".to_owned(),
            },
            false,
        );
        assert_eq!(app.pending_steers.len(), 2);
        app.interruption_in_progress = true;

        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![committed(
                    HistoryKind::User,
                    "You",
                    "consumed by boundary",
                    None,
                )],
                consumed_message_ids: vec!["steer-consumed".to_owned()],
                interrupted: true,
                ..FoldOutcome::default()
            },
        );
        app.apply_interrupt_result(Ok(true));

        assert_eq!(app.pending_steers.len(), 1);
        assert_eq!(
            app.pending_steers[0].message_id.as_deref(),
            Some("steer-kept")
        );
        assert!(app.entries.iter().any(|entry| {
            entry.kind == EntryKind::User && entry.body == "consumed by boundary"
        }));
    }

    #[test]
    fn steered_message_renders_at_its_journal_position() {
        let mut app = app();
        let root = ActorAddress::new("/root").unwrap();
        let run_id = RunId::new("run-1").unwrap();
        app.apply_agent_event(AgentSystemEvent::Run {
            address: root.clone(),
            event: RunEvent::Started {
                run_id: run_id.clone(),
            },
        });

        // The eval result commits, then the steer delivery commits, exactly
        // as journal order has it.
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![
                    committed(
                        HistoryKind::ToolCall,
                        "/root · Inspect",
                        "1 + 1",
                        Some("run-1"),
                    ),
                    committed(
                        HistoryKind::ToolResult,
                        "/root · eval result",
                        "2",
                        Some("run-1"),
                    ),
                ],
                model_turns: vec!["run-1".to_owned()],
                active_run: Some("run-1".to_owned()),
                ..FoldOutcome::default()
            },
        );
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![committed(
                    HistoryKind::User,
                    "You",
                    "also check the tests",
                    None,
                )],
                consumed_message_ids: vec!["steer-1".to_owned()],
                active_run: Some("run-1".to_owned()),
                ..FoldOutcome::default()
            },
        );
        // The model's next turn responds to the steer.
        app.apply_agent_event(AgentSystemEvent::Run {
            address: root,
            event: RunEvent::ModelDelta {
                run_id,
                delta: ModelDelta::Text("Good point, doing that now.".to_owned()),
            },
        });
        // The send receipt arrives last; the projector reports it consumed.
        app.apply_message_receipt(
            SentMessage {
                message_id: "steer-1".to_owned(),
                text: "also check the tests".to_owned(),
            },
            true,
        );

        let tool_result = app
            .entries
            .iter()
            .position(|entry| entry.kind == EntryKind::ToolResult)
            .expect("the eval result renders");
        let user = app
            .entries
            .iter()
            .position(|entry| entry.kind == EntryKind::User && entry.body == "also check the tests")
            .expect("the steer renders");
        let response = app
            .entries
            .iter()
            .position(|entry| {
                entry.kind == EntryKind::Assistant && entry.body.contains("Good point")
            })
            .expect("the response renders");
        assert!(
            tool_result < user && user < response,
            "journal order is eval result, steer, response: {tool_result} < {user} < {response}"
        );
        assert!(app.pending_steers.is_empty());
    }

    #[test]
    fn committed_terminal_output_replaces_the_streamed_overlay_below_the_tool_calls() {
        let mut app = app();
        let root = ActorAddress::new("/root").unwrap();
        let run_id = RunId::new("run-1").unwrap();
        app.apply_agent_event(AgentSystemEvent::Run {
            address: root.clone(),
            event: RunEvent::Started {
                run_id: run_id.clone(),
            },
        });

        // Turn one streams a preamble, then commits with its tool call.
        app.apply_agent_event(AgentSystemEvent::Run {
            address: root.clone(),
            event: RunEvent::ModelStarted {
                run_id: run_id.clone(),
            },
        });
        app.apply_agent_event(AgentSystemEvent::Run {
            address: root.clone(),
            event: RunEvent::ModelDelta {
                run_id: run_id.clone(),
                delta: ModelDelta::Text("I found the bug.".to_owned()),
            },
        });
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![
                    committed(
                        HistoryKind::Assistant,
                        "/root",
                        "I found the bug.",
                        Some("run-1"),
                    ),
                    committed(
                        HistoryKind::ToolCall,
                        "/root · first",
                        "1 + 1",
                        Some("run-1"),
                    ),
                ],
                model_turns: vec!["run-1".to_owned()],
                active_run: Some("run-1".to_owned()),
                ..FoldOutcome::default()
            },
        );
        // The eval result commits.
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![committed(
                    HistoryKind::ToolResult,
                    "/root · eval result",
                    "2",
                    Some("run-1"),
                )],
                active_run: Some("run-1".to_owned()),
                ..FoldOutcome::default()
            },
        );
        // The terminal turn streams, commits, and the run completes.
        app.apply_agent_event(AgentSystemEvent::Run {
            address: root.clone(),
            event: RunEvent::ModelStarted {
                run_id: run_id.clone(),
            },
        });
        app.apply_agent_event(AgentSystemEvent::Run {
            address: root.clone(),
            event: RunEvent::ModelDelta {
                run_id: run_id.clone(),
                delta: ModelDelta::Text("The fix is to flip the flag.".to_owned()),
            },
        });
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![committed(
                    HistoryKind::Assistant,
                    "/root",
                    "The fix is to flip the flag.",
                    Some("run-1"),
                )],
                model_turns: vec!["run-1".to_owned()],
                dead_runs: vec!["run-1".to_owned()],
                ..FoldOutcome::default()
            },
        );
        app.apply_agent_event(AgentSystemEvent::Run {
            address: root,
            event: RunEvent::Completed { run_id },
        });

        let final_row = app
            .entries
            .iter()
            .position(|entry| {
                entry.kind == EntryKind::Assistant && entry.body.contains("flip the flag")
            })
            .expect("the final output renders");
        let last_tool = app
            .entries
            .iter()
            .rposition(|entry| matches!(entry.kind, EntryKind::ToolCall | EntryKind::ToolResult))
            .expect("tool rows render");
        let duplicates = app
            .entries
            .iter()
            .filter(|entry| entry.body.contains("flip the flag"))
            .count();
        assert_eq!(duplicates, 1, "the overlay is replaced, not duplicated");
        assert!(
            final_row > last_tool,
            "the final output renders below the tool rows: {final_row} > {last_tool}"
        );
        assert!(!app.busy);
        assert_eq!(app.status, "Ready");
    }

    #[test]
    fn quit_is_an_alias_for_exit() {
        let mut app = app();
        app.input = InputBuffer::at_end("/quit".to_owned());

        let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(command.is_none());
        assert!(app.should_exit);
    }

    #[test]
    fn ctrl_c_clears_draft_before_exiting() {
        let mut app = app();
        app.input = InputBuffer::at_end("unfinished prompt".to_owned());

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(app.input.text.is_empty());
        assert!(!app.should_exit);

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(app.should_exit);
    }

    #[test]
    fn vertical_input_navigation_uses_visual_rows_before_user_history() {
        let mut app = app();
        app.apply_fold(
            "/root",
            fold_with_rows(
                vec![
                    committed(HistoryKind::User, "You", "one", None),
                    committed(HistoryKind::User, "You", "two", None),
                ],
                None,
            ),
        );
        app.input_width = 4;
        app.input = InputBuffer::at_end("abcdef".to_owned());

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input.text, "abcdef");
        assert_eq!(app.input.cursor, 2, "cursor should move to the row above");

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input.text, "two");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input.text, "one");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.input.text, "two");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.input.text, "abcdef");
        assert_eq!(
            app.input.cursor, 2,
            "history should restore the draft cursor"
        );
    }

    #[test]
    fn vertical_input_navigation_respects_explicit_newlines() {
        let mut input = InputBuffer::at_end("first\nxy".to_owned());

        assert!(input.move_vertical(-1, 20));
        assert_eq!(input.cursor, 2);
        assert!(input.move_vertical(1, 20));
        assert_eq!(input.cursor, input.char_count());
    }

    #[test]
    fn browsing_selection_is_stable_until_navigation_returns_to_tail() {
        let mut app = app();
        app.push_entry(EntryKind::System, "Older", "first");
        app.push_entry(EntryKind::System, "Newer", "second");
        app.focus = Focus::Conversation;
        app.move_selection(-1);
        let selected = app.selected_entry;
        assert!(!app.follow_conversation_tail);

        app.apply_agent_event(AgentSystemEvent::Run {
            address: ActorAddress::new("/root").unwrap(),
            event: RunEvent::ModelDelta {
                run_id: RunId::new("run-1").unwrap(),
                delta: ModelDelta::Text("streaming below".to_owned()),
            },
        });
        assert_eq!(app.selected_entry, selected);
        assert!(!app.follow_conversation_tail);

        app.move_selection(100);
        assert_eq!(app.selected_entry, app.entries.len().checked_sub(1));
        assert!(app.follow_conversation_tail);

        app.push_entry(EntryKind::System, "Latest", "third");
        assert_eq!(app.selected_entry, app.entries.len().checked_sub(1));
    }

    #[test]
    fn mouse_wheel_scrolls_within_one_oversized_entry() {
        let mut app = app();
        app.push_entry(EntryKind::System, "Older", "first");
        app.apply_fold(
            "/root",
            fold_with_rows(
                vec![committed(
                    HistoryKind::Assistant,
                    "Agent",
                    "long response",
                    None,
                )],
                None,
            ),
        );
        app.focus = Focus::Conversation;
        app.selected_entry = Some(2);
        app.conversation_ranges = vec![(0, 1), (2, 9), (10, 109)];
        app.conversation_viewport_height = 10;
        app.conversation_total_lines = 110;
        app.conversation_offset = 100;
        app.follow_conversation_tail = true;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.conversation_offset, 98);
        assert_eq!(app.selected_entry, Some(2));
        assert!(!app.follow_conversation_tail);
        assert!(!app.selection_drives_viewport);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.conversation_offset, 100);
        assert!(app.follow_conversation_tail);
    }

    #[test]
    fn mouse_selection_follows_the_visible_viewport_but_keyboard_drives_it() {
        let mut app = app();
        app.push_entry(EntryKind::System, "Middle", "second");
        app.push_entry(EntryKind::System, "Latest", "third");
        app.focus = Focus::Conversation;
        app.selected_entry = Some(2);
        app.conversation_ranges = vec![(0, 9), (10, 19), (20, 29)];
        app.conversation_viewport_height = 10;
        app.conversation_total_lines = 30;
        app.conversation_offset = 20;
        app.follow_conversation_tail = true;
        let wheel_up = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };

        for _ in 0..5 {
            app.handle_mouse(wheel_up);
        }

        assert_eq!(app.conversation_offset, 10);
        assert_eq!(app.selected_entry, Some(1));
        assert!(!app.selection_drives_viewport);

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(app.selected_entry, Some(0));
        assert!(app.selection_drives_viewport);
    }

    #[test]
    fn ordinary_input_sends_a_message() {
        let mut app = app();
        app.input.text = "inspect the workspace".to_owned();
        app.input.cursor = app.input.char_count();
        let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(command, Some(Command::Message(input)) if input == "inspect the workspace")
        );
        // The message is not blocking and its row renders from the journal
        // fold once the runner delivers it.
        assert!(!app.busy);
        assert_eq!(
            app.sessions
                .iter()
                .find(|session| session.id == app.session_id)
                .and_then(|session| session.preview.as_deref()),
            Some("inspect the workspace")
        );
    }

    #[test]
    fn new_command_requests_a_fresh_session() {
        let mut app = app();
        app.input.text = "/new".to_owned();
        app.input.cursor = app.input.char_count();

        let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(command, Some(Command::New)));
        assert!(app.busy);
        assert_eq!(app.status, "Starting a new session…");
    }

    #[test]
    fn durable_history_is_restored_before_the_ready_row() {
        let app = app();
        let restored = App::new(
            "/tmp/project".to_owned(),
            "/tmp/providers.toml".to_owned(),
            SessionView {
                id: 7,
                journal_path: "/tmp/session-00000007.redb".to_owned(),
                resumed: true,
                agents: vec![AgentHistory::root(vec![
                    committed(HistoryKind::User, "You", "hello", None),
                    committed(HistoryKind::Assistant, "/root", "hi", Some("run-1")),
                    committed(
                        HistoryKind::ToolCall,
                        "/root · Calculate the result",
                        "1 + 1",
                        Some("run-1"),
                    ),
                ])],
                choices: vec![SessionChoice {
                    id: 7,
                    preview: Some("hello".to_owned()),
                }],
            },
            app.models,
            0,
            "high",
            Vec::new(),
        );

        assert_eq!(restored.entries.len(), 4);
        assert!(restored.entries[0].expanded);
        assert!(restored.entries[1].expanded);
        assert!(!restored.entries[2].expanded);
        assert_eq!(restored.entries[3].title, "Ready");
        assert!(
            restored.entries[3]
                .body
                .starts_with("Resumed durable session #7")
        );
    }

    #[test]
    fn committed_tool_calls_render_as_pending_source_rows() {
        let mut app = app();
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![committed(
                    HistoryKind::ToolCall,
                    "/root · Inspect the workspace files",
                    "const files = await lam.fs.list({ path: '.' });",
                    Some("run-1"),
                )],
                model_turns: vec!["run-1".to_owned()],
                active_run: Some("run-1".to_owned()),
                ..FoldOutcome::default()
            },
        );
        let tool = app.entries.last().unwrap();
        assert_eq!(tool.title, "/root · Inspect the workspace files");
        assert!(tool.body.contains("lam.fs.list"));
        assert!(tool.pending_tool);
    }

    #[test]
    fn streamed_tool_arguments_reconcile_with_separate_result_row() {
        let mut app = app();
        let address = ActorAddress::new("/root").unwrap();
        let run_id = RunId::new("run-1").unwrap();
        for delta in [
            ToolCallDelta {
                index: 0,
                call_id: Some("call-1".to_owned()),
                name: Some("eval".to_owned()),
                arguments: "{\"intent\":\"Calculate the result\",\"source\":".to_owned(),
            },
            ToolCallDelta {
                index: 0,
                call_id: None,
                name: None,
                arguments: "\"1 + 1\"}".to_owned(),
            },
        ] {
            app.apply_agent_event(AgentSystemEvent::Run {
                address: address.clone(),
                event: RunEvent::ModelDelta {
                    run_id: run_id.clone(),
                    delta: ModelDelta::ToolCall(delta),
                },
            });
        }
        assert_eq!(app.entries.len(), 2);
        assert_eq!(app.entries[1].kind, EntryKind::ToolCall);
        assert_eq!(
            app.entries[1].body,
            "{\"intent\":\"Calculate the result\",\"source\":\"1 + 1\"}"
        );

        // The model turn commits: the fold's authoritative tool-call row
        // replaces the streamed overlay row.
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![committed(
                    HistoryKind::ToolCall,
                    "/root · Calculate the result",
                    "1 + 1",
                    Some("run-1"),
                )],
                model_turns: vec!["run-1".to_owned()],
                active_run: Some("run-1".to_owned()),
                ..FoldOutcome::default()
            },
        );
        assert_eq!(
            app.entries.len(),
            2,
            "the committed row replaces the streamed row"
        );
        assert_eq!(app.entries[1].title, "/root · Calculate the result");
        assert_eq!(app.entries[1].body, "1 + 1");

        // The durable eval outcome commits, pairing with the pending call;
        // the EvalCompleted event that follows is status-only and must not
        // add a duplicate row.
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![committed(
                    HistoryKind::ToolResult,
                    "/root · eval result",
                    "2",
                    Some("run-1"),
                )],
                active_run: Some("run-1".to_owned()),
                ..FoldOutcome::default()
            },
        );
        assert_eq!(app.entries.len(), 3);
        assert_eq!(app.entries[1].kind, EntryKind::ToolCall);
        assert_eq!(app.entries[2].kind, EntryKind::ToolResult);
        assert_eq!(app.entries[2].body, "2");
        assert!(!app.entries[1].pending_tool);

        app.apply_agent_event(AgentSystemEvent::Run {
            address,
            event: RunEvent::EvalCompleted {
                run_id,
                outcome: EvalOutcome::Success {
                    output: EvalOutput {
                        result: EvalValue::Json(serde_json::json!(2)),
                        logs: Vec::new(),
                    },
                },
            },
        });
        assert_eq!(
            app.entries.len(),
            3,
            "the event adds no row: the committed result is already rendered"
        );
        assert_eq!(app.status, "/root finished eval");

        app.selected_entry = Some(1);
        app.toggle_selected();
        assert!(app.entries[1].expanded);
        assert_eq!(app.entries[1].title, "/root · Calculate the result");
        assert!(!app.entries[2].expanded);
    }

    #[test]
    fn parallel_tool_results_pair_with_calls_in_provider_order() {
        let mut app = app();
        let address = ActorAddress::new("/root").unwrap();
        let run_id = RunId::new("run-parallel").unwrap();
        for (index, id, intent, source) in [
            (0, "call-1", "Commit the change", "await commit()"),
            (1, "call-2", "Inspect the styling", "await inspect()"),
        ] {
            app.apply_agent_event(AgentSystemEvent::Run {
                address: address.clone(),
                event: RunEvent::ModelDelta {
                    run_id: run_id.clone(),
                    delta: ModelDelta::ToolCall(ToolCallDelta {
                        index,
                        call_id: Some(id.to_owned()),
                        name: Some("eval".to_owned()),
                        arguments: serde_json::json!({
                            "intent": intent,
                            "source": source,
                            "timeoutMs": null,
                        })
                        .to_string(),
                    }),
                },
            });
        }

        // The turn commits with the executed request and the rejected
        // sibling, replacing both streamed overlay rows.
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![
                    committed(
                        HistoryKind::ToolCall,
                        "/root · Commit the change",
                        "await commit()",
                        Some("run-parallel"),
                    ),
                    committed(
                        HistoryKind::ToolCall,
                        "/root · Inspect the styling",
                        "await inspect()",
                        Some("run-parallel"),
                    ),
                ],
                model_turns: vec!["run-parallel".to_owned()],
                active_run: Some("run-parallel".to_owned()),
                ..FoldOutcome::default()
            },
        );
        assert_eq!(app.entries[1].body, "await commit()");
        assert!(app.entries[1].pending_tool);
        assert!(app.entries[2].pending_tool);

        // Both outcomes commit in one batch; the committed result rows pair
        // with the pending calls in provider order.
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![
                    committed(
                        HistoryKind::ToolResult,
                        "/root · eval result",
                        "\"committed\"",
                        Some("run-parallel"),
                    ),
                    committed(
                        HistoryKind::ToolResult,
                        "/root · eval result",
                        "Combine multiple actions in one eval program.",
                        Some("run-parallel"),
                    ),
                ],
                active_run: Some("run-parallel".to_owned()),
                ..FoldOutcome::default()
            },
        );
        for outcome in [
            EvalOutcome::Success {
                output: EvalOutput {
                    result: EvalValue::Json(serde_json::json!("committed")),
                    logs: Vec::new(),
                },
            },
            EvalOutcome::Rejected {
                message: "Combine multiple actions in one eval program.".to_owned(),
            },
        ] {
            app.apply_agent_event(AgentSystemEvent::Run {
                address: address.clone(),
                event: RunEvent::EvalCompleted {
                    run_id: run_id.clone(),
                    outcome,
                },
            });
        }

        assert_eq!(app.entries.len(), 5, "the events add no rows");
        assert!(!app.entries[1].pending_tool);
        assert!(!app.entries[2].pending_tool);
        assert_eq!(app.entries[3].kind, EntryKind::ToolResult);
        assert_eq!(app.entries[4].kind, EntryKind::ToolResult);
        assert!(app.entries[4].body.contains("Combine multiple actions"));
    }

    #[test]
    fn streamed_eval_intent_updates_the_collapsed_title_from_partial_json() {
        let mut app = app();
        let address = ActorAddress::new("/root").unwrap();
        let run_id = RunId::new("run-intent").unwrap();
        let chunks = [
            (Some("eval"), r#"{"intent":"Inspect "#),
            (None, r#"the \u"#),
            (None, r#"0077orkspace","source":""#),
            (None, r#"await lam.dir()"}"#),
        ];
        let expected = [
            "/root · Inspect",
            "/root · Inspect the",
            "/root · Inspect the workspace",
            "/root · Inspect the workspace",
        ];

        for ((name, arguments), expected) in chunks.into_iter().zip(expected) {
            app.apply_agent_event(AgentSystemEvent::Run {
                address: address.clone(),
                event: RunEvent::ModelDelta {
                    run_id: run_id.clone(),
                    delta: ModelDelta::ToolCall(ToolCallDelta {
                        index: 0,
                        call_id: None,
                        name: name.map(str::to_owned),
                        arguments: arguments.to_owned(),
                    }),
                },
            });
            assert_eq!(app.entries.last().unwrap().title, expected);
            assert!(!app.entries.last().unwrap().expanded);
        }

        // The turn commits: the fold's authoritative row replaces the
        // streamed one with the final intent and source.
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![committed(
                    HistoryKind::ToolCall,
                    "/root · Inspect the workspace",
                    "await lam.dir()",
                    Some(run_id.as_str()),
                )],
                model_turns: vec![run_id.to_string()],
                active_run: Some(run_id.to_string()),
                ..FoldOutcome::default()
            },
        );
        assert_eq!(
            app.entries.last().unwrap().title,
            "/root · Inspect the workspace"
        );
        assert_eq!(app.entries.last().unwrap().body, "await lam.dir()");
    }

    #[test]
    fn partial_eval_intent_skips_complete_fields_and_decodes_escapes() {
        let intent = partial_eval_intent(
            r#"{"source":"const label = \"intent\";","intent":"Read \"quoted\" \ud83d\ude"#,
        )
        .expect("the top-level intent should be discoverable after source");

        assert_eq!(intent.text, "Read \"quoted\" ");
        assert!(!intent.complete);
    }

    #[test]
    fn committed_terminal_output_supersedes_stream_deltas_in_either_order() {
        // The terminal turn commits before any of its deltas render.
        let mut first_app = app();
        first_app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![committed(
                    HistoryKind::Assistant,
                    "/root",
                    "hello",
                    Some("run-1"),
                )],
                model_turns: vec!["run-1".to_owned()],
                dead_runs: vec!["run-1".to_owned()],
                ..FoldOutcome::default()
            },
        );
        for text in ["hel", "lo"] {
            first_app.apply_agent_event(AgentSystemEvent::Run {
                address: ActorAddress::new("/root").unwrap(),
                event: RunEvent::ModelDelta {
                    run_id: RunId::new("run-1").unwrap(),
                    delta: ModelDelta::Text(text.to_owned()),
                },
            });
        }
        let visible = first_app
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Assistant)
            .collect::<Vec<_>>();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].body, "hello");
        assert!(visible[0].expanded);

        // The deltas render first; the commit replaces the overlay segment.
        let mut second_app = app();
        for text in ["hel", "lo"] {
            second_app.apply_agent_event(AgentSystemEvent::Run {
                address: ActorAddress::new("/root").unwrap(),
                event: RunEvent::ModelDelta {
                    run_id: RunId::new("run-2").unwrap(),
                    delta: ModelDelta::Text(text.to_owned()),
                },
            });
        }
        second_app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![committed(
                    HistoryKind::Assistant,
                    "/root",
                    "hello",
                    Some("run-2"),
                )],
                model_turns: vec!["run-2".to_owned()],
                dead_runs: vec!["run-2".to_owned()],
                ..FoldOutcome::default()
            },
        );
        let visible = second_app
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Assistant)
            .collect::<Vec<_>>();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].body, "hello");
        assert!(visible[0].expanded);
    }

    #[test]
    fn streamed_agent_text_stays_expanded_when_the_run_completes() {
        let mut app = app();
        let address = ActorAddress::new("/root").unwrap();
        let run_id = RunId::new("run-1").unwrap();
        app.apply_agent_event(AgentSystemEvent::Run {
            address: address.clone(),
            event: RunEvent::ModelDelta {
                run_id: run_id.clone(),
                delta: ModelDelta::Text("Final answer".to_owned()),
            },
        });
        assert!(app.entries.last().unwrap().expanded);

        app.apply_agent_event(AgentSystemEvent::Run {
            address,
            event: RunEvent::Completed { run_id },
        });
        assert!(app.entries.last().unwrap().expanded);

        app.selected_entry = app.entries.len().checked_sub(1);
        app.toggle_selected();
        assert!(!app.entries.last().unwrap().expanded);
    }

    #[test]
    fn empty_model_deltas_do_not_create_conversation_rows() {
        let mut app = app();
        let address = ActorAddress::new("/root").unwrap();
        let run_id = RunId::new("run-1").unwrap();
        let initial_entries = app.entries.len();

        for delta in [
            ModelDelta::Text(String::new()),
            ModelDelta::Reasoning(String::new()),
        ] {
            app.apply_agent_event(AgentSystemEvent::Run {
                address: address.clone(),
                event: RunEvent::ModelDelta {
                    run_id: run_id.clone(),
                    delta,
                },
            });
        }
        assert_eq!(app.entries.len(), initial_entries);

        for delta in [
            ModelDelta::Text(" ".to_owned()),
            ModelDelta::Reasoning("\n".to_owned()),
        ] {
            app.apply_agent_event(AgentSystemEvent::Run {
                address: address.clone(),
                event: RunEvent::ModelDelta {
                    run_id: run_id.clone(),
                    delta,
                },
            });
        }
        assert_eq!(app.entries.len(), initial_entries + 2);
        assert_eq!(app.entries[initial_entries].body, " ");
        assert_eq!(app.entries[initial_entries + 1].body, "\n");
    }

    #[test]
    fn tool_call_keeps_intermediate_text_expanded() {
        let mut app = app();
        let address = ActorAddress::new("/root").unwrap();
        let first_run = RunId::new("run-1").unwrap();
        app.apply_agent_event(AgentSystemEvent::Run {
            address: address.clone(),
            event: RunEvent::ModelDelta {
                run_id: first_run.clone(),
                delta: ModelDelta::Text("Checking the workspace".to_owned()),
            },
        });
        let first_text = app.entries.len() - 1;
        assert!(app.entries[first_text].expanded);

        app.apply_agent_event(AgentSystemEvent::Run {
            address: address.clone(),
            event: RunEvent::ModelDelta {
                run_id: first_run,
                delta: ModelDelta::ToolCall(ToolCallDelta {
                    index: 0,
                    call_id: Some("call-1".to_owned()),
                    name: Some("eval".to_owned()),
                    arguments: "{\"source\":".to_owned(),
                }),
            },
        });
        assert!(app.entries[first_text].expanded);
        app.apply_agent_event(AgentSystemEvent::Run {
            address,
            event: RunEvent::ModelDelta {
                run_id: RunId::new("run-2").unwrap(),
                delta: ModelDelta::ToolCall(ToolCallDelta {
                    index: 0,
                    call_id: Some("call-2".to_owned()),
                    name: Some("eval".to_owned()),
                    arguments: "{\"source\":".to_owned(),
                }),
            },
        });
        assert!(app.entries[first_text].expanded);
    }

    #[test]
    fn streaming_marker_follows_the_active_row_and_clears_on_completion() {
        let mut app = app();
        let address = ActorAddress::new("/root").unwrap();
        let run_id = RunId::new("run-1").unwrap();

        app.apply_agent_event(AgentSystemEvent::Run {
            address: address.clone(),
            event: RunEvent::ModelDelta {
                run_id: run_id.clone(),
                delta: ModelDelta::Reasoning("thinking".to_owned()),
            },
        });
        let reasoning = app.entries.len() - 1;
        assert!(app.entries[reasoning].streaming);

        app.apply_agent_event(AgentSystemEvent::Run {
            address: address.clone(),
            event: RunEvent::ModelDelta {
                run_id: run_id.clone(),
                delta: ModelDelta::ToolCall(ToolCallDelta {
                    index: 0,
                    call_id: Some("call-1".to_owned()),
                    name: Some("eval".to_owned()),
                    arguments: r#"{"source":"1 + 1"}"#.to_owned(),
                }),
            },
        });
        let call = app.entries.len() - 1;
        assert!(!app.entries[reasoning].streaming);
        assert!(app.entries[call].streaming);

        // The turn and its eval outcome commit: the streamed rows are
        // replaced and the marker stays on the run's newest committed row.
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![
                    committed(
                        HistoryKind::ToolCall,
                        "/root · eval",
                        "1 + 1",
                        Some("run-1"),
                    ),
                    committed(
                        HistoryKind::ToolResult,
                        "/root · eval result",
                        "2",
                        Some("run-1"),
                    ),
                ],
                model_turns: vec!["run-1".to_owned()],
                active_run: Some("run-1".to_owned()),
                ..FoldOutcome::default()
            },
        );
        let call = app
            .entries
            .iter()
            .position(|entry| entry.kind == EntryKind::ToolCall)
            .unwrap();
        let result = app
            .entries
            .iter()
            .position(|entry| entry.kind == EntryKind::ToolResult)
            .unwrap();
        assert!(!app.entries[call].streaming);
        assert!(
            app.entries[result].streaming,
            "the run's newest committed row keeps full intensity"
        );

        app.apply_agent_event(AgentSystemEvent::Run {
            address: address.clone(),
            event: RunEvent::ModelDelta {
                run_id: run_id.clone(),
                delta: ModelDelta::Text("The answer is 2.".to_owned()),
            },
        });
        let text = app.entries.len() - 1;
        assert!(!app.entries[result].streaming);
        assert!(app.entries[text].streaming);

        app.apply_agent_event(AgentSystemEvent::Run {
            address,
            event: RunEvent::Completed { run_id },
        });
        assert!(app.entries.iter().all(|entry| !entry.streaming));
    }

    #[test]
    fn terminal_fold_recovers_when_the_completed_event_is_dropped() {
        let mut app = app();
        let address = ActorAddress::new("/root").unwrap();
        let run_id = RunId::new("run-1").unwrap();
        app.root_run_active = true;
        app.busy = true;
        app.status = "/root is responding…".to_owned();
        app.apply_agent_event(AgentSystemEvent::Run {
            address: address.clone(),
            event: RunEvent::ModelDelta {
                run_id: run_id.clone(),
                delta: ModelDelta::Text("Final ans".to_owned()),
            },
        });

        // The terminal turn commits; the Completed event is lost in transit.
        // The fold alone restores every invariant.
        app.apply_fold(
            "/root",
            FoldOutcome {
                rows: vec![committed(
                    HistoryKind::Assistant,
                    "/root",
                    "Final answer",
                    Some("run-1"),
                )],
                model_turns: vec!["run-1".to_owned()],
                dead_runs: vec!["run-1".to_owned()],
                ..FoldOutcome::default()
            },
        );
        assert!(!app.busy);
        assert!(!app.root_run_active);
        assert_eq!(app.entries.last().unwrap().body, "Final answer");
        assert!(app.entries.last().unwrap().expanded);

        app.apply_agent_event(AgentSystemEvent::Run {
            address: address.clone(),
            event: RunEvent::ModelStarted {
                run_id: run_id.clone(),
            },
        });
        app.apply_agent_event(AgentSystemEvent::Run {
            address,
            event: RunEvent::ModelDelta {
                run_id,
                delta: ModelDelta::Text("wer".to_owned()),
            },
        });
        assert_eq!(app.entries.last().unwrap().body, "Final answer");
    }

    #[test]
    fn child_streams_accumulate_without_moving_the_selected_agent() {
        let mut app = app();
        app.focus = Focus::Conversation;
        app.selected_entry = Some(0);
        app.follow_conversation_tail = false;
        app.conversation_offset = 3;
        let root_entries = app.entries.len();

        let child = ActorAddress::new("/root/researcher").unwrap();
        assert!(!app.apply_agent_event(AgentSystemEvent::Hosted {
            address: child.clone(),
            parent: Some(ActorAddress::new("/root").unwrap()),
        }));
        assert!(!app.apply_agent_event(AgentSystemEvent::Run {
            address: child.clone(),
            event: RunEvent::ModelDelta {
                run_id: RunId::new("child-run").unwrap(),
                delta: ModelDelta::Text("hello".to_owned()),
            },
        }));

        assert_eq!(app.current_agent, "/root");
        assert_eq!(app.entries.len(), root_entries);
        assert_eq!(app.selected_entry, Some(0));
        assert_eq!(app.conversation_offset, 3);

        app.focus = Focus::Input;
        app.input.text = "/agents /root/researcher".to_owned();
        app.input.cursor = app.input.char_count();
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(app.current_agent, "/root/researcher");
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].body, "hello");
        assert!(app.entries[0].expanded);

        assert!(app.apply_agent_event(AgentSystemEvent::Run {
            address: child,
            event: RunEvent::ModelDelta {
                run_id: RunId::new("child-run").unwrap(),
                delta: ModelDelta::Text(" world".to_owned()),
            },
        }));
        assert_eq!(app.entries[0].body, "hello world");
    }

    #[test]
    fn agents_command_remains_available_while_root_is_busy() {
        let mut app = app();
        app.apply_agent_event(AgentSystemEvent::Hosted {
            address: ActorAddress::new("/root/worker").unwrap(),
            parent: Some(ActorAddress::new("/root").unwrap()),
        });
        app.busy = true;
        app.input.text = "/agents /root/worker".to_owned();
        app.input.cursor = app.input.char_count();

        let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(command.is_none());
        assert_eq!(app.current_agent, "/root/worker");
        assert!(app.busy);
    }

    #[test]
    fn switching_agents_tracks_the_child_model_in_the_header() {
        let mut app = app();
        app.models.push(ModelChoice {
            registry_id: "fireworks/deepseek-v4-flash".to_owned(),
            provider: "fireworks".to_owned(),
            model: "deepseek-v4-flash".to_owned(),
            display_name: "DeepSeek V4 Flash".to_owned(),
            context_window: 128_000,
            efforts: vec!["none".to_owned(), "high".to_owned()],
        });
        app.selected_efforts.push(1);

        app.apply_agent_event(AgentSystemEvent::Hosted {
            address: ActorAddress::new("/root/worker").unwrap(),
            parent: Some(ActorAddress::new("/root").unwrap()),
        });
        app.apply_fold(
            "/root/worker",
            FoldOutcome {
                selected_model: Some("fireworks/deepseek-v4-flash".to_owned()),
                ..FoldOutcome::default()
            },
        );

        app.input.text = "/agents /root/worker".to_owned();
        app.input.cursor = app.input.char_count();
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );

        assert_eq!(app.current_agent, "/root/worker");
        assert_eq!(
            app.current_agent_model(),
            Some("fireworks/deepseek-v4-flash")
        );
        assert!(
            app.agent_detail("/root/worker")
                .contains("fireworks/deepseek-v4-flash")
        );

        // Root keeps its own selected model.
        app.input.text = "/agents /root".to_owned();
        app.input.cursor = app.input.char_count();
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(app.current_agent_model(), Some("openai/gpt-5"));
    }

    #[test]
    fn background_streaming_preserves_click_targets_in_the_visible_conversation() {
        let mut app = app();
        app.push_entry(
            EntryKind::ToolCall,
            "/root · Inspect the workspace",
            "const files = await lam.fs.list({ path: '.' });",
        );
        let tool = app.entries.len() - 1;
        let child = ActorAddress::new("/root/worker").unwrap();
        app.apply_agent_event(AgentSystemEvent::Hosted {
            address: child.clone(),
            parent: Some(ActorAddress::new("/root").unwrap()),
        });
        app.hitboxes = vec![Hitbox {
            top: 4,
            bottom: 4,
            header: Some(4),
            entry: tool,
        }];
        app.busy = true;

        assert!(!app.apply_agent_event(AgentSystemEvent::Run {
            address: child,
            event: RunEvent::ModelDelta {
                run_id: RunId::new("child-run").unwrap(),
                delta: ModelDelta::Text("streaming in the background".to_owned()),
            },
        }));
        assert_eq!(
            app.hitboxes,
            [Hitbox {
                top: 4,
                bottom: 4,
                header: Some(4),
                entry: tool
            }]
        );

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.selected_entry, Some(tool));
        assert!(app.entries[tool].expanded);
    }

    #[test]
    fn body_click_selects_without_collapsing() {
        let mut app = app();
        app.push_entry(EntryKind::System, "Expanded", "first line\nsecond line");
        let entry = app.entries.len() - 1;
        app.entries[entry].expanded = true;
        app.hitboxes = vec![Hitbox {
            top: 2,
            bottom: 4,
            header: Some(2),
            entry,
        }];

        app.follow_conversation_tail = true;
        app.selection_drives_viewport = true;
        app.conversation_offset = 7;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.focus, Focus::Conversation);
        assert_eq!(app.selected_entry, Some(entry));
        assert!(app.entries[entry].expanded);
        // Preserve scroll so tall expanded bodies stay put for copy selection.
        assert!(!app.follow_conversation_tail);
        assert!(!app.selection_drives_viewport);
        assert_eq!(app.conversation_offset, 7);
    }

    #[test]
    fn body_click_on_last_entry_does_not_enable_follow_tail() {
        let mut app = app();
        // Ensure the clicked entry is the last one — the previous bug set
        // follow_conversation_tail whenever entry + 1 == len.
        app.push_entry(
            EntryKind::Assistant,
            "Long",
            "line
"
            .repeat(40),
        );
        let entry = app.entries.len() - 1;
        app.entries[entry].expanded = true;
        app.follow_conversation_tail = true;
        app.selection_drives_viewport = true;
        app.conversation_offset = 12;
        app.hitboxes = vec![Hitbox {
            top: 5,
            bottom: 20,
            header: Some(5),
            entry,
        }];

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 8,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.selected_entry, Some(entry));
        assert_eq!(entry + 1, app.entries.len());
        assert!(!app.follow_conversation_tail);
        assert!(!app.selection_drives_viewport);
        assert_eq!(app.conversation_offset, 12);
    }

    #[test]
    fn keyboard_expand_requests_reveal_near_top() {
        let mut app = app();
        app.push_entry(
            EntryKind::Assistant,
            "Long",
            "body line
"
            .repeat(20),
        );
        let entry = app.entries.len() - 1;
        app.selected_entry = Some(entry);
        app.focus = Focus::Conversation;
        app.follow_conversation_tail = true;
        app.entries[entry].expanded = false;

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.entries[entry].expanded);
        assert_eq!(app.reveal_entry_top, Some(entry));
        assert!(!app.follow_conversation_tail);
        assert!(!app.selection_drives_viewport);
    }

    #[test]
    fn header_click_expand_does_not_request_reveal() {
        let mut app = app();
        app.push_entry(EntryKind::Assistant, "Long", "body");
        let entry = app.entries.len() - 1;
        app.entries[entry].expanded = false;
        app.hitboxes = vec![Hitbox {
            top: 2,
            bottom: 2,
            header: Some(2),
            entry,
        }];

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });

        assert!(app.entries[entry].expanded);
        assert_eq!(app.reveal_entry_top, None);
    }

    #[test]
    fn header_click_toggles_expand_and_collapse() {
        let mut app = app();
        app.push_entry(EntryKind::System, "Expanded", "body");
        let entry = app.entries.len() - 1;
        app.entries[entry].expanded = true;
        app.hitboxes = vec![Hitbox {
            top: 2,
            bottom: 2,
            header: Some(2),
            entry,
        }];

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!app.entries[entry].expanded);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.entries[entry].expanded);
    }

    #[test]
    fn body_click_with_scrolled_off_header_does_not_toggle() {
        let mut app = app();
        app.push_entry(EntryKind::System, "Expanded", "first line\nsecond line");
        let entry = app.entries.len() - 1;
        app.entries[entry].expanded = true;
        // The header is above the viewport; only body rows are clickable.
        app.hitboxes = vec![Hitbox {
            top: 2,
            bottom: 3,
            header: None,
            entry,
        }];

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.selected_entry, Some(entry));
        assert!(app.entries[entry].expanded);
    }

    fn selection_app() -> App {
        let mut app = app();
        app.conversation_area = Some(Rect {
            x: 2,
            y: 2,
            width: 40,
            height: 8,
        });
        app.conversation_rows = vec![
            CopyRow {
                pad: 0,
                text: "first line".to_owned(),
            },
            CopyRow {
                pad: 0,
                text: "second line".to_owned(),
            },
            CopyRow {
                pad: 0,
                text: "third line".to_owned(),
            },
            CopyRow {
                pad: 0,
                text: String::new(),
            },
        ];
        app
    }

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn drag_selects_visible_rows_and_returns_them_on_release() {
        let mut app = selection_app();
        app.handle_mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 3));
        app.handle_mouse(mouse_event(MouseEventKind::Drag(MouseButton::Left), 12, 5));
        let copied = app.handle_mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), 12, 5));
        assert_eq!(copied.as_deref(), Some("ond line\nthird line\n"));
        assert!(app.text_selection.is_none());
    }

    #[test]
    fn dragging_upwards_selects_backwards() {
        let mut app = selection_app();
        app.handle_mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), 12, 5));
        app.handle_mouse(mouse_event(MouseEventKind::Drag(MouseButton::Left), 5, 3));
        let copied = app.handle_mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), 5, 3));
        assert_eq!(copied.as_deref(), Some("ond line\nthird line\n"));
    }

    #[test]
    fn plain_click_copies_nothing() {
        let mut app = selection_app();
        app.handle_mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 3));
        assert!(
            app.handle_mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), 5, 3))
                .is_none()
        );
        assert!(app.text_selection.is_none());
    }

    #[test]
    fn header_click_toggles_without_arming_selection() {
        let mut app = selection_app();
        app.push_entry(EntryKind::System, "Expanded", "body");
        let entry = app.entries.len() - 1;
        app.entries[entry].expanded = true;
        app.hitboxes = vec![Hitbox {
            top: 2,
            bottom: 2,
            header: Some(2),
            entry,
        }];
        app.handle_mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 2));
        assert!(!app.entries[entry].expanded);
        assert!(app.text_selection.is_none());
    }

    #[test]
    fn body_click_selects_the_entry_and_arms_selection() {
        let mut app = selection_app();
        app.push_entry(EntryKind::System, "Expanded", "first line\nsecond line");
        let entry = app.entries.len() - 1;
        app.entries[entry].expanded = true;
        app.hitboxes = vec![Hitbox {
            top: 2,
            bottom: 4,
            header: Some(2),
            entry,
        }];
        app.handle_mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 3));
        assert_eq!(app.selected_entry, Some(entry));
        assert_eq!(
            app.text_selection,
            Some(TextSelection {
                anchor: CellPos { row: 1, col: 3 },
                head: CellPos { row: 1, col: 3 },
            })
        );
    }

    #[test]
    fn click_outside_the_conversation_clears_selection() {
        let mut app = selection_app();
        app.input_area = Some(Rect {
            x: 2,
            y: 10,
            width: 60,
            height: 1,
        });
        app.text_selection = Some(TextSelection {
            anchor: CellPos { row: 1, col: 0 },
            head: CellPos { row: 2, col: 4 },
        });
        app.handle_mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 10));
        assert!(app.text_selection.is_none());
        assert_eq!(app.focus, Focus::Input);
    }

    #[test]
    fn wheel_scroll_clears_selection() {
        let mut app = selection_app();
        app.text_selection = Some(TextSelection {
            anchor: CellPos { row: 1, col: 0 },
            head: CellPos { row: 2, col: 4 },
        });
        app.handle_mouse(mouse_event(MouseEventKind::ScrollDown, 0, 0));
        assert!(app.text_selection.is_none());
    }

    #[test]
    fn keystroke_clears_selection() {
        let mut app = selection_app();
        app.text_selection = Some(TextSelection {
            anchor: CellPos { row: 1, col: 0 },
            head: CellPos { row: 2, col: 4 },
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.text_selection.is_none());
    }

    #[test]
    fn selected_text_joins_rows_and_trims_trailing_whitespace() {
        let rows = vec![
            CopyRow {
                pad: 0,
                text: "abcdef".to_owned(),
            },
            CopyRow {
                pad: 0,
                text: "ghijkl   ".to_owned(),
            },
        ];
        assert_eq!(
            selected_text(
                &rows,
                CellPos { row: 0, col: 1 },
                CellPos { row: 1, col: 2 }
            ),
            "bcdef\nghi"
        );
    }

    #[test]
    fn selected_text_slices_wide_characters_by_cells() {
        let rows = vec![CopyRow {
            pad: 0,
            text: "ab界cd".to_owned(),
        }];
        assert_eq!(
            selected_text(
                &rows,
                CellPos { row: 0, col: 0 },
                CellPos { row: 0, col: 3 }
            ),
            "ab界"
        );
        assert_eq!(
            selected_text(
                &rows,
                CellPos { row: 0, col: 0 },
                CellPos { row: 0, col: 1 }
            ),
            "ab"
        );
        assert_eq!(
            selected_text(
                &rows,
                CellPos { row: 0, col: 4 },
                CellPos { row: 0, col: 5 }
            ),
            "cd"
        );
    }

    #[test]
    fn selected_text_normalizes_reverse_drags() {
        let rows = vec![CopyRow {
            pad: 0,
            text: "abcdef".to_owned(),
        }];
        assert_eq!(
            selected_text(
                &rows,
                CellPos { row: 0, col: 4 },
                CellPos { row: 0, col: 1 }
            ),
            "bcde"
        );
    }

    #[test]
    fn selected_text_out_of_bounds_is_empty() {
        let rows = vec![CopyRow {
            pad: 0,
            text: "abc".to_owned(),
        }];
        assert_eq!(
            selected_text(
                &rows,
                CellPos { row: 5, col: 0 },
                CellPos { row: 6, col: 1 }
            ),
            ""
        );
    }

    #[test]
    fn selected_text_omits_presentation_padding() {
        let rows = vec![
            CopyRow {
                pad: 4,
                text: "    Hello".to_owned(),
            },
            CopyRow {
                pad: 4,
                text: "    There".to_owned(),
            },
        ];
        assert_eq!(
            selected_text(
                &rows,
                CellPos { row: 0, col: 0 },
                CellPos { row: 1, col: 9 }
            ),
            "Hello\nThere"
        );
    }

    #[test]
    fn selected_text_keeps_content_level_indentation() {
        let rows = vec![CopyRow {
            pad: 4,
            text: "    let x = 1".to_owned(),
        }];
        assert_eq!(
            selected_text(
                &rows,
                CellPos { row: 0, col: 0 },
                CellPos { row: 0, col: 12 }
            ),
            "let x = 1"
        );
    }

    #[test]
    fn selected_text_starting_inside_padding_skips_it() {
        let rows = vec![CopyRow {
            pad: 4,
            text: "    Hello".to_owned(),
        }];
        assert_eq!(
            selected_text(
                &rows,
                CellPos { row: 0, col: 2 },
                CellPos { row: 0, col: 8 }
            ),
            "Hello"
        );
    }

    #[test]
    fn selected_text_header_furniture_copies_nothing() {
        let rows = vec![CopyRow {
            pad: 10,
            text: " ▾ you you".to_owned(),
        }];
        assert_eq!(
            selected_text(
                &rows,
                CellPos { row: 0, col: 0 },
                CellPos { row: 0, col: 9 }
            ),
            ""
        );
    }

    #[test]
    fn show_toast_records_text() {
        let mut app = app();
        app.show_toast("Copied selection".to_owned());
        assert_eq!(app.toast.as_ref().unwrap().text, "Copied selection");
    }

    #[test]
    fn model_completed_records_provider_usage() {
        let mut app = app();
        app.apply_agent_event(AgentSystemEvent::Run {
            address: ActorAddress::new("/root").unwrap(),
            event: RunEvent::ModelCompleted {
                run_id: RunId::new("run-1").unwrap(),
                metadata: ModelResponseMetadata {
                    usage: Some(TokenUsage {
                        input_tokens: 10_000,
                        cached_input_tokens: None,
                        output_tokens: 2_345,
                        reasoning_tokens: None,
                        total_tokens: 12_345,
                        native: serde_json::Value::Null,
                    }),
                    ..ModelResponseMetadata::default()
                },
            },
        });
        assert_eq!(app.context_tokens, Some(12_345));
    }

    #[test]
    fn apply_fold_seeds_context_tokens() {
        let mut app = app();
        app.apply_fold(
            "/root",
            FoldOutcome {
                context_tokens: Some(42),
                ..FoldOutcome::default()
            },
        );
        assert_eq!(app.context_tokens, Some(42));
    }

    #[test]
    fn empty_fold_preserves_existing_context_tokens() {
        let mut app = app();
        app.context_tokens = Some(42);
        // The projector already caught up at startup, so live folds report
        // no new usage; the seeded value must survive them.
        app.apply_fold("/root", FoldOutcome::default());
        assert_eq!(app.context_tokens, Some(42));
    }

    #[test]
    fn context_tokens_travel_with_the_agent() {
        let mut app = app();
        app.context_tokens = Some(7);
        app.ensure_agent("/root/worker", Some("/root".to_owned()), "Ready");
        app.with_agent("/root/worker", |app| {
            assert_eq!(app.context_tokens, None);
            app.context_tokens = Some(9);
        });
        assert_eq!(app.context_tokens, Some(7));
        app.with_agent("/root/worker", |app| {
            assert_eq!(app.context_tokens, Some(9))
        });
    }

    #[test]
    fn agent_picker_is_hierarchical_and_keeps_child_outcomes_out_of_root() {
        let mut app = app();
        let worker = ActorAddress::new("/root/worker").unwrap();
        let nested = ActorAddress::new("/root/worker/scout").unwrap();
        for (address, parent) in [
            (worker.clone(), ActorAddress::new("/root").unwrap()),
            (nested, worker.clone()),
        ] {
            app.apply_agent_event(AgentSystemEvent::Hosted {
                address,
                parent: Some(parent),
            });
        }
        let root_entries = app.entries.len();
        app.apply_agent_event(AgentSystemEvent::Outcome {
            outcome: AgentOutcome::Completed {
                address: worker,
                message_id: "message-1".to_owned(),
                output: "child result".to_owned(),
            },
        });
        assert_eq!(app.entries.len(), root_entries);

        app.input.text = "/agents ".to_owned();
        app.input.cursor = app.input.char_count();
        let suggestions = app.suggestions();
        assert_eq!(
            suggestions
                .iter()
                .map(|suggestion| suggestion.label.as_str())
                .collect::<Vec<_>>(),
            ["◉ [root]", "  └─ worker", "    └─ scout"]
        );
        assert!(suggestions[0].label.contains("[root]"));
        assert!(!suggestions[0].running);
    }

    #[test]
    fn agent_picker_orders_siblings_newest_created_first() {
        let mut app = app();
        let older = ActorAddress::new("/root/older").unwrap();
        let newer = ActorAddress::new("/root/newer").unwrap();
        let root = ActorAddress::new("/root").unwrap();
        app.apply_agent_event(AgentSystemEvent::Hosted {
            address: older,
            parent: Some(root.clone()),
        });
        app.apply_agent_event(AgentSystemEvent::Hosted {
            address: newer,
            parent: Some(root),
        });
        app.open_agents_drawer();
        let labels = app
            .suggestions()
            .iter()
            .map(|suggestion| suggestion.label.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![
                "◉ [root]".to_owned(),
                "  └─ newer".to_owned(),
                "  └─ older".to_owned(),
            ],
            "siblings should list newest-created first: {labels:?}"
        );
    }

    #[test]
    fn slash_suggestions_remain_available_while_other_agents_run() {
        let mut app = app();
        let worker = ActorAddress::new("/root/worker").unwrap();
        app.apply_agent_event(AgentSystemEvent::Hosted {
            address: worker.clone(),
            parent: Some(ActorAddress::new("/root").unwrap()),
        });
        app.apply_agent_event(AgentSystemEvent::Run {
            address: worker,
            event: RunEvent::Started {
                run_id: RunId::new("run-worker").unwrap(),
            },
        });
        assert!(app.agents_collapsed_visible());
        assert!(app.agents_drawer_animates());

        // Slash command discovery must not be suppressed by the agents strip.
        app.input.text = "/m".to_owned();
        app.input.cursor = app.input.char_count();
        let suggestions = app.suggestions();
        assert!(
            suggestions
                .iter()
                .any(|suggestion| suggestion.replacement.starts_with("/model")),
            "expected /model suggestions while agents run, got {suggestions:?}"
        );
        assert!(!app.agents_palette_open());
        assert!(app.agents_collapsed_visible());
    }

    #[test]
    fn agents_collapsed_strip_tracks_other_running_agents() {
        let mut app = app();
        let worker = ActorAddress::new("/root/worker").unwrap();
        let scout = ActorAddress::new("/root/scout").unwrap();
        for address in [worker.clone(), scout.clone()] {
            app.apply_agent_event(AgentSystemEvent::Hosted {
                address: address.clone(),
                parent: Some(ActorAddress::new("/root").unwrap()),
            });
        }
        assert!(!app.agents_collapsed_visible());

        app.apply_agent_event(AgentSystemEvent::Run {
            address: worker.clone(),
            event: RunEvent::Started {
                run_id: RunId::new("run-worker").unwrap(),
            },
        });
        assert!(app.agent_is_running("/root/worker"));
        assert!(app.agents_collapsed_visible());
        let summary = app.agents_collapsed_summary(80);
        assert!(summary.contains("1 agent running"), "{summary}");
        assert!(summary.contains("worker"), "{summary}");

        app.apply_agent_event(AgentSystemEvent::Run {
            address: scout,
            event: RunEvent::Started {
                run_id: RunId::new("run-scout").unwrap(),
            },
        });
        let summary = app.agents_collapsed_summary(80);
        assert!(summary.starts_with("2 agents running"), "{summary}");
        assert!(summary.contains("scout"), "{summary}");

        // Viewing a running child hides it from the ambient strip.
        app.input.text = "/agents /root/worker".to_owned();
        app.input.cursor = app.input.char_count();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.current_agent, "/root/worker");
        assert_eq!(app.other_running_agents(), vec!["/root/scout".to_owned()]);
        let summary = app.agents_collapsed_summary(80);
        assert!(summary.contains("1 agent running"), "{summary}");
        assert!(summary.contains("scout"), "{summary}");
        assert!(!summary.contains("worker"), "{summary}");
    }

    #[test]
    fn alt_a_opens_the_agents_drawer() {
        let mut app = app();
        app.focus = Focus::Conversation;
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT));
        assert_eq!(app.focus, Focus::Input);
        assert_eq!(app.input.text, "/agents ");
        assert!(app.agents_palette_open());
    }

    #[test]
    fn enter_on_an_agent_suggestion_switches_immediately() {
        let mut app = app();
        let worker = ActorAddress::new("/root/worker").unwrap();
        app.apply_agent_event(AgentSystemEvent::Hosted {
            address: worker,
            parent: Some(ActorAddress::new("/root").unwrap()),
        });
        app.open_agents_drawer();
        // Select the worker row (index 1: root, worker).
        app.suggestion_index = 1;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.current_agent, "/root/worker");
        assert!(app.input.text.is_empty());
        assert!(!app.agents_palette_open());
    }

    #[test]
    fn agents_palette_marks_running_and_current_and_esc_dismisses() {
        let mut app = app();
        let worker = ActorAddress::new("/root/worker").unwrap();
        app.apply_agent_event(AgentSystemEvent::Hosted {
            address: worker.clone(),
            parent: Some(ActorAddress::new("/root").unwrap()),
        });
        app.apply_agent_event(AgentSystemEvent::Run {
            address: worker,
            event: RunEvent::Started {
                run_id: RunId::new("run-worker").unwrap(),
            },
        });
        app.input.text = "/agents ".to_owned();
        app.input.cursor = app.input.char_count();
        assert!(app.agents_palette_open());
        assert!(!app.agents_collapsed_visible());

        let suggestions = app.suggestions();
        let worker = suggestions
            .iter()
            .find(|suggestion| suggestion.replacement == "/agents /root/worker")
            .expect("worker row");
        assert!(worker.running);
        assert!(worker.label.contains('●'));
        assert!(
            worker.detail.contains("working")
                || worker.detail.contains("Working")
                || !worker.detail.is_empty()
        );

        let root = suggestions
            .iter()
            .find(|suggestion| suggestion.replacement == "/agents /root")
            .expect("root row");
        assert!(root.label.contains("[root]"));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.agents_palette_open());
        assert!(app.input.text.is_empty());
        // Esc dismisses the drawer without arming interruption when root is idle.
        assert!(app.interruption_deadline().is_none());
    }

    #[test]
    fn agents_palette_keeps_completed_rows_and_clears_running_marker() {
        let mut app = app();
        let worker = ActorAddress::new("/root/worker").unwrap();
        app.apply_agent_event(AgentSystemEvent::Hosted {
            address: worker.clone(),
            parent: Some(ActorAddress::new("/root").unwrap()),
        });
        app.apply_agent_event(AgentSystemEvent::Run {
            address: worker.clone(),
            event: RunEvent::Started {
                run_id: RunId::new("run-worker").unwrap(),
            },
        });
        app.apply_agent_event(AgentSystemEvent::Run {
            address: worker.clone(),
            event: RunEvent::Completed {
                run_id: RunId::new("run-worker").unwrap(),
            },
        });
        app.input.text = "/agents ".to_owned();
        app.input.cursor = app.input.char_count();
        let suggestions = app.suggestions();
        assert_eq!(suggestions.len(), 2);
        let worker_row = &suggestions[1];
        assert!(!worker_row.running);
        assert!(!worker_row.label.contains('●'));
        assert!(
            worker_row.detail.contains("Complete"),
            "{}",
            worker_row.detail
        );
        assert!(!app.agents_collapsed_visible());
    }
}
