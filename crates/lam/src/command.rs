use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lam_core::{MessageEnvelope, ModelId, OutputContract};
use tokio::sync::{mpsc, oneshot};

use crate::{ActorError, MessageReceipt, RunEvent};
use crate::{CompactionReceipt, ModelSwitchPolicy, ModelSwitchReceipt};

pub(crate) enum RunnerCommand {
    Wake,
    Call(Box<CallRequest>),
    Compact(oneshot::Sender<Result<Option<CompactionReceipt>, ActorError>>),
    SwitchModel {
        model_id: ModelId,
        policy: ModelSwitchPolicy,
        completion: oneshot::Sender<Result<ModelSwitchReceipt, ActorError>>,
    },
    Shutdown,
}

pub(crate) struct CallRequest {
    pub(crate) message: MessageEnvelope,
    pub(crate) output: OutputContract,
    pub(crate) events: mpsc::Sender<RunEvent>,
    admission: Option<oneshot::Sender<Result<MessageReceipt, ActorError>>>,
    pub(crate) completion: oneshot::Sender<Result<serde_json::Value, ActorError>>,
    _lease: OperationLease,
}

impl CallRequest {
    pub(crate) fn new(
        message: MessageEnvelope,
        output: OutputContract,
        events: mpsc::Sender<RunEvent>,
        admission: oneshot::Sender<Result<MessageReceipt, ActorError>>,
        completion: oneshot::Sender<Result<serde_json::Value, ActorError>>,
        lease: OperationLease,
    ) -> Self {
        Self {
            message,
            output,
            events,
            admission: Some(admission),
            completion,
            _lease: lease,
        }
    }

    pub(crate) fn emit(&self, event: RunEvent) {
        let _ = self.events.try_send(event);
    }

    pub(crate) fn admitted(&mut self, receipt: MessageReceipt) {
        if let Some(admission) = self.admission.take() {
            let _ = admission.send(Ok(receipt));
        }
    }

    pub(crate) fn fail(mut self, error: ActorError) {
        if let Some(admission) = self.admission.take() {
            let _ = admission.send(Err(error.clone()));
        }
        self.emit(RunEvent::Failed {
            message: error.to_string(),
        });
        let _ = self.completion.send(Err(error));
    }
}

pub(crate) struct OperationLease {
    operation_active: Arc<AtomicBool>,
}

impl OperationLease {
    pub(crate) fn acquire(operation_active: Arc<AtomicBool>) -> Result<Self, ActorError> {
        operation_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| ActorError::Busy)?;
        Ok(Self { operation_active })
    }
}

impl Drop for OperationLease {
    fn drop(&mut self) {
        self.operation_active.store(false, Ordering::Release);
    }
}
