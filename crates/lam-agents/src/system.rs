use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use futures_util::future::join_all;
use futures_util::stream::{FuturesUnordered, StreamExt};
use lam::{
    AbortHandle, Actor, ActorBuilder, ActorId, ActorRef, ActorState, DeliveryMode, JournalStore,
    MessageReceipt,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

use crate::config::{AGENTS_NAMESPACE, ChildActorSpec};
use crate::namespace::{SpawnError, SpawnReceipt, SpawnRequest, agents_namespace};
use crate::{ActorAddress, AgentIdentity, AgentSystemBuildError, AgentSystemError, SubagentConfig};

const DEFAULT_MAX_AGENTS: usize = 64;

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

#[derive(Default)]
struct ActorTaskStatus {
    stopped: AtomicBool,
    panicked: AtomicBool,
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

        Ok(AgentSystem {
            inner: Arc::new(SystemInner {
                store: Arc::new(self.store),
                workers,
                next_worker: AtomicUsize::new(0),
                max_agents: self.max_agents,
                state: Mutex::new(SystemState::default()),
                shutdown: AsyncMutex::new(()),
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
        self.inner.stop(false).await
    }

    /// Aborts every resident actor, then joins all executor threads.
    pub async fn abort(&self) -> Result<(), AgentSystemError> {
        self.inner.stop(true).await
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
    pub fn abort_handle(&self) -> AbortHandle {
        self.resident.abort.clone()
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
        self.resident
            .actor_ref
            .send(input, delivery)
            .await
            .map_err(Into::into)
    }

    /// Runs one linear text call to completion.
    pub async fn call<T>(&self, input: T) -> Result<String, AgentSystemError>
    where
        T: Serialize,
    {
        self.resident.ensure_running()?;
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
        let mut actor = self.resident.owner.lock().await;
        let actor = actor.as_mut().ok_or(AgentSystemError::ShuttingDown)?;
        actor.call(input).output::<O>().await.map_err(Into::into)
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
}

struct SystemState<S>
where
    S: JournalStore + 'static,
{
    residents: BTreeMap<ActorAddress, Arc<ResidentActor<S>>>,
    reservations: BTreeSet<ActorAddress>,
    shutting_down: bool,
    stopped: bool,
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
    async fn host(
        self: &Arc<Self>,
        builder: ActorBuilder<Arc<S>>,
        address: ActorAddress,
    ) -> Result<Agent<S>, AgentSystemError> {
        let (resident, reservation) = self.launch(builder, address, false).await?;
        self.commit(resident, reservation)
    }

    async fn launch(
        self: &Arc<Self>,
        builder: ActorBuilder<Arc<S>>,
        address: ActorAddress,
        create_only: bool,
    ) -> Result<(Arc<ResidentActor<S>>, SpawnReservation<S>), AgentSystemError> {
        let reservation = SpawnReservation::acquire(Arc::clone(self), address.clone())?;
        let worker = self.next_worker.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        let (reply, result) = oneshot::channel();
        let status = Arc::new(ActorTaskStatus::default());
        self.workers[worker]
            .sender
            .send(WorkerCommand::Launch {
                builder: Box::new(builder),
                create_only,
                status: Arc::clone(&status),
                reply,
            })
            .map_err(|_| AgentSystemError::WorkerUnavailable)?;
        let actor = result
            .await
            .map_err(|_| AgentSystemError::WorkerUnavailable)??;
        Ok((ResidentActor::new(address, actor, status), reservation))
    }

    fn commit(
        self: &Arc<Self>,
        resident: Arc<ResidentActor<S>>,
        mut reservation: SpawnReservation<S>,
    ) -> Result<Agent<S>, AgentSystemError> {
        let address = resident.address.clone();
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

        Ok(Agent {
            resident,
            _system: Arc::clone(self),
        })
    }

    pub(crate) async fn spawn_child(
        self: &Arc<Self>,
        parent_address: ActorAddress,
        parent_depth: usize,
        config: Arc<SubagentConfig<S>>,
        request: SpawnRequest,
    ) -> Result<SpawnReceipt, SpawnError> {
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
        let admission = ChildAdmissionGuard::new(self, &child);
        child
            .resident
            .actor_ref
            .send_from_actor(&parent.actor_ref, request.task)
            .await
            .map_err(|error| SpawnError::StartFailed {
                message: error.to_string(),
            })?;
        admission.disarm();
        Ok(SpawnReceipt {
            address: child_address,
            model: target,
            namespaces: selected_paths,
            depth: child_depth,
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
        target
            .actor_ref
            .send_from_actor(&sender.actor_ref, message)
            .await
            .map_err(Into::into)
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
    }

    async fn stop(&self, abort: bool) -> Result<(), AgentSystemError> {
        if abort {
            let handles = lock(&self.state)
                .residents
                .values()
                .map(|resident| resident.abort.clone())
                .collect::<Vec<_>>();
            for handle in handles {
                handle.abort();
            }
        }
        let _shutdown = self.shutdown.lock().await;
        let residents = {
            let mut state = lock(&self.state);
            if state.stopped {
                return Ok(());
            }
            state.shutting_down = true;
            state.residents.values().cloned().collect::<Vec<_>>()
        };

        if abort {
            for resident in &residents {
                resident.abort.abort();
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
            self.system.retire(&self.resident.address, &self.resident);
            self.resident.abort.abort();
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
        builder: Box<ActorBuilder<Arc<S>>>,
        create_only: bool,
        status: Arc<ActorTaskStatus>,
        reply: oneshot::Sender<Result<Actor<Arc<S>>, lam::ActorBuildError>>,
    },
    Stop,
}

async fn worker_loop<S>(mut commands: mpsc::UnboundedReceiver<WorkerCommand<S>>)
where
    S: JournalStore + 'static,
{
    let mut tasks = FuturesUnordered::new();
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(WorkerCommand::Launch { builder, create_only, status, reply }) => {
                    let built = if create_only {
                        (*builder).build_new_task().await
                    } else {
                        (*builder).build_task().await
                    };
                    match built {
                        Ok((actor, task)) => {
                            let handle = tokio::task::spawn_local(task);
                            tasks.push(async move {
                                if handle.await.is_err() {
                                    status.panicked.store(true, Ordering::Release);
                                }
                                status.stopped.store(true, Ordering::Release);
                            });
                            let _ = reply.send(Ok(actor));
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
