use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use futures_util::future::join_all;
use futures_util::stream::{FuturesUnordered, StreamExt};
use lam::{
    AbortHandle, Actor, ActorBuilder, ActorId, ActorRef, ActorState, CompactionReceipt,
    DeliveryMode, JournalStore, MessageReceipt, ModelSwitchPolicy, ModelSwitchReceipt, RunEvents,
    RuntimeEvents,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc, oneshot};

use crate::config::{AGENTS_NAMESPACE, ChildActorSpec};
use crate::namespace::{
    SpawnError, SpawnReceipt, SpawnRequest, StopError, WaitError, WaitReceipt, WaitRequest,
    WaitedTask, agents_namespace,
};
use crate::{
    ActorAddress, AgentIdentity, AgentOutcome, AgentSystemBuildError, AgentSystemError,
    AgentSystemEvent, AgentSystemEvents, StopReason, SubagentConfig,
};

const DEFAULT_MAX_AGENTS: usize = 64;
const SYSTEM_EVENT_BUFFER: usize = 1_024;

struct ResidentActor<S>
where
    S: JournalStore + 'static,
{
    address: ActorAddress,
    owner: AsyncMutex<Option<Actor<Arc<S>>>>,
    actor_ref: ActorRef<Arc<S>>,
    abort: AbortHandle,
    status: Arc<ActorTaskStatus>,
}

struct PendingActor<S>
where
    S: JournalStore + 'static,
{
    address: ActorAddress,
    actor: Actor<Arc<S>>,
    status: Arc<ActorTaskStatus>,
    start: oneshot::Sender<()>,
}

#[derive(Default)]
struct ActorTaskStatus {
    stopped: AtomicBool,
    panicked: AtomicBool,
    reason: Mutex<Option<StopReason>>,
}

impl<S> ResidentActor<S>
where
    S: JournalStore + 'static,
{
    fn new(address: ActorAddress, actor: Actor<Arc<S>>, status: Arc<ActorTaskStatus>) -> Arc<Self> {
        Arc::new(Self {
            address,
            actor_ref: actor.actor_ref(),
            abort: actor.abort_handle(),
            owner: AsyncMutex::new(Some(actor)),
            status,
        })
    }

    fn is_stopped(&self) -> bool {
        self.status.stopped.load(Ordering::Acquire)
    }

    fn ensure_running(&self) -> Result<(), AgentSystemError> {
        if !self.is_stopped() {
            return Ok(());
        }
        let address = self.address.clone();
        if self.status.panicked.load(Ordering::Acquire) {
            Err(AgentSystemError::ActorTaskPanicked { address })
        } else {
            Err(AgentSystemError::ActorUnavailable { address })
        }
    }

    fn set_stop_reason(&self, reason: StopReason) {
        *lock(&self.status.reason) = Some(reason);
    }

    fn request_stop(&self, reason: StopReason) {
        self.set_stop_reason(reason);
        self.abort.abort();
    }

    async fn wait_stopped(&self, activity: &Notify) {
        loop {
            let notified = activity.notified();
            if self.is_stopped() {
                return;
            }
            notified.await;
        }
    }
}

/// Force-cancellation authority which also preserves the system stop reason.
#[derive(Clone)]
pub struct AgentAbortHandle {
    abort: AbortHandle,
    status: Arc<ActorTaskStatus>,
}

impl AgentAbortHandle {
    /// Cancels the actor's current operation and retires its runtime.
    pub fn abort(&self) {
        *lock(&self.status.reason) = Some(StopReason::Aborted);
        self.abort.abort();
    }
}

/// Builds a bounded pool of actor-local executor threads.
pub struct AgentSystemBuilder<S> {
    store: S,
    worker_threads: usize,
    max_agents: usize,
}

impl<S> AgentSystemBuilder<S>
where
    S: JournalStore + 'static,
{
    /// Sets the number of current-thread executors in the pool.
    #[must_use]
    pub const fn worker_threads(mut self, worker_threads: usize) -> Self {
        self.worker_threads = worker_threads;
        self
    }

    /// Sets the maximum number of building and resident actors.
    #[must_use]
    pub const fn max_agents(mut self, max_agents: usize) -> Self {
        self.max_agents = max_agents;
        self
    }

    /// Starts every executor and returns a ready multi-actor system.
    pub fn build(self) -> Result<AgentSystem<S>, AgentSystemBuildError> {
        if self.worker_threads == 0 {
            return Err(AgentSystemBuildError::ZeroWorkers);
        }
        if self.max_agents == 0 {
            return Err(AgentSystemBuildError::ZeroCapacity);
        }

        let mut workers = Vec::with_capacity(self.worker_threads);
        for index in 0..self.worker_threads {
            match Worker::start(index) {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    for worker in &workers {
                        worker.stop();
                    }
                    for worker in &workers {
                        let _ = worker.join();
                    }
                    return Err(error);
                }
            }
        }

        let (event_sender, event_receiver) = mpsc::channel(SYSTEM_EVENT_BUFFER);
        Ok(AgentSystem {
            inner: Arc::new(SystemInner {
                store: Arc::new(self.store),
                workers,
                next_worker: AtomicUsize::new(0),
                max_agents: self.max_agents,
                state: Mutex::new(SystemState::default()),
                shutdown: AsyncMutex::new(()),
                events: event_sender,
                event_receiver: Mutex::new(Some(AgentSystemEvents {
                    receiver: event_receiver,
                })),
                activity: Arc::new(Notify::new()),
            }),
        })
    }
}

/// Bounded owner of resident actors and their local executor pool.
pub struct AgentSystem<S>
where
    S: JournalStore + 'static,
{
    inner: Arc<SystemInner<S>>,
}

