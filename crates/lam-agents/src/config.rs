use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use lam::{
    ActorBuilder, DirectorySelection, DirectorySelectionSource, JournalStore, Lam, Model,
    ModelCodec, ModelProvider, Namespace,
};
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
    pub(crate) effort: String,
    factory: Arc<ModelFactory<S>>,
}

impl<S> Clone for ModelRegistration<S> {
    fn clone(&self) -> Self {
        Self {
            target: self.target.clone(),
            effort: self.effort.clone(),
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
    pub(crate) models: BTreeMap<(ModelTarget, String), ModelRegistration<S>>,
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
    /// Starts an explicit child policy with one allowed model/effort pair.
    #[must_use]
    pub fn builder<P, C>(model: Model<P, C>, effort: impl Into<String>) -> SubagentConfigBuilder<S>
    where
        P: ModelProvider,
        C: ModelCodec,
    {
        SubagentConfigBuilder::new(model, effort)
    }

    /// Iterates over directly selectable provider/model/effort combinations.
    pub fn model_efforts(&self) -> impl Iterator<Item = (&ModelTarget, &str)> {
        self.models
            .keys()
            .map(|(target, effort)| (target, effort.as_str()))
    }

    /// Iterates over the maximum namespace set a child may request.
    pub fn namespaces(&self) -> impl Iterator<Item = &str> {
        self.namespaces
            .keys()
            .map(String::as_str)
            .chain(self.allow_agent_namespace.then_some(AGENTS_NAMESPACE))
    }

    pub(crate) fn registration(
        &self,
        target: &ModelTarget,
        effort: &str,
    ) -> Option<&ModelRegistration<S>> {
        self.models.get(&(target.clone(), effort.to_owned()))
    }

    pub(crate) fn contains_model(&self, target: &ModelTarget) -> bool {
        self.models
            .keys()
            .any(|(configured, _)| configured == target)
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
    /// Starts a policy builder with one allowed model/effort pair.
    #[must_use]
    pub fn new<P, C>(model: Model<P, C>, effort: impl Into<String>) -> Self
    where
        P: ModelProvider,
        C: ModelCodec,
    {
        let target = ModelTarget::for_model(&model);
        Self {
            models: vec![model_registration(target, effort.into(), model)],
            namespaces: Vec::new(),
            required_instructions: Vec::new(),
            max_depth: 4,
            allow_agent_namespace: true,
            default_eval_timeout: None,
            max_eval_timeout: None,
            capture_console: true,
        }
    }

    /// Adds another directly selectable provider/model/effort combination.
    #[must_use]
    pub fn model<P, C>(mut self, model: Model<P, C>, effort: impl Into<String>) -> Self
    where
        P: ModelProvider,
        C: ModelCodec,
    {
        let target = ModelTarget::for_model(&model);
        self.models
            .push(model_registration(target, effort.into(), model));
        self
    }

    /// Adds one namespace to the maximum child capability set.
    #[must_use]
    pub fn namespace(mut self, namespace: Namespace) -> Self {
        self.namespaces.push(namespace);
        self
    }

    /// Adds several namespaces to the maximum child capability set.
    #[must_use]
    pub fn namespaces(mut self, namespaces: impl IntoIterator<Item = Namespace>) -> Self {
        self.namespaces.extend(namespaces);
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
            let effort = registration.effort.clone();
            if effort.trim().is_empty() || effort.trim() != effort {
                return Err(SubagentConfigError::InvalidEffort { effort });
            }
            if models
                .insert((target.clone(), effort.clone()), registration)
                .is_some()
            {
                return Err(SubagentConfigError::DuplicateModel {
                    provider: target.provider,
                    model: target.model,
                    effort,
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

fn model_registration<S, P, C>(
    target: ModelTarget,
    effort: String,
    model: Model<P, C>,
) -> ModelRegistration<S>
where
    S: JournalStore + 'static,
    P: ModelProvider,
    C: ModelCodec,
{
    // Match the root actor convention (`provider/model`) so the TUI and
    // journals can resolve a child to a configured ModelChoice. The Lam
    // builder default id is the opaque string "default", which is useless
    // once more than one model exists in a session.
    let model_id = target.to_string();
    let directory_target = target.clone();
    let directory_effort = effort.clone();
    ModelRegistration {
        target,
        effort,
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
                .initial_model_id(model_id.clone())
                .state_store(store)
                .directory_selection(DirectorySelectionSource::new({
                    let target = directory_target.clone();
                    let effort = directory_effort.clone();
                    move || DirectorySelection {
                        provider: target.provider.clone(),
                        model: target.model.clone(),
                        effort: Some(effort.clone()),
                    }
                }))
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
