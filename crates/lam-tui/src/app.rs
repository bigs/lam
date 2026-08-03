use std::collections::BTreeMap;

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use lam::{ModelDelta, RunEvent, RunId, RuntimeEvent, ToolCallDelta};
use lam_agents::{AgentOutcome, AgentSystemEvent};
use unicode_width::UnicodeWidthChar;

use crate::config::ModelChoice;
use crate::runtime::{
    AgentHistory, Command, CommandResult, CompletedCall, HistoryEntry, HistoryKind,
};

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
    model_owner: Option<String>,
    model_run: Option<String>,
    tool_owner: Option<String>,
    tool_run: Option<String>,
    tool_index: Option<usize>,
    tool_name: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Suggestion {
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) replacement: String,
    pub(crate) provider: Option<String>,
}

pub(crate) struct SessionView {
    pub(crate) id: u64,
    pub(crate) journal_path: String,
    pub(crate) resumed: bool,
    pub(crate) agents: Vec<AgentHistory>,
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

pub(crate) struct App {
    pub(crate) cwd: String,
    pub(crate) session_id: u64,
    pub(crate) models: Vec<ModelChoice>,
    pub(crate) selected_model: usize,
    selected_efforts: Vec<usize>,
    pub(crate) focus: Focus,
    pub(crate) input: InputBuffer,
    pub(crate) input_width: usize,
    pub(crate) entries: Vec<ConversationEntry>,
    pub(crate) selected_entry: Option<usize>,
    pub(crate) conversation_offset: usize,
    pub(crate) follow_conversation_tail: bool,
    pub(crate) suggestion_index: usize,
    pub(crate) busy: bool,
    pub(crate) status: String,
    pub(crate) should_exit: bool,
    pub(crate) hitboxes: Vec<(u16, u16, usize)>,
    pub(crate) current_agent: String,
    input_history: Option<InputHistory>,
    current_parent: Option<String>,
    current_model: Option<String>,
    inactive_agents: BTreeMap<String, AgentConversation>,
    output_fallback: Option<OutputFallback>,
    root_run_completed: bool,
    completed_run_id: Option<String>,
}

struct AgentConversation {
    entries: Vec<ConversationEntry>,
    selected_entry: Option<usize>,
    conversation_offset: usize,
    follow_conversation_tail: bool,
    status: String,
    parent: Option<String>,
    model: Option<String>,
    output_fallback: Option<OutputFallback>,
    run_completed: bool,
    completed_run_id: Option<String>,
}

struct OutputFallback {
    entry_index: usize,
    output: String,
    streamed: String,
}

impl App {
    pub(crate) fn new(
        cwd: String,
        config_path: String,
        session: SessionView,
        models: Vec<ModelChoice>,
        selected_model: usize,
    ) -> Self {
        let action = if session.resumed {
            "Resumed"
        } else {
            "Started"
        };
        let session_id = session.id;
        let session_path = session.journal_path;
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
        let mut entries = history_entries(root.history);
        entries.push(ConversationEntry {
            kind: EntryKind::System,
            title: "Ready".to_owned(),
            body: ready_message,
            expanded: false,
            pending_tool: false,
            model_owner: None,
            model_run: None,
            tool_owner: None,
            tool_run: None,
            tool_index: None,
            tool_name: String::new(),
        });
        let selected_entry = entries.len().checked_sub(1);
        let selected_efforts = models.iter().map(|model| model.efforts.len() - 1).collect();
        let inactive_agents = agents
            .into_iter()
            .map(|agent| {
                let address = agent.address.clone();
                (address, AgentConversation::from_history(agent))
            })
            .collect();
        Self {
            cwd,
            session_id,
            models,
            selected_model,
            selected_efforts,
            focus: Focus::Input,
            input: InputBuffer::default(),
            input_width: 80,
            entries,
            selected_entry,
            conversation_offset: 0,
            follow_conversation_tail: true,
            suggestion_index: 0,
            busy: false,
            status: root.status,
            should_exit: false,
            hitboxes: Vec::new(),
            current_agent: root.address,
            input_history: None,
            current_parent: root.parent,
            current_model: root.model,
            inactive_agents,
            output_fallback: None,
            root_run_completed: root.run_completed,
            completed_run_id: None,
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
                })
                .collect();
        }
        if input == "/agents" || input.starts_with("/agents ") {
            let query = input
                .strip_prefix("/agents")
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            return self
                .agent_addresses()
                .into_iter()
                .filter(|address| query.is_empty() || address.to_lowercase().contains(&query))
                .map(|address| Suggestion {
                    label: agent_tree_label(&address),
                    detail: self.agent_detail(&address),
                    replacement: format!("/agents {address}"),
                    provider: None,
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
        })
        .collect()
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<Command> {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return None;
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
            KeyCode::Enter if !suggestions.is_empty() => {
                let selected = &suggestions[self.suggestion_index];
                if self.input.text.trim_end() != selected.replacement.trim_end()
                    || selected.replacement.ends_with(' ')
                {
                    self.apply_suggestion(suggestions);
                    None
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
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.input_history = None;
                self.input.insert(character);
                self.suggestion_index = 0;
                None
            }
            KeyCode::Esc => {
                self.input = InputBuffer::default();
                self.input_history = None;
                self.suggestion_index = 0;
                None
            }
            _ => None,
        }
    }

    fn handle_conversation_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc => self.focus = Focus::Input,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-8),
            KeyCode::PageDown => self.move_selection(8),
            KeyCode::Home => {
                self.selected_entry = (!self.entries.is_empty()).then_some(0);
                self.follow_conversation_tail = false;
            }
            KeyCode::End => {
                self.selected_entry = self.entries.len().checked_sub(1);
                self.follow_conversation_tail = true;
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_selected(),
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
            self.input = InputBuffer::default();
            self.input_history = None;
            self.suggestion_index = 0;
            self.input = InputBuffer::at_end("/agents ".to_owned());
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
        if self.busy {
            self.status = "Wait for the current root operation to finish".to_owned();
            return None;
        }
        self.input = InputBuffer::default();
        self.input_history = None;
        self.suggestion_index = 0;
        self.switch_agent("/root");
        match input.as_str() {
            "/compact" => {
                self.busy = true;
                self.status = "Compacting context…".to_owned();
                Some(Command::Compact)
            }
            "/new" => {
                self.busy = true;
                self.status = "Starting a new session…".to_owned();
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
                self.busy = true;
                self.status = format!("Switching reasoning effort to {effort}…");
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
                self.busy = true;
                self.status = format!("Switching to {}…", self.models[index].display_name);
                Some(Command::SwitchModel {
                    index,
                    registry_id: self.models[index].registry_id.clone(),
                })
            }
            _ if input.starts_with('/') => {
                self.push_error(
                    "Unknown command",
                    format!("`{input}` is not a Lam command."),
                );
                None
            }
            _ => {
                self.output_fallback = None;
                self.root_run_completed = false;
                self.completed_run_id = None;
                self.push_expanded_entry(EntryKind::User, "You", input.clone());
                self.busy = true;
                self.status = "Thinking…".to_owned();
                Some(Command::Call(input))
            }
        }
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

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.focus = Focus::Conversation;
                self.move_selection(-1);
            }
            MouseEventKind::ScrollDown => {
                self.focus = Focus::Conversation;
                self.move_selection(1);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, _, index)) = self
                    .hitboxes
                    .iter()
                    .find(|(start, end, _)| mouse.row >= *start && mouse.row <= *end)
                {
                    self.focus = Focus::Conversation;
                    self.selected_entry = Some(*index);
                    self.follow_conversation_tail = *index + 1 == self.entries.len();
                    self.toggle_selected();
                }
            }
            _ => {}
        }
    }

    pub(crate) fn handle_paste(&mut self, text: &str) {
        if self.focus == Focus::Input {
            self.input_history = None;
            self.input.insert_text(text);
            self.suggestion_index = 0;
        }
    }

    pub(crate) fn apply_agent_event(&mut self, event: AgentSystemEvent) -> bool {
        let picker_open = self.input.text.trim_start().starts_with("/agents");
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
                    app.push_entry(
                        EntryKind::System,
                        "Agent stopped",
                        format!("{address}: {reason:?}"),
                    );
                    app.status = "Stopped".to_owned();
                });
                address
            }
            AgentSystemEvent::Outcome { outcome } => {
                let address = outcome_address(&outcome).to_owned();
                self.ensure_agent(&address, parent_address(&address), "Finishing…");
                self.with_agent(&address, move |app| app.apply_outcome(outcome));
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
        picker_open || affected == self.current_agent
    }

    fn apply_run_event(&mut self, address: &str, event: RunEvent) {
        match event {
            RunEvent::Started { run_id } => {
                if self.is_completed_run(&run_id) {
                    return;
                }
                self.output_fallback = None;
                self.root_run_completed = false;
                self.completed_run_id = None;
                self.status = format!("{address} is working…");
            }
            RunEvent::MessagesDelivered { run_id, .. } => {
                if !self.is_completed_run(&run_id) {
                    self.status = format!("{address} is working…");
                }
            }
            RunEvent::ModelStarted { run_id } => {
                if !self.is_completed_run(&run_id) {
                    self.status = format!("{address} is thinking…");
                }
            }
            RunEvent::ModelDelta { run_id, delta } => match delta {
                ModelDelta::Text(text) => {
                    if !self.is_completed_run(&run_id) {
                        self.append_delta(EntryKind::Assistant, address, &run_id, text);
                        self.status = format!("{address} is responding…");
                    }
                }
                ModelDelta::Reasoning(text) => {
                    self.append_delta(EntryKind::Reasoning, address, &run_id, text);
                    if !self.is_completed_run(&run_id) {
                        self.status = format!("{address} is reasoning…");
                    }
                }
                ModelDelta::ToolCall(delta) => {
                    self.collapse_streamed_text(address, &run_id);
                    self.append_tool_delta(address, &run_id, delta);
                    if !self.is_completed_run(&run_id) {
                        self.status = format!("{address} is preparing a tool call…");
                    }
                }
            },
            RunEvent::ModelCompleted { .. } => {}
            RunEvent::EvalStarted {
                run_id, request, ..
            } => {
                self.collapse_streamed_text(address, &run_id);
                let body = eval_request_body(&request);
                let title = format!("{address} · {}", request.intent);
                if let Some(entry) = self.pending_tool_mut(address, &run_id) {
                    entry.tool_name = "eval".to_owned();
                    entry.title = title;
                    entry.body = body;
                } else {
                    self.push_entry(EntryKind::ToolCall, title, body);
                    if let Some(entry) = self.entries.last_mut() {
                        entry.pending_tool = true;
                        entry.tool_owner = Some(address.to_owned());
                        entry.tool_run = Some(run_id.to_string());
                        entry.tool_name = "eval".to_owned();
                    }
                }
                if !self.is_completed_run(&run_id) {
                    self.status = format!("{address} is evaluating TypeScript…");
                }
            }
            RunEvent::EvalCompleted {
                run_id, outcome, ..
            } => {
                let result = serde_json::to_string_pretty(&outcome)
                    .unwrap_or_else(|_| format!("{outcome:?}"));
                let tool_name = if let Some(entry) = self.pending_tool_mut(address, &run_id) {
                    entry.pending_tool = false;
                    entry.tool_name.clone()
                } else {
                    "eval".to_owned()
                };
                self.push_entry(
                    EntryKind::ToolResult,
                    format!("{address} · {tool_name} result"),
                    result,
                );
                if !self.is_completed_run(&run_id) {
                    self.status = format!("{address} finished eval");
                }
            }
            RunEvent::CompactionStarted { run_id, .. } => {
                if !self.is_completed_run(&run_id) {
                    self.status = format!("{address} is compacting context…");
                }
            }
            RunEvent::CompactionCompleted { covers_through, .. } => self.push_entry(
                EntryKind::System,
                "Context compacted",
                format!(
                    "{address} compacted through sequence {}.",
                    covers_through.get()
                ),
            ),
            RunEvent::CompactionFailed {
                run_id, message, ..
            } => {
                let completed = self.is_completed_run(&run_id);
                self.push_error("Compaction failed", message);
                if completed {
                    self.status = if address == "/root" {
                        "Ready".to_owned()
                    } else {
                        "Complete".to_owned()
                    };
                }
            }
            RunEvent::Completed { run_id } => {
                self.root_run_completed = true;
                self.completed_run_id = Some(run_id.to_string());
                if let Some(entry) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|entry| entry.kind == EntryKind::Assistant && entry.title == address)
                {
                    entry.expanded = true;
                }
                self.status = if address == "/root" {
                    "Ready".to_owned()
                } else {
                    "Complete".to_owned()
                };
            }
            RunEvent::Failed { message } => self.push_error("Run failed", message),
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
            AgentOutcome::Completed { output, .. } => {
                let _ = self.reconcile_root_output(output);
                self.root_run_completed = true;
                self.status = "Complete".to_owned();
            }
            AgentOutcome::Failed { address, error, .. } => {
                self.push_error(format!("{address} failed"), error);
            }
            AgentOutcome::Cancelled {
                address, reason, ..
            } => self.push_error(
                format!("{address} cancelled"),
                reason.unwrap_or_else(|| "No reason was reported.".to_owned()),
            ),
        }
    }

    pub(crate) fn apply_command_result(&mut self, result: CommandResult) {
        self.busy = false;
        let current = self.current_agent.clone();
        self.switch_agent("/root");
        match result {
            CommandResult::Call(Ok(CompletedCall { output, run_id })) => {
                self.root_run_completed = true;
                self.completed_run_id = if output.trim().is_empty() {
                    run_id
                } else {
                    self.reconcile_root_output(output).or(run_id)
                };
                self.status = "Ready".to_owned();
            }
            CommandResult::Call(Err(error)) => self.push_error("Agent failed", error),
            CommandResult::Compact(Ok(message)) => {
                self.push_entry(EntryKind::System, "Compact", message);
                self.status = "Ready".to_owned();
            }
            CommandResult::Compact(Err(error)) => self.push_error("Compaction failed", error),
            CommandResult::SwitchModel {
                index,
                result: Ok(message),
            } => {
                self.selected_model = index;
                self.current_model = Some(self.models[index].registry_id.clone());
                self.push_entry(EntryKind::System, "Model", message);
                self.status = "Ready".to_owned();
            }
            CommandResult::SwitchModel {
                result: Err(error), ..
            } => self.push_error("Model switch failed", error),
            CommandResult::SetEffort {
                index,
                effort,
                result: Ok(message),
            } => {
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
            } => self.push_error("Effort switch failed", error),
        }
        if current != "/root" {
            self.switch_agent(&current);
        }
    }

    fn append_delta(&mut self, kind: EntryKind, address: &str, run_id: &RunId, delta: String) {
        if kind == EntryKind::Assistant && self.suppress_fallback_delta(&delta) {
            return;
        }
        let title = if kind == EntryKind::Reasoning {
            format!("{address} · reasoning")
        } else {
            address.to_owned()
        };
        if let Some(entry) = self.entries.last_mut()
            && entry.kind == kind
            && entry.title == title
            && entry.model_owner.as_deref() == Some(address)
            && entry.model_run.as_deref() == Some(run_id.as_str())
        {
            entry.body.push_str(&delta);
            if kind == EntryKind::Assistant {
                entry.expanded = true;
            }
        } else {
            self.push_entry_with_expansion(kind, title, delta, kind == EntryKind::Assistant);
            if let Some(entry) = self.entries.last_mut() {
                entry.model_owner = Some(address.to_owned());
                entry.model_run = Some(run_id.to_string());
            }
        }
    }

    fn is_completed_run(&self, run_id: &RunId) -> bool {
        self.root_run_completed && self.completed_run_id.as_deref() == Some(run_id.as_str())
    }

    fn collapse_streamed_text(&mut self, address: &str, run_id: &RunId) {
        if let Some(entry) = self.entries.iter_mut().rev().find(|entry| {
            entry.kind == EntryKind::Assistant
                && entry.model_owner.as_deref() == Some(address)
                && entry.model_run.as_deref() == Some(run_id.as_str())
        }) {
            entry.expanded = false;
        }
    }

    fn reconcile_root_output(&mut self, output: String) -> Option<String> {
        if let Some(entry) =
            self.entries.iter_mut().rev().find(|entry| {
                entry.kind == EntryKind::Assistant && entry.body.trim() == output.trim()
            })
        {
            entry.expanded = true;
            return entry.model_run.clone();
        }

        let streamed_entry = self.entries.iter().enumerate().rev().find(|(_, entry)| {
            entry.kind == EntryKind::Assistant
                && entry.title == self.current_agent
                && output.starts_with(&entry.body)
        });
        let (entry_index, streamed, streamed_run_id) = if let Some((index, entry)) = streamed_entry
        {
            (index, entry.body.clone(), entry.model_run.clone())
        } else {
            self.push_entry(
                EntryKind::Assistant,
                self.current_agent.clone(),
                String::new(),
            );
            (self.entries.len() - 1, String::new(), None)
        };
        self.entries[entry_index].body.clone_from(&output);
        self.entries[entry_index].expanded = true;
        self.output_fallback = Some(OutputFallback {
            entry_index,
            output,
            streamed,
        });
        streamed_run_id
    }

    fn suppress_fallback_delta(&mut self, delta: &str) -> bool {
        let Some(fallback) = self.output_fallback.as_mut() else {
            return false;
        };
        fallback.streamed.push_str(delta);
        if fallback.output.starts_with(&fallback.streamed) {
            if let Some(entry) = self.entries.get_mut(fallback.entry_index) {
                entry.body.clone_from(&fallback.output);
            }
            return true;
        }
        self.output_fallback = None;
        false
    }

    fn append_tool_delta(&mut self, address: &str, run_id: &RunId, delta: ToolCallDelta) {
        let run_id = run_id.as_str();
        let entry = self.entries.iter_mut().rev().find(|entry| {
            entry.kind == EntryKind::ToolCall
                && entry.pending_tool
                && entry.tool_owner.as_deref() == Some(address)
                && entry.tool_run.as_deref() == Some(run_id)
                && entry.tool_index == Some(delta.index)
        });
        if let Some(entry) = entry {
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
        self.push_entry(EntryKind::ToolCall, title, delta.arguments);
        if let Some(entry) = self.entries.last_mut() {
            entry.pending_tool = true;
            entry.tool_owner = Some(address.to_owned());
            entry.tool_run = Some(run_id.to_owned());
            entry.tool_index = Some(delta.index);
            entry.tool_name = name;
            update_streamed_eval_title(entry, address);
        }
    }

    fn pending_tool_mut(
        &mut self,
        address: &str,
        run_id: &RunId,
    ) -> Option<&mut ConversationEntry> {
        self.entries.iter_mut().rev().find(|entry| {
            entry.kind == EntryKind::ToolCall
                && entry.pending_tool
                && entry.tool_owner.as_deref() == Some(address)
                && entry.tool_run.as_deref() == Some(run_id.as_str())
        })
    }

    fn push_error(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.push_entry(EntryKind::Error, title, body);
        self.status = "Error".to_owned();
    }

    fn push_entry(&mut self, kind: EntryKind, title: impl Into<String>, body: impl Into<String>) {
        self.push_entry_with_expansion(kind, title, body, false);
    }

    fn push_expanded_entry(
        &mut self,
        kind: EntryKind,
        title: impl Into<String>,
        body: impl Into<String>,
    ) {
        self.push_entry_with_expansion(kind, title, body, true);
    }

    fn push_entry_with_expansion(
        &mut self,
        kind: EntryKind,
        title: impl Into<String>,
        body: impl Into<String>,
        expanded: bool,
    ) {
        self.entries.push(ConversationEntry {
            kind,
            title: title.into(),
            body: body.into(),
            expanded,
            pending_tool: false,
            model_owner: None,
            model_run: None,
            tool_owner: None,
            tool_run: None,
            tool_index: None,
            tool_name: String::new(),
        });
        if self.focus == Focus::Input || self.follow_conversation_tail {
            self.selected_entry = self.entries.len().checked_sub(1);
        }
    }

    fn move_selection(&mut self, amount: isize) {
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

    fn toggle_selected(&mut self) {
        self.follow_conversation_tail = false;
        if let Some(index) = self.selected_entry
            && let Some(entry) = self.entries.get_mut(index)
        {
            entry.expanded = !entry.expanded;
        }
    }

    fn agent_addresses(&self) -> Vec<String> {
        let mut addresses = self.inactive_agents.keys().cloned().collect::<Vec<_>>();
        addresses.push(self.current_agent.clone());
        addresses.sort();
        addresses
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
        self.inactive_agents
            .entry(address.to_owned())
            .or_insert_with(|| AgentConversation::empty(parent, status));
    }

    fn switch_agent(&mut self, address: &str) -> bool {
        if address == self.current_agent {
            return true;
        }
        let Some(next) = self.inactive_agents.remove(address) else {
            return false;
        };
        let previous = AgentConversation {
            entries: std::mem::take(&mut self.entries),
            selected_entry: self.selected_entry,
            conversation_offset: self.conversation_offset,
            follow_conversation_tail: self.follow_conversation_tail,
            status: std::mem::take(&mut self.status),
            parent: self.current_parent.take(),
            model: self.current_model.take(),
            output_fallback: self.output_fallback.take(),
            run_completed: self.root_run_completed,
            completed_run_id: self.completed_run_id.take(),
        };
        let previous_address = std::mem::replace(&mut self.current_agent, address.to_owned());
        self.inactive_agents.insert(previous_address, previous);
        self.entries = next.entries;
        self.selected_entry = next.selected_entry;
        self.conversation_offset = next.conversation_offset;
        self.follow_conversation_tail = next.follow_conversation_tail;
        self.status = next.status;
        self.current_parent = next.parent;
        self.current_model = next.model;
        self.output_fallback = next.output_fallback;
        self.root_run_completed = next.run_completed;
        self.completed_run_id = next.completed_run_id;
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

impl AgentConversation {
    fn empty(parent: Option<String>, status: &str) -> Self {
        Self {
            entries: Vec::new(),
            selected_entry: None,
            conversation_offset: 0,
            follow_conversation_tail: true,
            status: status.to_owned(),
            parent,
            model: None,
            output_fallback: None,
            run_completed: false,
            completed_run_id: None,
        }
    }

    fn from_history(history: AgentHistory) -> Self {
        let entries = history_entries(history.history);
        Self {
            selected_entry: entries.len().checked_sub(1),
            entries,
            conversation_offset: 0,
            follow_conversation_tail: true,
            status: history.status,
            parent: history.parent,
            model: history.model,
            output_fallback: None,
            run_completed: history.run_completed,
            completed_run_id: None,
        }
    }
}

fn history_entries(history: Vec<HistoryEntry>) -> Vec<ConversationEntry> {
    history.into_iter().map(historical_entry).collect()
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

fn agent_tree_label(address: &str) -> String {
    let depth = address.matches('/').count().saturating_sub(1);
    let name = address.rsplit('/').next().unwrap_or(address);
    if depth == 0 {
        return format!("◉ {name}");
    }
    format!("{}└─ {name}", "  ".repeat(depth))
}

fn historical_entry(entry: HistoryEntry) -> ConversationEntry {
    let kind = match entry.kind {
        HistoryKind::User => EntryKind::User,
        HistoryKind::Assistant => EntryKind::Assistant,
        HistoryKind::ToolCall => EntryKind::ToolCall,
        HistoryKind::ToolResult => EntryKind::ToolResult,
        HistoryKind::System => EntryKind::System,
    };
    ConversationEntry {
        kind,
        title: entry.title,
        body: entry.body,
        expanded: matches!(kind, EntryKind::User | EntryKind::Assistant),
        pending_tool: false,
        model_owner: None,
        model_run: None,
        tool_owner: None,
        tool_run: None,
        tool_index: None,
        tool_name: String::new(),
    }
}

fn eval_request_body(request: &lam::EvalRequest) -> String {
    let timeout = request
        .timeout
        .map(|timeout| format!("\n\ntimeout: {:.1}s", timeout.as_secs_f64()))
        .unwrap_or_default();
    format!("{}{timeout}", request.source)
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
}

fn byte_index(text: &str, character: usize) -> usize {
    text.char_indices()
        .nth(character)
        .map_or(text.len(), |(index, _)| index)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use lam::{
        EvalOutcome, EvalOutput, EvalRequest, EvalValue, ModelDelta, RunEvent, RunId, ToolCallDelta,
    };
    use lam_agents::{ActorAddress, AgentOutcome, AgentSystemEvent};

    use super::{App, EntryKind, Focus, InputBuffer, SessionView, partial_eval_intent};
    use crate::config::ModelChoice;
    use crate::runtime::{
        AgentHistory, Command, CommandResult, CompletedCall, HistoryEntry, HistoryKind,
    };

    fn app() -> App {
        App::new(
            "/tmp/project".to_owned(),
            "/tmp/providers.toml".to_owned(),
            SessionView {
                id: 7,
                journal_path: "/tmp/session-00000007.redb".to_owned(),
                resumed: false,
                agents: vec![AgentHistory::root(Vec::new())],
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
        )
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
    fn bracketed_paste_preserves_multiline_prompts() {
        let mut app = app();
        app.handle_paste("first line\nsecond line");
        assert_eq!(app.input.text, "first line\nsecond line");
        assert_eq!(app.input.cursor, 22);
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
    }

    #[test]
    fn tab_moves_focus_when_no_completion_is_open() {
        let mut app = app();
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Conversation);
        assert!(app.follow_conversation_tail);
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
        app.push_expanded_entry(EntryKind::User, "You", "one");
        app.push_expanded_entry(EntryKind::User, "You", "two");
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
    fn ordinary_input_starts_a_call() {
        let mut app = app();
        app.input.text = "inspect the workspace".to_owned();
        app.input.cursor = app.input.char_count();
        let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(command, Some(Command::Call(input)) if input == "inspect the workspace"));
        assert!(app.busy);
        assert!(app.entries.last().unwrap().expanded);
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
                    HistoryEntry {
                        kind: HistoryKind::User,
                        title: "You".to_owned(),
                        body: "hello".to_owned(),
                    },
                    HistoryEntry {
                        kind: HistoryKind::Assistant,
                        title: "/root".to_owned(),
                        body: "hi".to_owned(),
                    },
                    HistoryEntry {
                        kind: HistoryKind::ToolCall,
                        title: "/root · Calculate the result".to_owned(),
                        body: "1 + 1".to_owned(),
                    },
                ])],
            },
            app.models,
            0,
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
    fn eval_events_create_an_expandable_source_row() {
        let mut app = app();
        app.apply_agent_event(AgentSystemEvent::Run {
            address: ActorAddress::new("/root").unwrap(),
            event: RunEvent::EvalStarted {
                run_id: RunId::new("run-1").unwrap(),
                request: EvalRequest {
                    intent: "Inspect the workspace files".to_owned(),
                    source: "const files = await lam.fs.list({ path: '.' });".to_owned(),
                    timeout: None,
                },
            },
        });
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

        app.apply_agent_event(AgentSystemEvent::Run {
            address: address.clone(),
            event: RunEvent::EvalStarted {
                run_id: run_id.clone(),
                request: EvalRequest {
                    intent: "Calculate the result".to_owned(),
                    source: "1 + 1".to_owned(),
                    timeout: None,
                },
            },
        });
        assert_eq!(app.entries.len(), 2, "eval start reuses the streamed row");
        assert_eq!(app.entries[1].title, "/root · Calculate the result");
        assert_eq!(app.entries[1].body, "1 + 1");

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
        assert_eq!(app.entries.len(), 3);
        assert_eq!(app.entries[1].kind, EntryKind::ToolCall);
        assert_eq!(app.entries[2].kind, EntryKind::ToolResult);
        assert!(!app.entries[1].pending_tool);

        app.selected_entry = Some(1);
        app.toggle_selected();
        assert!(app.entries[1].expanded);
        assert_eq!(app.entries[1].title, "/root · Calculate the result");
        assert!(!app.entries[2].expanded);
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

        app.apply_agent_event(AgentSystemEvent::Run {
            address,
            event: RunEvent::EvalStarted {
                run_id,
                request: EvalRequest {
                    intent: "Inspect the workspace".to_owned(),
                    source: "await lam.dir()".to_owned(),
                    timeout: None,
                },
            },
        });
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
    fn terminal_output_reconciles_with_late_stream_deltas() {
        let mut first_app = app();
        first_app.apply_command_result(CommandResult::Call(Ok(CompletedCall {
            output: "hello".to_owned(),
            run_id: Some("run-1".to_owned()),
        })));
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

        let mut second_app = app();
        second_app.apply_agent_event(AgentSystemEvent::Run {
            address: ActorAddress::new("/root").unwrap(),
            event: RunEvent::ModelDelta {
                run_id: RunId::new("run-2").unwrap(),
                delta: ModelDelta::Text("hel".to_owned()),
            },
        });
        second_app.apply_command_result(CommandResult::Call(Ok(CompletedCall {
            output: "hello".to_owned(),
            run_id: Some("run-2".to_owned()),
        })));
        second_app.apply_agent_event(AgentSystemEvent::Run {
            address: ActorAddress::new("/root").unwrap(),
            event: RunEvent::ModelDelta {
                run_id: RunId::new("run-2").unwrap(),
                delta: ModelDelta::Text("lo".to_owned()),
            },
        });
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
    fn tool_call_collapses_only_text_from_the_same_run() {
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
        assert!(!app.entries[first_text].expanded);

        app.entries[first_text].expanded = true;
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
    fn completed_call_recovers_when_the_terminal_event_is_missing() {
        let mut app = app();
        let address = ActorAddress::new("/root").unwrap();
        let run_id = RunId::new("run-1").unwrap();
        app.root_run_completed = false;
        app.busy = true;
        app.status = "/root is responding…".to_owned();
        app.apply_agent_event(AgentSystemEvent::Run {
            address: address.clone(),
            event: RunEvent::ModelDelta {
                run_id: run_id.clone(),
                delta: ModelDelta::Text("Final ans".to_owned()),
            },
        });

        app.apply_command_result(CommandResult::Call(Ok(CompletedCall {
            output: "Final answer".to_owned(),
            run_id: Some(run_id.to_string()),
        })));
        assert!(!app.busy);
        assert_eq!(app.status, "Ready");
        assert!(app.root_run_completed);
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
        assert_eq!(app.status, "Ready");
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
        app.hitboxes = vec![(4, 4, tool)];
        app.busy = true;

        assert!(!app.apply_agent_event(AgentSystemEvent::Run {
            address: child,
            event: RunEvent::ModelDelta {
                run_id: RunId::new("child-run").unwrap(),
                delta: ModelDelta::Text("streaming in the background".to_owned()),
            },
        }));
        assert_eq!(app.hitboxes, [(4, 4, tool)]);

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
            ["◉ root", "  └─ worker", "    └─ scout"]
        );
    }
}
