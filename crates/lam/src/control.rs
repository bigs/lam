use std::sync::{Mutex, MutexGuard};

use lam_core::RunId;
use lam_deno::IsolateInterrupt;
use tokio::sync::{Notify, oneshot};

use crate::{ActorError, InterruptionReceipt};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunPhase {
    Boundary,
    Inference,
    Eval,
}

pub(crate) struct RunControl {
    state: Mutex<ControlState>,
    changed: Notify,
    isolate_interrupt: IsolateInterrupt,
}

#[derive(Default)]
struct ControlState {
    active: Option<ActiveRun>,
}

struct ActiveRun {
    run_id: RunId,
    phase: RunPhase,
    requested: bool,
    eval_terminated: bool,
    waiters: Vec<InterruptionWaiter>,
}

type InterruptionResult = Result<Option<InterruptionReceipt>, ActorError>;
type InterruptionWaiter = oneshot::Sender<InterruptionResult>;

impl RunControl {
    pub(crate) fn new(isolate_interrupt: IsolateInterrupt) -> Self {
        Self {
            state: Mutex::new(ControlState::default()),
            changed: Notify::new(),
            isolate_interrupt,
        }
    }

    pub(crate) fn activate(&self, run_id: RunId) -> Result<(), ActorError> {
        let mut state = self.lock();
        if state.active.is_some() {
            return Err(ActorError::State {
                message: "a process-local run control was already active".to_owned(),
            });
        }
        state.active = Some(ActiveRun {
            run_id,
            phase: RunPhase::Boundary,
            requested: false,
            eval_terminated: false,
            waiters: Vec::new(),
        });
        Ok(())
    }

    pub(crate) fn set_phase(&self, run_id: &RunId, phase: RunPhase) -> Result<bool, ActorError> {
        let mut state = self.lock();
        let active = active_mut(&mut state, run_id)?;
        active.phase = phase;
        Ok(active.requested)
    }

    pub(crate) fn is_requested(&self, run_id: &RunId) -> bool {
        self.lock()
            .active
            .as_ref()
            .is_some_and(|active| active.run_id == *run_id && active.requested)
    }

    pub(crate) fn eval_was_terminated(&self, run_id: &RunId) -> bool {
        self.lock()
            .active
            .as_ref()
            .is_some_and(|active| active.run_id == *run_id && active.eval_terminated)
    }

    pub(crate) async fn wait_for_request(&self, run_id: &RunId) {
        loop {
            let notified = self.changed.notified();
            if self.is_requested(run_id) {
                return;
            }
            notified.await;
        }
    }

    pub(crate) async fn interrupt(&self) -> InterruptionResult {
        let (receiver, terminate_eval) = {
            let mut state = self.lock();
            let Some(active) = state.active.as_mut() else {
                return Ok(None);
            };
            let (sender, receiver) = oneshot::channel();
            active.waiters.push(sender);
            let mut terminate_eval = false;
            if !active.requested {
                active.requested = true;
                if active.phase == RunPhase::Eval {
                    active.eval_terminated = true;
                    terminate_eval = true;
                }
                self.changed.notify_waiters();
            }
            (receiver, terminate_eval)
        };
        if terminate_eval {
            self.isolate_interrupt.terminate();
        }
        receiver.await.unwrap_or(Err(ActorError::Unavailable))
    }

    pub(crate) fn finish(&self, run_id: &RunId, result: InterruptionResult) {
        let waiters = {
            let mut state = self.lock();
            let Some(active) = state.active.take() else {
                return;
            };
            debug_assert_eq!(&active.run_id, run_id);
            active.waiters
        };
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
        self.changed.notify_waiters();
    }

    fn lock(&self) -> MutexGuard<'_, ControlState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn active_mut<'a>(
    state: &'a mut ControlState,
    run_id: &RunId,
) -> Result<&'a mut ActiveRun, ActorError> {
    let active = state.active.as_mut().ok_or_else(|| ActorError::State {
        message: "the process-local run control is not active".to_owned(),
    })?;
    if active.run_id != *run_id {
        return Err(ActorError::State {
            message: format!(
                "run control `{}` does not match active run `{}`",
                run_id, active.run_id
            ),
        });
    }
    Ok(active)
}
