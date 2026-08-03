use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use lam::{ModelDelta, RunEvent, RunId, RuntimeEvent, ToolCallDelta};
use lam_agents::{AgentOutcome, AgentSystemEvent};

use crate::config::ModelChoice;
use crate::runtime::{Command, CommandResult};

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

#[derive(Default)]
pub(crate) struct InputBuffer {
    pub(crate) text: String,
    pub(crate) cursor: usize,
}

pub(crate) struct App {
    pub(crate) cwd: String,
    pub(crate) models: Vec<ModelChoice>,
    pub(crate) selected_model: usize,
    pub(crate) focus: Focus,
    pub(crate) input: InputBuffer,
    pub(crate) entries: Vec<ConversationEntry>,
    pub(crate) selected_entry: Option<usize>,
    pub(crate) suggestion_index: usize,
    pub(crate) busy: bool,
    pub(crate) status: String,
    pub(crate) should_exit: bool,
    pub(crate) hitboxes: Vec<(u16, u16, usize)>,
    output_fallback: Option<OutputFallback>,
    root_run_completed: bool,
    pending_root_output: Option<String>,
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
        models: Vec<ModelChoice>,
        selected_model: usize,
    ) -> Self {
        let ready_message = format!(
            "Coding and multi-agent capabilities are active. Using {} ({} token context). Configuration: {}.",
            models[selected_model].display_name, models[selected_model].context_window, config_path
        );
        Self {
            cwd,
            models,
            selected_model,
            focus: Focus::Input,
            input: InputBuffer::default(),
            entries: vec![ConversationEntry {
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
            }],
            selected_entry: Some(0),
            suggestion_index: 0,
            busy: false,
            status: "Ready".to_owned(),
            should_exit: false,
            hitboxes: Vec::new(),
            output_fallback: None,
            root_run_completed: true,
            pending_root_output: None,
        }
    }

    pub(crate) fn selected_model(&self) -> &ModelChoice {
        &self.models[self.selected_model]
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
                    label: model.display_name.clone(),
                    detail: format!("{} · {}k", model.model, model.context_window / 1_000),
                    replacement: format!("/model {}", model.registry_id),
                    provider: Some(model.provider.clone()),
                })
                .collect();
        }
        if !input.starts_with('/') || input.contains(' ') {
            return Vec::new();
        }
        let query = input.trim_start_matches('/').to_lowercase();
        [
            ("compact", "Compact the current context", "/compact"),
            ("model", "Choose a provider and model", "/model "),
            ("exit", "Close Lam", "/exit"),
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
            self.should_exit = true;
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
            KeyCode::Left => {
                self.input.cursor = self.input.cursor.saturating_sub(1);
                None
            }
            KeyCode::Right => {
                self.input.cursor = (self.input.cursor + 1).min(self.input.char_count());
                None
            }
            KeyCode::Home => {
                self.input.cursor = 0;
                None
            }
            KeyCode::End => {
                self.input.cursor = self.input.char_count();
                None
            }
            KeyCode::Backspace => {
                self.input.backspace();
                self.suggestion_index = 0;
                None
            }
            KeyCode::Delete => {
                self.input.delete();
                self.suggestion_index = 0;
                None
            }
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.input.insert(character);
                self.suggestion_index = 0;
                None
            }
            KeyCode::Esc => {
                self.input = InputBuffer::default();
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
            KeyCode::Home => self.selected_entry = (!self.entries.is_empty()).then_some(0),
            KeyCode::End => self.selected_entry = self.entries.len().checked_sub(1),
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_selected(),
            _ => {}
        }
    }

    fn submit(&mut self) -> Option<Command> {
        let input = self.input.text.trim().to_owned();
        if input.is_empty() {
            return None;
        }
        if self.busy {
            self.status = "Wait for the current root operation to finish".to_owned();
            return None;
        }
        self.input = InputBuffer::default();
        self.suggestion_index = 0;
        match input.as_str() {
            "/exit" => {
                self.should_exit = true;
                None
            }
            "/compact" => {
                self.busy = true;
                self.status = "Compacting context…".to_owned();
                Some(Command::Compact)
            }
            "/model" => {
                self.input.text = "/model ".to_owned();
                self.input.cursor = self.input.char_count();
                None
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
                self.pending_root_output = None;
                self.push_expanded_entry(EntryKind::User, "You", input.clone());
                self.busy = true;
                self.status = "Thinking…".to_owned();
                Some(Command::Call(input))
            }
        }
    }

    fn apply_suggestion(&mut self, suggestions: &[Suggestion]) {
        let replacement = suggestions[self.suggestion_index].replacement.clone();
        self.input.text = replacement;
        self.input.cursor = self.input.char_count();
        self.suggestion_index = 0;
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
                    self.toggle_selected();
                }
            }
            _ => {}
        }
    }

    pub(crate) fn handle_paste(&mut self, text: &str) {
        if self.focus == Focus::Input {
            self.input.insert_text(text);
            self.suggestion_index = 0;
        }
    }

    pub(crate) fn apply_agent_event(&mut self, event: AgentSystemEvent) {
        match event {
            AgentSystemEvent::Hosted { address, parent } => {
                if parent.is_some() {
                    self.push_entry(EntryKind::System, "Subagent started", address.to_string());
                }
            }
            AgentSystemEvent::Retired { address, reason } => self.push_entry(
                EntryKind::System,
                "Subagent stopped",
                format!("{address}: {reason:?}"),
            ),
            AgentSystemEvent::Outcome { outcome } => self.apply_outcome(outcome),
            AgentSystemEvent::ActorRuntime { address, event } => {
                self.apply_runtime_event(address.as_str(), event);
            }
            AgentSystemEvent::Run { address, event } => {
                self.apply_run_event(address.as_str(), event);
            }
        }
    }

    fn apply_run_event(&mut self, address: &str, event: RunEvent) {
        match event {
            RunEvent::Started { .. } | RunEvent::MessagesDelivered { .. } => {
                self.status = format!("{address} is working…");
            }
            RunEvent::ModelStarted { .. } => self.status = format!("{address} is thinking…"),
            RunEvent::ModelDelta { run_id, delta } => match delta {
                ModelDelta::Text(text) => {
                    self.append_delta(EntryKind::Assistant, address, &run_id, text);
                    self.status = format!("{address} is responding…");
                }
                ModelDelta::Reasoning(text) => {
                    self.append_delta(EntryKind::Reasoning, address, &run_id, text);
                    self.status = format!("{address} is reasoning…");
                }
                ModelDelta::ToolCall(delta) => {
                    self.collapse_streamed_text(address, &run_id);
                    self.append_tool_delta(address, &run_id, delta);
                    self.status = format!("{address} is preparing a tool call…");
                }
            },
            RunEvent::ModelCompleted { .. } => {}
            RunEvent::EvalStarted {
                run_id, request, ..
            } => {
                self.collapse_streamed_text(address, &run_id);
                let body = eval_request_body(&request);
                if let Some(entry) = self.pending_tool_mut(address, &run_id) {
                    entry.tool_name = "eval".to_owned();
                    entry.title = format!("{address} · eval");
                    entry.body = body;
                } else {
                    self.push_entry(EntryKind::ToolCall, format!("{address} · eval"), body);
                    if let Some(entry) = self.entries.last_mut() {
                        entry.pending_tool = true;
                        entry.tool_owner = Some(address.to_owned());
                        entry.tool_run = Some(run_id.to_string());
                        entry.tool_name = "eval".to_owned();
                    }
                }
                self.status = format!("{address} is evaluating TypeScript…");
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
                self.status = format!("{address} finished eval");
            }
            RunEvent::CompactionStarted { .. } => {
                self.status = format!("{address} is compacting context…");
            }
            RunEvent::CompactionCompleted { covers_through, .. } => self.push_entry(
                EntryKind::System,
                "Context compacted",
                format!(
                    "{address} compacted through sequence {}.",
                    covers_through.get()
                ),
            ),
            RunEvent::CompactionFailed { message, .. } => {
                self.push_error("Compaction failed", message);
            }
            RunEvent::Completed { .. } => {
                if address == "/root" {
                    self.root_run_completed = true;
                    if let Some(output) = self.pending_root_output.take() {
                        self.reconcile_root_output(output);
                    }
                }
                if let Some(entry) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|entry| entry.kind == EntryKind::Assistant && entry.title == address)
                {
                    entry.expanded = true;
                }
                self.status = "Ready".to_owned();
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
            AgentOutcome::Completed {
                address, output, ..
            } => self.push_expanded_entry(
                EntryKind::Assistant,
                format!("{address} · outcome"),
                output,
            ),
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
        match result {
            CommandResult::Call(Ok(output)) => {
                if !output.trim().is_empty() {
                    if self.root_run_completed {
                        self.reconcile_root_output(output);
                    } else {
                        self.pending_root_output = Some(output);
                    }
                }
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
                self.push_entry(EntryKind::System, "Model", message);
                self.status = "Ready".to_owned();
            }
            CommandResult::SwitchModel {
                result: Err(error), ..
            } => self.push_error("Model switch failed", error),
        }
    }

    fn append_delta(&mut self, kind: EntryKind, address: &str, run_id: &RunId, delta: String) {
        if kind == EntryKind::Assistant
            && address == "/root"
            && self.suppress_fallback_delta(&delta)
        {
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

    fn collapse_streamed_text(&mut self, address: &str, run_id: &RunId) {
        if let Some(entry) = self.entries.iter_mut().rev().find(|entry| {
            entry.kind == EntryKind::Assistant
                && entry.model_owner.as_deref() == Some(address)
                && entry.model_run.as_deref() == Some(run_id.as_str())
        }) {
            entry.expanded = false;
        }
    }

    fn reconcile_root_output(&mut self, output: String) {
        if let Some(entry) =
            self.entries.iter_mut().rev().find(|entry| {
                entry.kind == EntryKind::Assistant && entry.body.trim() == output.trim()
            })
        {
            entry.expanded = true;
            return;
        }

        let streamed_entry = self.entries.iter().enumerate().rev().find(|(_, entry)| {
            entry.kind == EntryKind::Assistant
                && entry.title == "/root"
                && output.starts_with(&entry.body)
        });
        let (entry_index, streamed) = if let Some((index, entry)) = streamed_entry {
            (index, entry.body.clone())
        } else {
            self.push_entry(EntryKind::Assistant, "/root", String::new());
            (self.entries.len() - 1, String::new())
        };
        self.entries[entry_index].body.clone_from(&output);
        self.entries[entry_index].expanded = true;
        self.output_fallback = Some(OutputFallback {
            entry_index,
            output,
            streamed,
        });
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
        self.selected_entry = self.entries.len().checked_sub(1);
    }

    fn move_selection(&mut self, amount: isize) {
        if self.entries.is_empty() {
            self.selected_entry = None;
            return;
        }
        let current = self.selected_entry.unwrap_or(self.entries.len() - 1);
        self.selected_entry = Some(
            current
                .saturating_add_signed(amount)
                .min(self.entries.len() - 1),
        );
    }

    fn toggle_selected(&mut self) {
        if let Some(index) = self.selected_entry
            && let Some(entry) = self.entries.get_mut(index)
        {
            entry.expanded = !entry.expanded;
        }
    }
}

fn eval_request_body(request: &lam::EvalRequest) -> String {
    let timeout = request
        .timeout
        .map(|timeout| format!("\n\ntimeout: {:.1}s", timeout.as_secs_f64()))
        .unwrap_or_default();
    format!("{}{timeout}", request.source)
}

impl InputBuffer {
    pub(crate) fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    pub(crate) fn text_before_cursor(&self) -> String {
        self.text.chars().take(self.cursor).collect()
    }

    fn insert(&mut self, character: char) {
        let byte = byte_index(&self.text, self.cursor);
        self.text.insert(byte, character);
        self.cursor += 1;
    }

    fn insert_text(&mut self, text: &str) {
        let byte = byte_index(&self.text, self.cursor);
        self.text.insert_str(byte, text);
        self.cursor += text.chars().count();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = byte_index(&self.text, self.cursor - 1);
        let end = byte_index(&self.text, self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        if self.cursor == self.char_count() {
            return;
        }
        let start = byte_index(&self.text, self.cursor);
        let end = byte_index(&self.text, self.cursor + 1);
        self.text.replace_range(start..end, "");
    }
}

fn byte_index(text: &str, character: usize) -> usize {
    text.char_indices()
        .nth(character)
        .map_or(text.len(), |(index, _)| index)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use lam::{
        EvalOutcome, EvalOutput, EvalRequest, EvalValue, ModelDelta, RunEvent, RunId, ToolCallDelta,
    };
    use lam_agents::{ActorAddress, AgentSystemEvent};

    use super::{App, EntryKind, Focus, InputBuffer};
    use crate::config::ModelChoice;
    use crate::runtime::{Command, CommandResult};

    fn app() -> App {
        App::new(
            "/tmp/project".to_owned(),
            "/tmp/providers.toml".to_owned(),
            vec![ModelChoice {
                registry_id: "openai/gpt-5".to_owned(),
                provider: "openai".to_owned(),
                model: "gpt-5".to_owned(),
                display_name: "GPT-5".to_owned(),
                context_window: 400_000,
            }],
            0,
        )
    }

    #[test]
    fn edits_unicode_by_character_position() {
        let mut input = InputBuffer {
            text: "aλ".to_owned(),
            cursor: 2,
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
    fn slash_completion_populates_model_arguments() {
        let mut app = app();
        app.input.text = "/".to_owned();
        app.input.cursor = 1;
        app.suggestion_index = 1;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input.text, "/model ");
        let suggestions = app.suggestions();
        assert_eq!(suggestions[0].replacement, "/model openai/gpt-5");
    }

    #[test]
    fn tab_moves_focus_when_no_completion_is_open() {
        let mut app = app();
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Conversation);
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
    fn eval_events_create_an_expandable_source_row() {
        let mut app = app();
        app.apply_agent_event(AgentSystemEvent::Run {
            address: ActorAddress::new("/root").unwrap(),
            event: RunEvent::EvalStarted {
                run_id: RunId::new("run-1").unwrap(),
                request: EvalRequest {
                    source: "const files = await lam.fs.list({ path: '.' });".to_owned(),
                    timeout: None,
                },
            },
        });
        let tool = app.entries.last().unwrap();
        assert_eq!(tool.title, "/root · eval");
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
                arguments: "{\"source\":".to_owned(),
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
        assert_eq!(app.entries[1].body, "{\"source\":\"1 + 1\"}");

        app.apply_agent_event(AgentSystemEvent::Run {
            address: address.clone(),
            event: RunEvent::EvalStarted {
                run_id: run_id.clone(),
                request: EvalRequest {
                    source: "1 + 1".to_owned(),
                    timeout: None,
                },
            },
        });
        assert_eq!(app.entries.len(), 2, "eval start reuses the streamed row");
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
        assert!(!app.entries[2].expanded);
    }

    #[test]
    fn terminal_output_reconciles_with_late_stream_deltas() {
        let mut first_app = app();
        first_app.apply_command_result(CommandResult::Call(Ok("hello".to_owned())));
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
        second_app.apply_command_result(CommandResult::Call(Ok("hello".to_owned())));
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
    fn root_fallback_waits_for_ordered_completion() {
        let mut app = app();
        app.root_run_completed = false;
        app.apply_command_result(CommandResult::Call(Ok("Final answer".to_owned())));
        assert!(
            app.entries
                .iter()
                .all(|entry| entry.kind != EntryKind::Assistant)
        );

        app.push_entry(EntryKind::ToolResult, "/root · eval result", "42");
        app.apply_agent_event(AgentSystemEvent::Run {
            address: ActorAddress::new("/root").unwrap(),
            event: RunEvent::Completed {
                run_id: RunId::new("run-1").unwrap(),
            },
        });
        assert_eq!(
            app.entries[app.entries.len() - 2].kind,
            EntryKind::ToolResult
        );
        assert_eq!(app.entries.last().unwrap().kind, EntryKind::Assistant);
        assert!(app.entries.last().unwrap().expanded);
    }
}
