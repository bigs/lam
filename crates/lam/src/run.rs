use std::future::{Future, poll_fn};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::task::{Context, Poll};

use futures_core::Stream;
use lam_core::{
    CompactionReason, ContextSequence, MessageEnvelope, MessageId, ModelDelta,
    ModelResponseMetadata, OutputContract, RunId,
};
use schemars::{JsonSchema, schema_for};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, oneshot};

use crate::command::{CallLease, CallRequest, RunnerCommand};
use crate::{ActorError, EvalOutcome, MessageReceipt};

pub(crate) const RUN_EVENT_BUFFER: usize = 256;

/// Ephemeral progress emitted while one actor run executes.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RunEvent {
    /// The input entered a concrete actor activation.
    Started {
        /// Durable activation identity.
        run_id: RunId,
    },
    /// A mailbox batch entered model-visible context.
    MessagesDelivered {
        /// Activation receiving the messages.
        run_id: RunId,
        /// Messages delivered in admission order.
        message_ids: Vec<MessageId>,
    },
    /// Lam began one external model request.
    ModelStarted {
        /// Activation making the request.
        run_id: RunId,
    },
    /// An ephemeral provider delta arrived.
    ModelDelta {
        /// Activation receiving the delta.
        run_id: RunId,
        /// Provider-neutral display view.
        delta: ModelDelta,
    },
    /// A complete native model response became available.
    ModelCompleted {
        /// Activation which produced the response.
        run_id: RunId,
        /// Best-effort usage and cost view for downstream observability.
        metadata: ModelResponseMetadata,
    },
    /// Lam began creating a replacement view over old context.
    CompactionStarted {
        /// Activation being compacted.
        run_id: RunId,
        /// Trigger requesting compaction.
        reason: CompactionReason,
    },
    /// A compaction marker became durable.
    CompactionCompleted {
        /// Activation being compacted.
        run_id: RunId,
        /// Trigger requesting compaction.
        reason: CompactionReason,
        /// Inclusive raw-context boundary replaced by the marker.
        covers_through: ContextSequence,
        /// Compaction-inference usage and cost.
        metadata: ModelResponseMetadata,
    },
    /// A compactor failed without installing a marker.
    CompactionFailed {
        /// Activation being compacted.
        run_id: RunId,
        /// Trigger requesting compaction.
        reason: CompactionReason,
        /// Human-readable failure.
        message: String,
    },
    /// Lam began executing the model's TypeScript program.
    EvalStarted {
        /// Activation requesting eval.
        run_id: RunId,
    },
    /// One complete eval outcome became available.
    EvalCompleted {
        /// Activation requesting eval.
        run_id: RunId,
        /// Structured success or failure.
        outcome: EvalOutcome,
    },
    /// The run reached its durable terminal context entry.
    Completed {
        /// Completed activation.
        run_id: RunId,
    },
    /// The observed run failed before durable completion.
    Failed {
        /// Human-readable failure.
        message: String,
    },
}

struct RunStart {
    commands: mpsc::UnboundedSender<RunnerCommand>,
    active: Arc<AtomicBool>,
    message: Result<MessageEnvelope, ActorError>,
    output: OutputContract,
    events: mpsc::Sender<RunEvent>,
    admission: oneshot::Sender<Result<MessageReceipt, ActorError>>,
    completion: oneshot::Sender<Result<serde_json::Value, ActorError>>,
}

/// One linear call which can be awaited for output or consumed as events.
pub struct Run<'actor, T> {
    message_id: MessageId,
    start: Option<RunStart>,
    events: mpsc::Receiver<RunEvent>,
    admission: Option<oneshot::Receiver<Result<MessageReceipt, ActorError>>>,
    completion: oneshot::Receiver<Result<serde_json::Value, ActorError>>,
    marker: PhantomData<(&'actor mut (), T)>,
}

impl<T> Unpin for Run<'_, T> {}

impl<'actor> Run<'actor, String> {
    /// Changes this call from ordinary text to schema-constrained output.
    ///
    /// # Panics
    ///
    /// Panics if the run has already been polled and sent to the actor.
    #[must_use]
    pub fn output<T>(self) -> Run<'actor, T>
    where
        T: DeserializeOwned + JsonSchema,
    {
        assert!(
            self.start.is_some(),
            "the output contract cannot change after a run starts"
        );
        let schema = serde_json::to_value(schema_for!(T))
            .expect("schemars schemas are always JSON serializable");
        let mut start = self.start;
        start.as_mut().expect("start was checked").output = OutputContract::Structured { schema };
        Run {
            message_id: self.message_id,
            start,
            events: self.events,
            admission: self.admission,
            completion: self.completion,
            marker: PhantomData,
        }
    }
}

