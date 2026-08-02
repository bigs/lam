use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use lam::{ActorBuilder, JournalStore, Lam, Model, ModelCodec, ModelProvider, Namespace};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ActorAddress, SubagentConfigError};

pub(crate) const AGENTS_NAMESPACE: &str = "lam.agents";

/// Direct identity of one configured inference model.
#[derive(Clone, Debug, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelTarget {
    /// Inference provider name.
    pub provider: String,
    /// Provider-specific model identifier.
    pub model: String,
}

impl ModelTarget {
    pub(crate) fn for_model<P, C>(model: &Model<P, C>) -> Self {
        Self {
            provider: model.descriptor().provider().to_owned(),
            model: model.descriptor().model().to_owned(),
        }
    }
}

impl fmt::Display for ModelTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider, self.model)
    }
}

pub(crate) struct ChildActorSpec<S> {
    pub(crate) address: ActorAddress,
    pub(crate) store: Arc<S>,
    pub(crate) namespaces: Vec<Namespace>,
    pub(crate) system_prompt: Option<String>,
    pub(crate) instructions: Vec<String>,
    pub(crate) default_eval_timeout: Option<Duration>,
    pub(crate) max_eval_timeout: Option<Duration>,
    pub(crate) capture_console: bool,
}

type ModelFactory<S> = dyn Fn(ChildActorSpec<S>) -> ActorBuilder<Arc<S>> + Send + Sync;

pub(crate) struct ModelRegistration<S> {
    pub(crate) target: ModelTarget,
    factory: Arc<ModelFactory<S>>,
}

impl<S> Clone for ModelRegistration<S> {
    fn clone(&self) -> Self {
        Self {
            target: self.target.clone(),
            factory: Arc::clone(&self.factory),
        }
    }
}

impl<S> ModelRegistration<S> {
    pub(crate) fn actor_builder(&self, spec: ChildActorSpec<S>) -> ActorBuilder<Arc<S>> {
        (self.factory)(spec)
    }
}

/// Explicit host policy inherited by children created through one namespace.
pub struct SubagentConfig<S> {
    pub(crate) default_model: ModelTarget,
    pub(crate) models: BTreeMap<ModelTarget, ModelRegistration<S>>,
    pub(crate) namespaces: BTreeMap<String, Namespace>,
    pub(crate) required_instructions: Vec<String>,
    pub(crate) max_depth: usize,
    pub(crate) allow_agent_namespace: bool,
    pub(crate) default_eval_timeout: Option<Duration>,
    pub(crate) max_eval_timeout: Option<Duration>,
    pub(crate) capture_console: bool,
}

impl<S> Clone for SubagentConfig<S> {
    fn clone(&self) -> Self {
        Self {
            default_model: self.default_model.clone(),
            models: self.models.clone(),
            namespaces: self.namespaces.clone(),
            required_instructions: self.required_instructions.clone(),
            max_depth: self.max_depth,
            allow_agent_namespace: self.allow_agent_namespace,
            default_eval_timeout: self.default_eval_timeout,
            max_eval_timeout: self.max_eval_timeout,
            capture_console: self.capture_console,
        }
    }
}

impl<S> SubagentConfig<S>
where
    S: JournalStore + 'static,
{
    /// Starts an explicit child policy with its default model.
    #[must_use]
    pub fn builder<P, C>(default_model: Model<P, C>) -> SubagentConfigBuilder<S>
    where
        P: ModelProvider,
        C: ModelCodec,
    {
        SubagentConfigBuilder::new(default_model)
    }

    /// Returns the model used when a spawn request omits `model`.
    #[must_use]
    pub const fn default_model(&self) -> &ModelTarget {
        &self.default_model
    }

    /// Iterates over directly selectable provider/model pairs.
    pub fn models(&self) -> impl Iterator<Item = &ModelTarget> {
        self.models.keys()
    }

    /// Iterates over the maximum namespace set a child may request.
    pub fn namespaces(&self) -> impl Iterator<Item = &str> {
        self.namespaces
            .keys()
            .map(String::as_str)
            .chain(self.allow_agent_namespace.then_some(AGENTS_NAMESPACE))
    }

    pub(crate) fn registration(&self, target: &ModelTarget) -> Option<&ModelRegistration<S>> {
        self.models.get(target)
    }

    pub(crate) fn select_namespace_paths(
        &self,
        requested: Option<Vec<String>>,
    ) -> Result<Vec<String>, String> {
        let available = self.namespaces().collect::<BTreeSet<_>>();
        let selected: Vec<String> = requested.map_or_else(
            || available.iter().map(|path| (*path).to_owned()).collect(),
            |paths| {
                paths
                    .into_iter()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            },
        );
        if let Some(path) = selected
            .iter()
            .find(|path| !available.contains(path.as_str()))
        {
            return Err(path.clone());
        }
        Ok(selected)
    }
}

/// Builds one explicit, reusable child policy value.
pub struct SubagentConfigBuilder<S> {
    default_model: ModelTarget,
    models: Vec<ModelRegistration<S>>,
    namespaces: Vec<Namespace>,
    required_instructions: Vec<String>,
    max_depth: usize,
    allow_agent_namespace: bool,
    default_eval_timeout: Option<Duration>,
    max_eval_timeout: Option<Duration>,
    capture_console: bool,
}

