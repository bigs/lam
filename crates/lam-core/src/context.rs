use serde::{Deserialize, Serialize};

use crate::{ContextSequence, EncodedPayload, MessageId, RunId, Timestamp};

/// Whether a model context item continues or completes its run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RunProgress {
    /// The run continues after this item.
    Continue,
    /// This item durably completes the run.
    Complete,
}

/// One structurally valid transition in the model-visible context stream.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ContextTransition {
    /// A mailbox-ordered batch which starts or steers a run.
    Messages {
        /// Run receiving the messages.
        run_id: RunId,
        /// Messages incorporated in delivery order.
        consumed_message_ids: Vec<MessageId>,
    },
    /// Untouched provider-native model output.
    Model {
        /// Run which produced the output.
        run_id: RunId,
        /// Whether the model continues or completes the run.
        progress: RunProgress,
    },
    /// Model-visible result of Lam's single eval tool.
    Eval {
        /// Run which requested the evaluation.
        run_id: RunId,
    },
    /// A logical replacement view over an earlier context prefix.
    Compaction {
        /// Latest context entry replaced by this view.
        covers_through: ContextSequence,
        /// Active run associated with the compaction, if any.
        run_id: Option<RunId>,
    },
}

impl ContextTransition {
    /// Returns the associated run identity, if any.
    #[must_use]
    pub const fn run_id(&self) -> Option<&RunId> {
        match self {
            Self::Messages { run_id, .. } | Self::Model { run_id, .. } | Self::Eval { run_id } => {
                Some(run_id)
            }
            Self::Compaction { run_id, .. } => run_id.as_ref(),
        }
    }

    /// Returns messages consumed by this transition.
    #[must_use]
    pub fn consumed_message_ids(&self) -> &[MessageId] {
        match self {
            Self::Messages {
                consumed_message_ids,
                ..
            } => consumed_message_ids,
            Self::Model { .. } | Self::Eval { .. } | Self::Compaction { .. } => &[],
        }
    }
}

/// One append-only model-visible context item.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEntry {
    /// Context and run transition encoded by this item.
    pub transition: ContextTransition,
    /// Authoritative codec-tagged value.
    pub payload: EncodedPayload,
    /// Host-observed record time.
    pub recorded_at: Timestamp,
}
