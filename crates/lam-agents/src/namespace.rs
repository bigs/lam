use std::sync::{Arc, Weak};

use lam::{JournalStore, Namespace};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{AGENTS_NAMESPACE, ModelTarget};
use crate::system::SystemInner;
use crate::{ActorAddress, AgentOutcome, SubagentConfig};

/// Input accepted by both `lam.agents.spawn` and `lam.agents.call`.
#[derive(Clone, Debug, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildRequest {
    /// One path segment used to derive the child address.
    pub name: String,
    /// Initial task delivered to the child as an always-steering actor message.
    pub task: String,
    /// Provider/model pair from `lam.agents.models()`.
    pub model: ModelTarget,
    /// Explicit reasoning effort allowed for the selected model.
    pub effort: String,
    /// Exact extension namespace subset, or the complete configured set when omitted.
    #[serde(default)]
    pub namespaces: Option<Vec<String>>,
    /// Complete replacement for the child's compact default system prompt.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Instructions appended after the default or replacement system prompt.
    #[serde(default)]
    pub instructions: Option<String>,
}

/// Backward-compatible name for the shared child-creation request.
pub type SpawnRequest = ChildRequest;

/// One model entry suitable for `spawn` / `call` `model` fields.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    /// Inference provider name (pass as `model.provider`).
    pub provider: String,
    /// Provider-specific model identifier (pass as `model.model`).
    pub model: String,
    /// Allowed explicit reasoning efforts.
    pub efforts: Vec<String>,
}

/// Models available under one provider for subagent creation.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModels {
    /// Provider name shared by every entry in `models`.
    pub provider: String,
    /// Models for this provider, in host configuration order.
    pub models: Vec<ModelInfo>,
}

/// Catalog of models this actor may use when spawning or calling children.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsCatalog {
    /// All allowed models grouped by provider (provider name ascending).
    pub providers: Vec<ProviderModels>,
}

/// Address and parent relationship of one actor.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentIdentity {
    /// Canonical actor address.
    pub address: ActorAddress,
    /// Canonical parent address, absent for a top-level actor.
    pub parent: Option<ActorAddress>,
}

impl AgentIdentity {
    pub(crate) fn new(address: ActorAddress) -> Self {
        let parent = address.parent();
        Self { address, parent }
    }
}

/// Successful, durably queued subagent creation.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnReceipt {
    /// Canonical child address.
    pub address: ActorAddress,
    /// Provider/model pair selected for the child.
    pub model: ModelTarget,
    /// Reasoning effort selected for the child.
    pub effort: String,
    /// Installed extension namespace paths. Kernel `lam` utilities are implicit.
    pub namespaces: Vec<String>,
    /// Child depth relative to the actor which owns this subagent policy.
    pub depth: usize,
    /// Stable identity of the admitted initial task.
    pub message_id: String,
    /// Child-local journal revision containing the task admission.
    pub revision: u64,
}

/// Structured failure returned to TypeScript by `lam.agents.spawn`.
#[derive(Clone, Debug, JsonSchema, Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "camelCase")]
pub enum SpawnError {
    /// The supplied child name was not one canonical path segment.
    #[error("invalid child name `{name}`: {message}")]
    InvalidName {
        /// Rejected child name.
        name: String,
        /// Canonicalization diagnostic.
        message: String,
    },
    /// The requested child address is already resident, launching, or durable.
    #[error("actor address `{address}` is already in use")]
    AddressInUse {
        /// Conflicting canonical address.
        address: ActorAddress,
    },
    /// The configured recursive spawn depth was reached.
    #[error("subagent depth limit {max_depth} was reached")]
    DepthLimit {
        /// Configured maximum depth.
        max_depth: usize,
    },
    /// The requested direct provider/model pair was not configured by the host.
    #[error("model `{provider}/{model}` is not allowed for subagents")]
    ModelNotAllowed {
        /// Requested provider.
        provider: String,
        /// Requested model.
        model: String,
    },
    /// The model exists but does not allow the requested effort.
    #[error("effort `{effort}` is not allowed for subagent model `{provider}/{model}`")]
    EffortNotAllowed {
        /// Requested provider.
        provider: String,
        /// Requested model.
        model: String,
        /// Requested effort.
        effort: String,
    },
    /// The requested manifest namespace was not configured by the host.
    #[error("namespace `{path}` is not allowed for subagents")]
    NamespaceNotAllowed {
        /// Requested namespace path.
        path: String,
    },
    /// The bounded system cannot admit another resident actor.
    #[error("the agent system reached its limit of {max_agents} resident actors")]
    Capacity {
        /// Configured bound.
        max_agents: usize,
    },
    /// The owning agent system is no longer available.
    #[error("the agent system is unavailable")]
    Unavailable,
    /// A configured child runtime could not start or receive its task.
    #[error("the subagent could not start: {message}")]
    StartFailed {
        /// Host diagnostic.
        message: String,
    },
}

