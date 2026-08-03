use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lam_core::{
    ActorId, ActorState, CompactionConfig, Compactor, DeliveryMode, EncodedPayload, JournalStore,
    MemStore, MessageEnvelope, MessageId, MessageSource, ModelCodec, ModelId, ModelProvider,
    ModelSelection, Revision, RunId, Timestamp,
};
use lam_deno::{Isolate, IsolateInterrupt, Namespace};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};

use crate::command::{OperationLease, RunnerCommand};
use crate::compaction::{CompactionReceipt, SummaryTailCompactor};
use crate::model::RegisteredModel;
use crate::prompt::SystemPrompt;
use crate::recovery::recover_actor;
use crate::run::RUN_EVENT_BUFFER;
use crate::runner::ActorRunner;
use crate::runtime_journal::{admit_message, ensure_model_selection, load_state};
use crate::{ActorBuildError, ActorError, ActorTask, Model, Run, RunEvents, RuntimeEvents};

const RUNTIME_EVENT_BUFFER: usize = 256;
const ACTOR_EXISTENCE_PROBE: NonZeroUsize = NonZeroUsize::new(1).expect("one is nonzero");

/// How an explicit model switch treats existing model-visible context.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ModelSwitchPolicy {
    /// Replace the complete effective history with a target-compatible
    /// checkpoint before selecting the target model.
    #[default]
    Compact,
    /// Reuse the current context only after the target codec can encode it.
    ReuseContext,
}

/// Durable result of selecting another registered model.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSwitchReceipt {
    /// Model selected before the operation.
    pub previous_model_id: ModelId,
    /// Model selected after the operation.
    pub selected_model_id: ModelId,
    /// Journal revision containing `ModelSelected`.
    pub revision: Revision,
    /// Compaction installed atomically with the selection, when required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionReceipt>,
}

/// Supplies informational host timestamps for durable actor entries.
pub trait Clock: Send + Sync + 'static {
    /// Returns the current host-observed time.
    fn now(&self) -> Timestamp;
}

/// Wall clock used by default actor builders.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let millis = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
            Err(error) => -i64::try_from(error.duration().as_millis()).unwrap_or(i64::MAX),
        };
        Timestamp::from_unix_millis(millis)
    }
}

pub(crate) struct RuntimeIds {
    seed: u128,
    next: AtomicU64,
}

impl RuntimeIds {
    fn new() -> Self {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let seed = (time << 32) ^ u128::from(std::process::id());
        Self {
            seed,
            next: AtomicU64::new(1),
        }
    }

    pub(crate) fn message_id(&self) -> MessageId {
        MessageId::new(self.next("message")).expect("runtime ids are nonempty")
    }

    pub(crate) fn run_id(&self) -> RunId {
        RunId::new(self.next("run")).expect("runtime ids are nonempty")
    }

    fn next(&self, kind: &str) -> String {
        let sequence = self.next.fetch_add(1, Ordering::Relaxed);
        format!("lam-{kind}-{:x}-{sequence:x}", self.seed)
    }
}

/// Entry point for configuring a Lam runtime.
pub struct Lam;

impl Lam {
    /// Starts a runtime builder with in-memory state and safe isolate defaults.
    #[must_use]
    pub fn builder<P, C>(model: Model<P, C>) -> LamBuilder<MemStore>
    where
        P: ModelProvider,
        C: ModelCodec,
    {
        let compactor = Arc::new(SummaryTailCompactor::new(model.clone()));
        LamBuilder {
            initial_model_id: ModelId::new("default"),
            initial_model: RegisteredModel::new(model, Some(compactor)),
            additional_models: Vec::new(),
            store: MemStore::new(),
            namespaces: Vec::new(),
            default_timeout: None,
            max_timeout: None,
            capture_console: true,
            clock: Arc::new(SystemClock),
            system_prompt: SystemPrompt::default(),
            compaction_config: CompactionConfig::default(),
            compaction_enabled: true,
        }
    }
}

/// Configures the dependencies shared by an actor runtime.
pub struct LamBuilder<S> {
    initial_model_id: Result<ModelId, lam_core::InvalidIdentifier>,
    initial_model: RegisteredModel,
    additional_models: Vec<PendingModel>,
    store: S,
    namespaces: Vec<Namespace>,
    default_timeout: Option<Duration>,
    max_timeout: Option<Duration>,
    capture_console: bool,
    clock: Arc<dyn Clock>,
    system_prompt: SystemPrompt,
    compaction_config: CompactionConfig,
    compaction_enabled: bool,
}