impl<S> SubagentConfigBuilder<S>
where
    S: JournalStore + 'static,
{
    /// Starts a policy builder whose default model is registered immediately.
    #[must_use]
    pub fn new<P, C>(default_model: Model<P, C>) -> Self
    where
        P: ModelProvider,
        C: ModelCodec,
    {
        let default_target = ModelTarget::for_model(&default_model);
        Self {
            default_model: default_target.clone(),
            models: vec![model_registration(default_target, default_model)],
            namespaces: Vec::new(),
            required_instructions: Vec::new(),
            max_depth: 4,
            allow_agent_namespace: true,
            default_eval_timeout: None,
            max_eval_timeout: None,
            capture_console: true,
        }
    }

    /// Adds another directly selectable provider/model pair.
    #[must_use]
    pub fn model<P, C>(mut self, model: Model<P, C>) -> Self
    where
        P: ModelProvider,
        C: ModelCodec,
    {
        let target = ModelTarget::for_model(&model);
        self.models.push(model_registration(target, model));
        self
    }

    /// Adds one namespace to the maximum child capability set.
    #[must_use]
    pub fn namespace(mut self, namespace: Namespace) -> Self {
        self.namespaces.push(namespace);
        self
    }

    /// Appends host-required instructions to every child prompt.
    #[must_use]
    pub fn required_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.required_instructions.push(instructions.into());
        self
    }

    /// Sets the maximum descendant depth below the actor owning the namespace.
    #[must_use]
    pub const fn max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Enables or disables `lam.agents` in the maximum child capability set.
    ///
    /// At the configured depth limit the namespace retains parent messaging
    /// but omits recursive spawning.
    #[must_use]
    pub const fn agent_namespace(mut self, enabled: bool) -> Self {
        self.allow_agent_namespace = enabled;
        self
    }

    /// Sets the default eval timeout inherited by children.
    #[must_use]
    pub const fn default_eval_timeout(mut self, timeout: Duration) -> Self {
        self.default_eval_timeout = Some(timeout);
        self
    }

    /// Sets the maximum eval timeout inherited by children.
    #[must_use]
    pub const fn max_eval_timeout(mut self, timeout: Duration) -> Self {
        self.max_eval_timeout = Some(timeout);
        self
    }

    /// Enables or disables model-visible console capture for children.
    #[must_use]
    pub const fn capture_console(mut self, enabled: bool) -> Self {
        self.capture_console = enabled;
        self
    }

    /// Validates this policy and freezes it for namespace installation.
    pub fn build(self) -> Result<SubagentConfig<S>, SubagentConfigError> {
        let mut models = BTreeMap::new();
        for registration in self.models {
            let target = registration.target.clone();
            if models.insert(target.clone(), registration).is_some() {
                return Err(SubagentConfigError::DuplicateModel {
                    provider: target.provider,
                    model: target.model,
                });
            }
        }

        let mut namespaces = BTreeMap::new();
        for namespace in self.namespaces {
            let path = namespace.path().to_owned();
            if path == "lam" || path == AGENTS_NAMESPACE || path.starts_with("lam.agents.") {
                return Err(SubagentConfigError::ReservedNamespace { path });
            }
            if namespaces.insert(path.clone(), namespace).is_some() {
                return Err(SubagentConfigError::DuplicateNamespace { path });
            }
        }

        Ok(SubagentConfig {
            default_model: self.default_model,
            models,
            namespaces,
            required_instructions: self.required_instructions,
            max_depth: self.max_depth,
            allow_agent_namespace: self.allow_agent_namespace,
            default_eval_timeout: self.default_eval_timeout,
            max_eval_timeout: self.max_eval_timeout,
            capture_console: self.capture_console,
        })
    }
}

fn model_registration<S, P, C>(target: ModelTarget, model: Model<P, C>) -> ModelRegistration<S>
where
    S: JournalStore + 'static,
    P: ModelProvider,
    C: ModelCodec,
{
    ModelRegistration {
        target,
        factory: Arc::new(move |spec| {
            let ChildActorSpec {
                address,
                store,
                namespaces,
                system_prompt,
                instructions,
                default_eval_timeout,
                max_eval_timeout,
                capture_console,
            } = spec;
            let mut builder = Lam::builder(model.clone())
                .state_store(store)
                .capture_console(capture_console);
            for namespace in namespaces {
                builder = builder.namespace(namespace);
            }
            if let Some(system_prompt) = system_prompt {
                builder = builder.system_prompt(system_prompt);
            }
            for instructions in instructions {
                builder = builder.annotate_system_prompt(instructions);
            }
            if let Some(timeout) = default_eval_timeout {
                builder = builder.default_eval_timeout(timeout);
            }
            if let Some(timeout) = max_eval_timeout {
                builder = builder.max_eval_timeout(timeout);
            }
            builder.build().actor(address.to_string())
        }),
    }
}