/// Input accepted by `lam.agents.send`.
#[derive(Clone, Debug, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendRequest {
    /// Canonical address of the recipient.
    pub to: ActorAddress,
    /// Structured value delivered to the recipient.
    pub message: Value,
}

/// Confirmation that an actor message is durable in its recipient's mailbox.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendReceipt {
    /// Actor which received the message.
    pub address: ActorAddress,
    /// Stable identity of the admitted message.
    pub message_id: String,
    /// Recipient-local journal revision containing the admission.
    pub revision: u64,
}

/// Structured failure returned to TypeScript by `lam.agents.send`.
#[derive(Clone, Debug, JsonSchema, Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "camelCase")]
pub enum SendError {
    /// The addressed actor is not resident in this system.
    #[error("actor `{address}` is unavailable")]
    AddressUnavailable {
        /// Unknown or stopped recipient.
        address: ActorAddress,
    },
    /// The owning agent system is no longer available.
    #[error("the agent system is unavailable")]
    Unavailable,
    /// Durable mailbox admission failed.
    #[error("the message could not be delivered: {message}")]
    DeliveryFailed {
        /// Host diagnostic.
        message: String,
    },
}

/// Optional input accepted by `lam.agents.list`.
#[derive(Clone, Debug, Default, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListRequest {
    /// Namespace whose direct resident children should be listed.
    #[serde(default)]
    pub path: Option<ActorAddress>,
}

/// Structured failure returned to TypeScript by `lam.agents.list`.
#[derive(Clone, Debug, JsonSchema, Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "camelCase")]
pub enum ListError {
    /// The owning agent system is no longer available.
    #[error("the agent system is unavailable")]
    Unavailable,
}

/// Input accepted by `lam.agents.wait`.
#[derive(Clone, Debug, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitRequest {
    /// Direct-child addresses returned by `lam.agents.spawn`.
    pub addresses: Vec<ActorAddress>,
}

/// One completed spawned task whose outcome is durable in the caller's inbox.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitedTask {
    /// Child which performed the initial spawned task.
    pub address: ActorAddress,
    /// Stable identity of the child's initial task message.
    pub message_id: String,
    /// Stable identity of the outcome message admitted to the caller.
    pub inbox_message_id: String,
    /// Caller-local journal revision containing the outcome message.
    pub inbox_revision: u64,
}

/// Confirmation that every requested spawned outcome is durable in the inbox.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitReceipt {
    /// Completed tasks in the same order as the requested addresses.
    pub completed: Vec<WaitedTask>,
}

/// Structured failure returned to TypeScript by `lam.agents.wait`.
#[derive(Clone, Debug, JsonSchema, Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "camelCase")]
pub enum WaitError {
    /// At least one child address is required.
    #[error("at least one spawned child address is required")]
    Empty,
    /// The same child appeared more than once.
    #[error("actor `{address}` was requested more than once")]
    Duplicate {
        /// Duplicate requested address.
        address: ActorAddress,
    },
    /// Actors may await only their own direct children.
    #[error("actor `{requester}` cannot wait for non-child `{address}`")]
    NotDirectChild {
        /// Actor requesting the wait.
        requester: ActorAddress,
        /// Rejected target.
        address: ActorAddress,
    },
    /// The address was not created by this caller through `spawn`.
    #[error("actor `{address}` is not a spawned task owned by `{requester}`")]
    NotSpawned {
        /// Actor requesting the wait.
        requester: ActorAddress,
        /// Unknown target.
        address: ActorAddress,
    },
    /// The child finished but its outcome could not enter the caller's inbox.
    #[error("outcome from `{address}` could not be delivered: {message}")]
    DeliveryFailed {
        /// Child whose outcome was not delivered.
        address: ActorAddress,
        /// Durable-delivery diagnostic.
        message: String,
    },
    /// The child was cancelled and its terminal outcome is durable in this
    /// actor's inbox. Cancellation remains a failed wait rather than a normal
    /// completion, while preserving the structured outcome for inspection.
    #[error("spawned task `{address}` was cancelled")]
    Cancelled {
        /// Cancelled direct child.
        address: ActorAddress,
        /// Durable terminal child outcome.
        outcome: AgentOutcome,
        /// Stable identity of the inbox message carrying the outcome.
        inbox_message_id: String,
        /// Caller-local journal revision containing the outcome.
        inbox_revision: u64,
    },
    /// The owning agent system is no longer available.
    #[error("the agent system is unavailable")]
    Unavailable,
}