struct PendingModel {
    id: Result<ModelId, lam_core::InvalidIdentifier>,
    model: RegisteredModel,
}

impl<S> LamBuilder<S> {
    /// Replaces the actor journal implementation.
    #[must_use]
    pub fn state_store<T>(self, store: T) -> LamBuilder<T> {
        LamBuilder {
            initial_model_id: self.initial_model_id,
            initial_model: self.initial_model,
            additional_models: self.additional_models,
            store,
            namespaces: self.namespaces,
            default_timeout: self.default_timeout,
            max_timeout: self.max_timeout,
            capture_console: self.capture_console,
            clock: self.clock,
            system_prompt: self.system_prompt,
            compaction_config: self.compaction_config,
            compaction_enabled: self.compaction_enabled,
        }
    }

    /// Replaces the stable registry identity of the model passed to
    /// [`Lam::builder`]. The default is `default`.
    #[must_use]
    pub fn initial_model_id(mut self, model_id: impl Into<String>) -> Self {
        self.initial_model_id = ModelId::new(model_id);
        self
    }

    /// Registers another switchable model with the default summary-tail
    /// compactor backed by that model.
    #[must_use]
    pub fn model<P, C>(mut self, model_id: impl Into<String>, model: Model<P, C>) -> Self
    where
        P: ModelProvider,
        C: ModelCodec,
    {
        let compactor = Arc::new(SummaryTailCompactor::new(model.clone()));
        self.additional_models.push(PendingModel {
            id: ModelId::new(model_id),
            model: RegisteredModel::new(model, Some(compactor)),
        });
        self
    }

    /// Registers another switchable model with an explicit compactor.
    #[must_use]
    pub fn model_with_compactor<P, C>(
        mut self,
        model_id: impl Into<String>,
        model: Model<P, C>,
        compactor: impl Compactor,
    ) -> Self
    where
        P: ModelProvider,
        C: ModelCodec,
    {
        self.additional_models.push(PendingModel {
            id: ModelId::new(model_id),
            model: RegisteredModel::new(model, Some(Arc::new(compactor))),
        });
        self
    }

    /// Registers one Rust-backed TypeScript namespace.
    #[must_use]
    pub fn namespace(mut self, namespace: Namespace) -> Self {
        self.namespaces.push(namespace);
        self
    }

    /// Registers several Rust-backed TypeScript namespaces.
    #[must_use]
    pub fn namespaces(mut self, namespaces: impl IntoIterator<Item = Namespace>) -> Self {
        self.namespaces.extend(namespaces);
        self
    }

    /// Replaces the complete model system prompt.
    ///
    /// This removes the default prompt and generated API inventory. Later or
    /// earlier annotations are still appended in registration order.
    #[must_use]
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt.replace(prompt);
        self
    }

    /// Appends instructions to the default or replacement system prompt.
    #[must_use]
    pub fn annotate_system_prompt(mut self, instructions: impl Into<String>) -> Self {
        self.system_prompt.annotate(instructions);
        self
    }

    /// Replaces automatic compaction thresholds and budgets.
    #[must_use]
    pub fn compaction_config(mut self, config: CompactionConfig) -> Self {
        self.compaction_config = config;
        self
    }

    /// Declares a fallback context window for models without their own value.
    #[must_use]
    pub fn context_window_tokens(mut self, tokens: u64) -> Self {
        self.compaction_config = self.compaction_config.context_window_tokens(tokens);
        self
    }

    /// Replaces the default summary-tail strategy with one compactor.
    #[must_use]
    pub fn compactor(mut self, compactor: impl Compactor) -> Self {
        self.initial_model.compactor = Some(Arc::new(compactor));
        self
    }

    /// Disables manual, threshold, and overflow compaction.
    #[must_use]
    pub fn disable_compaction(mut self) -> Self {
        self.compaction_enabled = false;
        self
    }

    /// Sets the default timeout for eval programs.
    #[must_use]
    pub const fn default_eval_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = Some(timeout);
        self
    }

    /// Sets the hard upper bound for eval timeouts.
    #[must_use]
    pub const fn max_eval_timeout(mut self, timeout: Duration) -> Self {
        self.max_timeout = Some(timeout);
        self
    }

    /// Enables or disables collection of JavaScript `console` calls.
    ///
    /// Capture is enabled by default. Disabling it leaves `console` available
    /// to evaluated programs but omits its calls from model-visible outcomes.
    #[must_use]
    pub const fn capture_console(mut self, capture: bool) -> Self {
        self.capture_console = capture;
        self
    }

    /// Replaces the informational host clock.
    #[must_use]
    pub fn clock(mut self, clock: impl Clock) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Freezes runtime configuration for one actor in this slice.
    #[must_use]
    pub fn build(self) -> LamRuntime<S> {
        LamRuntime {
            initial_model_id: self.initial_model_id,
            initial_model: self.initial_model,
            additional_models: self.additional_models,
            store: self.store,
            namespaces: self.namespaces,
            default_timeout: self.default_timeout,
            max_timeout: self.max_timeout,
            capture_console: self.capture_console,
            clock: self.clock,
            system_prompt: self.system_prompt,
            compaction_config: self.compaction_config,
            compaction_enabled: self.compaction_enabled,
        }
    }
}

