use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::{JsonSchema, schema_for};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::IsolateBuildError;

/// Model and reasoning effort currently selected by the embedding.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorySelection {
    /// Inference provider name used by model-selection APIs.
    pub provider: String,
    /// Provider-specific model identifier.
    pub model: String,
    /// Embedding-defined reasoning effort, when the embedding exposes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Cloneable synchronous source for the selection reported by `lam.dir`.
#[derive(Clone)]
pub struct DirectorySelectionSource {
    current: Arc<dyn Fn() -> DirectorySelection + Send + Sync>,
}

impl DirectorySelectionSource {
    /// Creates a source backed by an embedding-provided synchronous callback.
    pub fn new(current: impl Fn() -> DirectorySelection + Send + Sync + 'static) -> Self {
        Self {
            current: Arc::new(current),
        }
    }

    pub(crate) fn current(&self) -> DirectorySelection {
        (self.current)()
    }
}

type HandlerFuture = Pin<Box<dyn Future<Output = Result<CallResult, InvocationError>> + Send>>;

/// The impossible error type for builtins which cannot fail.
#[derive(Debug, Serialize, JsonSchema)]
pub enum Never {}

/// Context supplied by Lam to every Rust builtin invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationContext {
    isolate_generation: u64,
}

impl OperationContext {
    pub(crate) const fn new(isolate_generation: u64) -> Self {
        Self { isolate_generation }
    }

    /// Returns the generation of the isolate which initiated this operation.
    #[must_use]
    pub const fn isolate_generation(self) -> u64 {
        self.isolate_generation
    }
}

/// A query accepted by the synchronous `lam.dir` builtin.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DirQuery {
    /// Optional namespace or fully-qualified function path to inspect.
    path: Option<String>,
}

/// Discoverable metadata for one Rust-backed TypeScript function.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FunctionDescriptor {
    /// Function name relative to its namespace.
    pub(crate) name: String,
    /// Human-readable documentation.
    pub(crate) docs: String,
    /// JSON Schema inferred from the Rust input type.
    pub(crate) input_schema: Value,
    /// JSON Schema inferred from the Rust success type.
    pub(crate) output_schema: Value,
    /// JSON Schema inferred from the Rust typed-error type.
    error_schema: Value,
}

/// Discoverable metadata for a namespace and its functions.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NamespaceDescriptor {
    /// Fully-qualified JavaScript path, such as `lam.math` or `acme.search`.
    pub(crate) path: String,
    /// Human-readable namespace documentation.
    docs: String,
    /// Functions registered directly beneath this namespace.
    pub(crate) functions: Vec<FunctionDescriptor>,
    /// Current model selection. Present only on the kernel `lam` descriptor.
    #[serde(skip_serializing_if = "Option::is_none")]
    current_selection: Option<DirectorySelection>,
}

/// A typed namespace which can be installed into an isolate.
#[derive(Clone)]
pub struct Namespace {
    path: String,
    docs: String,
    functions: Vec<Arc<dyn ErasedBuiltin>>,
}

impl Namespace {
    /// Creates an empty namespace at a fully-qualified JavaScript path.
    #[must_use]
    pub fn new(path: impl Into<String>, docs: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            docs: docs.into(),
            functions: Vec::new(),
        }
    }

    /// Returns the fully-qualified JavaScript path for capability selection.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Adds a typed async Rust function to this namespace.
    ///
    /// Input, output, and typed-error schemas are inferred from `I`, `O`, and
    /// `E`. The handler is invoked only after its single structured input has
    /// successfully deserialized.
    #[must_use]
    pub fn function<I, O, E, F, Fut>(
        mut self,
        name: impl Into<String>,
        docs: impl Into<String>,
        handler: F,
    ) -> Self
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: Serialize + JsonSchema + Send + 'static,
        E: Serialize + JsonSchema + Send + 'static,
        F: Fn(OperationContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, E>> + Send + 'static,
    {
        self.functions.push(Arc::new(TypedBuiltin {
            descriptor: FunctionDescriptor {
                name: name.into(),
                docs: docs.into(),
                input_schema: schema_value::<I>(),
                output_schema: schema_value::<O>(),
                error_schema: schema_value::<E>(),
            },
            handler,
            marker: std::marker::PhantomData,
        }));
        self
    }
}