impl<S> Clone for AgentSystem<S>
where
    S: JournalStore + 'static,
{
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<S> AgentSystem<S>
where
    S: JournalStore + 'static,
{
    /// Starts a system builder with one worker and a 64-actor residency bound.
    #[must_use]
    pub const fn builder(store: S) -> AgentSystemBuilder<S> {
        AgentSystemBuilder {
            store,
            worker_threads: 1,
            max_agents: DEFAULT_MAX_AGENTS,
        }
    }

    /// Clones the shared journal handle used when constructing actor builders.
    #[must_use]
    pub fn state_store(&self) -> Arc<S> {
        Arc::clone(&self.inner.store)
    }

    /// Takes the single-consumer stream covering every managed actor.
    pub fn take_events(&self) -> Option<AgentSystemEvents> {
        lock(&self.inner.event_receiver).take()
    }

    /// Hosts an actor builder on the next local executor in the pool.
    pub async fn host(&self, builder: ActorBuilder<Arc<S>>) -> Result<Agent<S>, AgentSystemError> {
        let address = builder_address(&builder)?;
        self.inner
            .host(
                builder.annotate_system_prompt(identity_instruction(&address)),
                address,
            )
            .await
    }

    /// Installs the subagent capability pack and hosts one root actor.
    ///
    /// The namespace derives its authoritative sender identity from the actor
    /// builder, so the caller cannot accidentally bind a different identity.
    pub async fn host_with_subagents(
        &self,
        builder: ActorBuilder<Arc<S>>,
        config: SubagentConfig<S>,
    ) -> Result<Agent<S>, AgentSystemError> {
        let address = builder_address(&builder)?;
        let namespace = agents_namespace(
            Arc::downgrade(&self.inner),
            address.clone(),
            0,
            Arc::new(config),
        );
        self.inner
            .host(
                builder
                    .namespace(namespace)
                    .annotate_system_prompt(identity_instruction(&address)),
                address,
            )
            .await
    }

    /// Gracefully stops every resident actor, then joins all executor threads.
    pub async fn shutdown(&self) -> Result<(), AgentSystemError> {
        self.inner.stop_all(false).await
    }

    /// Aborts every resident actor, then joins all executor threads.
    pub async fn abort(&self) -> Result<(), AgentSystemError> {
        self.inner.stop_all(true).await
    }

    /// Waits until all managed actors and eligible mailbox work are quiescent.
    pub async fn wait(&self) -> Result<(), AgentSystemError> {
        self.inner.wait().await
    }

    /// Forcefully retires one addressed actor and all of its descendants.
    pub async fn stop(&self, address: &ActorAddress) -> Result<(), AgentSystemError> {
        let _activity = self.inner.begin_activity()?;
        self.inner.stop_subtree(address, StopReason::Stopped).await
    }
}

/// Cloneable embedded handle to one actor hosted by an [`AgentSystem`].
pub struct Agent<S>
where
    S: JournalStore + 'static,
{
    resident: Arc<ResidentActor<S>>,
    _system: Arc<SystemInner<S>>,
}

impl<S> Clone for Agent<S>
where
    S: JournalStore + 'static,
{
    fn clone(&self) -> Self {
        Self {
            resident: Arc::clone(&self.resident),
            _system: Arc::clone(&self._system),
        }
    }
}

impl<S> Agent<S>
where
    S: JournalStore + 'static,
{
    /// Returns this actor's canonical address.
    #[must_use]
    pub fn address(&self) -> &ActorAddress {
        &self.resident.address
    }

    /// Returns the underlying single-actor journal identity.
    #[must_use]
    pub fn actor_id(&self) -> &ActorId {
        self.resident.actor_ref.actor_id()
    }

    /// Returns a cloneable send-only mailbox address.
    #[must_use]
    pub fn actor_ref(&self) -> ActorRef<Arc<S>> {
        self.resident.actor_ref.clone()
    }

    /// Returns explicit force-cancellation authority while the actor is live.
    #[must_use]
    pub fn abort_handle(&self) -> AgentAbortHandle {
        AgentAbortHandle {
            abort: self.resident.abort.clone(),
            status: Arc::clone(&self.resident.status),
        }
    }

    /// Durably admits ordinary user input without waiting for completion.
    pub async fn send<T>(
        &self,
        input: T,
        delivery: DeliveryMode,
    ) -> Result<MessageReceipt, AgentSystemError>
    where
        T: Serialize,
    {
        self.resident.ensure_running()?;
        let _activity = self._system.begin_activity()?;
        let receipt = self
            .resident
            .actor_ref
            .send(input, delivery)
            .await
            .map_err(AgentSystemError::from)?;
        self._system.activity.notify_waiters();
        Ok(receipt)
    }

    /// Runs one linear text call to completion.
    pub async fn call<T>(&self, input: T) -> Result<String, AgentSystemError>
    where
        T: Serialize,
    {
        self.resident.ensure_running()?;
        let _activity = self._system.begin_activity()?;
        let mut actor = self.resident.owner.lock().await;
        let actor = actor.as_mut().ok_or(AgentSystemError::ShuttingDown)?;
        actor.call(input).await.map_err(Into::into)
    }

    /// Runs one linear schema-constrained call to completion.
    pub async fn call_structured<I, O>(&self, input: I) -> Result<O, AgentSystemError>
    where
        I: Serialize,
        O: DeserializeOwned + JsonSchema,
    {
        self.resident.ensure_running()?;
        let _activity = self._system.begin_activity()?;
        let mut actor = self.resident.owner.lock().await;
        let actor = actor.as_mut().ok_or(AgentSystemError::ShuttingDown)?;
        actor.call(input).output::<O>().await.map_err(Into::into)
    }

    /// Explicitly compacts this actor's current model-visible context.
    pub async fn compact(&self) -> Result<Option<CompactionReceipt>, AgentSystemError> {
        self.resident.ensure_running()?;
        let _activity = self._system.begin_activity()?;
        let mut actor = self.resident.owner.lock().await;
        let actor = actor.as_mut().ok_or(AgentSystemError::ShuttingDown)?;
        actor.compact().await.map_err(Into::into)
    }

    /// Compacts existing history and selects another registered root model.
    pub async fn switch_model(
        &self,
        model_id: impl Into<String>,
    ) -> Result<ModelSwitchReceipt, AgentSystemError> {
        self.switch_model_with_policy(model_id, ModelSwitchPolicy::Compact)
            .await
    }

    /// Selects another registered root model using an explicit context policy.
    pub async fn switch_model_with_policy(
        &self,
        model_id: impl Into<String>,
        policy: ModelSwitchPolicy,
    ) -> Result<ModelSwitchReceipt, AgentSystemError> {
        self.resident.ensure_running()?;
        let _activity = self._system.begin_activity()?;
        let mut actor = self.resident.owner.lock().await;
        let actor = actor.as_mut().ok_or(AgentSystemError::ShuttingDown)?;
        actor
            .switch_model_with_policy(model_id, policy)
            .await
            .map_err(Into::into)
    }

    /// Rebuilds this actor's current projection from its journal.
    pub async fn state(&self) -> Result<ActorState, AgentSystemError> {
        self.resident.ensure_running()?;
        self.resident.actor_ref.state().await.map_err(Into::into)
    }
}

pub(crate) struct SystemInner<S>
where
    S: JournalStore + 'static,
{
    store: Arc<S>,
    workers: Vec<Worker<S>>,
    next_worker: AtomicUsize,
    max_agents: usize,
    state: Mutex<SystemState<S>>,
    shutdown: AsyncMutex<()>,
    events: mpsc::Sender<AgentSystemEvent>,
    event_receiver: Mutex<Option<AgentSystemEvents>>,
    activity: Arc<Notify>,
}

struct SystemState<S>
where
    S: JournalStore + 'static,
{
    residents: BTreeMap<ActorAddress, Arc<ResidentActor<S>>>,
    reservations: BTreeSet<ActorAddress>,
    shutting_down: bool,
    stopped: bool,
    active_operations: usize,
    spawned_tasks: BTreeMap<ActorAddress, SpawnedTask>,
}

struct SpawnedTask {
    parent: ActorAddress,
    message_id: String,
    delivery: OutcomeDelivery,
}

enum OutcomeDelivery {
    Pending,
    Delivered {
        inbox_message_id: String,
        inbox_revision: u64,
    },
    Failed {
        message: String,
    },
}

struct ChildLaunch<S>
where
    S: JournalStore + 'static,
{
    child: Agent<S>,
    parent: Arc<ResidentActor<S>>,
    address: ActorAddress,
    model: crate::ModelTarget,
    namespaces: Vec<String>,
    depth: usize,
    task: String,
}

impl<S> Default for SystemState<S>
where
    S: JournalStore + 'static,
{
    fn default() -> Self {
        Self {
            residents: BTreeMap::new(),
            reservations: BTreeSet::new(),
            shutting_down: false,
            stopped: false,
            active_operations: 0,
            spawned_tasks: BTreeMap::new(),
        }
    }
}

impl<S> SystemState<S>
where
    S: JournalStore + 'static,
{
    fn prune_stopped(&mut self) {
        self.residents.retain(|_, resident| !resident.is_stopped());
    }
}

impl<S> SystemInner<S>
where
    S: JournalStore + 'static,
{
    fn begin_activity(self: &Arc<Self>) -> Result<ActivityGuard<S>, AgentSystemError> {
        {
            let mut state = lock(&self.state);
            if state.shutting_down {
                return Err(AgentSystemError::ShuttingDown);
            }
            state.active_operations += 1;
        }
        self.activity.notify_waiters();
        Ok(ActivityGuard {
            system: Arc::clone(self),
        })
    }

    async fn host(
        self: &Arc<Self>,
        builder: ActorBuilder<Arc<S>>,
        address: ActorAddress,
    ) -> Result<Agent<S>, AgentSystemError> {
        let (pending, reservation) = self.launch(builder, address, false).await?;
        self.commit(pending, reservation)
    }

    async fn launch(
        self: &Arc<Self>,
        builder: ActorBuilder<Arc<S>>,
        address: ActorAddress,
        create_only: bool,
    ) -> Result<(PendingActor<S>, SpawnReservation<S>), AgentSystemError> {
        let reservation = SpawnReservation::acquire(Arc::clone(self), address.clone())?;
        let worker = self.next_worker.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        let (reply, result) = oneshot::channel();
        let status = Arc::new(ActorTaskStatus::default());
        self.workers[worker]
            .sender
            .send(WorkerCommand::Launch {
                address: address.clone(),
                builder: Box::new(builder),
                create_only,
                status: Arc::clone(&status),
                events: self.events.clone(),
                activity: Arc::clone(&self.activity),
                reply,
            })
            .map_err(|_| AgentSystemError::WorkerUnavailable)?;
        let launched = result
            .await
            .map_err(|_| AgentSystemError::WorkerUnavailable)??;
        Ok((
            PendingActor {
                address,
                actor: launched.actor,
                status,
                start: launched.start,
            },
            reservation,
        ))
    }

    fn commit(
        self: &Arc<Self>,
        mut pending: PendingActor<S>,
        mut reservation: SpawnReservation<S>,
    ) -> Result<Agent<S>, AgentSystemError> {
        let address = pending.address.clone();
        let run_events = pending.actor.take_run_events();
        let runtime_events = pending.actor.take_runtime_events();
        let resident = ResidentActor::new(address.clone(), pending.actor, pending.status);
        debug_assert_eq!(resident.actor_ref.actor_id().as_str(), address.as_str());
        {
            let mut state = lock(&self.state);
            state.reservations.remove(&address);
            reservation.active = false;
            if state.shutting_down {
                return Err(AgentSystemError::ShuttingDown);
            }
            state.prune_stopped();
            match state.residents.entry(address.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert(Arc::clone(&resident));
                }
                Entry::Occupied(_) => {
                    return Err(AgentSystemError::AddressInUse { address });
                }
            }
        }

        self.emit(AgentSystemEvent::Hosted {
            address: address.clone(),
            parent: address.parent(),
        });
        self.forward_actor_events(&resident, run_events, runtime_events);
        if pending.start.send(()).is_err() {
            self.retire(&address, &resident);
            self.emit(AgentSystemEvent::Retired {
                address,
                reason: StopReason::Failed {
                    message: "actor worker stopped before startup".to_owned(),
                },
            });
            return Err(AgentSystemError::WorkerUnavailable);
        }

        Ok(Agent {
            resident,
            _system: Arc::clone(self),
        })
    }

    fn emit(&self, event: AgentSystemEvent) {
        let _ = self.events.try_send(event);
        self.activity.notify_waiters();
    }

    fn forward_actor_events(
        &self,
        resident: &Arc<ResidentActor<S>>,
        run_events: Option<RunEvents>,
        runtime_events: Option<RuntimeEvents>,
    ) {
        if let Some(mut run_events) = run_events {
            let address = resident.address.clone();
            let events = self.events.clone();
            let activity = Arc::clone(&self.activity);
            tokio::spawn(async move {
                while let Some(event) = run_events.next().await {
                    let _ = events.try_send(AgentSystemEvent::Run {
                        address: address.clone(),
                        event,
                    });
                    activity.notify_waiters();
                }
            });
        }
        if let Some(mut runtime_events) = runtime_events {
            let address = resident.address.clone();
            let events = self.events.clone();
            let activity = Arc::clone(&self.activity);
            tokio::spawn(async move {
                while let Some(event) = runtime_events.next().await {
                    let _ = events.try_send(AgentSystemEvent::ActorRuntime {
                        address: address.clone(),
                        event,
                    });
                    activity.notify_waiters();
                }
            });
        }
    }

    pub(crate) async fn spawn_child(
        self: &Arc<Self>,
        parent_address: ActorAddress,
        parent_depth: usize,
        config: Arc<SubagentConfig<S>>,
        request: SpawnRequest,
    ) -> Result<SpawnReceipt, SpawnError> {
        let ChildLaunch {
            child,
            parent,
            address,
            model,
            namespaces,
            depth,
            task,
        } = self
            .prepare_child(parent_address, parent_depth, config, request)
            .await?;
        let admission_guard = ChildAdmissionGuard::new(self, &child);
        let activity = self.begin_activity().map_err(spawn_system_error)?;
        let (ready, admitted) = oneshot::channel();
        let (accept, accepted) = oneshot::channel();
        let system = Arc::clone(self);
        let background_child = child.clone();
        tokio::spawn(async move {
            system
                .run_background_child(parent, background_child, task, ready, accepted, activity)
                .await;
        });

        let receipt = admitted
            .await
            .map_err(|_| SpawnError::Unavailable)?
            .map_err(|error| SpawnError::StartFailed {
                message: error.to_string(),
            })?;
        accept.send(()).map_err(|_| SpawnError::Unavailable)?;
        admission_guard.disarm();
        Ok(SpawnReceipt {
            address,
            model,
            namespaces,
            depth,
            message_id: receipt.message_id.to_string(),
            revision: receipt.revision.get(),
        })
    }

    pub(crate) async fn call_child(
        self: &Arc<Self>,
        parent_address: ActorAddress,
        parent_depth: usize,
        config: Arc<SubagentConfig<S>>,
        request: SpawnRequest,
    ) -> Result<AgentOutcome, SpawnError> {
        let ChildLaunch {
            child,
            parent,
            address,
            task,
            ..
        } = self
            .prepare_child(parent_address, parent_depth, config, request)
            .await?;
        let admission_guard = ChildAdmissionGuard::new(self, &child);
        let _activity = self.begin_activity().map_err(spawn_system_error)?;
        let mut owner = child.resident.owner.lock().await;
        let actor = owner.as_mut().ok_or(SpawnError::Unavailable)?;
        let mut run = actor.call_from_actor(&parent.actor_ref, task);
        let message_id = run.message_id().clone();
        run.wait_admitted()
            .await
            .map_err(|error| SpawnError::StartFailed {
                message: error.to_string(),
            })?;
        let result = run.await;
        drop(owner);
        let outcome = AgentOutcome::from_result(address, &message_id, result);
        self.emit(AgentSystemEvent::Outcome {
            outcome: outcome.clone(),
        });
        admission_guard.disarm();
        Ok(outcome)
    }

    async fn run_background_child(
        self: Arc<Self>,
        parent: Arc<ResidentActor<S>>,
        child: Agent<S>,
        task: String,
        ready: oneshot::Sender<Result<MessageReceipt, lam::ActorError>>,
        accepted: oneshot::Receiver<()>,
        _activity: ActivityGuard<S>,
    ) {
        let mut owner = child.resident.owner.lock().await;
        let Some(actor) = owner.as_mut() else {
            let _ = ready.send(Err(lam::ActorError::Unavailable));
            return;
        };
        let mut run = actor.call_from_actor(&parent.actor_ref, task);
        let message_id = run.message_id().clone();
        let receipt = match run.wait_admitted().await {
            Ok(receipt) => receipt,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        self.track_spawned_task(
            parent.address.clone(),
            child.resident.address.clone(),
            message_id.to_string(),
        );
        if ready.send(Ok(receipt)).is_err() || accepted.await.is_err() {
            self.fail_spawned_delivery(
                &child.resident.address,
                "spawn was cancelled before its admission receipt was accepted".to_owned(),
            );
            drop(owner);
            self.cancel_subtree(&child.resident.address, StopReason::Cancelled);
            return;
        }
        let result = run.await;
        drop(owner);
        let outcome =
            AgentOutcome::from_result(child.resident.address.clone(), &message_id, result);
        self.emit(AgentSystemEvent::Outcome {
            outcome: outcome.clone(),
        });
        let delivery = match parent.ensure_running() {
            Ok(()) => parent
                .actor_ref
                .send_from_actor(&child.resident.actor_ref, outcome)
                .await
                .map_err(AgentSystemError::from),
            Err(error) => Err(error),
        };
        match delivery {
            Ok(receipt) => self.complete_spawned_delivery(&child.resident.address, receipt),
            Err(error) => {
                self.fail_spawned_delivery(&child.resident.address, error.to_string());
            }
        }
    }

    fn track_spawned_task(&self, parent: ActorAddress, address: ActorAddress, message_id: String) {
        let previous = lock(&self.state).spawned_tasks.insert(
            address,
            SpawnedTask {
                parent,
                message_id,
                delivery: OutcomeDelivery::Pending,
            },
        );
        debug_assert!(previous.is_none(), "spawned addresses are create-only");
        self.activity.notify_waiters();
    }

    fn complete_spawned_delivery(&self, address: &ActorAddress, receipt: MessageReceipt) {
        if let Some(task) = lock(&self.state).spawned_tasks.get_mut(address) {
            task.delivery = OutcomeDelivery::Delivered {
                inbox_message_id: receipt.message_id.to_string(),
                inbox_revision: receipt.revision.get(),
            };
        }
        self.activity.notify_waiters();
    }

    fn fail_spawned_delivery(&self, address: &ActorAddress, message: String) {
        if let Some(task) = lock(&self.state).spawned_tasks.get_mut(address) {
            task.delivery = OutcomeDelivery::Failed { message };
        }
        self.activity.notify_waiters();
    }

    async fn prepare_child(
        self: &Arc<Self>,
        parent_address: ActorAddress,
        parent_depth: usize,
        config: Arc<SubagentConfig<S>>,
        request: SpawnRequest,
    ) -> Result<ChildLaunch<S>, SpawnError> {
        if parent_depth >= config.max_depth {
            return Err(SpawnError::DepthLimit {
                max_depth: config.max_depth,
            });
        }
        let child_address =
            parent_address
                .child(&request.name)
                .map_err(|error| SpawnError::InvalidName {
                    name: request.name.clone(),
                    message: error.to_string(),
                })?;
        let target = request
            .model
            .unwrap_or_else(|| config.default_model.clone());
        let registration =
            config
                .registration(&target)
                .cloned()
                .ok_or_else(|| SpawnError::ModelNotAllowed {
                    provider: target.provider.clone(),
                    model: target.model.clone(),
                })?;
        let selected_paths = config
            .select_namespace_paths(request.namespaces)
            .map_err(|path| SpawnError::NamespaceNotAllowed { path })?;
        let child_depth = parent_depth + 1;
        let mut namespaces = Vec::with_capacity(selected_paths.len());
        for path in &selected_paths {
            if path == AGENTS_NAMESPACE {
                namespaces.push(agents_namespace(
                    Arc::downgrade(self),
                    child_address.clone(),
                    child_depth,
                    config.clone(),
                ));
            } else {
                namespaces.push(
                    config
                        .namespaces
                        .get(path)
                        .expect("selected namespace was validated")
                        .clone(),
                );
            }
        }
        let mut instructions = Vec::new();
        if let Some(instruction) = request.instructions {
            instructions.push(instruction);
        }
        instructions.extend(config.required_instructions.clone());
        instructions.push(identity_instruction(&child_address));
        let builder = registration.actor_builder(ChildActorSpec {
            address: child_address.clone(),
            store: Arc::clone(&self.store),
            namespaces,
            system_prompt: request.system_prompt,
            instructions,
            default_eval_timeout: config.default_eval_timeout,
            max_eval_timeout: config.max_eval_timeout,
            capture_console: config.capture_console,
        });
        let parent = self.resident(&parent_address).map_err(spawn_system_error)?;
        let (child, reservation) = self
            .launch(builder, child_address.clone(), true)
            .await
            .map_err(spawn_system_error)?;
        let child = self
            .commit(child, reservation)
            .map_err(spawn_system_error)?;
        Ok(ChildLaunch {
            child,
            parent,
            address: child_address,
            model: target,
            namespaces: selected_paths,
            depth: child_depth,
            task: request.task,
        })
    }

    pub(crate) async fn stop_child(
        &self,
        requester: &ActorAddress,
        address: &ActorAddress,
    ) -> Result<(), StopError> {
        if !address.is_direct_child_of(requester) {
            return Err(StopError::NotDirectChild {
                requester: requester.clone(),
                address: address.clone(),
            });
        }
        self.stop_subtree(address, StopReason::Stopped)
            .await
            .map_err(|error| match error {
                AgentSystemError::ActorUnavailable { address } => {
                    StopError::AddressUnavailable { address }
                }
                AgentSystemError::ShuttingDown | AgentSystemError::WorkerUnavailable => {
                    StopError::Unavailable
                }
                error => StopError::StopFailed {
                    message: error.to_string(),
                },
            })
    }

    fn resident(&self, address: &ActorAddress) -> Result<Arc<ResidentActor<S>>, AgentSystemError> {
        let mut state = lock(&self.state);
        state.prune_stopped();
        if state.shutting_down {
            return Err(AgentSystemError::ShuttingDown);
        }
        state
            .residents
            .get(address)
            .cloned()
            .ok_or_else(|| AgentSystemError::ActorUnavailable {
                address: address.clone(),
            })
    }

    pub(crate) async fn send(
        &self,
        sender_address: &ActorAddress,
        target_address: &ActorAddress,
        message: serde_json::Value,
    ) -> Result<MessageReceipt, AgentSystemError> {
        let sender = self.resident(sender_address)?;
        let target = self.resident(target_address)?;
        let receipt = target
            .actor_ref
            .send_from_actor(&sender.actor_ref, message)
            .await
            .map_err(AgentSystemError::from)?;
        self.activity.notify_waiters();
        Ok(receipt)
    }

    pub(crate) async fn wait_for_spawned(
        &self,
        requester: &ActorAddress,
        request: WaitRequest,
    ) -> Result<WaitReceipt, WaitError> {
        if request.addresses.is_empty() {
            return Err(WaitError::Empty);
        }
        let mut unique = BTreeSet::new();
        for address in &request.addresses {
            if !address.is_direct_child_of(requester) {
                return Err(WaitError::NotDirectChild {
                    requester: requester.clone(),
                    address: address.clone(),
                });
            }
            if !unique.insert(address.clone()) {
                return Err(WaitError::Duplicate {
                    address: address.clone(),
                });
            }
        }

        loop {
            let notified = self.activity.notified();
            let completed = {
                let state = lock(&self.state);
                if state.shutting_down {
                    return Err(WaitError::Unavailable);
                }
                let mut completed = Vec::with_capacity(request.addresses.len());
                let mut pending = false;
                for address in &request.addresses {
                    let Some(task) = state.spawned_tasks.get(address) else {
                        return Err(WaitError::NotSpawned {
                            requester: requester.clone(),
                            address: address.clone(),
                        });
                    };
                    if task.parent != *requester {
                        return Err(WaitError::NotSpawned {
                            requester: requester.clone(),
                            address: address.clone(),
                        });
                    }
                    match &task.delivery {
                        OutcomeDelivery::Pending => pending = true,
                        OutcomeDelivery::Delivered {
                            inbox_message_id,
                            inbox_revision,
                        } => completed.push(WaitedTask {
                            address: address.clone(),
                            message_id: task.message_id.clone(),
                            inbox_message_id: inbox_message_id.clone(),
                            inbox_revision: *inbox_revision,
                        }),
                        OutcomeDelivery::Failed { message } => {
                            return Err(WaitError::DeliveryFailed {
                                address: address.clone(),
                                message: message.clone(),
                            });
                        }
                    }
                }
                (!pending).then_some(completed)
            };
            if let Some(completed) = completed {
                return Ok(WaitReceipt { completed });
            }
            notified.await;
        }
    }

    pub(crate) fn list_children(
        &self,
        parent: &ActorAddress,
    ) -> Result<Vec<AgentIdentity>, AgentSystemError> {
        let mut state = lock(&self.state);
        state.prune_stopped();
        if state.shutting_down {
            return Err(AgentSystemError::ShuttingDown);
        }
        Ok(state
            .residents
            .keys()
            .filter(|address| address.is_direct_child_of(parent))
            .cloned()
            .map(AgentIdentity::new)
            .collect())
    }

    fn retire(&self, address: &ActorAddress, resident: &Arc<ResidentActor<S>>) {
        let mut state = lock(&self.state);
        if state
            .residents
            .get(address)
            .is_some_and(|current| Arc::ptr_eq(current, resident))
        {
            state.residents.remove(address);
        }
        self.activity.notify_waiters();
    }

    async fn wait(&self) -> Result<(), AgentSystemError> {
        loop {
            let notified = self.activity.notified();
            let (residents, busy) = {
                let mut state = lock(&self.state);
                state.prune_stopped();
                if state.shutting_down {
                    if state.stopped {
                        return Ok(());
                    }
                    return Err(AgentSystemError::ShuttingDown);
                }
                (
                    state.residents.values().cloned().collect::<Vec<_>>(),
                    state.active_operations != 0 || !state.reservations.is_empty(),
                )
            };
            if busy {
                notified.await;
                continue;
            }

            let mut idle = true;
            for resident in residents {
                if resident.is_stopped() {
                    continue;
                }
                let state = resident.actor_ref.state().await?;
                if state.active_run().is_some() || state.eligible_messages().next().is_some() {
                    idle = false;
                    break;
                }
            }
            if idle {
                let mut state = lock(&self.state);
                state.prune_stopped();
                if !state.shutting_down
                    && state.active_operations == 0
                    && state.reservations.is_empty()
                {
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    async fn stop_subtree(
        &self,
        address: &ActorAddress,
        reason: StopReason,
    ) -> Result<(), AgentSystemError> {
        let residents = {
            let mut state = lock(&self.state);
            state.prune_stopped();
            if state.shutting_down {
                return Err(AgentSystemError::ShuttingDown);
            }
            let residents = state
                .residents
                .iter()
                .filter(|(candidate, _)| {
                    *candidate == address || candidate.is_descendant_of(address)
                })
                .map(|(_, resident)| Arc::clone(resident))
                .collect::<Vec<_>>();
            if residents.is_empty() {
                return Err(AgentSystemError::ActorUnavailable {
                    address: address.clone(),
                });
            }
            residents
        };

        for resident in &residents {
            resident.request_stop(reason.clone());
        }
        let results = join_all(residents.iter().map(|resident| async move {
            let actor = resident.owner.lock().await.take();
            match actor {
                Some(actor) => actor.abort().await,
                None => Ok(()),
            }
        }))
        .await;
        join_all(
            residents
                .iter()
                .map(|resident| resident.wait_stopped(&self.activity)),
        )
        .await;
        {
            let mut state = lock(&self.state);
            for resident in &residents {
                if state
                    .residents
                    .get(&resident.address)
                    .is_some_and(|current| Arc::ptr_eq(current, resident))
                {
                    state.residents.remove(&resident.address);
                }
            }
        }
        self.activity.notify_waiters();
        match results.into_iter().find_map(Result::err) {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }

    fn cancel_subtree(&self, address: &ActorAddress, reason: StopReason) {
        let residents = {
            let state = lock(&self.state);
            state
                .residents
                .iter()
                .filter(|(candidate, _)| {
                    *candidate == address || candidate.is_descendant_of(address)
                })
                .map(|(_, resident)| Arc::clone(resident))
                .collect::<Vec<_>>()
        };
        for resident in residents {
            resident.request_stop(reason.clone());
        }
        self.activity.notify_waiters();
    }

    async fn stop_all(&self, abort: bool) -> Result<(), AgentSystemError> {
        if abort {
            let residents = lock(&self.state)
                .residents
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for resident in residents {
                resident.request_stop(StopReason::Aborted);
            }
        }
        let _shutdown = self.shutdown.lock().await;
        let residents = {
            let mut state = lock(&self.state);
            if state.stopped {
                return Ok(());
            }
            state.shutting_down = true;
            let residents = state.residents.values().cloned().collect::<Vec<_>>();
            for resident in &residents {
                resident.set_stop_reason(if abort {
                    StopReason::Aborted
                } else {
                    StopReason::Shutdown
                });
            }
            residents
        };

        if abort {
            for resident in &residents {
                resident.request_stop(StopReason::Aborted);
            }
        }
        let results = join_all(residents.into_iter().map(|resident| async move {
            let actor = resident.owner.lock().await.take();
            match (actor, abort) {
                (Some(actor), true) => actor.abort().await,
                (Some(actor), false) => actor.shutdown().await,
                (None, _) => Ok(()),
            }
        }))
        .await;

        for worker in &self.workers {
            worker.stop();
        }
        let mut first_error = results.into_iter().find_map(Result::err).map(Into::into);
        for worker in &self.workers {
            if let Err(error) = worker.join()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        {
            let mut state = lock(&self.state);
            state.residents.clear();
            state.stopped = true;
        }
        self.activity.notify_waiters();
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl<S> Drop for SystemInner<S>
where
    S: JournalStore + 'static,
{
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.shutting_down = true;
        state.residents.clear();
        for worker in &self.workers {
            worker.stop();
        }
    }
}

struct ActivityGuard<S>
where
    S: JournalStore + 'static,
{
    system: Arc<SystemInner<S>>,
}

impl<S> Drop for ActivityGuard<S>
where
    S: JournalStore + 'static,
{
    fn drop(&mut self) {
        let mut state = lock(&self.system.state);
        state.active_operations = state
            .active_operations
            .checked_sub(1)
            .expect("activity guards balance their registration");
        drop(state);
        self.system.activity.notify_waiters();
    }
}

struct ChildAdmissionGuard<S>
where
    S: JournalStore + 'static,
{
    system: Arc<SystemInner<S>>,
    resident: Arc<ResidentActor<S>>,
    armed: bool,
}

impl<S> ChildAdmissionGuard<S>
where
    S: JournalStore + 'static,
{
    fn new(system: &Arc<SystemInner<S>>, child: &Agent<S>) -> Self {
        Self {
            system: Arc::clone(system),
            resident: Arc::clone(&child.resident),
            armed: true,
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl<S> Drop for ChildAdmissionGuard<S>
where
    S: JournalStore + 'static,
{
    fn drop(&mut self) {
        if self.armed {
            self.system
                .cancel_subtree(&self.resident.address, StopReason::Cancelled);
        }
    }
}

struct SpawnReservation<S>
where
    S: JournalStore + 'static,
{
    system: Arc<SystemInner<S>>,
    address: ActorAddress,
    active: bool,
}

impl<S> SpawnReservation<S>
where
    S: JournalStore + 'static,
{
    fn acquire(
        system: Arc<SystemInner<S>>,
        address: ActorAddress,
    ) -> Result<Self, AgentSystemError> {
        {
            let mut state = lock(&system.state);
            if state.shutting_down {
                return Err(AgentSystemError::ShuttingDown);
            }
            state.prune_stopped();
            if state.residents.contains_key(&address) || state.reservations.contains(&address) {
                return Err(AgentSystemError::AddressInUse { address });
            }
            if state.residents.len() + state.reservations.len() >= system.max_agents {
                return Err(AgentSystemError::Capacity {
                    max_agents: system.max_agents,
                });
            }
            state.reservations.insert(address.clone());
        }
        system.activity.notify_waiters();
        Ok(Self {
            system,
            address,
            active: true,
        })
    }
}

impl<S> Drop for SpawnReservation<S>
where
    S: JournalStore + 'static,
{
    fn drop(&mut self) {
        if self.active {
            let mut state = lock(&self.system.state);
            state.reservations.remove(&self.address);
            drop(state);
            self.system.activity.notify_waiters();
        }
    }
}

struct Worker<S>
where
    S: JournalStore + 'static,
{
    sender: mpsc::UnboundedSender<WorkerCommand<S>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl<S> Worker<S>
where
    S: JournalStore + 'static,
{
    fn start(index: usize) -> Result<Self, AgentSystemBuildError> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let (ready, readiness) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name(format!("lam-agent-worker-{index}"))
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready.send(Err(error.to_string()));
                        return;
                    }
                };
                let local = tokio::task::LocalSet::new();
                let _ = ready.send(Ok(()));
                local.block_on(&runtime, worker_loop(receiver));
            })
            .map_err(AgentSystemBuildError::ThreadSpawn)?;
        match readiness.recv() {
            Ok(Ok(())) => Ok(Self {
                sender,
                thread: Mutex::new(Some(thread)),
            }),
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(AgentSystemBuildError::WorkerInitialization { message })
            }
            Err(_closed) => {
                let _ = thread.join();
                Err(AgentSystemBuildError::WorkerInitialization {
                    message: "worker exited during initialization".to_owned(),
                })
            }
        }
    }

    fn stop(&self) {
        let _ = self.sender.send(WorkerCommand::Stop);
    }

    fn join(&self) -> Result<(), AgentSystemError> {
        let Some(thread) = lock(&self.thread).take() else {
            return Ok(());
        };
        thread.join().map_err(|panic| AgentSystemError::WorkerJoin {
            message: panic_message(panic),
        })
    }
}

enum WorkerCommand<S>
where
    S: JournalStore + 'static,
{
    Launch {
        address: ActorAddress,
        builder: Box<ActorBuilder<Arc<S>>>,
        create_only: bool,
        status: Arc<ActorTaskStatus>,
        events: mpsc::Sender<AgentSystemEvent>,
        activity: Arc<Notify>,
        reply: oneshot::Sender<Result<LaunchedActor<S>, lam::ActorBuildError>>,
    },
    Stop,
}

struct LaunchedActor<S>
where
    S: JournalStore + 'static,
{
    actor: Actor<Arc<S>>,
    start: oneshot::Sender<()>,
}

async fn worker_loop<S>(mut commands: mpsc::UnboundedReceiver<WorkerCommand<S>>)
where
    S: JournalStore + 'static,
{
    let mut tasks = FuturesUnordered::new();
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(WorkerCommand::Launch {
                    address,
                    builder,
                    create_only,
                    status,
                    events,
                    activity,
                    reply,
                }) => {
                    let built = if create_only {
                        (*builder).build_new_task().await
                    } else {
                        (*builder).build_task().await
                    };
                    match built {
                        Ok((actor, task)) => {
                            let (start, started) = oneshot::channel();
                            tasks.push(async move {
                                if started.await.is_err() {
                                    status.stopped.store(true, Ordering::Release);
                                    activity.notify_waiters();
                                    return;
                                }
                                let result = tokio::task::spawn_local(task).await;
                                let reason = match result {
                                    Ok(()) => lock(&status.reason)
                                        .take()
                                        .unwrap_or(StopReason::Stopped),
                                    Err(error) => {
                                        status.panicked.store(true, Ordering::Release);
                                        StopReason::Failed {
                                            message: if error.is_panic() {
                                                panic_message(error.into_panic())
                                            } else {
                                                "actor task was cancelled by its executor".to_owned()
                                            },
                                        }
                                    }
                                };
                                status.stopped.store(true, Ordering::Release);
                                let _ = events.try_send(AgentSystemEvent::Retired {
                                    address,
                                    reason,
                                });
                                activity.notify_waiters();
                            });
                            let _ = reply.send(Ok(LaunchedActor { actor, start }));
                        }
                        Err(error) => {
                            let _ = reply.send(Err(error));
                        }
                    }
                }
                Some(WorkerCommand::Stop) | None => break,
            },
            Some(()) = tasks.next(), if !tasks.is_empty() => {}
        }
    }
    while tasks.next().await.is_some() {}
}

fn spawn_system_error(error: AgentSystemError) -> SpawnError {
    match error {
        AgentSystemError::Capacity { max_agents } => SpawnError::Capacity { max_agents },
        AgentSystemError::AddressInUse { address } => SpawnError::AddressInUse { address },
        AgentSystemError::ActorBuild(lam::ActorBuildError::ActorAlreadyExists { actor_id }) => {
            SpawnError::AddressInUse {
                address: ActorAddress::new(actor_id.to_string())
                    .expect("agent-system actor ids are validated addresses"),
            }
        }
        AgentSystemError::ShuttingDown | AgentSystemError::WorkerUnavailable => {
            SpawnError::Unavailable
        }
        error => SpawnError::StartFailed {
            message: error.to_string(),
        },
    }
}

fn builder_address<S>(builder: &ActorBuilder<Arc<S>>) -> Result<ActorAddress, AgentSystemError>
where
    S: JournalStore + 'static,
{
    let actor_id = builder.actor_id().map_err(|error| {
        AgentSystemError::ActorBuild(lam::ActorBuildError::InvalidActorId(error.clone()))
    })?;
    ActorAddress::new(actor_id.to_string()).map_err(|error| AgentSystemError::InvalidAddress {
        address: actor_id.to_string(),
        message: error.to_string(),
    })
}

fn identity_instruction(address: &ActorAddress) -> String {
    match address.parent() {
        Some(parent) => format!("Agent identity: {address}. Parent agent: {parent}."),
        None => format!("Agent identity: {address}. Parent agent: none."),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "agent worker panicked without a string payload".to_owned()
    }
}