/// Frozen single-actor configuration.
pub struct LamRuntime<S> {
    initial_model_id: Result<ModelId, lam_core::InvalidIdentifier>,
    initial_model: RegisteredModel,
    additional_models: Vec<PendingModel>,
    store: S,
    namespaces: Vec<Namespace>,
    default_timeout: Option<Duration>,
    max_timeout: Option<Duration>,
    capture_console: bool,
    clock: Arc<dyn Clock>,
    system_prompt: SystemPrompt,
    compaction_config: CompactionConfig,
    compaction_enabled: bool,
}

impl<S> LamRuntime<S> {
    /// Consumes this pre-subagent runtime into an actor builder.
    #[must_use]
    pub fn actor(self, actor_id: impl Into<String>) -> ActorBuilder<S> {
        ActorBuilder {
            actor_id: ActorId::new(actor_id),
            initial_model_id: self.initial_model_id,
            initial_model: self.initial_model,
            additional_models: self.additional_models,
            store: self.store,
            namespaces: self.namespaces,
            default_timeout: self.default_timeout,
            max_timeout: self.max_timeout,
            capture_console: self.capture_console,
            clock: self.clock,
            system_prompt: self.system_prompt,
            compaction_config: self.compaction_config,
            compaction_enabled: self.compaction_enabled,
        }
    }
}

/// Starts one actor on a dedicated thread or an external local executor.
pub struct ActorBuilder<S> {
    actor_id: Result<ActorId, lam_core::InvalidIdentifier>,
    initial_model_id: Result<ModelId, lam_core::InvalidIdentifier>,
    initial_model: RegisteredModel,
    additional_models: Vec<PendingModel>,
    store: S,
    namespaces: Vec<Namespace>,
    default_timeout: Option<Duration>,
    max_timeout: Option<Duration>,
    capture_console: bool,
    clock: Arc<dyn Clock>,
    system_prompt: SystemPrompt,
    compaction_config: CompactionConfig,
    compaction_enabled: bool,
}

