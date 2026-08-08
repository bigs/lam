use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use deno_core::futures::FutureExt;
use deno_core::futures::channel::oneshot;
use deno_core::futures::future::{Either, select};
use deno_core::{JsRuntime, RuntimeOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bridge::ConsoleBuffer;
use crate::builtin::{DirectorySelectionSource, Namespace, Registry};
use crate::error::{EvalError, IsolateBuildError, RuntimeException};
use crate::inspector::InspectorClient;
use crate::transpile;

pub use crate::bridge::{ConsoleEntry, ConsoleLevel};

#[allow(unsafe_code)]
mod parked;

use parked::Kernel;

const DEFAULT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);

/// The JSON-only value returned by a successful evaluation.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum EvalValue {
    /// The cell completed without a value.
    Undefined,
    /// A JSON value crossed the isolate boundary.
    Json(Value),
}

/// The complete successful result of one TypeScript cell.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalOutput {
    /// Final expression, with returned Promises automatically awaited.
    pub result: EvalValue,
    /// Console calls made while this cell ran.
    pub logs: Vec<ConsoleEntry>,
}

/// Per-cell evaluation overrides.
#[derive(Clone, Copy, Debug, Default)]
pub struct EvalOptions {
    timeout: Option<Duration>,
}

impl EvalOptions {
    /// Sets an explicit wall-clock deadline for this cell.
    ///
    /// When omitted, the isolate uses its optional builder default wall
    /// deadline, or runs without a wall deadline when that default is unset.
    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// Configures and constructs a persistent TypeScript isolate.
pub struct IsolateBuilder {
    namespaces: Vec<Namespace>,
    directory_selection: Option<DirectorySelectionSource>,
    default_timeout: Option<Duration>,
    execution_timeout: Duration,
    capture_console: bool,
}

impl Default for IsolateBuilder {
    fn default() -> Self {
        Self {
            namespaces: Vec::new(),
            directory_selection: None,
            default_timeout: None,
            execution_timeout: DEFAULT_EXECUTION_TIMEOUT,
            capture_console: true,
        }
    }
}

impl IsolateBuilder {
    /// Registers a typed namespace in the isolate.
    #[must_use]
    pub fn namespace(mut self, namespace: Namespace) -> Self {
        self.namespaces.push(namespace);
        self
    }

    /// Registers several typed namespaces in the isolate.
    #[must_use]
    pub fn namespaces(mut self, namespaces: impl IntoIterator<Item = Namespace>) -> Self {
        self.namespaces.extend(namespaces);
        self
    }

    /// Supplies the live model selection reported by `lam.dir`.
    #[must_use]
    pub fn directory_selection(mut self, selection: DirectorySelectionSource) -> Self {
        self.directory_selection = Some(selection);
        self
    }

    /// Sets the optional wall-clock deadline used by cells without an explicit
    /// override.
    ///
    /// By default there is no wall-clock deadline. Long `Poll::Pending` waits
    /// only hit a wall deadline when this default or
    /// [`EvalOptions::timeout`] is set.
    #[must_use]
    pub const fn default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = Some(timeout);
        self
    }

    /// Sets the maximum continuous time spent inside one eval-future poll.
    ///
    /// Defaults to 30 seconds. The limit applies only while JavaScript or
    /// other work is actively progressing a poll; async builtin waits that
    /// return `Poll::Pending` do not consume it.
    #[must_use]
    pub const fn execution_timeout(mut self, timeout: Duration) -> Self {
        self.execution_timeout = timeout;
        self
    }

    /// Enables or disables collection of `console` calls in eval outputs.
    ///
    /// The JavaScript `console` global remains available when capture is
    /// disabled; calls are simply discarded.
    #[must_use]
    pub const fn capture_console(mut self, capture: bool) -> Self {
        self.capture_console = capture;
        self
    }

    /// Validates the registry and starts the first isolate generation.
    pub async fn build(self) -> Result<Isolate, IsolateBuildError> {
        if self
            .default_timeout
            .is_some_and(|timeout| timeout.is_zero())
            || self.execution_timeout.is_zero()
        {
            return Err(IsolateBuildError::InvalidTimeout);
        }

        let registry = Arc::new(Registry::build(self.namespaces, self.directory_selection)?);
        let console = ConsoleBuffer::new(self.capture_console);
        let generation = 1;
        let mut kernel = Kernel::new(Arc::clone(&registry), console.clone(), generation)?;
        let handle = kernel.isolate_handle();
        let interrupt = IsolateInterrupt::new(handle.clone());
        let execution_watchdog =
            ExecutionWatchdog::start(handle, self.execution_timeout).map_err(|error| {
                IsolateBuildError::RuntimeInitialization {
                    message: error.to_string(),
                }
            })?;

        Ok(Isolate {
            execution_watchdog: Some(execution_watchdog),
            kernel: Some(kernel),
            registry,
            console,
            generation,
            next_cell_id: 1,
            default_timeout: self.default_timeout,
            execution_timeout: self.execution_timeout,
            interrupt,
        })
    }
}

