use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lam_core::{
    ActorId, ActorState, DeliveryMode, EncodedPayload, JournalStore, MemStore, MessageEnvelope,
    MessageId, MessageSource, ModelCodec, ModelProvider, Revision, RunId, Timestamp,
};
use lam_deno::{Isolate, IsolateInterrupt, Namespace};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};

use crate::command::RunnerCommand;
use crate::recovery::recover_actor;
use crate::runner::ActorRunner;
use crate::runtime_journal::{admit_message, load_state};
use crate::{ActorBuildError, ActorError, Model, Run, RuntimeEvents};

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
    pub fn builder<P, C>(model: Model<P, C>) -> LamBuilder<P, C, MemStore> {
        LamBuilder {
            model,
            store: MemStore::new(),
            namespaces: Vec::new(),
            default_timeout: None,
            max_timeout: None,
            clock: Arc::new(SystemClock),
        }
    }
}

/// Configures the dependencies shared by an actor runtime.
pub struct LamBuilder<P, C, S> {
    model: Model<P, C>,
    store: S,
    namespaces: Vec<Namespace>,
    default_timeout: Option<Duration>,
    max_timeout: Option<Duration>,
    clock: Arc<dyn Clock>,
}

impl<P, C, S> LamBuilder<P, C, S> {
    /// Replaces the actor journal implementation.
    #[must_use]
    pub fn state_store<T>(self, store: T) -> LamBuilder<P, C, T> {
        LamBuilder {
            model: self.model,
            store,
            namespaces: self.namespaces,
            default_timeout: self.default_timeout,
            max_timeout: self.max_timeout,
            clock: self.clock,
        }
    }

    /// Registers one Rust-backed TypeScript namespace.
    #[must_use]
    pub fn namespace(mut self, namespace: Namespace) -> Self {
        self.namespaces.push(namespace);
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

    /// Replaces the informational host clock.
    #[must_use]
    pub fn clock(mut self, clock: impl Clock) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Freezes runtime configuration for one actor in this slice.
    #[must_use]
    pub fn build(self) -> LamRuntime<P, C, S> {
        LamRuntime {
            model: self.model,
            store: self.store,
            namespaces: self.namespaces,
            default_timeout: self.default_timeout,
            max_timeout: self.max_timeout,
            clock: self.clock,
        }
    }
}

/// Frozen single-actor configuration.
pub struct LamRuntime<P, C, S> {
    model: Model<P, C>,
    store: S,
    namespaces: Vec<Namespace>,
    default_timeout: Option<Duration>,
    max_timeout: Option<Duration>,
    clock: Arc<dyn Clock>,
}

impl<P, C, S> LamRuntime<P, C, S> {
    /// Consumes this pre-subagent runtime into an actor builder.
    #[must_use]
    pub fn actor(self, actor_id: impl Into<String>) -> ActorBuilder<P, C, S> {
        ActorBuilder {
            actor_id: ActorId::new(actor_id),
            model: self.model,
            store: self.store,
            namespaces: self.namespaces,
            default_timeout: self.default_timeout,
            max_timeout: self.max_timeout,
            clock: self.clock,
        }
    }
}

/// Starts the dedicated runner for one actor.
pub struct ActorBuilder<P, C, S> {
    actor_id: Result<ActorId, lam_core::InvalidIdentifier>,
    model: Model<P, C>,
    store: S,
    namespaces: Vec<Namespace>,
    default_timeout: Option<Duration>,
    max_timeout: Option<Duration>,
    clock: Arc<dyn Clock>,
}

impl<P, C, S> ActorBuilder<P, C, S>
where
    P: ModelProvider,
    C: ModelCodec,
    S: JournalStore + 'static,
{
    /// Builds the persistent isolate on its dedicated actor thread.
    pub async fn build(self) -> Result<Actor<S>, ActorBuildError> {
        let actor_id = self.actor_id?;
        let actor_name = actor_id.to_string();
        let store = Arc::new(self.store);
        let ids = Arc::new(RuntimeIds::new());
        let call_active = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (commands, receiver) = mpsc::unbounded_channel();
        let (runtime_event_sender, runtime_event_receiver) = mpsc::unbounded_channel();
        let (abort, abort_receiver) = watch::channel(false);
        let (initialized, initialization) = oneshot::channel();
        let (stopped_sender, stopped) = oneshot::channel();

        let runner_actor_id = actor_id.clone();
        let runner_store = Arc::clone(&store);
        let runner_ids = Arc::clone(&ids);
        let runner_clock = Arc::clone(&self.clock);
        let runner_shutdown = Arc::clone(&shutdown);
        let provider = self.model.provider;
        let codec = self.model.codec;
        let join = std::thread::Builder::new()
            .name(format!("lam-{actor_name}"))
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread().build() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = initialized.send(Err(error.to_string()));
                        let _ = stopped_sender.send(());
                        return;
                    }
                };
                runtime.block_on(async move {
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
                    let isolate = match isolate_builder.build().await {
                        Ok(isolate) => isolate,
                        Err(error) => {
                            let _ = initialized.send(Err(error.to_string()));
                            return;
                        }
                    };
                    let interrupt = isolate.interrupt_handle();
                    let recovery = match recover_actor(
                        &runner_actor_id,
                        runner_store.as_ref(),
                        codec.as_ref(),
                        runner_clock.as_ref(),
                        runner_ids.as_ref(),
                    )
                    .await
                    {
                        Ok(recovery) => recovery,
                        Err(error) => {
                            let _ = initialized.send(Err(error.to_string()));
                            return;
                        }
                    };
                    if let Some(event) = recovery.event {
                        let _ = runtime_event_sender.send(event);
                    }
                    let runner = ActorRunner {
                        actor_id: runner_actor_id,
                        store: runner_store,
                        provider,
                        codec,
                        clock: runner_clock,
                        ids: runner_ids,
                        isolate,
                        commands: receiver,
                        abort: abort_receiver,
                        shutdown: runner_shutdown,
                    };
                    let _ = initialized.send(Ok(interrupt));
                    runner.run(recovery.wake).await;
                });
                let _ = stopped_sender.send(());
            })
            .map_err(ActorBuildError::ThreadSpawn)?;

        let interrupt = match initialization.await {
            Ok(Ok(interrupt)) => interrupt,
            Ok(Err(message)) => {
                let _ = join.join();
                return Err(ActorBuildError::Initialization { message });
            }
            Err(_closed) => {
                let _ = join.join();
                return Err(ActorBuildError::Initialization {
                    message: "actor thread exited during initialization".to_owned(),
                });
            }
        };

        let actor_ref = ActorRef {
            actor_id,
            store,
            commands: commands.clone(),
            clock: self.clock,
            ids,
        };
        Ok(Actor {
            actor_ref,
            call_active,
            shutdown,
            thread: Some(join),
            stopped: Some(stopped),
            abort_handle: AbortHandle { interrupt, abort },
            runtime_events: Some(RuntimeEvents::new(runtime_event_receiver)),
        })
    }
}