impl<S> ActorBuilder<S>
where
    S: JournalStore + 'static,
{
    /// Returns the actor identity selected for this builder.
    ///
    /// Multi-actor hosts use this before startup to bind actor-local
    /// capabilities without asking callers to repeat the identity.
    pub fn actor_id(&self) -> Result<&ActorId, &lam_core::InvalidIdentifier> {
        self.actor_id.as_ref()
    }

    /// Registers one Rust-backed TypeScript namespace after actor selection.
    #[must_use]
    pub fn namespace(mut self, namespace: Namespace) -> Self {
        self.namespaces.push(namespace);
        self
    }

    /// Registers several Rust-backed TypeScript namespaces.
    #[must_use]
    pub fn namespaces(mut self, namespaces: impl IntoIterator<Item = Namespace>) -> Self {
        self.namespaces.extend(namespaces);
        self
    }

    /// Appends actor-specific instructions after actor selection.
    #[must_use]
    pub fn annotate_system_prompt(mut self, instructions: impl Into<String>) -> Self {
        self.system_prompt.annotate(instructions);
        self
    }

    /// Builds the persistent isolate on its dedicated actor thread.
    pub async fn build(self) -> Result<Actor<S>, ActorBuildError> {
        let prepared = PreparedActor::from_builder(self)?;
        let actor_name = prepared.actor_id.to_string();
        let (initialized, initialization) = oneshot::channel();
        let join = std::thread::Builder::new()
            .name(format!("lam-{actor_name}"))
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = initialized.send(Err(ActorBuildError::Initialization {
                            message: error.to_string(),
                        }));
                        return;
                    }
                };
                runtime.block_on(async move {
                    match prepared.start(false).await {
                        Ok((actor, task)) => {
                            if initialized.send(Ok(actor)).is_ok() {
                                task.await;
                            }
                        }
                        Err(error) => {
                            let _ = initialized.send(Err(error));
                        }
                    }
                });
            })
            .map_err(ActorBuildError::ThreadSpawn)?;

        let mut actor = match initialization.await {
            Ok(Ok(actor)) => actor,
            Ok(Err(error)) => {
                let _ = join.join();
                return Err(error);
            }
            Err(_closed) => {
                let _ = join.join();
                return Err(ActorBuildError::Initialization {
                    message: "actor thread exited during initialization".to_owned(),
                });
            }
        };
        actor.thread = Some(join);
        Ok(actor)
    }

    /// Builds the persistent isolate for an external current-thread executor.
    ///
    /// The returned non-`Send` task must be polled on the same local executor
    /// where this method is awaited. This is the lower-level lifecycle used by
    /// multi-actor schedulers; most embeddings should use [`Self::build`].
    pub async fn build_task(self) -> Result<(Actor<S>, ActorTask), ActorBuildError> {
        PreparedActor::from_builder(self)?.start(false).await
    }

    /// Builds a new actor for an external current-thread executor.
    ///
    /// Unlike [`Self::build_task`], this rejects an identity with any durable
    /// history. Multi-actor schedulers use it to make spawning create-only
    /// while keeping ordinary actor startup resumable.
    pub async fn build_new_task(self) -> Result<(Actor<S>, ActorTask), ActorBuildError> {
        PreparedActor::from_builder(self)?.start(true).await
    }
}

struct PreparedActor<S> {
    actor_id: ActorId,
    initial_model_id: ModelId,
    models: BTreeMap<ModelId, RegisteredModel>,
    store: S,
    namespaces: Vec<Namespace>,
    default_timeout: Option<Duration>,
    max_timeout: Option<Duration>,
    capture_console: bool,
    clock: Arc<dyn Clock>,
    system_prompt: SystemPrompt,
    compaction_config: CompactionConfig,
}