trait ErasedBuiltin: Send + Sync {
    fn descriptor(&self) -> &FunctionDescriptor;
    fn call(&self, context: OperationContext, input: Value) -> HandlerFuture;
}

struct TypedBuiltin<I, O, E, F> {
    descriptor: FunctionDescriptor,
    handler: F,
    marker: std::marker::PhantomData<fn(I) -> Result<O, E>>,
}

impl<I, O, E, F, Fut> ErasedBuiltin for TypedBuiltin<I, O, E, F>
where
    I: DeserializeOwned + JsonSchema + Send + 'static,
    O: Serialize + JsonSchema + Send + 'static,
    E: Serialize + JsonSchema + Send + 'static,
    F: Fn(OperationContext, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, E>> + Send + 'static,
{
    fn descriptor(&self) -> &FunctionDescriptor {
        &self.descriptor
    }

    fn call(&self, context: OperationContext, input: Value) -> HandlerFuture {
        let input = serde_json::from_value(input).map_err(|error| InvocationError::InvalidInput {
            message: error.to_string(),
        });

        let future = input.map(|input| (self.handler)(context, input));
        Box::pin(async move {
            let result = future?.await;
            match result {
                Ok(output) => Ok(CallResult::success(output)?),
                Err(error) => Ok(CallResult::failure(error)?),
            }
        })
    }
}

fn schema_value<T: JsonSchema>() -> Value {
    serde_json::to_value(schema_for!(T)).expect("schemars schemas are always JSON serializable")
}

#[derive(Debug, Serialize)]
pub(crate) struct CallResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

impl CallResult {
    fn success(output: impl Serialize) -> Result<Self, InvocationError> {
        Ok(Self {
            ok: true,
            value: Some(serde_json::to_value(output).map_err(|error| {
                InvocationError::InvalidOutput {
                    message: error.to_string(),
                }
            })?),
            error: None,
        })
    }

    fn failure(error: impl Serialize) -> Result<Self, InvocationError> {
        Ok(Self {
            ok: false,
            value: None,
            error: Some(serde_json::to_value(error).map_err(|error| {
                InvocationError::InvalidError {
                    message: error.to_string(),
                }
            })?),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum InvocationError {
    #[error("builtin input did not match its Rust type: {message}")]
    InvalidInput { message: String },
    #[error("builtin output could not be serialized: {message}")]
    InvalidOutput { message: String },
    #[error("builtin error could not be serialized: {message}")]
    InvalidError { message: String },
    #[error("unknown builtin `{namespace}.{function}`")]
    UnknownBuiltin { namespace: String, function: String },
}

pub(crate) struct Registry {
    namespaces: Vec<NamespaceDescriptor>,
    functions: BTreeMap<(String, String), Arc<dyn ErasedBuiltin>>,
    selection: Option<DirectorySelectionSource>,
}

impl Registry {
    pub(crate) fn build(
        namespaces: Vec<Namespace>,
        selection: Option<DirectorySelectionSource>,
    ) -> Result<Self, IsolateBuildError> {
        let mut descriptors = vec![lam_namespace_descriptor()];
        let mut functions = BTreeMap::new();
        let mut namespace_paths = BTreeSet::from(["lam".to_owned()]);
        let mut function_paths = BTreeSet::from(["lam.dir".to_owned(), "lam.result".to_owned()]);

        for namespace in namespaces {
            validate_path("namespace", &namespace.path)?;
            if !namespace_paths.insert(namespace.path.clone()) {
                return Err(IsolateBuildError::DuplicateNamespace {
                    path: namespace.path,
                });
            }

            let mut function_names = BTreeSet::new();
            let mut function_descriptors = Vec::new();
            for function in namespace.functions {
                let descriptor = function.descriptor().clone();
                validate_segment("function", &descriptor.name)?;
                if !function_names.insert(descriptor.name.clone()) {
                    return Err(IsolateBuildError::DuplicateFunction {
                        path: format!("{}.{}", namespace.path, descriptor.name),
                    });
                }
                let function_path = format!("{}.{}", namespace.path, descriptor.name);
                function_paths.insert(function_path);
                functions.insert((namespace.path.clone(), descriptor.name.clone()), function);
                function_descriptors.push(descriptor);
            }
            function_descriptors.sort_by(|left, right| left.name.cmp(&right.name));
            descriptors.push(NamespaceDescriptor {
                path: namespace.path,
                docs: namespace.docs,
                functions: function_descriptors,
                current_selection: None,
            });
        }

        for function_path in function_paths {
            if let Some(namespace_path) = namespace_paths.iter().find(|namespace_path| {
                namespace_path.as_str() == function_path
                    || namespace_path.starts_with(&format!("{function_path}."))
            }) {
                return Err(IsolateBuildError::NamespaceFunctionConflict {
                    namespace: namespace_path.clone(),
                    function: function_path,
                });
            }
        }

        descriptors.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(Self {
            namespaces: descriptors,
            functions,
            selection,
        })
    }

    pub(crate) fn manifest(&self, query: Option<&DirQuery>) -> Vec<NamespaceDescriptor> {
        let path = query
            .and_then(|query| query.path.as_deref())
            .map(str::trim)
            .filter(|path| !path.is_empty());
        let mut namespaces = self
            .namespaces
            .iter()
            .filter_map(|namespace| {
                let Some(path) = path else {
                    return Some(namespace.clone());
                };
                if namespace.path == path || namespace.path.starts_with(&format!("{path}.")) {
                    return Some(namespace.clone());
                }

                let mut matched = namespace.clone();
                matched
                    .functions
                    .retain(|function| format!("{}.{}", namespace.path, function.name) == path);
                (!matched.functions.is_empty()).then_some(matched)
            })
            .collect::<Vec<_>>();
        if let Some(current) = self
            .selection
            .as_ref()
            .map(DirectorySelectionSource::current)
            && let Some(kernel) = namespaces
                .iter_mut()
                .find(|namespace| namespace.path == "lam")
        {
            kernel.current_selection = Some(current);
        }
        namespaces
    }

    pub(crate) fn prompt_inventory(&self) -> String {
        crate::prompt::render_inventory(&self.namespaces)
    }

    pub(crate) async fn call(
        &self,
        namespace: String,
        function: String,
        context: OperationContext,
        input: Value,
    ) -> Result<CallResult, InvocationError> {
        let handler = self
            .functions
            .get(&(namespace.clone(), function.clone()))
            .ok_or(InvocationError::UnknownBuiltin {
                namespace,
                function,
            })?;
        handler.call(context, input).await
    }
}

fn lam_namespace_descriptor() -> NamespaceDescriptor {
    NamespaceDescriptor {
        path: "lam".to_owned(),
        docs: "Lam kernel utilities.".to_owned(),
        current_selection: None,
        functions: vec![
            FunctionDescriptor {
                name: "dir".to_owned(),
                docs: "Discover installed namespaces, functions, and inferred schemas. When the embedding exposes a current model selection, the unfiltered result and `lam` path query include it as `currentSelection` on the `lam` namespace descriptor."
                    .to_owned(),
                input_schema: schema_value::<Option<DirQuery>>(),
                output_schema: schema_value::<Vec<NamespaceDescriptor>>(),
                error_schema: schema_value::<Never>(),
            },
            FunctionDescriptor {
                name: "result".to_owned(),
                docs: "Returns a JSON-serializable value unchanged, making the eval's final result explicit. Use it as the last expression."
                    .to_owned(),
                input_schema: schema_value::<Value>(),
                output_schema: schema_value::<Value>(),
                error_schema: schema_value::<Never>(),
            },
        ],
    }
}

fn validate_path(kind: &'static str, path: &str) -> Result<(), IsolateBuildError> {
    if path.is_empty() {
        return Err(IsolateBuildError::InvalidName {
            kind,
            name: path.to_owned(),
        });
    }
    for segment in path.split('.') {
        validate_segment(kind, segment)?;
    }
    Ok(())
}

fn validate_segment(kind: &'static str, segment: &str) -> Result<(), IsolateBuildError> {
    let mut characters = segment.chars();
    let valid_start = characters.next().is_some_and(|character| {
        character == '_' || character == '$' || character.is_ascii_alphabetic()
    });
    let valid_rest = characters
        .all(|character| character == '_' || character == '$' || character.is_ascii_alphanumeric());

    if valid_start && valid_rest {
        Ok(())
    } else {
        Err(IsolateBuildError::InvalidName {
            kind,
            name: segment.to_owned(),
        })
    }
}
