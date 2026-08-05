use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use lam::{
    ActorEventData, ActorId, ActorState, CompactionArtifact, CompactionRecord, ContextEntry,
    ContextTransition, DeliveryMode, EncodedPayload, InterruptedEvalOutcome, IsolateState,
    JournalStore, Lam, LamBuilder, MemStore, MessageId, MessageSource, Model, ModelCodec,
    ModelDelta, ModelDescriptor, ModelDirective, ModelRequestConfig, ModelResponseMetadata,
    ModelResponseProjection, ProjectedContextEntry, Revision, RunProgress, SYSTEM_NOTICE_CODEC_ID,
    SystemNotice,
};
use lam_agents::{Agent, AgentSystem, AgentSystemEvents, SubagentConfig, SubagentConfigBuilder};
use lam_code::{CodingPack, FilesystemAccess, LocalCommandRunner};
use lam_openai::chat_completions::{
    ChatCompletions, ChatCompletionsCodec, ChatCompletionsProvider,
};
use lam_openai::responses::{Responses, ResponsesCodec, ResponsesProvider};
use lam_redb::{ReadOnlyStore, RedbStore};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::boot::{phase, phase_sync};
use crate::config::{LoadedConfig, ModelChoice, ModelConfig, ProviderConfig, ProviderProtocol};
use crate::session::Session;

/// How long a quit or session switch waits for in-flight command tasks before
/// abandoning them and dropping the command runtime. Commands are normally
/// fast (steers, interrupts); the bound exists so a long compaction cannot
/// stall teardown indefinitely.
const COMMAND_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) struct Runtime {
    pub(crate) system: AgentSystem<RedbStore>,
    pub(crate) root: Agent<RedbStore>,
    pub(crate) events: AgentSystemEvents,
    pub(crate) models: Vec<ModelChoice>,
    pub(crate) selected_model: usize,
    pub(crate) agents: Vec<AgentHistory>,
    effort_controls: Vec<EffortControl>,
    history_models: Arc<[ConfiguredModel]>,
    store: Arc<RedbStore>,
    /// Executor for TUI commands. Command futures do journal I/O, including
    /// durable fsyncs, and must never run on the UI's single-thread runtime,
    /// where they would freeze rendering and input for their duration.
    command_runtime: Option<tokio::runtime::Runtime>,
    /// Handles of in-flight command tasks. Quiescence drains these before the
    /// command runtime is dropped so a quit or session switch never abandons
    /// a durable operation mid-flight.
    command_handles: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// One transcript projector per journaled actor. The projector is the
    /// TUI's only source of committed transcript content: it folds journal
    /// pages incrementally and renders each committed context entry through
    /// the same renderer the session-restore path uses, so the live view and
    /// the reload view agree by construction.
    projectors: BTreeMap<String, Projector>,
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
    /// Provider-reported context consumption (total_tokens) of the newest
    /// completed model response folded from the journal, when one exists.
    pub(crate) context_tokens: Option<u64>,
    pub(crate) history: Vec<CommittedRow>,
}

