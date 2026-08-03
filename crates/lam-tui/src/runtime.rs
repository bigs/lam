use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use lam::{
    ActorError, ActorEventData, ActorId, ActorState, CompactionArtifact, CompactionRecord,
    ContextTransition, EncodedPayload, InterruptedEvalOutcome, IsolateState, JournalStore, Lam,
    LamBuilder, MemStore, MessageSource, Model, ModelCodec, ModelDescriptor, ModelDirective,
    ModelRequestConfig, ModelResponseMetadata, ProjectedContextEntry, Revision,
    SYSTEM_NOTICE_CODEC_ID, SystemNotice,
};
use lam_agents::{
    Agent, AgentSystem, AgentSystemError, AgentSystemEvents, AgentTreeInterruptionReceipt,
    SubagentConfig, SubagentConfigBuilder,
};
use lam_code::{CodingPack, FilesystemAccess, LocalCommandRunner};
use lam_openai::chat_completions::{
    ChatCompletions, ChatCompletionsCodec, ChatCompletionsProvider,
};
use lam_openai::responses::{Responses, ResponsesCodec, ResponsesProvider};
use lam_redb::RedbStore;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::config::{LoadedConfig, ModelChoice, ModelConfig, ProviderConfig, ProviderProtocol};
use crate::session::Session;