impl<S> PreparedActor<S>
where
    S: JournalStore + 'static,
{
    fn from_builder(builder: ActorBuilder<S>) -> Result<Self, ActorBuildError> {
        let actor_id = builder.actor_id?;
        let initial_model_id = builder
            .initial_model_id
            .map_err(ActorBuildError::InvalidModelId)?;
        let mut models = BTreeMap::new();
        models.insert(initial_model_id.clone(), builder.initial_model);
        for pending in builder.additional_models {
            let model_id = pending.id.map_err(ActorBuildError::InvalidModelId)?;
            if models.insert(model_id.clone(), pending.model).is_some() {
                return Err(ActorBuildError::DuplicateModelId { model_id });
            }
        }
        if !builder.compaction_enabled {
            for model in models.values_mut() {
                model.compactor = None;
            }
        }
        for model in models.values() {
            model
                .compaction_config(&builder.compaction_config)
                .validate()
                .map_err(ActorBuildError::InvalidCompactionConfig)?;
        }

        Ok(Self {
            actor_id,
            initial_model_id,
            models,
            store: builder.store,
            namespaces: builder.namespaces,
            default_timeout: builder.default_timeout,
            max_timeout: builder.max_timeout,
            capture_console: builder.capture_console,
            clock: builder.clock,
            system_prompt: builder.system_prompt,
            compaction_config: builder.compaction_config,
        })
    }

    async fn start(self, create_only: bool) -> Result<(Actor<S>, ActorTask), ActorBuildError> {
        let store = Arc::new(self.store);
        if create_only
            && store
                .read(&self.actor_id, Revision::ZERO, ACTOR_EXISTENCE_PROBE)
                .await
                .map_err(initialization_error)?
                .head
                != Revision::ZERO
        {
            return Err(ActorBuildError::ActorAlreadyExists {
                actor_id: self.actor_id,
            });
        }
        let ids = Arc::new(RuntimeIds::new());
        let operation_active = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (commands, receiver) = mpsc::unbounded_channel();
        let (run_event_sender, run_event_receiver) = mpsc::channel(RUN_EVENT_BUFFER);
        let (runtime_event_sender, runtime_event_receiver) = mpsc::channel(RUNTIME_EVENT_BUFFER);
        let (abort, abort_receiver) = watch::channel(false);
        let (stopped_sender, stopped) = oneshot::channel();

        let mut isolate_builder = Isolate::builder();
        for namespace in self.namespaces {
            isolate_builder = isolate_builder.namespace(namespace);
        }
        if let Some(timeout) = self.default_timeout {
            isolate_builder = isolate_builder.default_timeout(timeout);
        }
        if let Some(timeout) = self.max_timeout {
            isolate_builder = isolate_builder.max_timeout(timeout);
        }
        isolate_builder = isolate_builder.capture_console(self.capture_console);
        let isolate = isolate_builder
            .build()
            .await
            .map_err(initialization_error)?;
        let system_prompt = self.system_prompt.render(&isolate.api_inventory());
        let interrupt = isolate.interrupt_handle();
        let initial_descriptor = self
            .models
            .get(&self.initial_model_id)
            .expect("the initial model was inserted")
            .descriptor()
            .clone();
        let initial_selection = ModelSelection::new(self.initial_model_id, initial_descriptor);
        let (state, created) =
            ensure_model_selection(store.as_ref(), &self.actor_id, initial_selection)
                .await
                .map_err(initialization_error)?;
        if create_only && !created {
            return Err(ActorBuildError::ActorAlreadyExists {
                actor_id: self.actor_id,
            });
        }
        let selection = state
            .selected_model()
            .expect("model initialization establishes a selection");
        let selected_model = self.models.get(&selection.model_id).ok_or_else(|| {
            ActorBuildError::Initialization {
                message: format!(
                    "durable model `{}` is not present in the runtime registry",
                    selection.model_id
                ),
            }
        })?;
        if selected_model.descriptor() != &selection.descriptor {
            return Err(ActorBuildError::Initialization {
                message: format!(
                    "durable model `{}` descriptor does not match the runtime registry",
                    selection.model_id
                ),
            });
        }
        let recovery = recover_actor(
            &self.actor_id,
            store.as_ref(),
            selected_model,
            self.clock.as_ref(),
            ids.as_ref(),
            !created,
        )
        .await
        .map_err(initialization_error)?;
        if let Some(event) = recovery.event {
            let _ = runtime_event_sender.try_send(event);
        }

        let runner = ActorRunner {
            actor_id: self.actor_id.clone(),
            store: Arc::clone(&store),
            models: self.models,
            clock: Arc::clone(&self.clock),
            ids: Arc::clone(&ids),
            isolate,
            system_prompt,
            compaction_config: self.compaction_config,
            commands: receiver,
            abort: abort_receiver,
            shutdown: Arc::clone(&shutdown),
            run_events: run_event_sender,
            runtime_events: runtime_event_sender,
        };
        let actor_ref = ActorRef {
            actor_id: self.actor_id,
            store,
            commands: commands.clone(),
            clock: self.clock,
            ids,
        };
        let handle = ActorHandle {
            actor_ref,
            operation_active,
        };
        let actor = Actor {
            handle,
            shutdown,
            thread: None,
            stopped: Some(stopped),
            abort_handle: AbortHandle { interrupt, abort },
            run_events: Some(RunEvents::new(run_event_receiver)),
            runtime_events: Some(RuntimeEvents::new(runtime_event_receiver)),
        };
        let task = ActorTask::new(async move {
            runner.run(recovery.wake).await;
            let _ = stopped_sender.send(());
        });
        Ok((actor, task))
    }
}

fn initialization_error(error: impl std::fmt::Display) -> ActorBuildError {
    ActorBuildError::Initialization {
        message: error.to_string(),
    }
}

/// Cloneable authority for correlated actor operations.
///
/// Calls, compaction, and model switches are mutually exclusive. A conflicting
/// operation returns [`ActorError::Busy`] instead of waiting for the active
/// operation to finish. Ordinary mailbox delivery remains available through
/// [`ActorRef`].
pub struct ActorHandle<S>
where
    S: JournalStore + 'static,
{
    actor_ref: ActorRef<S>,
    operation_active: Arc<AtomicBool>,
}