/// Input accepted by `lam.agents.stop`.
#[derive(Clone, Debug, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopRequest {
    /// Direct child whose complete descendant subtree should be retired.
    pub address: ActorAddress,
}

/// Confirmation that a direct child subtree is no longer resident.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopReceipt {
    /// Direct child requested by the caller.
    pub address: ActorAddress,
}

/// Structured failure returned to TypeScript by `lam.agents.stop`.
#[derive(Clone, Debug, JsonSchema, Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "camelCase")]
pub enum StopError {
    /// Actors may stop only their own direct children.
    #[error("actor `{requester}` cannot stop non-child `{address}`")]
    NotDirectChild {
        /// Actor requesting the stop.
        requester: ActorAddress,
        /// Rejected target.
        address: ActorAddress,
    },
    /// The addressed child is no longer resident.
    #[error("actor `{address}` is unavailable")]
    AddressUnavailable {
        /// Unknown or stopped child.
        address: ActorAddress,
    },
    /// The owning agent system is no longer available.
    #[error("the agent system is unavailable")]
    Unavailable,
    /// The child subtree could not be fully retired.
    #[error("the child could not be stopped: {message}")]
    StopFailed {
        /// Host diagnostic.
        message: String,
    },
}

pub(crate) fn agents_namespace<S>(
    system: Weak<SystemInner<S>>,
    address: ActorAddress,
    depth: usize,
    config: Arc<SubagentConfig<S>>,
) -> Namespace
where
    S: JournalStore + 'static,
{
    let namespace_docs = config.namespaces().collect::<Vec<_>>().join(", ");
    let docs = format!("Create and communicate with addressed actors. Current actor: {address}.");
    let models_docs = "List every provider/model/effort combination allowed for subagent spawn/call from this actor, grouped by provider. Both `model` and `effort` are required by the creation commands. Unless the user or project instructions request otherwise, pass the current selection from `lam.dir({ path: 'lam' })` when it includes an effort and is allowed by this catalog; otherwise choose an allowed combination explicitly.";
    let spawn_docs = format!(
        "Create a persistent direct child at {address}/<name> and return after its task is durably queued. It uses the same child-request shape as `lam.agents.call`. Its eventual AgentOutcome is delivered as a steering actor message. Child addresses are create-only and cannot be reused once durable. `model: {{ provider, model }}` and `effort` are required. Unless the user or project instructions request otherwise, pass the current selection from `lam.dir({{ path: 'lam' }})` when it includes an effort and is allowed by `lam.agents.models()`; otherwise choose an allowed combination explicitly. Omit namespaces for the complete configured set; allowed namespaces: [{}]. Kernel lam.dir and lam.result are always present.",
        namespace_docs,
    );
    let call_docs = format!(
        "Create a persistent direct child at {address}/<name>, wait asynchronously for its initial task, and return AgentOutcome. It uses the same child-request shape as `lam.agents.spawn`. Cancellation stops the child subtree. `model: {{ provider, model }}` and `effort` are required. Unless the user or project instructions request otherwise, pass the current selection from `lam.dir({{ path: 'lam' }})` when it includes an effort and is allowed by `lam.agents.models()`; otherwise choose an allowed combination explicitly. Omit namespaces for the complete configured set; allowed namespaces: [{}].",
        namespace_docs,
    );
    let wait_docs = "Wait without steering for the initial tasks of direct children returned by `lam.agents.spawn`. Resolves only after every terminal `AgentOutcome` is durably admitted to this actor's inbox; the next model continuation receives the wait tool result and those inbox messages together. A cancelled child produces a structured failed wait carrying its durable `AgentOutcome::Cancelled`. Waiting does not message, interrupt, stop, or otherwise steer the children. If the surrounding eval is cancelled or times out, admitted children continue running and their outcomes are still delivered.";

    let mut namespace = Namespace::new(AGENTS_NAMESPACE, docs);
    namespace = namespace.function(
        "identity",
        "Return this actor's canonical address and parent address.",
        {
            let address = address.clone();
            move |_context, (): ()| {
                let identity = AgentIdentity::new(address.clone());
                async move { Ok::<_, lam::Never>(identity) }
            }
        },
    );
    namespace = namespace.function("models", models_docs, {
        let config = config.clone();
        move |_context, (): ()| {
            let catalog = models_catalog(&config);
            async move { Ok::<_, lam::Never>(catalog) }
        }
    });
    namespace = namespace.function(
        "list",
        "List direct resident children of path, or of the current actor when called without arguments.",
        {
            let system = system.clone();
            let address = address.clone();
            move |_context, request: Option<ListRequest>| {
                let system = system.clone();
                let current = address.clone();
                async move {
                    let system = system.upgrade().ok_or(ListError::Unavailable)?;
                    let path = request.and_then(|request| request.path).unwrap_or(current);
                    system
                        .list_children(&path)
                        .map_err(|_| ListError::Unavailable)
                }
            }
        },
    );
    namespace = namespace.function(
        "send",
        "Send one structured value to an addressed resident actor. Delivery is durable and always steers an active run.",
        {
            let system = system.clone();
            let address = address.clone();
            move |_context, request: SendRequest| {
                let system = system.clone();
                let sender = address.clone();
                async move {
                    let system = system.upgrade().ok_or(SendError::Unavailable)?;
                    let target = request.to;
                    let receipt = system
                        .send(&sender, &target, request.message)
                        .await
                        .map_err(send_system_error)?;
                    Ok::<_, SendError>(SendReceipt {
                        address: target,
                        message_id: receipt.message_id.to_string(),
                        revision: receipt.revision.get(),
                    })
                }
            }
        },
    );
    namespace = namespace.function(
        "stop",
        "Stop one direct child and its descendants, cancelling active work and releasing their residency slots.",
        {
            let system = system.clone();
            let address = address.clone();
            move |_context, request: StopRequest| {
                let system = system.clone();
                let requester = address.clone();
                async move {
                    let system = system.upgrade().ok_or(StopError::Unavailable)?;
                    let target = request.address;
                    system.stop_child(&requester, &target).await?;
                    Ok::<_, StopError>(StopReceipt { address: target })
                }
            }
        },
    );
    if depth < config.max_depth {
        namespace = namespace.function("wait", wait_docs, {
            let system = system.clone();
            let address = address.clone();
            move |_context, request: WaitRequest| {
                let system = system.clone();
                let requester = address.clone();
                async move {
                    let system = system.upgrade().ok_or(WaitError::Unavailable)?;
                    system.wait_for_spawned(&requester, request).await
                }
            }
        });
        namespace = namespace.function("call", call_docs, {
            let system = system.clone();
            let address = address.clone();
            let config = config.clone();
            move |_context, request: ChildRequest| {
                let system = system.clone();
                let address = address.clone();
                let config = config.clone();
                async move {
                    let system = system.upgrade().ok_or(SpawnError::Unavailable)?;
                    system.call_child(address, depth, config, request).await
                }
            }
        });
        namespace = namespace.function("spawn", spawn_docs, {
            let system = system.clone();
            let address = address.clone();
            let config = config.clone();
            move |_context, request: ChildRequest| {
                let system = system.clone();
                let address = address.clone();
                let config = config.clone();
                async move {
                    let system = system.upgrade().ok_or(SpawnError::Unavailable)?;
                    system.spawn_child(address, depth, config, request).await
                }
            }
        });
    }
    namespace
}