/// Linear owner of one actor's correlated call interface.
pub struct Actor<S>
where
    S: JournalStore + 'static,
{
    actor_ref: ActorRef<S>,
    call_active: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    stopped: Option<oneshot::Receiver<()>>,
    abort_handle: AbortHandle,
    runtime_events: Option<RuntimeEvents>,
}

impl<S> Actor<S>
where
    S: JournalStore + 'static,
{
    /// Returns a cloneable send-only mailbox address.
    #[must_use]
    pub fn actor_ref(&self) -> ActorRef<S> {
        self.actor_ref.clone()
    }

    /// Returns explicit kill authority for cancelling work from another task.
    #[must_use]
    pub fn abort_handle(&self) -> AbortHandle {
        self.abort_handle.clone()
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
    /// Clone an [`ActorRef`] before starting a call when the actor must be
    /// steered while that call holds its mutable borrow.
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

    /// Starts a linear text call when the returned run is first polled.
    pub fn call<T>(&mut self, input: T) -> Run<'_, String>
    where
        T: Serialize,
    {
        let message_id = self.actor_ref.ids.message_id();
        let message = self
            .actor_ref
            .user_message(message_id.clone(), input, DeliveryMode::Steer);
        Run::new(
            self.actor_ref.commands.clone(),
            Arc::clone(&self.call_active),
            message_id,
            message,
        )
    }

    /// Gracefully stops this actor and waits for its dedicated thread to exit.
    ///
    /// The currently executing command is allowed to finish. Pending durable
    /// mailbox messages are not discarded.
    pub async fn shutdown(mut self) -> Result<(), ActorError> {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.actor_ref.commands.send(RunnerCommand::Shutdown);
        self.join_thread().await
    }

    /// Forcefully aborts current work and waits for the actor thread to exit.
    ///
    /// Provider futures are dropped and active JavaScript is interrupted. Host
    /// effects which completed before interruption are not rolled back.
    pub async fn abort(mut self) -> Result<(), ActorError> {
        self.abort_handle.abort();
        self.join_thread().await
    }

    async fn join_thread(&mut self) -> Result<(), ActorError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        if let Some(stopped) = self.stopped.take() {
            let _ = stopped.await;
        }
        thread.join().map_err(|panic| ActorError::RunnerJoin {
            message: panic_message(panic),
        })
    }
}

/// Cloneable, explicit authority to forcefully stop an actor runtime.
///
/// Signalling is synchronous. Use [`Actor::abort`] on the linear owner when
/// the caller must also wait for the dedicated thread to exit.
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
        if self.thread.is_some() {
            self.shutdown.store(true, Ordering::Release);
            let _ = self.actor_ref.commands.send(RunnerCommand::Shutdown);
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
        let message_id = self.ids.message_id();
        let message = self.user_message(message_id, input, delivery)?;
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
        let payload =
            EncodedPayload::lam_json(input).map_err(|error| ActorError::InputSerialization {
                message: error.to_string(),
            })?;
        MessageEnvelope::new(
            message_id,
            MessageSource::User { principal: None },
            delivery,
            payload,
            self.clock.now(),
        )
        .map_err(|error| ActorError::State {
            message: error.to_string(),
        })
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