impl<S> Clone for ActorHandle<S>
where
    S: JournalStore + 'static,
{
    fn clone(&self) -> Self {
        Self {
            actor_ref: self.actor_ref.clone(),
            operation_active: Arc::clone(&self.operation_active),
        }
    }
}

impl<S> ActorHandle<S>
where
    S: JournalStore + 'static,
{
    /// Returns this actor's stable journal identity.
    #[must_use]
    pub const fn actor_id(&self) -> &ActorId {
        self.actor_ref.actor_id()
    }

    /// Returns a cloneable send-only mailbox address.
    #[must_use]
    pub fn actor_ref(&self) -> ActorRef<S> {
        self.actor_ref.clone()
    }

    /// Rebuilds the current actor projection from its authoritative journal.
    pub async fn state(&self) -> Result<ActorState, ActorError> {
        self.actor_ref.state().await
    }

    /// Durably admits a value to this actor's mailbox.
    pub async fn send<T>(
        &self,
        input: T,
        delivery: DeliveryMode,
    ) -> Result<MessageReceipt, ActorError>
    where
        T: Serialize,
    {
        self.actor_ref.send(input, delivery).await
    }

    /// Explicitly compacts the current context with the configured strategy.
    ///
    /// `None` means the context does not yet contain a prefix outside the
    /// retained-tail target.
    pub async fn compact(&self) -> Result<Option<CompactionReceipt>, ActorError> {
        let _lease = OperationLease::acquire(Arc::clone(&self.operation_active))?;
        let (completion, result) = oneshot::channel();
        self.actor_ref
            .commands
            .send(RunnerCommand::Compact(completion))
            .map_err(|_| ActorError::Unavailable)?;
        result.await.map_err(|_| ActorError::Unavailable)?
    }

    /// Compacts existing history and selects another registered model.
    pub async fn switch_model(
        &self,
        model_id: impl Into<String>,
    ) -> Result<ModelSwitchReceipt, ActorError> {
        self.switch_model_with_policy(model_id, ModelSwitchPolicy::Compact)
            .await
    }

    /// Selects another registered model using an explicit context policy.
    pub async fn switch_model_with_policy(
        &self,
        model_id: impl Into<String>,
        policy: ModelSwitchPolicy,
    ) -> Result<ModelSwitchReceipt, ActorError> {
        let _lease = OperationLease::acquire(Arc::clone(&self.operation_active))?;
        let model_id = ModelId::new(model_id).map_err(|error| ActorError::InvalidModelId {
            message: error.to_string(),
        })?;
        let (completion, result) = oneshot::channel();
        self.actor_ref
            .commands
            .send(RunnerCommand::SwitchModel {
                model_id,
                policy,
                completion,
            })
            .map_err(|_| ActorError::Unavailable)?;
        result.await.map_err(|_| ActorError::Unavailable)?
    }

    /// Creates a lazy text run which starts when first polled.
    #[must_use]
    pub fn call<T>(&self, input: T) -> Run<String>
    where
        T: Serialize,
    {
        let message_id = self.actor_ref.ids.message_id();
        let message = self
            .actor_ref
            .user_message(message_id.clone(), input, DeliveryMode::Steer);
        Run::new(
            self.actor_ref.commands.clone(),
            Arc::clone(&self.operation_active),
            message_id,
            message,
        )
    }

    /// Creates a lazy text run with authenticated actor provenance.
    ///
    /// Multi-actor runtimes use this correlated counterpart to
    /// [`ActorRef::send_from_actor`] when the sender must await an outcome.
    #[must_use]
    pub fn call_from_actor<T>(&self, sender: &ActorRef<S>, input: T) -> Run<String>
    where
        T: Serialize,
    {
        let message_id = self.actor_ref.ids.message_id();
        let message = self.actor_ref.message(
            message_id.clone(),
            MessageSource::Actor {
                actor_id: sender.actor_id.clone(),
            },
            input,
            DeliveryMode::Steer,
        );
        Run::new(
            self.actor_ref.commands.clone(),
            Arc::clone(&self.operation_active),
            message_id,
            message,
        )
    }
}

/// Linear owner of one actor runtime and its single-consumer event streams.
pub struct Actor<S>
where
    S: JournalStore + 'static,
{
    handle: ActorHandle<S>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    stopped: Option<oneshot::Receiver<()>>,
    abort_handle: AbortHandle,
    run_events: Option<RunEvents>,
    runtime_events: Option<RuntimeEvents>,
}