impl AgentHistory {
    pub(crate) fn root(history: Vec<CommittedRow>) -> Self {
        Self {
            address: "/root".to_owned(),
            parent: None,
            model: None,
            status: "Ready".to_owned(),
            run_completed: true,
            context_tokens: None,
            history,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HistoryKind {
    User,
    Assistant,
    Reasoning,
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
    /// A user message. Always admitted as a durable steer: if a run is
    /// active it is delivered at the next model boundary, otherwise the
    /// runner wakes and starts a new run with it.
    Message(String),
    Interrupt,
    Compact,
    SwitchModel {
        index: usize,
        registry_id: String,
    },
    SetEffort {
        index: usize,
        effort: String,
    },
    New,
    LoadSession(u64),
    RefreshSessions,
}

/// Command results carry state transitions only. Transcript content always
/// comes from the journal projectors, never from a command result.
#[derive(Debug)]
pub(crate) enum CommandResult {
    Message(Result<SentMessage, String>),
    /// Whether the interruption found a run to stop.
    Interrupt(Result<bool, String>),
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

/// Receipt for a durably admitted user message.
#[derive(Debug)]
pub(crate) struct SentMessage {
    /// Durable identity of the admitted mailbox message.
    pub(crate) message_id: String,
    /// The user's message text.
    pub(crate) text: String,
}

/// One committed transcript row rendered from a journal context entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommittedRow {
    pub(crate) entry: HistoryEntry,
    /// Run which produced the row, for model- and eval-derived rows. The
    /// view uses it to replace the matching streaming overlay rows.
    pub(crate) run_id: Option<String>,
}

/// Everything one incremental fold committed, in journal order.
#[derive(Debug, Default)]
pub(crate) struct FoldOutcome {
    pub(crate) rows: Vec<CommittedRow>,
    /// Provider-reported context consumption (total_tokens) of the newest
    /// completed model response folded by this pass, when one was present.
    pub(crate) context_tokens: Option<u64>,
    /// Run id of every model turn committed by this fold, in order. Each one
    /// supersedes the oldest streaming overlay segment for that run.
    pub(crate) model_turns: Vec<String>,
    /// Mailbox messages consumed into context by this fold.
    pub(crate) consumed_message_ids: Vec<String>,
    /// The actor's active run after the fold, if any.
    pub(crate) active_run: Option<String>,
    /// Runs this fold observed reaching a terminal or interrupted entry.
    /// Streaming deltas for these runs are stale and must be ignored.
    pub(crate) dead_runs: Vec<String>,
    /// Whether the fold ended on an interruption boundary.
    pub(crate) interrupted: bool,
}

struct Projector {
    actor_id: ActorId,
    /// `None` only while a fold is in flight or after a poisoned fold.
    state: Option<ActorState>,
}

impl Runtime {
    pub(crate) async fn build(
        config: &LoadedConfig,
        cwd: PathBuf,
        session: &Session,
        preferences: Option<&RuntimePreferences>,
    ) -> Result<Self, RuntimeError> {
        let models = phase_sync("configured_models", || configured_models(config))?;
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
        let coding = phase_sync("coding_pack_build", || {
            CodingPack::builder(&cwd)
                .filesystem_access(FilesystemAccess::ReadWrite)
                .shell(LocalCommandRunner::default())
                .build()
                .map_err(|error| RuntimeError::CodingPack(error.to_string()))
        })?;
        let store = phase_sync("redb_open", || RedbStore::open(&session.database_path))
            .map_err(RuntimeError::Journal)?;
        let system = phase_sync("agent_system_build", || {
            AgentSystem::builder(store)
                .worker_threads(default_worker_threads())
                .max_agents(64)
                .build()
                .map_err(|error| RuntimeError::AgentSystem(error.to_string()))
        })?;
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
        let root = phase("host_root_actor", async {
            system
                .host_with_subagents(root_builder.build().actor("/root"), children)
                .await
                .map_err(|error| RuntimeError::AgentSystem(error.to_string()))
        })
        .await?;

        let history_models: Arc<[ConfiguredModel]> = models.into();
        let store = system.state_store();
        let mut actor_ids = store
            .actor_ids()
            .map_err(|error| RuntimeError::AgentSystem(error.to_string()))?;
        if !actor_ids.iter().any(|actor| actor.as_str() == "/root") {
            actor_ids.push(ActorId::new("/root").expect("the root actor ID is valid"));
        }
        let mut projectors = BTreeMap::new();
        let mut agents = Vec::new();
        for actor in actor_ids {
            let address = actor.as_str().to_owned();
            // Bootstrap the projector from the newest checkpoint so a cold
            // start folds only post-compaction events. The checkpoint's
            // context is rendered into rows through the same path as live
            // folds, so the bootstrapped and fully-replayed views agree.
            let phase_name = format!("projector_bootstrap_{address}");
            let result = phase(&phase_name, async {
                let (initial, mut initial_rows) = match store
                    .read_checkpoint(&actor)
                    .await
                    .map_err(|error| RuntimeError::AgentSystem(error.to_string()))?
                {
                    Some((_, blob)) => match lam::Checkpoint::decode(&blob) {
                        Ok(checkpoint) => {
                            let state = checkpoint.into_state();
                            let mut rows = FoldOutcome::default();
                            for projected in state.context() {
                                accumulate_entry(
                                    &mut rows,
                                    &address,
                                    &projected.entry,
                                    &state,
                                    &history_models,
                                );
                            }
                            (state, rows.rows)
                        }
                        Err(_) => (ActorState::new(), Vec::new()),
                    },
                    None => (ActorState::new(), Vec::new()),
                };
                let mut projector = Projector {
                    actor_id: actor,
                    state: Some(initial),
                };
                let outcome =
                    fold_projector(store.as_ref(), &history_models, &address, &mut projector)
                        .await
                        .map_err(RuntimeError::AgentSystem)?;
                initial_rows.extend(outcome.rows);
                let state = projector
                    .state
                    .as_ref()
                    .expect("a successful fold always restores the projector state");
                let history = agent_history(
                    &address,
                    state,
                    initial_rows,
                    outcome.context_tokens,
                    address != "/root",
                );
                Ok::<_, RuntimeError>((history, projector))
            })
            .await?;
            let (history, projector) = result;
            agents.push(history);
            projectors.insert(address, projector);
        }
        agents.sort_by(|left, right| left.address.cmp(&right.address));

        let selected_model_id = projectors
            .get("/root")
            .and_then(|projector| projector.state.as_ref())
            .and_then(ActorState::selected_model)
            .expect("actor initialization establishes a model selection")
            .model_id
            .as_str()
            .to_owned();
        let selected_model = config
            .models
            .iter()
            .position(|model| model.registry_id == selected_model_id)
            .expect("runtime registration mirrors the validated model list");
        // Built last so no error path in this function ever owns a multi-thread
        // tokio runtime: an unbuilt runtime needs no teardown, and a built one
        // is only dropped from quiesce on a blocking thread.
        let command_runtime = phase_sync("command_runtime_build", || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("lam-tui-command")
                .enable_all()
                .build()
                .map_err(|error| RuntimeError::AgentSystem(format!("command runtime: {error}")))
        })?;

        Ok(Self {
            system,
            root,
            events,
            models: config.models.clone(),
            selected_model,
            agents,
            effort_controls,
            history_models,
            store,
            command_runtime: Some(command_runtime),
            command_handles: Mutex::new(Vec::new()),
            projectors,
        })
    }

    /// Folds any newly committed journal events for the actor and returns the
    /// rendered rows in journal order. Creates the projector on first contact
    /// so freshly spawned children fold from revision zero.
    pub(crate) async fn fold(&mut self, address: &str) -> Result<FoldOutcome, String> {
        if !self.projectors.contains_key(address) {
            let actor_id = ActorId::new(address).map_err(|error| error.to_string())?;
            self.projectors.insert(
                address.to_owned(),
                Projector {
                    actor_id,
                    state: Some(ActorState::new()),
                },
            );
        }
        let projector = self
            .projectors
            .get_mut(address)
            .expect("the projector was just ensured");
        fold_projector(
            self.store.as_ref(),
            &self.history_models,
            address,
            projector,
        )
        .await
    }

    /// Addresses with a live transcript projector.
    pub(crate) fn projected_addresses(&self) -> Vec<String> {
        self.projectors.keys().cloned().collect()
    }

    /// Whether the message has already been consumed into model-visible
    /// context, according to the projector's folded state. Callers must fold
    /// first; folds and receipt handling share the main loop, so the answer
    /// is race-free.
    pub(crate) fn is_consumed(&self, address: &str, message_id: &str) -> bool {
        let Ok(message_id) = MessageId::new(message_id) else {
            return false;
        };
        self.projectors
            .get(address)
            .and_then(|projector| projector.state.as_ref())
            .and_then(|state| state.message(&message_id))
            .is_some_and(|message| message.consumed_at.is_some())
    }

    /// Consumes the runtime and returns the actor journal store, releasing
    /// the agent system, root actor, and projectors. Teardown paths use this
    /// to run best-effort maintenance (journal compaction) once nothing else
    /// holds the store.
    pub(crate) fn into_store(self) -> Arc<RedbStore> {
        let store = self.store.clone();
        drop(self);
        store
    }

    pub(crate) fn selected_effort(&self) -> String {
        self.effort_controls[self.selected_model].selected()
    }

    pub(crate) fn execute(&self, command: Command, output: mpsc::UnboundedSender<CommandResult>) {
        let root = self.root.clone();
        let effort_controls = self.effort_controls.clone();
        let runtime = self
            .command_runtime
            .as_ref()
            .expect("the command runtime is present while the TUI is running");
        let handle = runtime.spawn(async move {
            let result = match command {
                Command::Message(input) => {
                    let result = match root.send(input.clone(), DeliveryMode::Steer).await {
                        Ok(receipt) => {
                            tracing::debug!(
                                target: "lam_tui::runtime",
                                event = "tui.message_admitted",
                                actor_id = "/root",
                                message_id = %receipt.message_id,
                                "user message durably admitted"
                            );
                            Ok(SentMessage {
                                message_id: receipt.message_id.to_string(),
                                text: input,
                            })
                        }
                        Err(error) => Err(error.to_string()),
                    };
                    CommandResult::Message(result)
                }
                Command::Interrupt => {
                    let result = match root.interrupt().await {
                        Ok(receipt) => Ok(receipt.is_some()),
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
        self.command_handles.lock().unwrap().push(handle);
    }

    /// Winds the command executor down: waits up to COMMAND_DRAIN_TIMEOUT for
    /// in-flight command tasks, then drops the multi-thread command runtime on
    /// a thread where blocking is permitted. Call before dropping the Runtime
    /// on the quit and session-switch paths.
    pub(crate) async fn quiesce(&mut self) {
        quiesce_command_runtime(&mut self.command_runtime, &self.command_handles).await;
    }

    /// Snapshots every projector at the journal head so the next cold boot
    /// bootstraps from the head and folds approximately nothing. Compaction is
    /// the only other checkpoint writer, so without this a long stretch of work
    /// without a compaction costs a long fold at the next boot. Call after the
    /// agent system is stopped and the command runtime is quiesced, so the
    /// journal cannot advance past the snapshot, and before journal compaction,
    /// so the orphaned old blobs are reclaimed in the same teardown.
    ///
    /// Best-effort throughout: teardown never fails because maintenance did.
    pub(crate) async fn write_teardown_checkpoints(&mut self) {
        for (address, projector) in &mut self.projectors {
            if let Err(error) = checkpoint_projector(
                self.store.as_ref(),
                &self.history_models,
                address,
                projector,
            )
            .await
            {
                tracing::warn!(
                    target: "lam_tui::runtime",
                    event = "session.checkpoint_skipped",
                    actor_id = %address,
                    %error,
                    "teardown checkpoint skipped; the next boot folds from the older checkpoint"
                );
            }
        }
    }
}

/// Drains in-flight command tasks and then drops the command runtime. Kept as
/// a free function so the teardown sequence is testable without a full
/// Runtime.
async fn quiesce_command_runtime(
    command_runtime: &mut Option<tokio::runtime::Runtime>,
    command_handles: &Mutex<Vec<tokio::task::JoinHandle<()>>>,
) {
    let handles = std::mem::take(&mut *command_handles.lock().unwrap());
    let drain = async {
        for handle in handles {
            let _ = handle.await;
        }
    };
    let _ = tokio::time::timeout(COMMAND_DRAIN_TIMEOUT, drain).await;
    if let Some(runtime) = command_runtime.take() {
        drop_runtime_outside_async(runtime);
    }
}

/// Dropping a multi-thread tokio runtime joins its worker threads, which is
/// only permitted where blocking is allowed. tokio_main tears down inside a
/// current-thread async context, so the runtime is handed to a plain thread,
/// the canonical context where runtime shutdown may block, and joined.
fn drop_runtime_outside_async(runtime: tokio::runtime::Runtime) {
    let thread = std::thread::Builder::new()
        .name("lam-tui-command-shutdown".to_owned())
        .spawn(move || drop(runtime));
    if let Ok(handle) = thread {
        let _ = handle.join();
    }
}

pub(crate) async fn first_user_message(session: &Session) -> Result<Option<String>, RuntimeError> {
    const PAGE_SIZE: NonZeroUsize = NonZeroUsize::new(256).expect("256 is nonzero");
    // Previews are one picker row plus its search text; a bound keeps the
    // cached copy in the session catalog small even for pasted novels.
    const PREVIEW_MAX_CHARS: usize = 300;
    let store = ReadOnlyStore::open(&session.database_path).map_err(RuntimeError::Journal)?;
    let actor = ActorId::new("/root").expect("the root actor ID is valid");
    let mut after = Revision::ZERO;
    loop {
        let page = store
            .read_page(&actor, after, PAGE_SIZE)
            .map_err(RuntimeError::Journal)?;
        let head = page.head;
        for stored in page.events {
            after = stored.revision;
            let ActorEventData::MessageAdmitted { message } = stored.event.data() else {
                continue;
            };
            if matches!(message.source(), MessageSource::User { .. }) {
                let text = display_json(&message.payload().value);
                let text = match text.char_indices().nth(PREVIEW_MAX_CHARS) {
                    Some((cut, _)) => text[..cut].to_owned(),
                    None => text,
                };
                return Ok(Some(text));
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

    fn project_response(
        &self,
        response: &EncodedPayload,
    ) -> Result<ModelResponseProjection, Self::Error> {
        self.inner.project_response(response)
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

    fn project_response(&self, payload: &lam::EncodedPayload) -> Option<ModelResponseProjection> {
        match self {
            Self::Responses(model) => model.shared_parts().1.project_response(payload).ok(),
            Self::ChatCompletions(model) => model.shared_parts().1.project_response(payload).ok(),
        }
    }

    fn response_metadata(&self, payload: &lam::EncodedPayload) -> Option<ModelResponseMetadata> {
        match self {
            Self::Responses(model) => {
                let codec = model.shared_parts().1;
                (payload.codec.id.as_str() == lam_openai::responses::RESPONSE_CODEC_ID
                    && payload.codec.version == lam_openai::responses::PAYLOAD_VERSION)
                    .then(|| codec.response_metadata(payload))
            }
            Self::ChatCompletions(model) => {
                let codec = model.shared_parts().1;
                (payload.codec.id.as_str() == lam_openai::chat_completions::RESPONSE_CODEC_ID
                    && payload.codec.version == lam_openai::chat_completions::PAYLOAD_VERSION)
                    .then(|| codec.response_metadata(payload))
            }
        }
    }
}

fn append_model_delta(history: &mut Vec<HistoryEntry>, address: &str, delta: ModelDelta) {
    let (kind, title, body) = match delta {
        ModelDelta::Text(text) if text.is_empty() => return,
        ModelDelta::Text(text) => (HistoryKind::Assistant, address.to_owned(), text),
        ModelDelta::Reasoning(text) if text.is_empty() => return,
        ModelDelta::Reasoning(text) => (
            HistoryKind::Reasoning,
            format!("{address} · reasoning"),
            text,
        ),
        ModelDelta::ToolCall(_) => return,
    };
    if let Some(last) = history.last_mut()
        && last.kind == kind
        && last.title == title
    {
        last.body.push_str(&body);
    } else {
        history.push(HistoryEntry { kind, title, body });
    }
}

fn append_model_projection(
    history: &mut Vec<HistoryEntry>,
    address: &str,
    projection: ModelResponseProjection,
) {
    let mut projected = Vec::new();
    let mut tool_calls = BTreeMap::<usize, (String, String)>::new();
    for delta in projection.display {
        match delta {
            ModelDelta::ToolCall(delta) => {
                let call = tool_calls.entry(delta.index).or_default();
                if let Some(name) = delta.name {
                    call.0.push_str(&name);
                }
                call.1.push_str(&delta.arguments);
            }
            delta => append_model_delta(&mut projected, address, delta),
        }
    }
    match projection.directive {
        ModelDirective::Eval(request) => {
            projected.push(HistoryEntry {
                kind: HistoryKind::ToolCall,
                title: format!("{address} · {}", request.intent),
                body: request.source,
            });
            for (_, (name, arguments)) in tool_calls.into_iter().skip(1) {
                projected.push(historical_tool_call(address, &name, arguments));
            }
        }
        ModelDirective::Output(output) => {
            let has_text = projected
                .iter()
                .any(|entry| entry.kind == HistoryKind::Assistant);
            if !has_text {
                projected.push(HistoryEntry {
                    kind: HistoryKind::Assistant,
                    title: address.to_owned(),
                    body: display_json(&output),
                });
            }
        }
        // No call parsed, so every native call renders from its raw
        // fragments; the rejection message itself arrives as the eval
        // result rows that follow this entry in the journal.
        ModelDirective::Rejected { .. } => {
            for (_, (name, arguments)) in tool_calls {
                projected.push(historical_tool_call(address, &name, arguments));
            }
        }
    }
    history.extend(projected);
}

fn historical_tool_call(address: &str, name: &str, arguments: String) -> HistoryEntry {
    let parsed = serde_json::from_str::<serde_json::Value>(&arguments).ok();
    let intent = parsed
        .as_ref()
        .and_then(|value| value.get("intent"))
        .and_then(serde_json::Value::as_str);
    let source = parsed
        .as_ref()
        .and_then(|value| value.get("source"))
        .and_then(serde_json::Value::as_str);
    let label = intent
        .or((!name.is_empty()).then_some(name))
        .unwrap_or("tool call");
    HistoryEntry {
        kind: HistoryKind::ToolCall,
        title: format!("{address} · {label}"),
        body: source.map_or(arguments, str::to_owned),
    }
}

/// Renders one committed context entry into transcript rows. This is the
/// single renderer for all transcript content: session restore and live
/// folds both pass through it, so the two views agree by construction.
fn render_context_entry(
    address: &str,
    entry: &ContextEntry,
    state: &ActorState,
    models: &[ConfiguredModel],
) -> Vec<HistoryEntry> {
    let mut history = Vec::new();
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
            let projection = models
                .iter()
                .find_map(|configured| configured.model.project_response(&entry.payload));
            match projection {
                Some(projection) => {
                    append_model_projection(&mut history, address, projection);
                }
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
    history
}

const PROJECTOR_PAGE: NonZeroUsize = NonZeroUsize::new(256).expect("256 is nonzero");

/// Folds every journal event past the projector's revision and renders the
/// newly committed context entries in journal order.
async fn fold_projector(
    store: &RedbStore,
    models: &[ConfiguredModel],
    address: &str,
    projector: &mut Projector,
) -> Result<FoldOutcome, String> {
    let mut outcome = FoldOutcome::default();
    let mut state = projector.state.take().ok_or_else(|| {
        format!("the transcript projector for {address} is poisoned by an earlier fold failure")
    })?;
    loop {
        let page = match store
            .read(&projector.actor_id, state.revision(), PROJECTOR_PAGE)
            .await
        {
            Ok(page) => page,
            Err(error) => {
                projector.state = Some(state);
                return Err(error.to_string());
            }
        };
        let head = page.head;
        if page.events.is_empty() {
            break;
        }
        let entries =
            page.events
                .iter()
                .filter_map(|stored| match stored.event.data() {
                    ActorEventData::ContextAppended { entry } => Some(entry.clone()),
                    ActorEventData::MessageAdmitted { .. }
                    | ActorEventData::ModelSelected { .. } => None,
                })
                .collect::<Vec<_>>();
        state = state.fold_page(page).map_err(|error| error.to_string())?;
        for entry in &entries {
            accumulate_entry(&mut outcome, address, entry, &state, models);
        }
        if state.revision() >= head {
            break;
        }
    }
    outcome.active_run = state.active_run().map(ToString::to_string);
    outcome.interrupted = state.context().last().is_some_and(|entry| {
        matches!(
            entry.entry.transition,
            ContextTransition::Interrupted { .. }
        )
    });
    projector.state = Some(state);
    Ok(outcome)
}

/// Snapshots one projector at the journal head and reports whether a
/// checkpoint was written. The projector is folded forward first: interruption
/// records and other events may have landed after the last UI fold, and only a
/// head-fresh snapshot spares the next boot a fold.
///
/// The write is gated on strict revision progress. Replacing a checkpoint at a
/// revision already stored orphans the previous blob's pages, so an ungated
/// rewrite on every quit would manufacture compaction waste for no benefit.
async fn checkpoint_projector(
    store: &RedbStore,
    models: &[ConfiguredModel],
    address: &str,
    projector: &mut Projector,
) -> Result<bool, String> {
    let started = std::time::Instant::now();
    fold_projector(store, models, address, projector).await?;
    let state = projector.state.as_ref().ok_or_else(|| {
        format!("the transcript projector for {address} has no folded state to snapshot")
    })?;
    let revision = state.revision();
    if revision == Revision::ZERO {
        return Ok(false);
    }
    let stored = store
        .read_checkpoint(&projector.actor_id)
        .await
        .map_err(|error| error.to_string())?;
    if stored.is_some_and(|(stored, _)| stored >= revision) {
        return Ok(false);
    }
    let blob = lam::Checkpoint::from_state(state).encode();
    store
        .write_checkpoint(&projector.actor_id, revision, &blob)
        .await
        .map_err(|error| error.to_string())?;
    tracing::info!(
        target: "lam_tui::runtime",
        event = "session.checkpoint",
        actor_id = %address,
        revision = revision.get(),
        blob_bytes = blob.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "teardown checkpoint written at the journal head"
    );
    Ok(true)
}

fn accumulate_entry(
    outcome: &mut FoldOutcome,
    address: &str,
    entry: &ContextEntry,
    state: &ActorState,
    models: &[ConfiguredModel],
) {
    match &entry.transition {
        ContextTransition::Model { run_id, progress } => {
            outcome.model_turns.push(run_id.to_string());
            if *progress == RunProgress::Complete {
                outcome.dead_runs.push(run_id.to_string());
            }
            if let Some(usage) = models
                .iter()
                .find_map(|configured| configured.model.response_metadata(&entry.payload))
                .and_then(|metadata| metadata.usage)
            {
                outcome.context_tokens = Some(usage.total_tokens);
            }
        }
        ContextTransition::Interrupted { run_id, .. } => {
            outcome.dead_runs.push(run_id.to_string());
        }
        ContextTransition::Eval { .. }
        | ContextTransition::Messages { .. }
        | ContextTransition::Compaction { .. } => {}
    }
    for message_id in entry.transition.consumed_message_ids() {
        outcome.consumed_message_ids.push(message_id.to_string());
    }
    let run_id = match &entry.transition {
        ContextTransition::Model { run_id, .. } | ContextTransition::Eval { run_id } => {
            Some(run_id.to_string())
        }
        ContextTransition::Messages { .. }
        | ContextTransition::Interrupted { .. }
        | ContextTransition::Compaction { .. } => None,
    };
    for row in render_context_entry(address, entry, state, models) {
        outcome.rows.push(CommittedRow {
            entry: row,
            run_id: run_id.clone(),
        });
    }
}

fn agent_history(
    address: &str,
    state: &ActorState,
    history: Vec<CommittedRow>,
    context_tokens: Option<u64>,
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
        context_tokens,
        history,
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
        ActorEvent, ActorId, ActorState, AppendOutcome, CodecId, CodecRef, ContextEntry,
        ContextTransition, DeliveryMode, EncodedPayload, EventBatch, InterruptionReason,
        IsolateState, JournalStore, MessageEnvelope, MessageId, MessageSource, Model, ModelDelta,
        ModelDescriptor, ModelDirective, ModelResponseProjection, Revision, RunId, RunProgress,
        SYSTEM_NOTICE_CODEC_ID, SYSTEM_NOTICE_CODEC_VERSION, SystemNotice, Timestamp,
    };
    use lam_openai::chat_completions::{
        ChatCompletions, PAYLOAD_VERSION as CHAT_PAYLOAD_VERSION,
        RESPONSE_CODEC_ID as CHAT_RESPONSE_CODEC_ID,
    };
    use lam_openai::responses::{PAYLOAD_VERSION, RESPONSE_CODEC_ID, Responses};
    use lam_redb::RedbStore;
    use serde_json::json;

    use super::{
        AnyModel, ConfiguredModel, EffortCodec, EffortControl, HistoryKind, Projector,
        append_model_projection, checkpoint_projector, coding_instruction,
        drop_runtime_outside_async, first_user_message, fold_projector, insert_json_path,
        quiesce_command_runtime, runtime_notice,
    };
    use crate::config::ModelChoice;
    use crate::session::Session;

    fn context_entry(transition: ContextTransition) -> ActorEvent {
        ActorEvent::context_appended(ContextEntry {
            transition,
            payload: EncodedPayload::lam_json(json!("payload")).unwrap(),
            recorded_at: Timestamp::from_unix_millis(1),
        })
    }

    fn user_message(id: &str, text: &str) -> ActorEvent {
        ActorEvent::message_admitted(
            MessageEnvelope::new(
                MessageId::new(id).unwrap(),
                MessageSource::User { principal: None },
                DeliveryMode::Steer,
                EncodedPayload::lam_json(json!(text)).unwrap(),
                Timestamp::from_unix_millis(1),
            )
            .unwrap(),
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn projector_folds_incrementally_and_renders_journal_order() {
        let directory = tempfile::tempdir().unwrap();
        let store = RedbStore::create(directory.path().join("session.redb")).unwrap();
        let actor = ActorId::new("/root").unwrap();
        let run = RunId::new("run-1").unwrap();

        let appended = store
            .append(
                &actor,
                Revision::ZERO,
                EventBatch::new(
                    user_message("m-1", "start the task"),
                    vec![
                        context_entry(ContextTransition::Messages {
                            run_id: run.clone(),
                            consumed_message_ids: vec![MessageId::new("m-1").unwrap()],
                        }),
                        context_entry(ContextTransition::Model {
                            run_id: run.clone(),
                            progress: RunProgress::Continue,
                        }),
                        context_entry(ContextTransition::Eval {
                            run_id: run.clone(),
                        }),
                    ],
                ),
            )
            .await
            .unwrap();
        assert!(matches!(appended, AppendOutcome::Appended { .. }));

        let mut projector = Projector {
            actor_id: actor.clone(),
            state: Some(ActorState::new()),
        };
        let outcome = fold_projector(&store, &[], "/root", &mut projector)
            .await
            .unwrap();
        let kinds = outcome
            .rows
            .iter()
            .map(|row| row.entry.kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                HistoryKind::User,
                HistoryKind::System,
                HistoryKind::ToolResult
            ]
        );
        assert_eq!(outcome.rows[0].entry.body, "start the task");
        assert_eq!(outcome.rows[2].run_id.as_deref(), Some("run-1"));
        assert_eq!(outcome.consumed_message_ids, ["m-1"]);
        assert_eq!(outcome.active_run.as_deref(), Some("run-1"));
        assert_eq!(outcome.model_turns, ["run-1"]);
        assert!(outcome.dead_runs.is_empty());
        assert!(!outcome.interrupted);

        let appended = store
            .append(
                &actor,
                Revision::new(4),
                EventBatch::new(
                    user_message("m-2", "also do this"),
                    vec![
                        context_entry(ContextTransition::Messages {
                            run_id: run.clone(),
                            consumed_message_ids: vec![MessageId::new("m-2").unwrap()],
                        }),
                        context_entry(ContextTransition::Model {
                            run_id: run.clone(),
                            progress: RunProgress::Complete,
                        }),
                    ],
                ),
            )
            .await
            .unwrap();
        assert!(matches!(appended, AppendOutcome::Appended { .. }));

        // The second fold returns only the newly committed rows.
        let outcome = fold_projector(&store, &[], "/root", &mut projector)
            .await
            .unwrap();
        let kinds = outcome
            .rows
            .iter()
            .map(|row| row.entry.kind)
            .collect::<Vec<_>>();
        assert_eq!(kinds, [HistoryKind::User, HistoryKind::System]);
        assert_eq!(outcome.rows[0].entry.body, "also do this");
        assert_eq!(outcome.consumed_message_ids, ["m-2"]);
        assert_eq!(outcome.active_run, None);
        assert_eq!(outcome.dead_runs, ["run-1"]);

        // A fold with nothing new commits nothing.
        let outcome = fold_projector(&store, &[], "/root", &mut projector)
            .await
            .unwrap();
        assert!(outcome.rows.is_empty());
        assert!(outcome.consumed_message_ids.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fold_records_provider_usage_from_model_entries() {
        let directory = tempfile::tempdir().unwrap();
        let store = RedbStore::create(directory.path().join("session.redb")).unwrap();
        let actor = ActorId::new("/root").unwrap();
        let run = RunId::new("run-1").unwrap();

        let (transport, codec) = Responses::builder("gpt-test")
            .api_key("test-key")
            .build_parts()
            .unwrap();
        let effort = EffortControl::new(vec!["effort".to_owned()], &["high".to_owned()]);
        let model = Model::new(
            transport,
            EffortCodec {
                inner: codec,
                control: effort.clone(),
            },
        )
        .with_descriptor(ModelDescriptor::new("openai", "gpt-test", "openai/responses").unwrap())
        .with_context_window_tokens(400_000);
        let configured = ConfiguredModel {
            choice: ModelChoice {
                registry_id: "openai/gpt-test".to_owned(),
                provider: "openai".to_owned(),
                model: "gpt-test".to_owned(),
                display_name: "GPT Test".to_owned(),
                context_window: 400_000,
                efforts: vec!["high".to_owned()],
            },
            model: AnyModel::Responses(model),
            effort,
        };

        let response = |total: u64| {
            EncodedPayload::new(
                CodecRef::new(CodecId::new(RESPONSE_CODEC_ID).unwrap(), PAYLOAD_VERSION),
                json!({
                    "outputKind": "text",
                    "model": "gpt-test",
                    "response": {
                        "id": "resp-1",
                        "model": "gpt-test",
                        "output": [{
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": "hello" }]
                        }],
                        "usage": {
                            "input_tokens": 10_000,
                            "output_tokens": total - 10_000,
                            "total_tokens": total,
                        }
                    }
                }),
            )
        };

        let appended = store
            .append(
                &actor,
                Revision::ZERO,
                EventBatch::new(
                    user_message("m-1", "start the task"),
                    vec![
                        context_entry(ContextTransition::Messages {
                            run_id: run.clone(),
                            consumed_message_ids: vec![MessageId::new("m-1").unwrap()],
                        }),
                        ActorEvent::context_appended(ContextEntry {
                            transition: ContextTransition::Model {
                                run_id: run.clone(),
                                progress: RunProgress::Continue,
                            },
                            payload: response(12_345),
                            recorded_at: Timestamp::from_unix_millis(1),
                        }),
                        ActorEvent::context_appended(ContextEntry {
                            transition: ContextTransition::Model {
                                run_id: run.clone(),
                                progress: RunProgress::Complete,
                            },
                            payload: response(20_000),
                            recorded_at: Timestamp::from_unix_millis(2),
                        }),
                    ],
                ),
            )
            .await
            .unwrap();
        assert!(matches!(appended, AppendOutcome::Appended { .. }));

        let mut projector = Projector {
            actor_id: actor.clone(),
            state: Some(ActorState::new()),
        };
        let outcome = fold_projector(&store, &[configured], "/root", &mut projector)
            .await
            .unwrap();

        // The newest completed model response wins: 20_000, not 12_345.
        assert_eq!(outcome.context_tokens, Some(20_000));
        assert!(!outcome.rows.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fold_matches_payload_codec_across_model_variants() {
        // openai/responses is configured first, but the journal payload is a
        // chat-completions stream; the fold must pick the codec that owns the
        // payload instead of always using the first configured model.
        let directory = tempfile::tempdir().unwrap();
        let store = RedbStore::create(directory.path().join("session.redb")).unwrap();
        let actor = ActorId::new("/root").unwrap();
        let run = RunId::new("run-1").unwrap();

        let (transport, codec) = Responses::builder("gpt-test")
            .api_key("test-key")
            .build_parts()
            .unwrap();
        let effort = EffortControl::new(vec!["effort".to_owned()], &["high".to_owned()]);
        let responses = ConfiguredModel {
            choice: ModelChoice {
                registry_id: "openai/gpt-test".to_owned(),
                provider: "openai".to_owned(),
                model: "gpt-test".to_owned(),
                display_name: "GPT Test".to_owned(),
                context_window: 1_050_000,
                efforts: vec!["high".to_owned()],
            },
            model: AnyModel::Responses(
                Model::new(
                    transport,
                    EffortCodec {
                        inner: codec,
                        control: effort.clone(),
                    },
                )
                .with_descriptor(
                    ModelDescriptor::new("openai", "gpt-test", "openai/responses").unwrap(),
                )
                .with_context_window_tokens(1_050_000),
            ),
            effort: effort.clone(),
        };
        let (transport, codec) = ChatCompletions::builder("gpt-test")
            .api_key("test-key")
            .build_parts()
            .unwrap();
        let chat = ConfiguredModel {
            choice: ModelChoice {
                registry_id: "fireworks/gpt-test".to_owned(),
                provider: "fireworks".to_owned(),
                model: "gpt-test".to_owned(),
                display_name: "GPT Test".to_owned(),
                context_window: 1_040_000,
                efforts: vec!["high".to_owned()],
            },
            model: AnyModel::ChatCompletions(
                Model::new(
                    transport,
                    EffortCodec {
                        inner: codec,
                        control: effort.clone(),
                    },
                )
                .with_descriptor(
                    ModelDescriptor::new("fireworks", "gpt-test", "openai/chat-completions")
                        .unwrap(),
                )
                .with_context_window_tokens(1_040_000),
            ),
            effort,
        };

        let payload = EncodedPayload::new(
            CodecRef::new(
                CodecId::new(CHAT_RESPONSE_CODEC_ID).unwrap(),
                CHAT_PAYLOAD_VERSION,
            ),
            json!({
                "outputKind": "text",
                "model": "gpt-test",
                "chunks": [{
                    "id": "chatcmpl-1",
                    "object": "chat.completion.chunk",
                    "model": "gpt-test",
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 10_000,
                        "completion_tokens": 2_345,
                        "total_tokens": 12_345
                    }
                }]
            }),
        );

        let appended = store
            .append(
                &actor,
                Revision::ZERO,
                EventBatch::new(
                    user_message("m-1", "start the task"),
                    vec![
                        context_entry(ContextTransition::Messages {
                            run_id: run.clone(),
                            consumed_message_ids: vec![MessageId::new("m-1").unwrap()],
                        }),
                        ActorEvent::context_appended(ContextEntry {
                            transition: ContextTransition::Model {
                                run_id: run.clone(),
                                progress: RunProgress::Complete,
                            },
                            payload,
                            recorded_at: Timestamp::from_unix_millis(1),
                        }),
                    ],
                ),
            )
            .await
            .unwrap();
        assert!(matches!(appended, AppendOutcome::Appended { .. }));

        let mut projector = Projector {
            actor_id: actor.clone(),
            state: Some(ActorState::new()),
        };
        // The responses model is listed first, but the payload belongs to
        // the chat-completions codec.
        let outcome = fold_projector(&store, &[responses, chat], "/root", &mut projector)
            .await
            .unwrap();
        assert_eq!(outcome.context_tokens, Some(12_345));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn teardown_checkpoint_snapshots_the_head_then_skips_an_unchanged_projector() {
        let directory = tempfile::tempdir().unwrap();
        let store = RedbStore::create(directory.path().join("session.redb")).unwrap();
        let actor = ActorId::new("/root").unwrap();
        let run = RunId::new("run-1").unwrap();

        let appended = store
            .append(
                &actor,
                Revision::ZERO,
                EventBatch::new(
                    user_message("m-1", "start the task"),
                    vec![
                        context_entry(ContextTransition::Messages {
                            run_id: run.clone(),
                            consumed_message_ids: vec![MessageId::new("m-1").unwrap()],
                        }),
                        context_entry(ContextTransition::Model {
                            run_id: run.clone(),
                            progress: RunProgress::Complete,
                        }),
                    ],
                ),
            )
            .await
            .unwrap();
        assert!(matches!(appended, AppendOutcome::Appended { .. }));

        let mut projector = Projector {
            actor_id: actor.clone(),
            state: Some(ActorState::new()),
        };
        fold_projector(&store, &[], "/root", &mut projector)
            .await
            .unwrap();
        let head = projector.state.as_ref().unwrap().revision();
        assert_eq!(head, Revision::new(3));

        assert!(
            checkpoint_projector(&store, &[], "/root", &mut projector)
                .await
                .unwrap()
        );
        let (revision, blob) = store.read_checkpoint(&actor).await.unwrap().unwrap();
        assert_eq!(revision, head);
        assert_eq!(
            &lam::Checkpoint::decode(&blob).unwrap().into_state(),
            projector.state.as_ref().unwrap()
        );

        // Nothing new committed: a rewrite at the stored revision would only
        // orphan the stored blob's pages.
        assert!(
            !checkpoint_projector(&store, &[], "/root", &mut projector)
                .await
                .unwrap()
        );
        assert_eq!(
            store.read_checkpoint(&actor).await.unwrap().unwrap().0,
            head
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn teardown_checkpoint_folds_a_stale_projector_to_the_head() {
        let directory = tempfile::tempdir().unwrap();
        let store = RedbStore::create(directory.path().join("session.redb")).unwrap();
        let actor = ActorId::new("/root").unwrap();
        let run = RunId::new("run-1").unwrap();

        let appended = store
            .append(
                &actor,
                Revision::ZERO,
                EventBatch::new(
                    user_message("m-1", "start the task"),
                    vec![context_entry(ContextTransition::Messages {
                        run_id: run.clone(),
                        consumed_message_ids: vec![MessageId::new("m-1").unwrap()],
                    })],
                ),
            )
            .await
            .unwrap();
        assert!(matches!(appended, AppendOutcome::Appended { .. }));
        let mut projector = Projector {
            actor_id: actor.clone(),
            state: Some(ActorState::new()),
        };
        fold_projector(&store, &[], "/root", &mut projector)
            .await
            .unwrap();
        assert!(
            checkpoint_projector(&store, &[], "/root", &mut projector)
                .await
                .unwrap()
        );

        // Events committed after the last fold, as an interruption record
        // appended during teardown would be.
        let appended = store
            .append(
                &actor,
                Revision::new(2),
                EventBatch::new(
                    context_entry(ContextTransition::Model {
                        run_id: run.clone(),
                        progress: RunProgress::Continue,
                    }),
                    vec![context_entry(ContextTransition::Eval {
                        run_id: run.clone(),
                    })],
                ),
            )
            .await
            .unwrap();
        assert!(matches!(appended, AppendOutcome::Appended { .. }));

        // No intervening fold: the checkpoint pass folds forward itself.
        assert!(
            checkpoint_projector(&store, &[], "/root", &mut projector)
                .await
                .unwrap()
        );
        let (revision, blob) = store.read_checkpoint(&actor).await.unwrap().unwrap();
        assert_eq!(revision, Revision::new(4));
        let snapshot = lam::Checkpoint::decode(&blob).unwrap().into_state();
        assert_eq!(snapshot.revision(), Revision::new(4));
        assert_eq!(snapshot.context().len(), 3);
    }

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
    fn historical_model_projection_restores_reasoning_before_terminal_text() {
        let mut history = Vec::new();
        append_model_projection(
            &mut history,
            "/root",
            ModelResponseProjection {
                display: vec![
                    ModelDelta::Reasoning("Inspect ".to_owned()),
                    ModelDelta::Reasoning("the workspace".to_owned()),
                    ModelDelta::Text("All clear".to_owned()),
                ],
                directive: ModelDirective::Output(json!("All clear")),
                rejected_eval_calls: 0,
            },
        );

        assert_eq!(history.len(), 2);
        assert_eq!(history[0].kind, HistoryKind::Reasoning);
        assert_eq!(history[0].title, "/root · reasoning");
        assert_eq!(history[0].body, "Inspect the workspace");
        assert_eq!(history[1].kind, HistoryKind::Assistant);
        assert_eq!(history[1].body, "All clear");
    }

    #[test]
    fn historical_model_projection_keeps_rejected_sibling_tool_calls() {
        let mut history = Vec::new();
        append_model_projection(
            &mut history,
            "/root",
            ModelResponseProjection {
                display: vec![
                    ModelDelta::ToolCall(lam::ToolCallDelta {
                        index: 0,
                        call_id: Some("call-1".to_owned()),
                        name: Some("eval".to_owned()),
                        arguments: json!({
                            "intent": "Commit the change",
                            "source": "await commit()",
                            "timeoutMs": null,
                        })
                        .to_string(),
                    }),
                    ModelDelta::ToolCall(lam::ToolCallDelta {
                        index: 1,
                        call_id: Some("call-2".to_owned()),
                        name: Some("eval".to_owned()),
                        arguments: json!({
                            "intent": "Inspect the styling",
                            "source": "await inspect()",
                            "timeoutMs": null,
                        })
                        .to_string(),
                    }),
                ],
                directive: ModelDirective::Eval(lam::EvalRequest {
                    intent: "Commit the change".to_owned(),
                    source: "await commit()".to_owned(),
                    timeout: None,
                }),
                rejected_eval_calls: 1,
            },
        );

        assert_eq!(history.len(), 2);
        assert_eq!(history[0].title, "/root · Commit the change");
        assert_eq!(history[0].body, "await commit()");
        assert_eq!(history[1].title, "/root · Inspect the styling");
        assert_eq!(history[1].body, "await inspect()");
    }

    #[test]
    fn historical_model_projection_renders_rejected_calls_from_raw_fragments() {
        let mut history = Vec::new();
        append_model_projection(
            &mut history,
            "/root",
            ModelResponseProjection {
                display: vec![ModelDelta::ToolCall(lam::ToolCallDelta {
                    index: 0,
                    call_id: Some("call-1".to_owned()),
                    name: Some("eval".to_owned()),
                    arguments: json!({
                        "intent": "Sum the numbers",
                        "source": "1 + 1",
                        "timeout": 5,
                    })
                    .to_string(),
                })],
                directive: ModelDirective::Rejected {
                    message: "This eval call was not executed.".to_owned(),
                },
                rejected_eval_calls: 0,
            },
        );

        assert_eq!(history.len(), 1);
        assert_eq!(history[0].kind, HistoryKind::ToolCall);
        assert_eq!(history[0].title, "/root · Sum the numbers");
        assert_eq!(history[0].body, "1 + 1");
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

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_a_multi_thread_runtime_inside_an_async_context_panics() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(runtime)));
        assert!(
            panic.is_err(),
            "tokio forbids dropping a multi-thread runtime inside an async context"
        );
    }

    #[test]
    fn dropping_a_multi_thread_runtime_outside_async_does_not_panic() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        drop_runtime_outside_async(runtime);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quiesce_drains_in_flight_tasks_then_drops_the_command_runtime() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut runtime = Some(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap(),
        );
        let handles = std::sync::Mutex::new(Vec::new());
        let finished = Arc::new(AtomicBool::new(false));
        let flag = finished.clone();
        let handle = runtime.as_ref().unwrap().spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            flag.store(true, Ordering::SeqCst);
        });
        handles.lock().unwrap().push(handle);

        quiesce_command_runtime(&mut runtime, &handles).await;

        assert!(runtime.is_none(), "quiesce takes the command runtime");
        assert!(
            finished.load(Ordering::SeqCst),
            "quiesce waits for in-flight command tasks to finish"
        );
    }
}
