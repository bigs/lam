use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;
use lam_core::{MessageId, Revision, RunId};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::{InterruptedEvalOutcome, IsolateState};

/// Actor-wide lifecycle information suitable for an embedding UI.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
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
}

/// Single-consumer stream of actor-wide runtime events.
pub struct RuntimeEvents {
    receiver: mpsc::UnboundedReceiver<RuntimeEvent>,
}

impl RuntimeEvents {
    pub(crate) const fn new(receiver: mpsc::UnboundedReceiver<RuntimeEvent>) -> Self {
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