impl<S> Actor<S>
where
    S: JournalStore + 'static,
{
    /// Returns a cloneable send-only mailbox address.
    #[must_use]
    pub fn actor_ref(&self) -> ActorRef<S> {
        self.handle.actor_ref()
    }

    /// Returns cloneable authority for correlated actor operations.
    #[must_use]
    pub fn handle(&self) -> ActorHandle<S> {
        self.handle.clone()
    }

    /// Returns explicit kill authority for cancelling work from another task.
    #[must_use]
    pub fn abort_handle(&self) -> AbortHandle {
        self.abort_handle.clone()
    }

    /// Takes the single-consumer stream covering every run on this actor.
    ///
    /// Unlike the event stream attached to a correlated [`Run`], this stream
    /// also includes work started by ordinary mailbox wakes.
    pub fn take_run_events(&mut self) -> Option<RunEvents> {
        self.run_events.take()
    }

    /// Takes the single-consumer actor-wide runtime event stream.
    ///
    /// The stream is buffered from actor startup, so a resumption event remains
    /// observable even though its durable notice is admitted before `build`
    /// returns.
    pub fn take_runtime_events(&mut self) -> Option<RuntimeEvents> {
        self.runtime_events.take()
    }

    /// Durably admits a value to this actor's mailbox.
    ///
    pub async fn send<T>(
        &self,
        input: T,
        delivery: DeliveryMode,
    ) -> Result<MessageReceipt, ActorError>
    where
        T: Serialize,
    {
        self.handle.send(input, delivery).await
    }

    /// Explicitly compacts the current context with the configured strategy.
    ///
    /// `None` means the context does not yet contain a prefix outside the
    /// retained-tail target.
    pub async fn compact(&mut self) -> Result<Option<CompactionReceipt>, ActorError> {
        self.handle.compact().await
    }

    /// Compacts existing history and selects another registered model.
    pub async fn switch_model(
        &mut self,
        model_id: impl Into<String>,
    ) -> Result<ModelSwitchReceipt, ActorError> {
        self.switch_model_with_policy(model_id, ModelSwitchPolicy::Compact)
            .await
    }

    /// Selects another registered model using an explicit context policy.
    pub async fn switch_model_with_policy(
        &mut self,
        model_id: impl Into<String>,
        policy: ModelSwitchPolicy,
    ) -> Result<ModelSwitchReceipt, ActorError> {
        self.handle.switch_model_with_policy(model_id, policy).await
    }

    /// Starts a linear text call when the returned run is first polled.
    pub fn call<T>(&mut self, input: T) -> Run<String>
    where
        T: Serialize,
    {
        self.handle.call(input)
    }

    /// Starts a linear text call with authenticated actor provenance.
    ///
    /// Multi-actor runtimes use this correlated counterpart to
    /// [`ActorRef::send_from_actor`] when the sender must await an outcome.
    pub fn call_from_actor<T>(&mut self, sender: &ActorRef<S>, input: T) -> Run<String>
    where
        T: Serialize,
    {
        self.handle.call_from_actor(sender, input)
    }

    /// Gracefully stops this actor and waits for its runner to exit.
    ///
    /// The currently executing command is allowed to finish. Pending durable
    /// mailbox messages are not discarded.
    pub async fn shutdown(mut self) -> Result<(), ActorError> {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.handle.actor_ref.commands.send(RunnerCommand::Shutdown);
        self.join_runner().await
    }

    /// Forcefully aborts current work and waits for the actor runner to exit.
    ///
    /// Provider futures are dropped and active JavaScript is interrupted. Host
    /// effects which completed before interruption are not rolled back.
    pub async fn abort(mut self) -> Result<(), ActorError> {
        self.abort_handle.abort();
        self.join_runner().await
    }

    async fn join_runner(&mut self) -> Result<(), ActorError> {
        if let Some(stopped) = self.stopped.take() {
            let _ = stopped.await;
        }
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|panic| ActorError::RunnerJoin {
                message: panic_message(panic),
            })?;
        }
        Ok(())
    }
}

/// Cloneable, explicit authority to forcefully stop an actor runtime.
///
/// Signalling is synchronous. Use [`Actor::abort`] on the linear owner when
/// the caller must also wait for the actor runner to exit.
#[derive(Clone)]
pub struct AbortHandle {
    interrupt: IsolateInterrupt,
    abort: watch::Sender<bool>,
}

