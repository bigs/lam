use lam::{ActorError, MessageId};
use schemars::JsonSchema;
use serde::Serialize;

use crate::ActorAddress;

/// Terminal result of one task admitted to a managed child actor.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum AgentOutcome {
    /// The child reached a durable terminal model response.
    Completed {
        /// Actor which performed the task.
        address: ActorAddress,
        /// Stable identity of the task's admitted message.
        message_id: String,
        /// Child model's terminal text.
        output: String,
    },
    /// The admitted task failed before a terminal model response became durable.
    Failed {
        /// Actor which performed the task.
        address: ActorAddress,
        /// Stable identity of the task's admitted message.
        message_id: String,
        /// Human-readable failure.
        error: String,
    },
    /// The admitted task was forcefully interrupted.
    Cancelled {
        /// Actor which performed the task.
        address: ActorAddress,
        /// Stable identity of the task's admitted message.
        message_id: String,
        /// Cancellation diagnostic, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl AgentOutcome {
    pub(crate) fn from_result(
        address: ActorAddress,
        message_id: &MessageId,
        result: Result<String, ActorError>,
    ) -> Self {
        let message_id = message_id.to_string();
        match result {
            Ok(output) => Self::Completed {
                address,
                message_id,
                output,
            },
            Err(error @ (ActorError::Aborted | ActorError::Interrupted)) => Self::Cancelled {
                address,
                message_id,
                reason: Some(error.to_string()),
            },
            Err(error) => Self::Failed {
                address,
                message_id,
                error: error.to_string(),
            },
        }
    }
}