/// Thread-safe control which interrupts the currently installed V8 isolate.
///
/// Interrupting execution poisons that isolate generation. Callers must either
/// drop the associated [`Isolate`] or replace the generation through
/// [`Isolate::restart_after_interruption`] before evaluating more code.
#[derive(Clone)]
pub struct IsolateInterrupt {
    current: Arc<Mutex<Option<deno_core::v8::IsolateHandle>>>,
}

impl IsolateInterrupt {
    fn new(handle: deno_core::v8::IsolateHandle) -> Self {
        Self {
            current: Arc::new(Mutex::new(Some(handle))),
        }
    }

    /// Terminates JavaScript execution in the current isolate generation.
    pub fn terminate(&self) {
        let current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(handle) = current.as_ref() {
            handle.terminate_execution();
            // Termination can race with an async host operation completing:
            // V8 may consume the first request before JavaScript resumes. The
            // queued interrupt closes that handoff without targeting a later
            // isolate generation.
            handle.request_interrupt(terminate_on_interrupt, std::ptr::null_mut());
        }
    }

    fn replace(&self, handle: deno_core::v8::IsolateHandle) {
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }

    fn clear(&self) {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

#[allow(unsafe_code)]
unsafe extern "C" fn terminate_on_interrupt(
    isolate: deno_core::v8::UnsafeRawIsolatePtr,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: V8 invokes interrupt callbacks on the owning isolate thread and
    // supplies the live isolate pointer for the duration of this callback.
    let isolate = unsafe { deno_core::v8::Isolate::ref_from_raw_isolate_ptr(&isolate) };
    isolate.terminate_execution();
}

/// A persistent, serially evaluated TypeScript isolate.
///
/// `Isolate` deliberately does not implement `Send`: an actor scheduler should
/// keep a resident isolate on one local runtime thread and move only durable
pub struct Isolate {
    execution_watchdog: Option<ExecutionWatchdog>,
    kernel: Option<Kernel>,
    registry: Arc<Registry>,
    console: ConsoleBuffer,
    generation: u64,
    next_cell_id: u64,
    default_timeout: Option<Duration>,
    execution_timeout: Duration,
    interrupt: IsolateInterrupt,
}

impl Isolate {
    /// Starts an isolate builder with safe, authority-free defaults.
    #[must_use]
    pub fn builder() -> IsolateBuilder {
        IsolateBuilder::default()
    }

    /// Returns the currently installed isolate generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns a thread-safe handle for forcefully interrupting this isolate.
    ///
    /// Once invoked, the isolate must be discarded rather than reused.
    #[must_use]
    pub fn interrupt_handle(&self) -> IsolateInterrupt {
        self.interrupt.clone()
    }

    /// Returns a compact model-facing synopsis of the installed APIs.
    ///
    /// The synopsis is derived from the same manifest used by `lam.dir()`
    /// inside the isolate. Full documentation and schemas remain available
    /// through that builtin.
    #[must_use]
    pub fn api_inventory(&self) -> String {
        self.registry.prompt_inventory()
    }

    /// Evaluates a TypeScript cell with the host's optional default wall
    /// deadline.
    pub async fn eval(&mut self, source: &str) -> Result<EvalOutput, EvalError> {
        self.eval_with(source, EvalOptions::default()).await
    }

    /// Evaluates a TypeScript cell with explicit per-cell options.
    ///
    /// Cells execute serially because this method requires exclusive access.
    /// After a wall or execution timeout, the poisoned generation is dropped
    /// and replaced before the timeout error is returned.
    pub async fn eval_with(
        &mut self,
        source: &str,
        options: EvalOptions,
    ) -> Result<EvalOutput, EvalError> {
        let wall_timeout = options.timeout.or(self.default_timeout);
        if wall_timeout.is_some_and(|timeout| timeout.is_zero()) {
            return Err(EvalError::internal(
                "evaluation wall timeouts must be greater than zero",
            ));
        }
        let wall_timeout_ms = wall_timeout.map(duration_millis);
        let execution_timeout_ms = duration_millis(self.execution_timeout);

        // A prior eval may have been cancelled/dropped after the watchdog
        // latched a fire but before generation replacement. Recover silently
        // before borrowing the kernel for this cell.
        self.recover_latched_execution_fire()?;

        let cell_id = self.next_cell_id;
        self.next_cell_id = self.next_cell_id.saturating_add(1);
        self.console.clear();

        let evaluation_outcome = {
            let kernel = self.kernel.as_mut().ok_or(EvalError::Poisoned)?;
            let watchdog = self
                .execution_watchdog
                .as_ref()
                .ok_or(EvalError::Poisoned)?;
            let fired = Arc::clone(watchdog.fired());
            // Register before polling so a fire wakes `eval_with` even when the
            // inner evaluation stays Pending after V8 termination.
            let (fire_signal, _fire_guard) = watchdog.register_fire_notify();

            // Acquire any wall-watchdog handle before `evaluate` borrows
            // `kernel` mutably for the duration of the evaluation future.
            let wall_handle = wall_timeout.is_some().then(|| kernel.isolate_handle());

            let evaluation = kernel
                .evaluate(source, cell_id, Arc::clone(watchdog.arm()))
                .fuse();
            deno_core::futures::pin_mut!(evaluation);

            match wall_timeout {
                Some(timeout) => {
                    let (wall_watchdog, timeout_signal) = WallWatchdog::start(
                        wall_handle.expect("wall handle acquired when timeout is configured"),
                        timeout,
                    )?;
                    let wall = async move {
                        let _ = timeout_signal.await;
                    }
                    .fuse();
                    let exec_fire = async move {
                        let _ = fire_signal.await;
                    }
                    .fuse();
                    deno_core::futures::pin_mut!(wall);
                    deno_core::futures::pin_mut!(exec_fire);

                    let result = match select(evaluation, select(wall, exec_fire)).await {
                        Either::Left((result, _)) => Some(result),
                        Either::Right((_timeout_or_fire, _)) => None,
                    };
                    let wall_fired = wall_watchdog.finish()?;
                    let execution_fired = fired.load(Ordering::SeqCst);
                    EvaluationOutcome {
                        result,
                        wall_fired,
                        execution_fired,
                    }
                }
                None => {
                    let exec_fire = async move {
                        let _ = fire_signal.await;
                    }
                    .fuse();
                    deno_core::futures::pin_mut!(exec_fire);

                    let result = match select(evaluation, exec_fire).await {
                        Either::Left((result, _)) => Some(result),
                        Either::Right(((), _)) => None,
                    };
                    let execution_fired = fired.load(Ordering::SeqCst);
                    EvaluationOutcome {
                        result,
                        wall_fired: false,
                        execution_fired,
                    }
                }
            }
        };

        // Execution termination wins over a concurrent wall deadline so the
        // tighter continuous-poll limit is reported accurately.
        if evaluation_outcome.execution_fired {
            return self.restart_after_execution_timeout(execution_timeout_ms);
        }
        if evaluation_outcome.wall_fired {
            return self.restart_after_wall_timeout(
                wall_timeout_ms.expect("wall timeout was configured when the watchdog fired"),
            );
        }

        let result = evaluation_outcome
            .result
            .ok_or_else(|| EvalError::internal("evaluation ended without a result or timeout"))?;
        result.map(|result| EvalOutput {
            result,
            logs: self.console.take(),
        })
    }

    fn restart_after_wall_timeout(&mut self, timeout_ms: u64) -> Result<EvalOutput, EvalError> {
        let previous_generation = self.generation;
        let new_generation = previous_generation.saturating_add(1);

        match self.replace_generation(new_generation) {
            Ok(()) => Err(EvalError::TimedOut {
                timeout_ms,
                previous_generation,
                new_generation,
            }),
            Err(error) => Err(EvalError::RestartFailed {
                timeout_ms,
                previous_generation,
                attempted_generation: new_generation,
                message: error.to_string(),
            }),
        }
    }

    fn restart_after_execution_timeout(
        &mut self,
        timeout_ms: u64,
    ) -> Result<EvalOutput, EvalError> {
        let previous_generation = self.generation;
        let new_generation = previous_generation.saturating_add(1);

        match self.replace_generation(new_generation) {
            Ok(()) => Err(EvalError::ExecutionTimedOut {
                timeout_ms,
                previous_generation,
                new_generation,
            }),
            Err(error) => Err(EvalError::ExecutionRestartFailed {
                timeout_ms,
                previous_generation,
                attempted_generation: new_generation,
                message: error.to_string(),
            }),
        }
    }

    /// Replaces a generation terminated by an out-of-band host interruption.
    ///
    /// The evaluation future using the old generation must already have been
    /// dropped. On success, returns the fresh usable isolate generation.
    pub fn restart_after_interruption(&mut self) -> Result<u64, EvalError> {
        let previous_generation = self.generation;
        let new_generation = previous_generation.saturating_add(1);
        match self.replace_generation(new_generation) {
            Ok(()) => Ok(new_generation),
            Err(error) => Err(EvalError::InterruptionRestartFailed {
                previous_generation,
                attempted_generation: new_generation,
                message: error.to_string(),
            }),
        }
    }

    /// Replaces a generation left poisoned by an abandoned execution timeout.
    ///
    /// When an `eval_with` future is cancelled or unwinds after the watchdog
    /// fires but before replacement, `fired` stays latched. The next eval must
    /// install a fresh generation before using the kernel. Successful recovery
    /// is silent for the abandoned prior eval.
    fn recover_latched_execution_fire(&mut self) -> Result<(), EvalError> {
        let Some(watchdog) = self.execution_watchdog.as_ref() else {
            return Err(EvalError::Poisoned);
        };
        if !watchdog.fired().load(Ordering::SeqCst) {
            return Ok(());
        }

        let new_generation = self.generation.saturating_add(1);
        self.replace_generation(new_generation).map_err(|error| {
            // Old generation is already unusable; never hand it back.
            EvalError::internal(format!(
                "failed to replace isolate after abandoned execution timeout: {error}"
            ))
        })
    }

    fn replace_generation(&mut self, new_generation: u64) -> Result<(), IsolateBuildError> {
        // A terminated V8 isolate is never made reusable. Drop it before starting
        // the replacement so no stale async ops or heap state survive.
        self.interrupt.clear();
        if let Some(watchdog) = self.execution_watchdog.as_ref() {
            watchdog.retire_handle();
        }
        drop(self.kernel.take());
        self.console.clear();

        let mut kernel = Kernel::new(
            Arc::clone(&self.registry),
            self.console.clone(),
            new_generation,
        )?;
        let handle = kernel.isolate_handle();
        self.interrupt.replace(handle.clone());
        match self.execution_watchdog.as_ref() {
            Some(watchdog) => watchdog.replace_handle(handle),
            None => {
                self.execution_watchdog = Some(
                    ExecutionWatchdog::start(handle, self.execution_timeout).map_err(|error| {
                        IsolateBuildError::RuntimeInitialization {
                            message: error.to_string(),
                        }
                    })?,
                );
            }
        }
        self.kernel = Some(kernel);
        self.generation = new_generation;
        Ok(())
    }
}

impl Drop for Isolate {
    fn drop(&mut self) {
        self.interrupt.clear();
        if let Some(watchdog) = self.execution_watchdog.take() {
            watchdog.shutdown();
        }
        drop(self.kernel.take());
    }
}

struct EvaluationOutcome {
    result: Option<Result<EvalValue, EvalError>>,
    wall_fired: bool,
    execution_fired: bool,
}

struct KernelInner {
    // The inspector owns handles into V8 and must be dropped before the runtime.
    inspector: InspectorClient,
    runtime: JsRuntime,
}

impl Kernel {
    fn new(
        registry: Arc<Registry>,
        console: ConsoleBuffer,
        generation: u64,
    ) -> Result<Self, IsolateBuildError> {
        let inner = KernelInner::new(registry, console, generation)?;
        Ok(Self::park(inner))
    }
}

impl KernelInner {
    fn new(
        registry: Arc<Registry>,
        console: ConsoleBuffer,
        generation: u64,
    ) -> Result<Self, IsolateBuildError> {
        let mut runtime = JsRuntime::try_new(RuntimeOptions {
            extensions: vec![crate::bridge::extension(registry, console, generation)],
            extension_transpiler: Some(Rc::new(transpile::extension)),
            inspector: true,
            ..Default::default()
        })
        .map_err(|error| IsolateBuildError::RuntimeInitialization {
            message: error.to_string(),
        })?;

        let inspector = InspectorClient::attach(&mut runtime).map_err(|error| {
            IsolateBuildError::RuntimeInitialization {
                message: error.to_string(),
            }
        })?;

        Ok(Self { inspector, runtime })
    }

    fn isolate_handle(&mut self) -> deno_core::v8::IsolateHandle {
        self.runtime.v8_isolate().thread_safe_handle()
    }

    async fn evaluate(&mut self, source: &str, cell_id: u64) -> Result<EvalValue, EvalError> {
        let source = transpile::transpile(source, cell_id)?;
        let context_id = self.inspector.context_id();
        let evaluated = self
            .inspector
            .post(
                &mut self.runtime,
                "Runtime.evaluate",
                json!({
                  "expression": source,
                  "contextId": context_id,
                  "awaitPromise": true,
                  "replMode": true,
                  "returnByValue": false
                }),
            )
            .await
            .map_err(EvalError::internal)?;
        let remote = self.remote_result(evaluated, context_id).await?;
        let argument = call_argument(&remote)?;

        let resolved = self
            .inspector
            .post(
                &mut self.runtime,
                "Runtime.callFunctionOn",
                json!({
                  "functionDeclaration":
                    "async function(value) {
                       return globalThis.__lamResolveEvaluation(await value);
                     }",
                  "executionContextId": context_id,
                  "arguments": [argument],
                  "awaitPromise": true,
                  "returnByValue": true
                }),
            )
            .await
            .map_err(EvalError::internal)?;
        let remote = self.remote_result(resolved, context_id).await?;
        let resolution =
            remote
                .get("value")
                .cloned()
                .ok_or_else(|| EvalError::ResultNotSerializable {
                    message: remote
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("the isolate did not return a JSON value")
                        .to_owned(),
                })?;
        let resolution: JsonResolution =
            serde_json::from_value(resolution).map_err(EvalError::internal)?;

        match resolution {
            JsonResolution::Undefined => Ok(EvalValue::Undefined),
            JsonResolution::Json { value } => Ok(EvalValue::Json(value)),
            JsonResolution::NotSerializable { message } => {
                Err(EvalError::ResultNotSerializable { message })
            }
        }
    }

    async fn remote_result(
        &mut self,
        response: Value,
        context_id: u64,
    ) -> Result<Value, EvalError> {
        if !response["exceptionDetails"].is_null() {
            let details = response["exceptionDetails"].clone();
            if let Some(error) = self.classify_builtin_failure(&details, context_id).await? {
                return Err(EvalError::BuiltinFailure { error });
            }
            return Err(runtime_error(details));
        }

        let result = response["result"].clone();
        if result.is_null() {
            Err(EvalError::internal(format!(
                "CDP evaluation response had no remote object: {response}"
            )))
        } else {
            Ok(result)
        }
    }

    async fn classify_builtin_failure(
        &mut self,
        details: &Value,
        context_id: u64,
    ) -> Result<Option<Value>, EvalError> {
        let exception = &details["exception"];
        if exception.is_null() {
            return Ok(None);
        }
        let Ok(argument) = call_argument(exception) else {
            return Ok(None);
        };

        let response = self
            .inspector
            .post(
                &mut self.runtime,
                "Runtime.callFunctionOn",
                json!({
                  "functionDeclaration":
                    "function(value) { return globalThis.__lamResolveException(value); }",
                  "executionContextId": context_id,
                  "arguments": [argument],
                  "returnByValue": true
                }),
            )
            .await
            .map_err(EvalError::internal)?;
        if !response["exceptionDetails"].is_null() {
            return Ok(None);
        }

        let Some(value) = response["result"].get("value").cloned() else {
            return Ok(None);
        };
        let resolution: ExceptionResolution =
            serde_json::from_value(value).map_err(EvalError::internal)?;
        match resolution {
            ExceptionResolution::Runtime => Ok(None),
            ExceptionResolution::BuiltinFailure { error } => Ok(Some(error)),
            ExceptionResolution::NotSerializable { message } => Err(EvalError::internal(format!(
                "builtin failure stopped being JSON serializable: {message}"
            ))),
        }
    }
}

fn runtime_error(details: Value) -> EvalError {
    let message = details["exception"]["description"]
        .as_str()
        .or_else(|| details["exception"]["value"].as_str())
        .or_else(|| details["text"].as_str())
        .unwrap_or("JavaScript evaluation failed")
        .to_owned();
    EvalError::Runtime {
        exception: RuntimeException { message, details },
    }
}

fn call_argument(remote: &Value) -> Result<Value, EvalError> {
    if let Some(object_id) = remote["objectId"].as_str() {
        return Ok(json!({ "objectId": object_id }));
    }
    if let Some(value) = remote.get("unserializableValue")
        && !value.is_null()
    {
        return Ok(json!({ "unserializableValue": value }));
    }
    if let Some(value) = remote.get("value") {
        return Ok(json!({ "value": value }));
    }
    if remote["type"] == "undefined" {
        return Ok(json!({}));
    }

    Err(EvalError::ResultNotSerializable {
        message: remote["description"]
            .as_str()
            .unwrap_or("the result has no CDP value representation")
            .to_owned(),
    })
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JsonResolution {
    Undefined,
    Json { value: Value },
    NotSerializable { message: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExceptionResolution {
    Runtime,
    BuiltinFailure { error: Value },
    NotSerializable { message: String },
}

/// One-shot wall-clock deadline for a single evaluation.
///
/// Instantiated only when the effective default/request timeout is `Some`.
struct WallWatchdog {
    cancel: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<bool>>,
}

impl WallWatchdog {
    fn start(
        isolate: deno_core::v8::IsolateHandle,
        timeout: Duration,
    ) -> Result<(Self, oneshot::Receiver<()>), EvalError> {
        let (cancel, receiver) = mpsc::channel();
        let (timeout_sender, timeout_receiver) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("lam-isolate-wall-watchdog".to_owned())
            .spawn(move || match receiver.recv_timeout(timeout) {
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    isolate.terminate_execution();
                    let _ = timeout_sender.send(());
                    true
                }
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => false,
            })
            .map_err(EvalError::internal)?;

        Ok((
            Self {
                cancel: Some(cancel),
                thread: Some(thread),
            },
            timeout_receiver,
        ))
    }

    fn finish(mut self) -> Result<bool, EvalError> {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        self.thread
            .take()
            .expect("an unfinished wall watchdog owns its thread")
            .join()
            .map_err(|_| EvalError::internal("isolate wall watchdog panicked"))
    }
}

impl Drop for WallWatchdog {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Shared arm token used by [`ActivatedFuture`] to bound one continuous poll.
pub(super) struct ExecutionArm {
    shared: Arc<ExecutionWatchdogShared>,
}

impl ExecutionArm {
    /// Arms the generation watchdog immediately before a poll begins.
    pub(super) fn arm(&self) -> ExecutionArmedGuard {
        self.shared.arm();
        ExecutionArmedGuard {
            shared: Arc::clone(&self.shared),
        }
    }
}

/// Disarms the execution watchdog when a poll returns or is cancelled.
pub(super) struct ExecutionArmedGuard {
    shared: Arc<ExecutionWatchdogShared>,
}

impl Drop for ExecutionArmedGuard {
    fn drop(&mut self) {
        self.shared.disarm();
    }
}

struct ExecutionWatchdogShared {
    state: Mutex<ExecutionWatchdogState>,
    cv: Condvar,
    timeout: Duration,
    /// Set from the watchdog thread when continuous execution is terminated.
    ///
    /// Remains latched until a fresh isolate handle is installed so a cancelled
    /// eval that misses replacement still poisons the generation.
    fired: Arc<AtomicBool>,
}

struct ExecutionWatchdogState {
    handle: Option<deno_core::v8::IsolateHandle>,
    /// Deadline of the currently armed poll, if any.
    deadline: Option<Instant>,
    /// Monotonic token identifying the active arm interval.
    ///
    /// Bumped on every arm and disarm so a terminate that loses the race with
    /// poll completion cannot mark a finished poll as timed out or leave the
    /// isolate terminated.
    arm_generation: u64,
    /// Oneshot sender that wakes the active `eval_with` when execution fires.
    fire_notify: Option<oneshot::Sender<()>>,
    /// Monotonic token identifying the registered fire-notify receiver.
    ///
    /// Bumped on register/clear so a dropped eval cannot leave a sender that
    /// later delivers into a different evaluation.
    notify_generation: u64,
    shut_down: bool,
}

/// Unregisters a fire-notify receiver when its evaluation ends or is cancelled.
struct FireNotifyGuard {
    shared: Arc<ExecutionWatchdogShared>,
    generation: u64,
}

impl Drop for FireNotifyGuard {
    fn drop(&mut self) {
        self.shared.clear_fire_notify(self.generation);
    }
}

impl ExecutionWatchdogShared {
    fn arm(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.shut_down || state.handle.is_none() {
            return;
        }
        state.arm_generation = state.arm_generation.wrapping_add(1);
        state.deadline = Some(Instant::now() + self.timeout);
        self.cv.notify_one();
    }

    fn disarm(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.arm_generation = state.arm_generation.wrapping_add(1);
        state.deadline = None;
        self.cv.notify_one();
    }

    fn register_fire_notify(self: &Arc<Self>) -> (oneshot::Receiver<()>, FireNotifyGuard) {
        let (sender, receiver) = oneshot::channel();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = state.notify_generation.wrapping_add(1);
        state.notify_generation = generation;
        state.fire_notify = Some(sender);
        (
            receiver,
            FireNotifyGuard {
                shared: Arc::clone(self),
                generation,
            },
        )
    }

    fn clear_fire_notify(&self, generation: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.notify_generation == generation {
            state.fire_notify = None;
            state.notify_generation = state.notify_generation.wrapping_add(1);
        }
    }

    fn take_fire_notify(state: &mut ExecutionWatchdogState) -> Option<oneshot::Sender<()>> {
        let sender = state.fire_notify.take();
        if sender.is_some() {
            state.notify_generation = state.notify_generation.wrapping_add(1);
        }
        sender
    }

    fn replace_handle(&self, handle: deno_core::v8::IsolateHandle) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.handle = Some(handle);
        state.deadline = None;
        state.arm_generation = state.arm_generation.wrapping_add(1);
        state.fire_notify = None;
        state.notify_generation = state.notify_generation.wrapping_add(1);
        // Fired resets only when a fresh handle is installed.
        self.fired.store(false, Ordering::SeqCst);
        self.cv.notify_one();
    }

    fn retire_handle(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.handle = None;
        state.deadline = None;
        state.arm_generation = state.arm_generation.wrapping_add(1);
        state.fire_notify = None;
        state.notify_generation = state.notify_generation.wrapping_add(1);
        self.cv.notify_one();
    }

    fn shutdown(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.shut_down = true;
        state.handle = None;
        state.deadline = None;
        state.arm_generation = state.arm_generation.wrapping_add(1);
        state.fire_notify = None;
        state.notify_generation = state.notify_generation.wrapping_add(1);
        self.cv.notify_one();
    }
}

/// Persistent per-generation watchdog that bounds continuous poll time.
///
/// One OS thread is retained for the isolate lifetime and reused across polls
/// and generation replacements.
struct ExecutionWatchdog {
    shared: Arc<ExecutionWatchdogShared>,
    arm: Arc<ExecutionArm>,
    thread: Option<JoinHandle<()>>,
}

impl ExecutionWatchdog {
    fn start(handle: deno_core::v8::IsolateHandle, timeout: Duration) -> Result<Self, EvalError> {
        let fired = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(ExecutionWatchdogShared {
            state: Mutex::new(ExecutionWatchdogState {
                handle: Some(handle),
                deadline: None,
                arm_generation: 0,
                fire_notify: None,
                notify_generation: 0,
                shut_down: false,
            }),
            cv: Condvar::new(),
            timeout,
            fired,
        });
        let arm = Arc::new(ExecutionArm {
            shared: Arc::clone(&shared),
        });
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("lam-isolate-execution-watchdog".to_owned())
            .spawn(move || execution_watchdog_loop(thread_shared))
            .map_err(EvalError::internal)?;

        Ok(Self {
            shared,
            arm,
            thread: Some(thread),
        })
    }

    fn arm(&self) -> &Arc<ExecutionArm> {
        &self.arm
    }

    fn fired(&self) -> &Arc<AtomicBool> {
        &self.shared.fired
    }

    fn register_fire_notify(&self) -> (oneshot::Receiver<()>, FireNotifyGuard) {
        self.shared.register_fire_notify()
    }

    fn replace_handle(&self, handle: deno_core::v8::IsolateHandle) {
        self.shared.replace_handle(handle);
    }

    fn retire_handle(&self) {
        self.shared.retire_handle();
    }

    fn shutdown(mut self) {
        self.shared.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ExecutionWatchdog {
    fn drop(&mut self) {
        self.shared.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn execution_watchdog_loop(shared: Arc<ExecutionWatchdogShared>) {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if state.shut_down {
            return;
        }

        let Some(deadline) = state.deadline else {
            state = shared
                .cv
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            continue;
        };
        let armed_generation = state.arm_generation;

        let now = Instant::now();
        if deadline > now {
            let wait = deadline.saturating_duration_since(now);
            let (guard, result) = shared
                .cv
                .wait_timeout(state, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = guard;
            if !result.timed_out() {
                // Arming state changed before the deadline; re-evaluate.
                continue;
            }
        }

        // Re-check under the lock so a disarm that races the timeout cannot
        // terminate a later poll or an idle isolate.
        if state.shut_down {
            return;
        }
        if state.arm_generation != armed_generation {
            continue;
        }
        let Some(current_deadline) = state.deadline else {
            continue;
        };
        if current_deadline > Instant::now() {
            continue;
        }
        let Some(handle) = state.handle.clone() else {
            state.deadline = None;
            continue;
        };

        // Latch fire and take the eval notify before leaving the lock. If the
        // poll returns in the same window, eval_with still classifies the cell
        // as an execution timeout and replaces the generation. The oneshot
        // independently wakes eval_with when the inner future stays Pending.
        state.deadline = None;
        shared.fired.store(true, Ordering::SeqCst);
        let fire_notify = ExecutionWatchdogShared::take_fire_notify(&mut state);
        drop(state);
        handle.terminate_execution();
        if let Some(notify) = fire_notify {
            let _ = notify.send(());
        }

        state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::Never;
    use std::future::poll_fn;
    use std::sync::OnceLock;
    use std::task::Poll;

    /// Installs the watchdog fire latch for a probe builtin before the eval arms.
    ///
    /// Unit tests share this exact `Arc<AtomicBool>` with the private
    /// [`ExecutionWatchdog::fired`] latch so the builtin can observe fire
    /// deterministically instead of sleeping past a timing assumption.
    fn install_watchdog_fire_slot(slot: &Arc<OnceLock<Arc<AtomicBool>>>, isolate: &Isolate) {
        let fired = Arc::clone(
            isolate
                .execution_watchdog
                .as_ref()
                .expect("execution watchdog is installed with the isolate")
                .fired(),
        );
        slot.set(fired)
            .unwrap_or_else(|_| panic!("watchdog fire latch slot already installed"));
    }

    /// Blocks the current poll until the shared watchdog fire latch is true.
    ///
    /// The deadline is hang protection only; causal order comes from observing
    /// the latch itself, not from waiting a fixed duration.
    fn wait_until_watchdog_fired(fired: &AtomicBool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !fired.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                panic!("execution watchdog never latched fired during the armed poll");
            }
            std::thread::yield_now();
        }
    }

    fn json_result(value: EvalValue) -> Value {
        match value {
            EvalValue::Json(value) => value,
            EvalValue::Undefined => panic!("expected a JSON evaluation result"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn execution_watchdog_wakes_eval_when_poll_stays_pending() {
        let fire_slot: Arc<OnceLock<Arc<AtomicBool>>> = Arc::new(OnceLock::new());
        let probe_slot = Arc::clone(&fire_slot);
        let busy_namespace = Namespace::new("lam.wait", "Execution wakeup probe.").function(
            "busy_then_pending",
            "Waits for the watchdog fire latch, then stays Pending forever.",
            move |_context, (): ()| {
                let probe_slot = Arc::clone(&probe_slot);
                async move {
                    poll_fn(move |_cx| -> Poll<()> {
                        let fired = probe_slot
                            .get()
                            .expect("test must install the watchdog fire latch before eval");
                        wait_until_watchdog_fired(fired);
                        // Stay Pending so only the independent fire notify can
                        // complete the outer eval_with select.
                        Poll::Pending
                    })
                    .await;
                    Ok::<(), Never>(())
                }
            },
        );

        let mut isolate = Isolate::builder()
            .namespace(busy_namespace)
            .execution_timeout(Duration::from_millis(15))
            .build()
            .await
            .expect("test isolate should initialize");
        install_watchdog_fire_slot(&fire_slot, &isolate);
        let previous_generation = isolate.generation();

        let error = tokio::time::timeout(
            Duration::from_secs(2),
            isolate.eval("await lam.wait.busy_then_pending()"),
        )
        .await
        .expect("execution fire must wake eval_with without waiting on the inner poll")
        .expect_err("a permanently-pending poll that observed fire must time out");

        assert_eq!(
            error,
            EvalError::ExecutionTimedOut {
                timeout_ms: 15,
                previous_generation,
                new_generation: previous_generation + 1,
            }
        );
        assert_eq!(isolate.generation(), previous_generation + 1);

        let recovered = isolate
            .eval("lam.result(7 * 6)")
            .await
            .expect("replacement generation must be usable after execution timeout");
        assert_eq!(json_result(recovered.result), json!(42));
        assert_eq!(isolate.generation(), previous_generation + 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn abandoned_execution_fire_poisons_until_next_eval_replaces() {
        let mut isolate = Isolate::builder()
            .execution_timeout(Duration::from_millis(15))
            .build()
            .await
            .expect("test isolate should initialize");
        let previous_generation = isolate.generation();

        // Durable heap evidence in the current generation. After silent
        // preflight recovery the replacement heap must not retain it.
        isolate
            .eval("globalThis.__lamWatchdogPoisonMarker = 'generation-poisoned'")
            .await
            .expect("marker install must succeed on a healthy generation");
        let marker_present = isolate
            .eval("lam.result(typeof globalThis.__lamWatchdogPoisonMarker)")
            .await
            .expect("marker probe must succeed before arming");
        assert_eq!(
            json_result(marker_present.result),
            json!("string"),
            "marker must exist on the current generation before the abandoned fire"
        );
        assert_eq!(isolate.generation(), previous_generation);

        // Model a fired execution watchdog whose owning eval was abandoned:
        // arm the real generation watchdog, wait on its private fired latch,
        // then drop the armed guard without driving eval_with classification.
        {
            let watchdog = isolate
                .execution_watchdog
                .as_ref()
                .expect("execution watchdog is installed with the isolate");
            let fired = Arc::clone(watchdog.fired());
            let armed = watchdog.arm().arm();
            wait_until_watchdog_fired(fired.as_ref());
            assert!(
                fired.load(Ordering::SeqCst),
                "direct arm must observe the real watchdog fire latch"
            );
            drop(armed);
        }

        assert!(
            isolate
                .execution_watchdog
                .as_ref()
                .expect("execution watchdog remains installed after the abandoned arm")
                .fired()
                .load(Ordering::SeqCst),
            "fired state must remain latched across the abandoned armed poll"
        );
        assert_eq!(
            isolate.generation(),
            previous_generation,
            "abandoning before classification must not itself install the replacement"
        );

        // Next bounded eval silently recover_latched_execution_fire, observes
        // the old marker absent, succeeds, and increments generation once.
        let recovered = tokio::time::timeout(
            Duration::from_secs(2),
            isolate.eval(
                "lam.result({ markerType: typeof globalThis.__lamWatchdogPoisonMarker, ready: true })",
            ),
        )
        .await
        .expect("next eval must not hang on a fired/poisoned generation")
        .expect("next eval must replace the terminated generation and succeed");
        assert_eq!(
            json_result(recovered.result),
            json!({ "markerType": "undefined", "ready": true }),
            "replacement heap must not retain the abandoned generation marker"
        );
        assert_eq!(isolate.generation(), previous_generation + 1);
    }
}
