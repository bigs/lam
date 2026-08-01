use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;
use lam_core::{
    CompactionReason, ContextSequence, MessageId, ModelResponseMetadata, Revision, RunId,
};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::{InterruptedEvalOutcome, IsolateState};

/// Actor-wide lifecycle information suitable for an embedding UI.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RuntimeEvent {
    /// A model-visible resumption notice became durable at actor startup.
    RuntimeResumed {
        /// Durable identity of the admitted notice.
        message_id: MessageId,
        /// Actor-journal revision containing the notice.
        revision: Revision,
        /// State of the newly installed TypeScript isolate.
        isolate_state: IsolateState,
        /// Run resumed by this runtime, when one was active.
        #[serde(skip_serializing_if = "Option::is_none")]
        resumed_run_id: Option<RunId>,
        /// Status of an interrupted eval lacking a durable outcome.
        #[serde(skip_serializing_if = "Option::is_none")]
        interrupted_eval_outcome: Option<InterruptedEvalOutcome>,
    },
    /// Lam began creating a replacement view over old context.
    CompactionStarted {
        /// Active run associated with the operation, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<RunId>,
        /// Trigger requesting compaction.
        reason: CompactionReason,
    },
    /// A compaction marker became durable.
    CompactionCompleted {
        /// Active run associated with the operation, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<RunId>,
        /// Trigger requesting compaction.
        reason: CompactionReason,
        /// Inclusive raw-context boundary replaced by the marker.
        covers_through: ContextSequence,
        /// Journal revision containing the marker.
        revision: Revision,
        /// Stable name of the selected strategy.
        strategy: String,
        /// Summary-inference usage and cost.
        metadata: ModelResponseMetadata,
    },
    /// A compactor failed without installing a marker.
    CompactionFailed {
        /// Active run associated with the operation, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<RunId>,
        /// Trigger requesting compaction.
        reason: CompactionReason,
        /// Human-readable failure.
        message: String,
    },
}

/// Single-consumer stream of actor-wide runtime events.
pub struct RuntimeEvents {
    receiver: mpsc::Receiver<RuntimeEvent>,
}

impl RuntimeEvents {
    pub(crate) const fn new(receiver: mpsc::Receiver<RuntimeEvent>) -> Self {
        Self { receiver }
    }

    /// Waits for the next runtime event.
    pub async fn next(&mut self) -> Option<RuntimeEvent> {
        self.receiver.recv().await
    }
}

impl Stream for RuntimeEvents {
    type Item = RuntimeEvent;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().receiver).poll_recv(context)
    }
}
