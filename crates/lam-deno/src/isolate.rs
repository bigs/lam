use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use deno_core::futures::FutureExt;
use deno_core::futures::channel::oneshot;
use deno_core::futures::future::{Either, select};
use deno_core::{JsRuntime, RuntimeOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bridge::ConsoleBuffer;
use crate::builtin::{Namespace, Registry};
use crate::error::{EvalError, IsolateBuildError, RuntimeException};
use crate::inspector::InspectorClient;
use crate::transpile;

pub use crate::bridge::{ConsoleEntry, ConsoleLevel};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_TIMEOUT: Duration = Duration::from_secs(30);

thread_local! {
    static THREAD_HAS_ISOLATE: Cell<bool> = const { Cell::new(false) };
}

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
    /// Uses a cell-specific timeout, still bounded by the host maximum.
    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// Configures and constructs a persistent TypeScript isolate.
pub struct IsolateBuilder {
    namespaces: Vec<Namespace>,
    default_timeout: Duration,
    max_timeout: Duration,
    capture_console: bool,
}

impl Default for IsolateBuilder {
    fn default() -> Self {
        Self {
            namespaces: Vec::new(),
            default_timeout: DEFAULT_TIMEOUT,
            max_timeout: DEFAULT_MAX_TIMEOUT,
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

    /// Sets the timeout used by cells without an explicit override.
    #[must_use]
    pub const fn default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    /// Sets the hard upper bound for all cell timeouts.
    #[must_use]
    pub const fn max_timeout(mut self, timeout: Duration) -> Self {
        self.max_timeout = timeout;
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
        if self.default_timeout.is_zero() || self.max_timeout.is_zero() {
            return Err(IsolateBuildError::InvalidTimeout);
        }

        let registry = Arc::new(Registry::build(self.namespaces)?);
        let thread_permit = ThreadPermit::acquire()?;
        let console = ConsoleBuffer::new(self.capture_console);
        let generation = 1;
        let mut kernel = Kernel::new(Arc::clone(&registry), console.clone(), generation).await?;
        let interrupt = IsolateInterrupt::new(kernel.isolate_handle());

        Ok(Isolate {
            kernel: Some(kernel),
            _thread_permit: thread_permit,
            registry,
            console,
            generation,
            next_cell_id: 1,
            default_timeout: self.default_timeout.min(self.max_timeout),
            max_timeout: self.max_timeout,
            interrupt,
        })
    }
}

/// Thread-safe control which interrupts the currently installed V8 isolate.
///
/// Interrupting execution poisons that isolate generation. Callers must stop
/// using and drop the associated [`Isolate`]; Lam's actor runtime does this as
/// part of its forceful abort path.
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

/// A persistent, serially evaluated TypeScript isolate.
///
/// `Isolate` deliberately does not implement `Send`: an actor scheduler should
/// keep a resident isolate on one local runtime thread and move only durable
/// actor state between scheduler slots.
pub struct Isolate {
    kernel: Option<Kernel>,
    // Declared after `kernel` so V8 is dropped before the thread is released.
    _thread_permit: ThreadPermit,
    registry: Arc<Registry>,
    console: ConsoleBuffer,
    generation: u64,
    next_cell_id: u64,
    default_timeout: Duration,
    max_timeout: Duration,
    interrupt: IsolateInterrupt,
}

struct ThreadPermit;

impl ThreadPermit {
    fn acquire() -> Result<Self, IsolateBuildError> {
        THREAD_HAS_ISOLATE.with(|occupied| {
            if occupied.get() {
                Err(IsolateBuildError::ThreadAlreadyOwnsIsolate)
            } else {
                occupied.set(true);
                Ok(Self)
            }
        })
    }
}

impl Drop for ThreadPermit {
    fn drop(&mut self) {
        THREAD_HAS_ISOLATE.with(|occupied| {
            debug_assert!(occupied.get(), "Lam isolate thread permit was not held");
            occupied.set(false);
        });
    }
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

    /// Evaluates a TypeScript cell with the host's default timeout.
    pub async fn eval(&mut self, source: &str) -> Result<EvalOutput, EvalError> {
        self.eval_with(source, EvalOptions::default()).await
    }

    /// Evaluates a TypeScript cell with explicit per-cell options.
    ///
    /// Cells execute serially because this method requires exclusive access.
    /// After a timeout, the poisoned generation is dropped and replaced before
    /// the timeout error is returned.
    pub async fn eval_with(
        &mut self,
        source: &str,
        options: EvalOptions,
    ) -> Result<EvalOutput, EvalError> {
        let timeout = options
            .timeout
            .unwrap_or(self.default_timeout)
            .min(self.max_timeout);
        let timeout_ms = duration_millis(timeout);
        let cell_id = self.next_cell_id;
        self.next_cell_id = self.next_cell_id.saturating_add(1);
        self.console.clear();

        let (result, timed_out) = {
            let kernel = self.kernel.as_mut().ok_or(EvalError::Poisoned)?;
            let (watchdog, timeout_signal) = Watchdog::start(kernel.isolate_handle(), timeout)?;
            let evaluation = kernel.evaluate(source, cell_id).fuse();
            let timeout = async move {
                let _ = timeout_signal.await;
            }
            .fuse();
            deno_core::futures::pin_mut!(evaluation, timeout);

            let result = match select(evaluation, timeout).await {
                Either::Left((result, _)) => Some(result),
                Either::Right(((), _pending_evaluation)) => None,
            };
            let timed_out = watchdog.finish()?;
            (result, timed_out)
        };

        if timed_out {
            return self.restart_after_timeout(timeout_ms).await;
        }

        let result = result
            .ok_or_else(|| EvalError::internal("watchdog ended evaluation without firing"))?;
        result.map(|result| EvalOutput {
            result,
            logs: self.console.take(),
        })
    }

    async fn restart_after_timeout(&mut self, timeout_ms: u64) -> Result<EvalOutput, EvalError> {
        let previous_generation = self.generation;
        let new_generation = previous_generation.saturating_add(1);

        // A terminated V8 isolate is never made reusable. Drop it before starting
        // the replacement so no stale async ops or heap state survive.
        self.interrupt.clear();
        drop(self.kernel.take());
        self.console.clear();

        match Kernel::new(
            Arc::clone(&self.registry),
            self.console.clone(),
            new_generation,
        )
        .await
        {
            Ok(mut kernel) => {
                self.interrupt.replace(kernel.isolate_handle());
                self.kernel = Some(kernel);
                self.generation = new_generation;
                Err(EvalError::TimedOut {
                    timeout_ms,
                    previous_generation,
                    new_generation,
                })
            }
            Err(error) => Err(EvalError::RestartFailed {
                timeout_ms,
                previous_generation,
                attempted_generation: new_generation,
                message: error.to_string(),
            }),
        }
    }
}

impl Drop for Isolate {
    fn drop(&mut self) {
        self.interrupt.clear();
    }
}

struct Kernel {
    // The inspector owns handles into V8 and must be dropped before the runtime.
    inspector: InspectorClient,
    runtime: JsRuntime,
}

impl Kernel {
    async fn new(
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

        let inspector = InspectorClient::attach(&mut runtime)
            .await
            .map_err(|error| IsolateBuildError::RuntimeInitialization {
                message: error.to_string(),
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

struct Watchdog {
    cancel: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<bool>>,
}

impl Watchdog {
    fn start(
        isolate: deno_core::v8::IsolateHandle,
        timeout: Duration,
    ) -> Result<(Self, oneshot::Receiver<()>), EvalError> {
        let (cancel, receiver) = mpsc::channel();
        let (timeout_sender, timeout_receiver) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("lam-isolate-watchdog".to_owned())
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
            .expect("an unfinished watchdog owns its thread")
            .join()
            .map_err(|_| EvalError::internal("isolate watchdog panicked"))
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
