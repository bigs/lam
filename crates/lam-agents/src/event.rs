use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::Stream;
use lam::{RunEvent, RuntimeEvent};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::{ActorAddress, AgentOutcome};

/// Why a formerly hosted actor released its residency slot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum StopReason {
    /// An owning actor explicitly stopped its child subtree.
    Stopped,
    /// Structured call cancellation retired the child it owned.
    Cancelled,
    /// Graceful system shutdown retired the actor.
    Shutdown,
    /// Forceful system or host cancellation retired the actor.
    Aborted,
    /// The actor task panicked.
    Failed {
        /// Panic diagnostic.
        message: String,
    },
}

/// Ephemeral, addressed progress from a managed actor system.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentSystemEvent {
    /// One actor became resident and ready to receive messages.
    Hosted {
        /// Canonical actor address.
        address: ActorAddress,
        /// Canonical parent, absent for a top-level actor.
        parent: Option<ActorAddress>,
    },
    /// Existing run-scoped progress tagged with its actor.
    Run {
        /// Actor performing the run.
        address: ActorAddress,
        /// Existing single-actor progress event.
        event: RunEvent,
    },
    /// Existing actor-wide runtime information tagged with its actor.
    ///
    /// A runtime event and a run event can describe the same operation at
    /// different scopes; neither projection is filtered or rewritten here.
    ActorRuntime {
        /// Actor producing the runtime event.
        address: ActorAddress,
        /// Existing single-actor runtime event.
        event: RuntimeEvent,
    },
    /// Terminal result of one managed child task.
    Outcome {
        /// Completed, failed, or cancelled task outcome.
        outcome: AgentOutcome,
    },
    /// One actor stopped and released its residency slot.
    Retired {
        /// Former resident address.
        address: ActorAddress,
        /// Why the actor stopped.
        reason: StopReason,
    },
}

/// Single-consumer stream of addressed system events.
pub struct AgentSystemEvents {
    pub(crate) receiver: mpsc::Receiver<AgentSystemEvent>,
}

impl AgentSystemEvents {
    /// Waits for the next addressed event.
    pub async fn next(&mut self) -> Option<AgentSystemEvent> {
        self.receiver.recv().await
    }
}

impl Stream for AgentSystemEvents {
    type Item = AgentSystemEvent;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().receiver).poll_recv(context)
    }
}