pub(crate) struct Runtime {
    pub(crate) system: AgentSystem<RedbStore>,
    pub(crate) root: Agent<RedbStore>,
    pub(crate) events: AgentSystemEvents,
    pub(crate) models: Vec<ModelChoice>,
    pub(crate) selected_model: usize,
    pub(crate) agents: Vec<AgentHistory>,
    effort_controls: Vec<EffortControl>,
    history_models: Arc<[ConfiguredModel]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimePreferences {
    pub(crate) model_id: String,
    pub(crate) effort: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentHistory {
    pub(crate) address: String,
    pub(crate) parent: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) status: String,
    pub(crate) run_completed: bool,
    pub(crate) history: Vec<HistoryEntry>,
}

impl AgentHistory {
    pub(crate) fn root(history: Vec<HistoryEntry>) -> Self {
        Self {
            address: "/root".to_owned(),
            parent: None,
            model: None,
            status: "Ready".to_owned(),
            run_completed: true,
            history,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HistoryKind {
    User,
    Assistant,
    ToolCall,
    ToolResult,
    System,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HistoryEntry {
    pub(crate) kind: HistoryKind,
    pub(crate) title: String,
    pub(crate) body: String,
}

#[derive(Clone, Debug)]
pub(crate) enum Command {
    Call(String),
    Interrupt,
    Compact,
    SwitchModel { index: usize, registry_id: String },
    SetEffort { index: usize, effort: String },
    New,
    LoadSession(u64),
    RefreshSessions,
}

#[derive(Debug)]
pub(crate) enum CommandResult {
    Call(Result<CompletedCall, String>),
    CallInterrupted,
    Interrupt(Result<Option<InterruptedTree>, String>),
    Compact(Result<String, String>),
    SwitchModel {
        index: usize,
        result: Result<String, String>,
    },
    SetEffort {
        index: usize,
        effort: String,
        result: Result<String, String>,
    },
}

#[derive(Debug)]
pub(crate) struct CompletedCall {
    pub(crate) output: String,
    pub(crate) run_id: Option<String>,
}

#[derive(Debug)]
pub(crate) struct InterruptedTree {
    pub(crate) agents: Vec<AgentHistory>,
    pub(crate) runs: Vec<InterruptedRun>,
}

#[derive(Debug)]
pub(crate) struct InterruptedRun {
    pub(crate) address: String,
    pub(crate) run_id: String,
}

impl Runtime {
    pub(crate) async fn build(
        config: &LoadedConfig,
        cwd: PathBuf,
        session: &Session,
        preferences: Option<&RuntimePreferences>,
    ) -> Result<Self, RuntimeError> {
        let models = configured_models(config)?;
        let effort_controls = models
            .iter()
            .map(|configured| configured.effort.clone())
            .collect::<Vec<_>>();
        let initial_index = preferences
            .and_then(|preferences| {
                models
                    .iter()
                    .position(|model| model.choice.registry_id == preferences.model_id)
            })
            .unwrap_or(config.default_index);
        if let Some(preferences) = preferences {
            effort_controls[initial_index]
                .set(&preferences.effort)
                .map_err(RuntimeError::Preferences)?;
        }
        let initial = models
            .get(initial_index)
            .expect("the selected model index comes from the validated model list");
        let coding = CodingPack::builder(&cwd)
            .filesystem_access(FilesystemAccess::ReadWrite)
            .shell(LocalCommandRunner::default())
            .build()
            .map_err(|error| RuntimeError::CodingPack(error.to_string()))?;
        let store = RedbStore::open(&session.database_path).map_err(RuntimeError::Journal)?;
        let stored_actors = load_stored_actors(&store)
            .await
            .map_err(RuntimeError::AgentSystem)?;
        let system = AgentSystem::builder(store)
            .worker_threads(default_worker_threads())
            .max_agents(64)
            .build()
            .map_err(|error| RuntimeError::AgentSystem(error.to_string()))?;
        let events = system
            .take_events()
            .expect("a new agent system owns its event receiver");
        let instruction = coding_instruction(&cwd, &config.models, initial_index);

        let mut root_builder = initial
            .model
            .lam_builder()
            .initial_model_id(initial.choice.registry_id.clone())
            .state_store(system.state_store())
            .namespaces(coding.namespaces())
            .annotate_system_prompt(&instruction);
        for configured in &models {
            if configured.choice.registry_id != initial.choice.registry_id {
                root_builder = configured
                    .model
                    .register_lam(root_builder, configured.choice.registry_id.clone());
            }
        }

        let mut child_builder: SubagentConfigBuilder<RedbStore> = initial.model.subagent_builder();
        for configured in &models {
            if configured.choice.registry_id != initial.choice.registry_id {
                child_builder = configured.model.register_subagent(child_builder);
            }
        }
        let children: SubagentConfig<RedbStore> = child_builder
            .namespaces(coding.namespaces())
            .required_instructions(instruction)
            .build()
            .map_err(|error| RuntimeError::AgentSystem(error.to_string()))?;
        let root = system
            .host_with_subagents(root_builder.build().actor("/root"), children)
            .await
            .map_err(|error| RuntimeError::AgentSystem(error.to_string()))?;
        let state = root
            .state()
            .await
            .map_err(|error| RuntimeError::AgentSystem(error.to_string()))?;
        let selected_model_id = state
            .selected_model()
            .expect("actor initialization establishes a model selection")
            .model_id
            .as_str()
            .to_owned();
        let selected_model = config
            .models
            .iter()
            .position(|model| model.registry_id == selected_model_id)
            .expect("runtime registration mirrors the validated model list");
        let mut agents = stored_actors
            .into_iter()
            .filter(|(actor, _)| actor.as_str() != "/root")
            .map(|(actor, state)| agent_history(actor.as_str(), &state, &models, true))
            .collect::<Vec<_>>();
        agents.push(agent_history("/root", &state, &models, false));
        agents.sort_by(|left, right| left.address.cmp(&right.address));

        Ok(Self {
            system,
            root,
            events,
            models: config.models.clone(),
            selected_model,
            agents,
            effort_controls,
            history_models: models.into(),
        })
    }

    pub(crate) fn selected_effort(&self) -> String {
        self.effort_controls[self.selected_model].selected()
    }

    pub(crate) fn execute(&self, command: Command, output: mpsc::UnboundedSender<CommandResult>) {
        let root = self.root.clone();
        let effort_controls = self.effort_controls.clone();
        let history_models = Arc::clone(&self.history_models);
        let store = self.system.state_store();
        tokio::spawn(async move {
            let result = match command {
                Command::Call(input) => match root.call(input).await {
                    Ok(output) => {
                        let run_id = root.state().await.ok().and_then(|state| {
                            state
                                .context()
                                .last()
                                .and_then(|projected| projected.entry.transition.run_id())
                                .map(ToString::to_string)
                        });
                        tracing::debug!(
                            target: "lam_tui::runtime",
                            event = "tui.call_completed",
                            actor_id = "/root",
                            run_id = ?run_id,
                            output_bytes = output.len(),
                            "root call completed"
                        );
                        CommandResult::Call(Ok(CompletedCall { output, run_id }))
                    }
                    Err(AgentSystemError::Actor(ActorError::Interrupted)) => {
                        tracing::debug!(
                            target: "lam_tui::runtime",
                            event = "tui.call_interrupted",
                            actor_id = "/root",
                            "root call was interrupted"
                        );
                        CommandResult::CallInterrupted
                    }
                    Err(error) => {
                        tracing::error!(
                            target: "lam_tui::runtime",
                            event = "tui.call_failed",
                            actor_id = "/root",
                            "root call failed"
                        );
                        CommandResult::Call(Err(error.to_string()))
                    }
                },
                Command::Interrupt => {
                    let result = match root.interrupt().await {
                        Ok(Some(receipt)) => {
                            interrupted_tree(store.as_ref(), &history_models, &receipt)
                                .await
                                .map(Some)
                        }
                        Ok(None) => Ok(None),
                        Err(error) => Err(error.to_string()),
                    };
                    CommandResult::Interrupt(result)
                }
                Command::Compact => {
                    let result = root.compact().await.map(|receipt| match receipt {
                        Some(receipt) => format!(
                            "Compacted context through sequence {}.",
                            receipt.covers_through.get()
                        ),
                        None => "Context is already compact.".to_owned(),
                    });
                    CommandResult::Compact(result.map_err(|error| error.to_string()))
                }
                Command::SwitchModel { index, registry_id } => {
                    let result = root
                        .switch_model(registry_id)
                        .await
                        .map(|receipt| format!("Switched to {}.", receipt.selected_model_id));
                    CommandResult::SwitchModel {
                        index,
                        result: result.map_err(|error| error.to_string()),
                    }
                }
                Command::SetEffort { index, effort } => {
                    let result = effort_controls
                        .get(index)
                        .ok_or_else(|| format!("model index {index} is not configured"))
                        .and_then(|control| control.set(&effort))
                        .map(|()| format!("Set reasoning effort to {effort}."));
                    CommandResult::SetEffort {
                        index,
                        effort,
                        result,
                    }
                }
                Command::New | Command::LoadSession(_) | Command::RefreshSessions => return,
            };
            let _ = output.send(result);
        });
    }
}

pub(crate) async fn first_user_message(session: &Session) -> Result<Option<String>, RuntimeError> {
    const PAGE_SIZE: NonZeroUsize = NonZeroUsize::new(256).expect("256 is nonzero");
    let store = RedbStore::open(&session.database_path).map_err(RuntimeError::Journal)?;
    let actor = ActorId::new("/root").expect("the root actor ID is valid");
    let mut after = Revision::ZERO;
    loop {
        let page = store
            .read(&actor, after, PAGE_SIZE)
            .await
            .map_err(|error| RuntimeError::AgentSystem(error.to_string()))?;
        let head = page.head;
        for stored in page.events {
            after = stored.revision;
            let ActorEventData::MessageAdmitted { message } = stored.event.data() else {
                continue;
            };
            if matches!(message.source(), MessageSource::User { .. }) {
                return Ok(Some(display_json(&message.payload().value)));
            }
        }
        if after >= head {
            return Ok(None);
        }
    }
}

struct ConfiguredModel {
    choice: ModelChoice,
    model: AnyModel,
    effort: EffortControl,
}

enum AnyModel {
    Responses(Model<ResponsesProvider, EffortCodec<ResponsesCodec>>),
    ChatCompletions(Model<ChatCompletionsProvider, EffortCodec<ChatCompletionsCodec>>),
}

#[derive(Clone)]
struct EffortControl {
    path: Arc<[String]>,
    allowed: Arc<[String]>,
    selected: Arc<RwLock<String>>,
}

impl EffortControl {
    fn new(path: Vec<String>, allowed: &[String]) -> Self {
        let selected = allowed
            .last()
            .expect("configuration validation requires at least one effort")
            .clone();
        Self {
            path: path.into(),
            allowed: allowed.to_vec().into(),
            selected: Arc::new(RwLock::new(selected)),
        }
    }

    fn set(&self, effort: &str) -> Result<(), String> {
        if !self.allowed.iter().any(|allowed| allowed == effort) {
            return Err(format!("reasoning effort `{effort}` is not supported"));
        }
        *self
            .selected
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = effort.to_owned();
        Ok(())
    }

    fn selected(&self) -> String {
        self.selected
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[derive(Clone)]
struct EffortCodec<C> {
    inner: C,
    control: EffortControl,
}

impl<C> ModelCodec for EffortCodec<C>
where
    C: ModelCodec,
{
    type Error = C::Error;

    fn encode_request(
        &self,
        context: &[ProjectedContextEntry],
        config: &ModelRequestConfig<'_>,
    ) -> Result<EncodedPayload, Self::Error> {
        let mut request = self.inner.encode_request(context, config)?;
        let body = request
            .value
            .get_mut("body")
            .expect("OpenAI request codecs always produce a body envelope");
        insert_json_path(
            body,
            &self.control.path,
            serde_json::Value::String(self.control.selected()),
        );
        Ok(request)
    }

    fn interpret_response(&self, response: &EncodedPayload) -> Result<ModelDirective, Self::Error> {
        self.inner.interpret_response(response)
    }

    fn response_metadata(&self, response: &EncodedPayload) -> ModelResponseMetadata {
        self.inner.response_metadata(response)
    }

    fn materialize_compaction(
        &self,
        artifact: &CompactionArtifact,
    ) -> Result<Option<EncodedPayload>, Self::Error> {
        self.inner.materialize_compaction(artifact)
    }

    fn accepts_compaction_replacement(&self, replacement: &EncodedPayload) -> bool {
        self.inner.accepts_compaction_replacement(replacement)
    }
}

fn insert_json_path(value: &mut serde_json::Value, path: &[String], leaf: serde_json::Value) {
    let mut object = value
        .as_object_mut()
        .expect("OpenAI request codecs always produce an object");
    for segment in &path[..path.len() - 1] {
        object = object
            .entry(segment.clone())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .expect("configuration validation reserves object-valued effort path prefixes");
    }
    object.insert(path[path.len() - 1].clone(), leaf);
}

impl AnyModel {
    fn lam_builder(&self) -> LamBuilder<MemStore> {
        match self {
            Self::Responses(model) => Lam::builder(model.clone()),
            Self::ChatCompletions(model) => Lam::builder(model.clone()),
        }
    }

    fn register_lam<S>(&self, builder: LamBuilder<S>, id: String) -> LamBuilder<S> {
        match self {
            Self::Responses(model) => builder.model(id, model.clone()),
            Self::ChatCompletions(model) => builder.model(id, model.clone()),
        }
    }

    fn subagent_builder<S>(&self) -> SubagentConfigBuilder<S>
    where
        S: JournalStore + 'static,
    {
        match self {
            Self::Responses(model) => SubagentConfig::builder(model.clone()),
            Self::ChatCompletions(model) => SubagentConfig::builder(model.clone()),
        }
    }

    fn register_subagent<S>(&self, builder: SubagentConfigBuilder<S>) -> SubagentConfigBuilder<S>
    where
        S: JournalStore + 'static,
    {
        match self {
            Self::Responses(model) => builder.model(model.clone()),
            Self::ChatCompletions(model) => builder.model(model.clone()),
        }
    }

    fn interpret_response(&self, payload: &lam::EncodedPayload) -> Option<ModelDirective> {
        match self {
            Self::Responses(model) => model.shared_parts().1.interpret_response(payload).ok(),
            Self::ChatCompletions(model) => model.shared_parts().1.interpret_response(payload).ok(),
        }
    }
}

fn session_history(
    address: &str,
    state: &ActorState,
    models: &[ConfiguredModel],
) -> Vec<HistoryEntry> {
    let mut history = Vec::new();
    for projected in state.context() {
        let entry = &projected.entry;
        match &entry.transition {
            ContextTransition::Messages {
                consumed_message_ids,
                ..
            }
            | ContextTransition::Interrupted {
                consumed_message_ids,
                ..
            } => {
                for message_id in consumed_message_ids {
                    let Some(message) = state.message(message_id) else {
                        continue;
                    };
                    if matches!(message.envelope.source(), MessageSource::Host { .. })
                        && let Some(entry) = runtime_notice(message.envelope.payload())
                    {
                        history.push(entry);
                        continue;
                    }
                    let (kind, title) = match message.envelope.source() {
                        MessageSource::User { .. } => (HistoryKind::User, "You".to_owned()),
                        MessageSource::Host { component } => {
                            (HistoryKind::System, component.as_str().to_owned())
                        }
                        MessageSource::Actor { actor_id } => {
                            (HistoryKind::System, actor_id.as_str().to_owned())
                        }
                    };
                    history.push(HistoryEntry {
                        kind,
                        title,
                        body: display_json(&message.envelope.payload().value),
                    });
                }
            }
            ContextTransition::Model { .. } => {
                let directive = models
                    .iter()
                    .find_map(|configured| configured.model.interpret_response(&entry.payload));
                match directive {
                    Some(ModelDirective::Eval(request)) => history.push(HistoryEntry {
                        kind: HistoryKind::ToolCall,
                        title: format!("{address} · {}", request.intent),
                        body: request.source,
                    }),
                    Some(ModelDirective::Output(output)) => history.push(HistoryEntry {
                        kind: HistoryKind::Assistant,
                        title: address.to_owned(),
                        body: display_json(&output),
                    }),
                    None => history.push(HistoryEntry {
                        kind: HistoryKind::System,
                        title: "Historical model output".to_owned(),
                        body: display_json(&entry.payload.value),
                    }),
                }
            }
            ContextTransition::Eval { .. } => history.push(HistoryEntry {
                kind: HistoryKind::ToolResult,
                title: format!("{address} · eval result"),
                body: display_json(&entry.payload.value),
            }),
            ContextTransition::Compaction { .. } => {
                let body = CompactionRecord::decode(&entry.payload)
                    .ok()
                    .flatten()
                    .and_then(|record| record.artifact)
                    .map_or_else(
                        || display_json(&entry.payload.value),
                        |artifact| artifact.summary,
                    );
                history.push(HistoryEntry {
                    kind: HistoryKind::System,
                    title: "Compact".to_owned(),
                    body,
                });
            }
        }
    }
    history
}

fn agent_history(
    address: &str,
    state: &ActorState,
    models: &[ConfiguredModel],
    restored_child: bool,
) -> AgentHistory {
    let active = state.active_run().is_some();
    let interrupted = state.context().last().is_some_and(|entry| {
        matches!(
            entry.entry.transition,
            ContextTransition::Interrupted { .. }
        )
    });
    AgentHistory {
        address: address.to_owned(),
        parent: address
            .rfind('/')
            .filter(|separator| *separator > 0)
            .map(|separator| address[..separator].to_owned()),
        model: state
            .selected_model()
            .map(|selection| selection.model_id.as_str().to_owned()),
        status: if active && restored_child {
            "Interrupted".to_owned()
        } else if active {
            "Recovering…".to_owned()
        } else if interrupted && restored_child {
            "Interrupted".to_owned()
        } else if restored_child {
            "Stored".to_owned()
        } else {
            "Ready".to_owned()
        },
        run_completed: !active,
        history: session_history(address, state, models),
    }
}

async fn load_stored_actors(store: &RedbStore) -> Result<Vec<(ActorId, ActorState)>, String> {
    let actor_ids = store.actor_ids().map_err(|error| error.to_string())?;
    let mut actors = Vec::with_capacity(actor_ids.len());
    for actor in actor_ids {
        let state = load_actor_state(store, &actor).await?;
        actors.push((actor, state));
    }
    Ok(actors)
}

async fn interrupted_tree(
    store: &RedbStore,
    models: &[ConfiguredModel],
    receipt: &AgentTreeInterruptionReceipt,
) -> Result<InterruptedTree, String> {
    let mut agents = Vec::with_capacity(receipt.actors.len());
    let mut runs = Vec::new();
    for actor in &receipt.actors {
        let actor_id = ActorId::new(actor.address.as_str()).map_err(|error| error.to_string())?;
        let state = load_actor_state(store, &actor_id).await?;
        agents.push(agent_history(
            actor.address.as_str(),
            &state,
            models,
            actor.address != receipt.root,
        ));
        if let Some(interruption) = &actor.interruption {
            runs.push(InterruptedRun {
                address: actor.address.to_string(),
                run_id: interruption.run_id.to_string(),
            });
        }
    }
    Ok(InterruptedTree { agents, runs })
}

async fn load_actor_state(store: &RedbStore, actor: &ActorId) -> Result<ActorState, String> {
    const PAGE_SIZE: NonZeroUsize = NonZeroUsize::new(256).expect("256 is nonzero");
    let mut state = ActorState::new();
    loop {
        let page = store
            .read(actor, state.revision(), PAGE_SIZE)
            .await
            .map_err(|error| error.to_string())?;
        let head = page.head;
        state = state.fold_page(page).map_err(|error| error.to_string())?;
        if state.revision() == head {
            return Ok(state);
        }
    }
}

fn runtime_notice(payload: &EncodedPayload) -> Option<HistoryEntry> {
    if payload.codec.id.as_str() != SYSTEM_NOTICE_CODEC_ID {
        return None;
    }
    let notice = payload.decode::<SystemNotice>().ok()?;
    let (title, body) = match notice {
        SystemNotice::RunInterrupted {
            run_id,
            isolate_state,
            interrupted_eval_outcome,
            ..
        } => {
            let mut body = format!("Run {run_id} was stopped at the user's request.");
            match isolate_state {
                IsolateState::Retained => body.push_str(" TypeScript state was retained."),
                IsolateState::Reset => body.push_str(
                    " The TypeScript isolate was reset; external effects may already have completed.",
                ),
            }
            match interrupted_eval_outcome {
                Some(InterruptedEvalOutcome::FailureRecorded) => {
                    body.push_str(" A failure result was recorded for the interrupted eval.");
                }
                Some(InterruptedEvalOutcome::Unknown) => {
                    body.push_str(" The interrupted eval has no authoritative result.");
                }
                None => {}
            }
            ("Run interrupted", body)
        }
        SystemNotice::RuntimeResumed {
            isolate_state,
            resumed_run_id,
            interrupted_eval_outcome,
        } => {
            let run = resumed_run_id
                .map(|run_id| format!(" while recovering run {run_id}"))
                .unwrap_or_default();
            let mut body = format!("The runtime resumed{run} with {isolate_state:?} state.");
            if interrupted_eval_outcome.is_some() {
                body.push_str(" A prior eval did not have an authoritative result.");
            }
            ("Runtime resumed", body)
        }
    };
    Some(HistoryEntry {
        kind: HistoryKind::System,
        title: title.to_owned(),
        body,
    })
}

fn display_json(value: &serde_json::Value) -> String {
    value.as_str().map_or_else(
        || serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
        str::to_owned,
    )
}

fn configured_models(config: &LoadedConfig) -> Result<Vec<ConfiguredModel>, RuntimeError> {
    let mut configured = Vec::with_capacity(config.models.len());
    for (provider, model, choice) in config.config.providers.iter().flat_map(|provider| {
        provider.models.iter().map(move |model| {
            let registry_id = format!("{}/{}", provider.name, model.id);
            let choice = config
                .models
                .iter()
                .find(|choice| choice.registry_id == registry_id)
                .expect("validated model list");
            (provider, model, choice)
        })
    }) {
        let (model, effort) = build_model(provider, model, choice)?;
        configured.push(ConfiguredModel {
            choice: choice.clone(),
            model,
            effort,
        });
    }
    Ok(configured)
}

fn build_model(
    provider: &ProviderConfig,
    model_config: &ModelConfig,
    choice: &ModelChoice,
) -> Result<(AnyModel, EffortControl), RuntimeError> {
    let api_key = provider
        .resolved_api_key()
        .map_err(|error| RuntimeError::Model(error.to_string()))?;
    let descriptor = |codec| {
        ModelDescriptor::new(&provider.name, &choice.model, codec)
            .expect("configuration validation rejects empty names")
    };
    let extra_body = serde_json::to_value(&model_config.extra_body)
        .map_err(|error| RuntimeError::Model(error.to_string()))?;
    let effort = EffortControl::new(
        provider
            .resolved_effort_path()
            .map_err(|error| RuntimeError::Model(error.to_string()))?,
        &choice.efforts,
    );
    match provider.protocol {
        ProviderProtocol::OpenaiResponses => {
            let key = api_key.ok_or_else(|| {
                RuntimeError::Model(format!(
                    "provider `{}` requires api_key or api_key_env for OpenAI Responses",
                    provider.name
                ))
            })?;
            let mut builder = Responses::builder(&choice.model)
                .api_key(key)
                .extra_body(extra_body);
            if let Some(base) = &provider.api_base {
                builder = builder.base_url(base);
            }
            let (transport, codec) = builder
                .build_parts()
                .map_err(|error| RuntimeError::Model(error.to_string()))?;
            let model = Model::new(
                transport,
                EffortCodec {
                    inner: codec,
                    control: effort.clone(),
                },
            )
            .with_descriptor(descriptor("openai/responses"))
            .with_context_window_tokens(choice.context_window);
            Ok((AnyModel::Responses(model), effort))
        }
        ProviderProtocol::OpenaiChatCompletions => {
            let mut builder = ChatCompletions::builder(&choice.model).extra_body(extra_body);
            if let Some(key) = api_key {
                builder = builder.api_key(key);
            }
            if let Some(base) = &provider.api_base {
                builder = builder.base_url(base);
            }
            let (transport, codec) = builder
                .build_parts()
                .map_err(|error| RuntimeError::Model(error.to_string()))?;
            let model = Model::new(
                transport,
                EffortCodec {
                    inner: codec,
                    control: effort.clone(),
                },
            )
            .with_descriptor(descriptor("openai/chat-completions"))
            .with_context_window_tokens(choice.context_window);
            Ok((AnyModel::ChatCompletions(model), effort))
        }
    }
}

fn default_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(2, usize::from)
        .clamp(1, 4)
}

fn coding_instruction(cwd: &Path, models: &[ModelChoice], default_model: usize) -> String {
    let configured_models = models
        .iter()
        .enumerate()
        .map(|(index, model)| {
            let default = if index == default_model {
                " (default)"
            } else {
                ""
            };
            format!(
                "- provider: `{}`; model: `{}`; selector: `{}`{default}; efforts: [{}]",
                model.provider,
                model.model,
                model.registry_id,
                model.efforts.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are a coding agent operating in `{}`. Work within this directory unless the user explicitly directs you otherwise. Use the installed coding and multi-agent namespaces when they help complete the task.\n\nConfigured inference providers and models:\n{configured_models}\n\nFor `lam.agents.spawn` and `lam.agents.call`, pass the exact raw values shown above as `model: {{ provider: \"<provider>\", model: \"<model>\" }}`. The selector is for model selection elsewhere; do not pass the selector in the `model` field. Omit `model` to use the configured default.",
        cwd.display()
    )
}

#[derive(Debug, Error)]
pub(crate) enum RuntimeError {
    #[error("could not open the durable session journal: {0}")]
    Journal(#[source] lam_redb::RedbStoreError),
    #[error("could not configure a model: {0}")]
    Model(String),
    #[error("could not configure coding capabilities: {0}")]
    CodingPack(String),
    #[error("could not restore runtime preferences: {0}")]
    Preferences(String),
    #[error("could not start the agent runtime: {0}")]
    AgentSystem(String),
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use lam::{
        ActorEvent, ActorId, AppendOutcome, CodecId, CodecRef, DeliveryMode, EncodedPayload,
        EventBatch, InterruptionReason, IsolateState, JournalStore, MessageEnvelope, MessageId,
        MessageSource, Revision, RunId, SYSTEM_NOTICE_CODEC_ID, SYSTEM_NOTICE_CODEC_VERSION,
        SystemNotice, Timestamp,
    };
    use lam_redb::RedbStore;
    use serde_json::json;

    use super::{
        EffortControl, coding_instruction, first_user_message, insert_json_path, runtime_notice,
    };
    use crate::config::ModelChoice;
    use crate::session::Session;

    #[tokio::test(flavor = "current_thread")]
    async fn session_preview_reads_the_first_user_message() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("session.redb");
        let store = RedbStore::create(&database_path).unwrap();
        let actor = ActorId::new("/root").unwrap();
        let message = MessageEnvelope::new(
            MessageId::new("message-1").unwrap(),
            MessageSource::User { principal: None },
            DeliveryMode::Steer,
            EncodedPayload::lam_json(json!("First question\nwith detail")).unwrap(),
            Timestamp::from_unix_millis(1),
        )
        .unwrap();
        let outcome = store
            .append(
                &actor,
                Revision::ZERO,
                EventBatch::one(ActorEvent::message_admitted(message)),
            )
            .await
            .unwrap();
        assert_eq!(
            outcome,
            AppendOutcome::Appended {
                head: Revision::new(1)
            }
        );
        drop(store);
        let session = Session {
            id: 1,
            cwd: directory.path().to_path_buf(),
            database_path,
        };

        assert_eq!(
            first_user_message(&session).await.unwrap().as_deref(),
            Some("First question\nwith detail")
        );
    }

    #[test]
    fn interruption_notice_has_a_stable_human_readable_history_row() {
        let payload = EncodedPayload::new(
            CodecRef::new(
                CodecId::new(SYSTEM_NOTICE_CODEC_ID).unwrap(),
                SYSTEM_NOTICE_CODEC_VERSION,
            ),
            serde_json::to_value(SystemNotice::RunInterrupted {
                run_id: RunId::new("run-1").unwrap(),
                reason: InterruptionReason::User,
                isolate_state: IsolateState::Reset,
                interrupted_eval_outcome: Some(lam::InterruptedEvalOutcome::FailureRecorded),
            })
            .unwrap(),
        );

        let row = runtime_notice(&payload).unwrap();

        assert_eq!(row.title, "Run interrupted");
        assert!(row.body.contains("stopped at the user's request"));
        assert!(
            row.body
                .contains("external effects may already have completed")
        );
        assert!(row.body.contains("failure result was recorded"));
    }

    #[test]
    fn coding_instruction_exposes_exact_subagent_model_coordinates() {
        let models = vec![
            ModelChoice {
                registry_id: "openai/gpt-5.6-luna".to_owned(),
                provider: "openai".to_owned(),
                model: "gpt-5.6-luna".to_owned(),
                display_name: "GPT-5.6 Luna".to_owned(),
                context_window: 400_000,
                efforts: vec!["none".to_owned(), "high".to_owned(), "max".to_owned()],
            },
            ModelChoice {
                registry_id: "fireworks/accounts/fireworks/models/deepseek-v4-flash-0731"
                    .to_owned(),
                provider: "fireworks".to_owned(),
                model: "accounts/fireworks/models/deepseek-v4-flash-0731".to_owned(),
                display_name: "DeepSeek V4 Flash".to_owned(),
                context_window: 1_040_000,
                efforts: vec!["none".to_owned(), "high".to_owned(), "max".to_owned()],
            },
        ];

        let instruction = coding_instruction(Path::new("/work/project"), &models, 0);

        assert!(instruction.contains("provider: `openai`; model: `gpt-5.6-luna`"));
        assert!(instruction.contains("selector: `openai/gpt-5.6-luna` (default)"));
        assert!(instruction.contains("efforts: [none, high, max]"));
        assert!(instruction.contains(
            "provider: `fireworks`; model: `accounts/fireworks/models/deepseek-v4-flash-0731`"
        ));
        assert!(instruction.contains("do not pass the selector in the `model` field"));
    }

    #[test]
    fn effort_control_defaults_to_last_value_and_updates_nested_request_field() {
        let control = EffortControl::new(
            vec!["reasoning".to_owned(), "effort".to_owned()],
            &["low".to_owned(), "high".to_owned()],
        );
        let mut request = json!({"outputKind": "text", "body": {"model": "example", "reasoning": {"summary": "auto"}}});

        insert_json_path(
            &mut request["body"],
            &control.path,
            serde_json::Value::String(control.selected()),
        );
        assert_eq!(request["body"]["reasoning"]["effort"], "high");
        assert_eq!(request["body"]["reasoning"]["summary"], "auto");
        assert!(request.get("reasoning").is_none());

        control.set("low").unwrap();
        insert_json_path(
            &mut request["body"],
            &control.path,
            serde_json::Value::String(control.selected()),
        );
        assert_eq!(request["body"]["reasoning"]["effort"], "low");
        assert!(control.set("max").is_err());
    }
}
