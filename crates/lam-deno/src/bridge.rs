use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{Extension, ExtensionFileSource, OpState, op2};
use deno_error::JsErrorBox;
use serde::Serialize;
use serde_json::Value;

use crate::builtin::{CallResult, DirQuery, NamespaceDescriptor, OperationContext, Registry};
use crate::transpile;

/// A console method captured during one cell evaluation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConsoleLevel {
    /// Diagnostic output.
    Debug,
    /// Ordinary log output.
    Log,
    /// Informational output.
    Info,
    /// Warning output.
    Warn,
    /// Error output.
    Error,
}

/// One structured console entry captured during an evaluation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleEntry {
    /// Console method used by the program.
    pub level: ConsoleLevel,
    /// JSON arguments in their original order, with textual fallbacks for
    /// values which JSON cannot represent.
    pub args: Vec<Value>,
}

#[derive(Clone)]
pub(crate) struct ConsoleBuffer {
    capture: bool,
    entries: Rc<RefCell<Vec<ConsoleEntry>>>,
}

impl ConsoleBuffer {
    pub(crate) fn new(capture: bool) -> Self {
        Self {
            capture,
            entries: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub(crate) fn clear(&self) {
        self.entries.borrow_mut().clear();
    }

    pub(crate) fn take(&self) -> Vec<ConsoleEntry> {
        std::mem::take(&mut *self.entries.borrow_mut())
    }

    fn push(&self, entry: ConsoleEntry) {
        if self.capture {
            self.entries.borrow_mut().push(entry);
        }
    }
}

#[derive(Clone, Copy)]
struct Generation(u64);

#[op2]
#[serde]
async fn op_lam_call(
    state: Rc<RefCell<OpState>>,
    #[string] namespace: String,
    #[string] function: String,
    #[serde] input: serde_json::Value,
) -> Result<CallResult, JsErrorBox> {
    let (registry, generation) = {
        let state = state.borrow();
        (
            state.borrow::<Arc<Registry>>().clone(),
            *state.borrow::<Generation>(),
        )
    };

    registry
        .call(
            namespace,
            function,
            OperationContext::new(generation.0),
            input,
        )
        .await
        .map_err(|error| JsErrorBox::generic(error.to_string()))
}

#[op2]
#[serde]
fn op_lam_manifest(
    state: &mut OpState,
    #[serde] query: Option<DirQuery>,
) -> Vec<NamespaceDescriptor> {
    state.borrow::<Arc<Registry>>().manifest(query.as_ref())
}

#[op2]
fn op_lam_console(
    state: &mut OpState,
    #[string] level: String,
    #[serde] args: Vec<Value>,
) -> Result<(), JsErrorBox> {
    let level = match level.as_str() {
        "debug" => ConsoleLevel::Debug,
        "log" => ConsoleLevel::Log,
        "info" => ConsoleLevel::Info,
        "warn" => ConsoleLevel::Warn,
        "error" => ConsoleLevel::Error,
        _ => return Err(JsErrorBox::type_error("invalid console level")),
    };
    state
        .borrow::<ConsoleBuffer>()
        .push(ConsoleEntry { level, args });
    Ok(())
}

fn embed_runtime_esm(extension: &mut Extension) {
    // Embed the TypeScript in the final binary. The ordinary `esm = [...]`
    // file form in deno_core 0.409 reads from the source tree at runtime unless
    // a snapshot build consumes it.
    extension.esm_files = Cow::Owned(vec![ExtensionFileSource::new(
        transpile::RUNTIME_ESM_SPECIFIER,
        deno_core::ascii_str_include!("runtime.ts"),
    )]);
}

deno_core::extension!(
    lam_runtime,
    ops = [op_lam_call, op_lam_manifest, op_lam_console],
    esm_entry_point = transpile::RUNTIME_ESM_SPECIFIER,
    options = {
        registry: Arc<Registry>,
        console: ConsoleBuffer,
        generation: Generation,
    },
    state = |state, options| {
        state.put(options.registry);
        state.put(options.console);
        state.put(options.generation);
    },
    customizer = embed_runtime_esm,
);

pub(crate) fn extension(
    registry: Arc<Registry>,
    console: ConsoleBuffer,
    generation: u64,
) -> Extension {
    lam_runtime::init(registry, console, Generation(generation))
}