impl AbortHandle {
    /// Cancels the actor's current operation and interrupts active JavaScript.
    pub fn abort(&self) {
        let _ = self.abort.send(true);
        self.interrupt.terminate();
    }
}

impl<S> Drop for Actor<S>
where
    S: JournalStore + 'static,
{
    fn drop(&mut self) {
        if self.stopped.is_some() {
            self.shutdown.store(true, Ordering::Release);
            let _ = self.handle.actor_ref.commands.send(RunnerCommand::Shutdown);
        }
    }
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "actor runner panicked without a string payload".to_owned()
    }
}

/// Cloneable send-only address for one actor mailbox.
pub struct ActorRef<S>
where
    S: JournalStore + 'static,
{
    actor_id: ActorId,
    store: Arc<S>,
    commands: mpsc::UnboundedSender<RunnerCommand>,
    clock: Arc<dyn Clock>,
    ids: Arc<RuntimeIds>,
}

impl<S> Clone for ActorRef<S>
where
    S: JournalStore + 'static,
{
    fn clone(&self) -> Self {
        Self {
            actor_id: self.actor_id.clone(),
            store: Arc::clone(&self.store),
            commands: self.commands.clone(),
            clock: Arc::clone(&self.clock),
            ids: Arc::clone(&self.ids),
        }
    }
}

impl<S> ActorRef<S>
where
    S: JournalStore + 'static,
{
    /// Returns this mailbox's stable actor identity.
    #[must_use]
    pub const fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    /// Rebuilds the current actor projection from its authoritative journal.
    pub async fn state(&self) -> Result<ActorState, ActorError> {
        load_state(self.store.as_ref(), &self.actor_id).await
    }

    /// Durably admits a user value, then wakes the actor runner.
    pub async fn send<T>(
        &self,
        input: T,
        delivery: DeliveryMode,
    ) -> Result<MessageReceipt, ActorError>
    where
        T: Serialize,
    {
        self.send_with_source(input, MessageSource::User { principal: None }, delivery)
            .await
    }

    /// Durably admits a value sent by another actor, then wakes this runner.
    ///
    /// Multi-actor routers use this entry point so durable provenance remains
    /// distinct from user and host input. Delivery is always steering.
    pub async fn send_from_actor<T>(
        &self,
        sender: &ActorRef<S>,
        input: T,
    ) -> Result<MessageReceipt, ActorError>
    where
        T: Serialize,
    {
        self.send_with_source(
            input,
            MessageSource::Actor {
                actor_id: sender.actor_id.clone(),
            },
            DeliveryMode::Steer,
        )
        .await
    }

    async fn send_with_source<T>(
        &self,
        input: T,
        source: MessageSource,
        delivery: DeliveryMode,
    ) -> Result<MessageReceipt, ActorError>
    where
        T: Serialize,
    {
        let message_id = self.ids.message_id();
        let message = self.message(message_id, source, input, delivery)?;
        let receipt = admit_message(self.store.as_ref(), &self.actor_id, message).await?;
        let _ = self.commands.send(RunnerCommand::Wake);
        Ok(receipt)
    }

    fn user_message<T>(
        &self,
        message_id: MessageId,
        input: T,
        delivery: DeliveryMode,
    ) -> Result<MessageEnvelope, ActorError>
    where
        T: Serialize,
    {
        self.message(
            message_id,
            MessageSource::User { principal: None },
            input,
            delivery,
        )
    }

    fn message<T>(
        &self,
        message_id: MessageId,
        source: MessageSource,
        input: T,
        delivery: DeliveryMode,
    ) -> Result<MessageEnvelope, ActorError>
    where
        T: Serialize,
    {
        let payload =
            EncodedPayload::lam_json(input).map_err(|error| ActorError::InputSerialization {
                message: error.to_string(),
            })?;
        MessageEnvelope::new(message_id, source, delivery, payload, self.clock.now()).map_err(
            |error| ActorError::State {
                message: error.to_string(),
            },
        )
    }
}

/// Confirmation that a message is present in the configured journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageReceipt {
    /// Recipient actor.
    pub actor_id: ActorId,
    /// Admitted message.
    pub message_id: MessageId,
    /// Actor-local revision which contains the admission.
    pub revision: Revision,
}