fn models_catalog<S>(config: &SubagentConfig<S>) -> ModelsCatalog
where
    S: JournalStore + 'static,
{
    models_catalog_from_targets(
        config
            .model_efforts()
            .map(|(target, effort)| (target.clone(), effort.to_owned())),
    )
}

fn models_catalog_from_targets(
    targets: impl IntoIterator<Item = (ModelTarget, String)>,
) -> ModelsCatalog {
    use std::collections::BTreeMap;

    let mut by_provider: BTreeMap<String, BTreeMap<String, ModelInfo>> = BTreeMap::new();
    for (target, effort) in targets {
        let model = by_provider
            .entry(target.provider.clone())
            .or_default()
            .entry(target.model.clone())
            .or_insert_with(|| ModelInfo {
                provider: target.provider.clone(),
                model: target.model,
                efforts: Vec::new(),
            });
        model.efforts.push(effort);
    }
    let providers = by_provider
        .into_iter()
        .map(|(provider, models)| ProviderModels {
            provider,
            models: models.into_values().collect(),
        })
        .collect();
    ModelsCatalog { providers }
}

fn send_system_error(error: crate::AgentSystemError) -> SendError {
    match error {
        crate::AgentSystemError::ActorUnavailable { address }
        | crate::AgentSystemError::ActorTaskPanicked { address } => {
            SendError::AddressUnavailable { address }
        }
        crate::AgentSystemError::ShuttingDown | crate::AgentSystemError::WorkerUnavailable => {
            SendError::Unavailable
        }
        error => SendError::DeliveryFailed {
            message: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spawn_and_call_request_schema_requires_model_and_effort() {
        let schema = serde_json::to_value(schemars::schema_for!(ChildRequest)).unwrap();
        assert_eq!(schema["title"], "ChildRequest");
        let description = schema["description"].as_str().unwrap();
        assert!(description.contains("lam.agents.spawn"));
        assert!(description.contains("lam.agents.call"));
        let required = schema["required"].as_array().unwrap();
        for field in ["name", "task", "model", "effort"] {
            assert!(required.iter().any(|value| value == field), "{field}");
        }

        assert!(
            serde_json::from_value::<ChildRequest>(json!({
                "name": "worker",
                "task": "work",
                "effort": "high"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ChildRequest>(json!({
                "name": "worker",
                "task": "work",
                "model": { "provider": "openai", "model": "gpt" }
            }))
            .is_err()
        );
    }

    #[test]
    fn models_catalog_groups_by_provider_in_name_order() {
        let catalog = models_catalog_from_targets([
            (target("fireworks", "deepseek-a"), "high".to_owned()),
            (target("openai", "gpt-default"), "low".to_owned()),
            (target("fireworks", "deepseek-b"), "max".to_owned()),
            (target("openai", "gpt-default"), "high".to_owned()),
            (target("openai", "gpt-other"), "xhigh".to_owned()),
        ]);
        assert_eq!(
            catalog
                .providers
                .iter()
                .map(|provider| provider.provider.as_str())
                .collect::<Vec<_>>(),
            ["fireworks", "openai"]
        );
        assert_eq!(
            catalog.providers[0]
                .models
                .iter()
                .map(|model| model.model.as_str())
                .collect::<Vec<_>>(),
            ["deepseek-a", "deepseek-b"]
        );
        assert_eq!(
            catalog.providers[1]
                .models
                .iter()
                .map(|model| model.model.as_str())
                .collect::<Vec<_>>(),
            ["gpt-default", "gpt-other"]
        );
        assert_eq!(catalog.providers[1].models[0].efforts, ["low", "high"]);
    }

    #[test]
    fn cancelled_wait_error_has_stable_structured_serialization() {
        let address = ActorAddress::new("/root/worker").unwrap();
        let error = WaitError::Cancelled {
            address: address.clone(),
            outcome: AgentOutcome::Cancelled {
                address,
                message_id: "child-message".to_owned(),
                reason: Some("interrupted".to_owned()),
            },
            inbox_message_id: "inbox-message".to_owned(),
            inbox_revision: 17,
        };

        let value = serde_json::to_value(error).unwrap();
        assert_eq!(value["code"], "cancelled");
        assert_eq!(value["address"], "/root/worker");
        assert_eq!(value["outcome"]["status"], "cancelled");
        assert_eq!(value["outcome"]["address"], "/root/worker");
        assert_eq!(value["outcome"]["message_id"], "child-message");
        assert_eq!(value["inbox_message_id"], "inbox-message");
        assert_eq!(value["inbox_revision"], 17);
    }

    fn target(provider: &str, model: &str) -> ModelTarget {
        ModelTarget {
            provider: provider.to_owned(),
            model: model.to_owned(),
        }
    }
}