impl<'actor, T> Run<'actor, T> {
    pub(crate) fn new(
        commands: mpsc::UnboundedSender<RunnerCommand>,
        active: Arc<AtomicBool>,
        message_id: MessageId,
        message: Result<MessageEnvelope, ActorError>,
    ) -> Self {
        let (event_sender, events) = mpsc::channel(RUN_EVENT_BUFFER);
        let (admission_sender, admission) = oneshot::channel();
        let (completion_sender, completion) = oneshot::channel();
        Self {
            message_id,
            start: Some(RunStart {
                commands,
                active,
                message,
                output: OutputContract::Text,
                events: event_sender,
                admission: admission_sender,
                completion: completion_sender,
            }),
            events,
            admission: Some(admission),
            completion,
            marker: PhantomData,
        }
    }

    /// Returns the durable message identity allocated for this call.
    #[must_use]
    pub const fn message_id(&self) -> &MessageId {
        &self.message_id
    }

    /// Waits for the next buffered runtime event.
    pub async fn next(&mut self) -> Option<RunEvent> {
        poll_fn(|context| Pin::new(&mut *self).poll_next(context)).await
    }

    /// Waits until this call's input is durably admitted to the actor mailbox.
    ///
    /// The returned receipt precedes the terminal call result. This is useful
    /// for background orchestration which must acknowledge durable admission
    /// without losing the correlated completion.
    pub async fn wait_admitted(&mut self) -> Result<MessageReceipt, ActorError> {
        self.ensure_started();
        let admission = self.admission.take().ok_or_else(|| ActorError::State {
            message: "call admission was already observed".to_owned(),
        })?;
        admission.await.map_err(|_| ActorError::Unavailable)?
    }

    fn ensure_started(&mut self) {
        let Some(start) = self.start.take() else {
            return;
        };
        let RunStart {
            commands,
            active,
            message,
            output,
            events,
            admission,
            completion,
        } = start;

        let message = match message {
            Ok(message) => message,
            Err(error) => {
                let _ = admission.send(Err(error.clone()));
                let _ = events.try_send(RunEvent::Failed {
                    message: error.to_string(),
                });
                let _ = completion.send(Err(error));
                return;
            }
        };
        let lease = match CallLease::acquire(active) {
            Ok(lease) => lease,
            Err(error) => {
                let _ = admission.send(Err(error.clone()));
                let _ = events.try_send(RunEvent::Failed {
                    message: error.to_string(),
                });
                let _ = completion.send(Err(error));
                return;
            }
        };
        let call = CallRequest::new(message, output, events, admission, completion, lease);
        if let Err(error) = commands.send(RunnerCommand::Call(Box::new(call))) {
            let RunnerCommand::Call(call) = error.0 else {
                unreachable!("only a call command was sent")
            };
            call.fail(ActorError::Unavailable);
        }
    }
}

/// Single-consumer stream of progress across every run performed by one actor.
pub struct RunEvents {
    receiver: mpsc::Receiver<RunEvent>,
}

impl RunEvents {
    pub(crate) const fn new(receiver: mpsc::Receiver<RunEvent>) -> Self {
        Self { receiver }
    }

    /// Waits for the next actor run event.
    pub async fn next(&mut self) -> Option<RunEvent> {
        self.receiver.recv().await
    }
}

impl Stream for RunEvents {
    type Item = RunEvent;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().receiver).poll_recv(context)
    }
}

impl<T> Stream for Run<'_, T> {
    type Item = RunEvent;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let run = self.get_mut();
        run.ensure_started();
        Pin::new(&mut run.events).poll_recv(context)
    }
}

impl<T> Future for Run<'_, T>
where
    T: DeserializeOwned,
{
    type Output = Result<T, ActorError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let run = self.get_mut();
        run.ensure_started();
        match Pin::new(&mut run.completion).poll(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(Ok(value))) => {
                Poll::Ready(serde_json::from_value(value).map_err(|error| {
                    ActorError::OutputDecode {
                        message: error.to_string(),
                    }
                }))
            }
            Poll::Ready(Ok(Err(error))) => Poll::Ready(Err(error)),
            Poll::Ready(Err(_closed)) => Poll::Ready(Err(ActorError::Unavailable)),
        }
    }
}
