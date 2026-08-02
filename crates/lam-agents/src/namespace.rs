use std::sync::{Arc, Weak};

use lam::{JournalStore, Namespace};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{AGENTS_NAMESPACE, ModelTarget};
use crate::system::SystemInner;
use crate::{ActorAddress, SubagentConfig};

/// Input accepted by `lam.agents.spawn`.
#[derive(Clone, Debug, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnRequest {
    /// One path segment used to derive the child address.
    pub name: String,
    /// Initial task delivered to the child as an always-steering actor message.
    pub task: String,
    /// Configured provider/model pair, or the host default when omitted.
    #[serde(default)]
    pub model: Option<ModelTarget>,
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
    /// Installed extension namespace paths. Kernel `lam` utilities are implicit.
    pub namespaces: Vec<String>,
    /// Child depth relative to the actor which owns this subagent policy.
    pub depth: usize,
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

pub(crate) fn agents_namespace<S>(
    system: Weak<SystemInner<S>>,
    address: ActorAddress,
    depth: usize,
    config: Arc<SubagentConfig<S>>,
) -> Namespace
where
    S: JournalStore + 'static,
{
    let model_docs = config
        .models()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let namespace_docs = config.namespaces().collect::<Vec<_>>().join(", ");
    let docs = format!("Create and communicate with addressed actors. Current actor: {address}.");
    let spawn_docs = format!(
        "Create a direct child at {address}/<name> and return after its task is durably queued. Child addresses are create-only and cannot be reused once durable. Omit model for `{}`; allowed models: [{}]. Omit namespaces for the complete configured set; allowed namespaces: [{}]. Kernel lam.dir and lam.result are always present.",
        config.default_model(),
        model_docs,
        namespace_docs,
    );

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
    if depth < config.max_depth {
        namespace = namespace.function("spawn", spawn_docs, {
            let system = system.clone();
            let address = address.clone();
            let config = config.clone();
            move |_context, request: SpawnRequest| {
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
