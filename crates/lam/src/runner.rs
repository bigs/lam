use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lam_core::{
    ActorEvent, ActorId, ActorState, CodecId, CodecRef, CompactionConfig, CompactionReason,
    ComponentId, ContextEntry, ContextTransition, DeliveryMode, EncodedPayload, EventBatch,
    JournalStore, MessageEnvelope, MessageId, MessageSource, ModelDirective, ModelEventSink,
    ModelId, ModelRequestConfig, ModelResponseMetadata, OutputContract, RunId, RunProgress,
    Timestamp,
};
use lam_deno::{EvalError, EvalOptions, Isolate};
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tracing::Instrument;

use crate::actor::{Clock, MessageReceipt, ModelSelectionObserver, RuntimeIds};
use crate::command::{CallRequest, RunnerCommand};
use crate::compaction::{estimated_context_tokens, model_context};
use crate::control::{RunControl, RunPhase};
use crate::model::RegisteredModel;
use crate::notice::system_notice_codec;
use crate::recovery::{has_pending_eval, has_recoverable_work};
use crate::runtime_journal::{
    AppendAttempt, admit_message_from_state, append_batch, append_context, append_event,
    load_state, refresh_state, state_error,
};
use crate::{
    ActorError, EvalOutcome, InterruptedEvalOutcome, InterruptionReceipt, IsolateState,
    RUNTIME_COMPONENT_ID, RunEvent, RuntimeEvent, SystemNotice,
};

/// Terminal bound on uninterrupted model self-correction.
///
/// A rejected directive is returned to the model as its call's result so an
/// occasional malformed call costs one round trip instead of the run. A model
/// that keeps sending invalid directives is not converging, so the run fails
/// once this many arrive with no valid directive between them.
const MAX_CONSECUTIVE_REJECTED_DIRECTIVES: u8 = 3;

pub(crate) struct ActorRunner<S> {
    pub(crate) actor_id: ActorId,
    pub(crate) store: Arc<S>,
    pub(crate) models: BTreeMap<ModelId, RegisteredModel>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) ids: Arc<RuntimeIds>,
    pub(crate) isolate: Isolate,
    pub(crate) system_prompt: String,
    pub(crate) compaction_config: CompactionConfig,
    pub(crate) commands: mpsc::UnboundedReceiver<RunnerCommand>,
    pub(crate) abort: watch::Receiver<bool>,
    pub(crate) shutdown: Arc<AtomicBool>,
    pub(crate) run_events: mpsc::Sender<RunEvent>,
    pub(crate) runtime_events: mpsc::Sender<RuntimeEvent>,
    pub(crate) control: Arc<RunControl>,
    pub(crate) model_selection_observer: Option<ModelSelectionObserver>,
    /// Last folded actor projection, refreshed incrementally between drives
    /// so a wake replays only journal activity since the previous drive.
    pub(crate) state: Option<ActorState>,
}

impl<S> ActorRunner<S>
where
    S: JournalStore + 'static,
{
    pub(crate) fn selected_model(
        &self,
        state: &ActorState,
    ) -> Result<&RegisteredModel, ActorError> {
        let selection = state.selected_model().ok_or_else(|| ActorError::State {
            message: "actor journal has no model selection".to_owned(),
        })?;
        let model =
            self.models
                .get(&selection.model_id)
                .ok_or_else(|| ActorError::UnknownModel {
                    model_id: selection.model_id.clone(),
                })?;
        if model.descriptor() != &selection.descriptor {
            return Err(ActorError::State {
                message: format!(
                    "durable model `{}` descriptor does not match the runtime registry",
                    selection.model_id
                ),
            });
        }
        Ok(model)
    }

    pub(crate) fn notify_model_selection(&self, state: &ActorState) {
        if let Some(observer) = &self.model_selection_observer
            && let Some(selection) = state.selected_model()
        {
            observer(selection);
        }
    }

    pub(crate) async fn run(mut self, wake: bool) {
        if wake && let Err(ActorError::Aborted) = self.drain_recovered_work().await {
            return;
        }

        loop {
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }
            let command = tokio::select! {
                biased;
                _ = wait_for_abort(&mut self.abort) => break,
                command = self.commands.recv() => command,
            };
            let Some(command) = command else {
                break;
            };
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }
            match command {
                RunnerCommand::Wake => {
                    if matches!(
                        self.drive_one(OutputContract::Text, None).await,
                        Err(ActorError::Aborted)
                    ) {
                        break;
                    }
                }
                RunnerCommand::Call(call) => {
                    if self.handle_call(*call).await {
                        break;
                    }
                }
                RunnerCommand::Compact(completion) => {
                    let result = self.compact_current().await;
                    let aborted = matches!(result, Err(ActorError::Aborted));
                    let _ = completion.send(result);
                    if aborted {
                        break;
                    }
                }
                RunnerCommand::SwitchModel {
                    model_id,
                    policy,
                    completion,
                } => {
                    let result = self.switch_model(model_id, policy).await;
                    let aborted = matches!(result, Err(ActorError::Aborted));
                    let _ = completion.send(result);
                    if aborted {
                        break;
                    }
                }
                RunnerCommand::Shutdown => break,
            }
        }
    }

    async fn drain_recovered_work(&mut self) -> Result<(), ActorError> {
        loop {
            self.drive_one(OutputContract::Text, None).await?;
            let state = self
                .state
                .as_ref()
                .expect("a successful drive stores its projection");
            if !has_recoverable_work(state) {
                return Ok(());
            }
        }
    }

    async fn handle_call(&mut self, mut call: CallRequest) -> bool {
        let result = self.prepare_call(&mut call).await;
        let aborted = matches!(result, Err(ActorError::Aborted));
        match result {
            Ok(value) => {
                call.release_lease();
                let _ = call.completion.send(Ok(value));
            }
            Err(error) => call.fail(error),
        }
        aborted
    }

    async fn prepare_call(
        &mut self,
        call: &mut CallRequest,
    ) -> Result<serde_json::Value, ActorError> {
        loop {
            let state = refresh_state(
                self.store.as_ref(),
                &self.actor_id,
                self.state.take().unwrap_or_default(),
            )
            .await?;
            if !has_recoverable_work(&state) {
                self.state = Some(state);
                break;
            }
            self.state = Some(state);
            self.drive_one(OutputContract::Text, None).await?;
        }

        let message_id = call.message.message_id().clone();
        if call.admission_cancel.try_recv().is_ok() {
            return Err(ActorError::Aborted);
        }
        let mut abort = self.abort.clone();
        let admission = {
            let admit = self.admit_call(call.message.clone());
            tokio::pin!(admit);
            tokio::select! {
                biased;
                Ok(()) = &mut call.admission_cancel => Ok(None),
                receipt = &mut admit => receipt.map(Some),
                _ = wait_for_abort(&mut abort) => Err(ActorError::Aborted),
            }
        }?;
        let receipt = match admission {
            Some(receipt) => receipt,
            None => {
                // The admission future is dropped before this read. Journal
                // state therefore decides the race: a committed message owns
                // the spawned child, while an absent one was cancelled first.
                let state =
                    refresh_state(self.store.as_ref(), &self.actor_id, ActorState::new()).await?;
                let revision = state.message(&message_id).map(|message| message.revision);
                self.state = Some(state);
                let Some(revision) = revision else {
                    return Err(ActorError::Aborted);
                };
                MessageReceipt {
                    actor_id: self.actor_id.clone(),
                    message_id,
                    revision,
                }
            }
        };
        call.admitted(receipt);
        self.drive_one(call.output.clone(), Some(&call.events))
            .await?
            .ok_or(ActorError::State {
                message: "call input did not start a run".to_owned(),
            })
    }

    /// Admits a call input against the cached projection so calls never
    /// replay the whole journal.
    async fn admit_call(&mut self, message: MessageEnvelope) -> Result<MessageReceipt, ActorError> {
        let state = refresh_state(
            self.store.as_ref(),
            &self.actor_id,
            self.state.take().unwrap_or_default(),
        )
        .await?;
        let (receipt, state) =
            admit_message_from_state(self.store.as_ref(), &self.actor_id, state, message).await?;
        self.state = Some(state);
        Ok(receipt)
    }

    async fn drive_one(
        &mut self,
        output: OutputContract,
        events: Option<&mpsc::Sender<RunEvent>>,
    ) -> Result<Option<serde_json::Value>, ActorError> {
        let state = refresh_state(
            self.store.as_ref(),
            &self.actor_id,
            self.state.take().unwrap_or_default(),
        )
        .await?;
        match self.drive_one_inner(state, output, events).await {
            Ok((value, state)) => {
                self.state = Some(state);
                Ok(value)
            }
            Err(error) => {
                // The projection was consumed by the failed drive. Re-warm it
                // so the next wake refreshes incrementally instead of replaying
                // the whole journal.
                match refresh_state(self.store.as_ref(), &self.actor_id, ActorState::new()).await {
                    Ok(state) => self.state = Some(state),
                    Err(_) => self.state = None,
                }
                Err(error)
            }
        }
    }

    async fn drive_one_inner(
        &mut self,
        mut state: ActorState,
        output: OutputContract,
        events: Option<&mpsc::Sender<RunEvent>>,
    ) -> Result<(Option<serde_json::Value>, ActorState), ActorError> {
        let model = self.selected_model(&state)?.clone();
        let model_id = state
            .selected_model()
            .expect("selected_model succeeded above")
            .model_id
            .clone();
        let descriptor = model.descriptor().clone();
        let run_id = match state.active_run() {
            Some(run_id) => run_id.clone(),
            None => {
                if state.eligible_messages().next().is_none() {
                    return Ok((None, state));
                }
                self.ids.run_id()
            }
        };
        self.control.activate(run_id.clone())?;
        let result = async {
            emit(
                &self.run_events,
                events,
                RunEvent::Started {
                    run_id: run_id.clone(),
                },
            );
            let mut consecutive_rejections: u8 = 0;
            loop {
                if self.control.is_requested(&run_id) {
                    return Err(ActorError::Interrupted);
                }
                state = self.deliver_eligible(state, &run_id, events).await?;
                state = self.maybe_compact(state, events).await?;
                state = self.deliver_eligible(state, &run_id, events).await?;
                let mut overflow_retried = false;
                let response = loop {
                    let context = model_context(&state, &model)?;
                    let config = ModelRequestConfig::agent(&output, &self.system_prompt);
                    let request = model
                        .encode_request(&context, &config)
                        .map_err(|message| ActorError::Codec { message })?;
                    emit(
                        &self.run_events,
                        events,
                        RunEvent::ModelStarted {
                            run_id: run_id.clone(),
                        },
                    );
                    let event_sender = events.cloned();
                    let run_events = self.run_events.clone();
                    let delta_run_id = run_id.clone();
                    let sink = ModelEventSink::new(move |delta| {
                        emit(
                            &run_events,
                            event_sender.as_ref(),
                            RunEvent::ModelDelta {
                                run_id: delta_run_id.clone(),
                                delta,
                            },
                        );
                    });
                    let mut abort = self.abort.clone();
                    let attempt = u8::from(overflow_retried) + 1;
                    let invoke_span = tracing::info_span!(
                        target: "lam::model",
                        "lam.model.request",
                        actor_id = %self.actor_id,
                        run_id = %run_id,
                        registry_model_id = %model_id,
                        provider = descriptor.provider(),
                        model = descriptor.model(),
                        codec = descriptor.codec(),
                        attempt,
                    );
                    if self.control.set_phase(&run_id, RunPhase::Inference)? {
                        return Err(ActorError::Interrupted);
                    }
                    let result = tokio::select! {
                        biased;
                        _ = wait_for_abort(&mut abort) => return Err(ActorError::Aborted),
                        _ = self.control.wait_for_request(&run_id) => {
                            return Err(ActorError::Interrupted)
                        }
                        result = model.invoke(request, sink).instrument(invoke_span) => result,
                    };
                    if self.control.set_phase(&run_id, RunPhase::Boundary)? {
                        return Err(ActorError::Interrupted);
                    }
                    if let Err(error) = &result {
                        tracing::error!(
                            target: "lam::model",
                            event = "model.request_failed",
                            actor_id = %self.actor_id,
                            run_id = %run_id,
                            registry_model_id = %model_id,
                            provider = descriptor.provider(),
                            model = descriptor.model(),
                            codec = descriptor.codec(),
                            attempt,
                            context_overflow = error.context_overflow,
                            "model request failed"
                        );
                    }
                    match result {
                        Ok(response) => break response,
                        Err(error) if error.context_overflow => {
                            if overflow_retried {
                                return Err(ActorError::ContextOverflow);
                            }
                            if model.compactor.is_none() {
                                return Err(ActorError::ContextOverflow);
                            }
                            let before = estimated_context_tokens(&context, &model)?;
                            let (next, receipt) = self
                                .perform_compaction(state, CompactionReason::Overflow, events)
                                .await?;
                            let Some(_receipt) = receipt else {
                                return Err(ActorError::Compaction {
                                    message: "overflow recovery found no context prefix to compact"
                                        .to_owned(),
                                });
                            };
                            let after_context = model_context(&next, &model)?;
                            let after = estimated_context_tokens(&after_context, &model)?;
                            if after >= before {
                                return Err(ActorError::Compaction {
                                    message: format!(
                                        "overflow compaction did not reduce estimated context ({before} to {after} tokens)"
                                    ),
                                });
                            }
                            state = self.deliver_eligible(next, &run_id, events).await?;
                            overflow_retried = true;
                        }
                        Err(error) => {
                            return Err(ActorError::Provider {
                                message: error.message,
                            });
                        }
                    }
                };
                let metadata = model.response_metadata(&response);
                trace_model_completed(&self.actor_id, &run_id, &metadata);
                emit(
                    &self.run_events,
                    events,
                    RunEvent::ModelCompleted {
                        run_id: run_id.clone(),
                        metadata,
                    },
                );
                let projection = model
                    .project_response(&response)
                    .map_err(|message| ActorError::Codec { message })?;
                let rejected_eval_calls = projection.rejected_eval_calls;
                let directive = projection.directive;
                let recorded_at = self.clock.now();

                match directive {
                    ModelDirective::Eval(request) => {
                        consecutive_rejections = 0;
                        let entry = model_entry(&run_id, RunProgress::Continue, response, recorded_at);
                        state =
                            append_context(self.store.as_ref(), &self.actor_id, state, entry).await?;
                        emit(
                            &self.run_events,
                            events,
                            RunEvent::EvalStarted {
                                run_id: run_id.clone(),
                                request: request.clone(),
                            },
                        );
                        let mut abort = self.abort.clone();
                        if self.control.set_phase(&run_id, RunPhase::Eval)? {
                            return Err(ActorError::Interrupted);
                        }
                        let result = tokio::select! {
                            biased;
                            _ = wait_for_abort(&mut abort) => return Err(ActorError::Aborted),
                            _ = self.control.wait_for_request(&run_id) => {
                                return Err(ActorError::Interrupted)
                            }
                            result = async {
                                match request.timeout {
                                    Some(timeout) => {
                                        self.isolate
                                            .eval_with(
                                                &request.source,
                                                EvalOptions::default().timeout(timeout),
                                            )
                                            .await
                                    }
                                    None => self.isolate.eval(&request.source).await,
                                }
                            } => result,
                        };
                        if self.control.set_phase(&run_id, RunPhase::Boundary)? {
                            return Err(ActorError::Interrupted);
                        }
                        let outcome = match result {
                            Ok(output) => EvalOutcome::Success { output },
                            Err(error) => EvalOutcome::Failure { error },
                        };
                        let mut outcomes = Vec::with_capacity(rejected_eval_calls + 1);
                        outcomes.push(outcome);
                        outcomes.extend(
                            std::iter::repeat_with(EvalOutcome::parallel_tool_call_rejected)
                                .take(rejected_eval_calls),
                        );
                        // Commit before emitting: like every other boundary
                        // event, EvalCompleted reports a durable fact, so
                        // subscribers may render from the journal on receipt.
                        state = self.append_eval_outcomes(state, &run_id, &outcomes).await?;
                        for outcome in outcomes {
                            emit(
                                &self.run_events,
                                events,
                                RunEvent::EvalCompleted {
                                    run_id: run_id.clone(),
                                    outcome,
                                },
                            );
                        }
                    }
                    ModelDirective::Output(value) => {
                        consecutive_rejections = 0;
                        match self
                            .append_output_candidate(state, &run_id, response, recorded_at)
                            .await?
                        {
                        OutputAppend::Completed(state) => {
                            emit(
                                &self.run_events,
                                events,
                                RunEvent::Completed {
                                    run_id: run_id.clone(),
                                },
                            );
                            return Ok((Some(value), state));
                        }
                            OutputAppend::Continued(next, delivered) => {
                                state = next;
                                emit(
                                    &self.run_events,
                                    events,
                                    RunEvent::MessagesDelivered {
                                        run_id: run_id.clone(),
                                        message_ids: delivered,
                                    },
                                );
                            }
                        }
                    }
                    ModelDirective::Rejected { message } => {
                        consecutive_rejections += 1;
                        let entry = model_entry(&run_id, RunProgress::Continue, response, recorded_at);
                        state =
                            append_context(self.store.as_ref(), &self.actor_id, state, entry).await?;
                        let mut outcomes = Vec::with_capacity(rejected_eval_calls + 1);
                        outcomes.push(EvalOutcome::Rejected { message });
                        outcomes.extend(
                            std::iter::repeat_with(EvalOutcome::parallel_tool_call_rejected)
                                .take(rejected_eval_calls),
                        );
                        // Commit before emitting, and even when the cap below
                        // fails the run: every native call has its durable
                        // rejection result, so the journal replays cleanly.
                        state = self.append_eval_outcomes(state, &run_id, &outcomes).await?;
                        for outcome in outcomes {
                            emit(
                                &self.run_events,
                                events,
                                RunEvent::EvalCompleted {
                                    run_id: run_id.clone(),
                                    outcome,
                                },
                            );
                        }
                        if consecutive_rejections >= MAX_CONSECUTIVE_REJECTED_DIRECTIVES {
                            return Err(ActorError::Codec {
                                message: format!(
                                    "the model sent {consecutive_rejections} consecutive invalid directives"
                                ),
                            });
                        }
                    }
                }
            }
        }
        .await;

        if self.control.is_requested(&run_id)
            && !matches!(&result, Err(ActorError::Aborted) | Ok((Some(_), _)))
        {
            let eval_terminated = self.control.eval_was_terminated(&run_id);
            let interrupted = self.persist_interruption(&run_id, eval_terminated).await;
            return match interrupted {
                Ok(receipt) => {
                    self.control.finish(&run_id, Ok(Some(receipt)));
                    Err(ActorError::Interrupted)
                }
                Err(error) => {
                    self.control.finish(&run_id, Err(error.clone()));
                    Err(error)
                }
            };
        }

        let control_result = match &result {
            Err(error) => Err(error.clone()),
            Ok(_) => Ok(None),
        };
        self.control.finish(&run_id, control_result);
        // Wake-driven runs have no per-call channel to report through, so a
        // failure would otherwise be invisible to actor-event subscribers.
        if events.is_none()
            && let Err(error) = &result
            && !matches!(error, ActorError::Aborted | ActorError::Interrupted)
        {
            emit(
                &self.run_events,
                None,
                RunEvent::Failed {
                    message: error.to_string(),
                },
            );
        }
        result
    }

    async fn persist_interruption(
        &mut self,
        run_id: &RunId,
        eval_terminated: bool,
    ) -> Result<InterruptionReceipt, ActorError> {
        let isolate_state = if eval_terminated {
            IsolateState::Reset
        } else {
            IsolateState::Retained
        };
        let interrupted_eval_error = eval_terminated.then(|| {
            let previous_generation = self.isolate.generation();
            match self.isolate.restart_after_interruption() {
                Ok(new_generation) => EvalError::Interrupted {
                    effects_may_have_completed: true,
                    previous_generation,
                    new_generation,
                },
                Err(error) => error,
            }
        });
        let notice_message_id = self.ids.message_id();
        let recorded_at = self.clock.now();

        let mut state = load_state(self.store.as_ref(), &self.actor_id).await?;
        loop {
            if state.active_run() != Some(run_id) {
                return Err(ActorError::State {
                    message: format!(
                        "run `{run_id}` completed before its interruption boundary was recorded"
                    ),
                });
            }
            let model = self.selected_model(&state)?;
            let pending_eval = has_pending_eval(&state, model);
            let interrupted_eval_outcome =
                pending_eval.then_some(InterruptedEvalOutcome::FailureRecorded);
            let notice = SystemNotice::run_interrupted(
                run_id.clone(),
                isolate_state,
                interrupted_eval_outcome,
            );
            let notice = MessageEnvelope::new(
                notice_message_id.clone(),
                MessageSource::Host {
                    component: ComponentId::new(RUNTIME_COMPONENT_ID)
                        .expect("Lam's runtime component id is valid"),
                },
                DeliveryMode::Steer,
                EncodedPayload::new(
                    system_notice_codec(),
                    serde_json::to_value(notice).map_err(|error| ActorError::State {
                        message: format!("interruption notice could not be encoded: {error}"),
                    })?,
                ),
                recorded_at,
            )
            .map_err(|error| ActorError::State {
                message: error.to_string(),
            })?;

            let delivered = state
                .eligible_messages()
                .map(|message| &message.envelope)
                .chain(std::iter::once(&notice))
                .collect::<Vec<_>>();
            let consumed_message_ids = delivered
                .iter()
                .map(|message| message.message_id().clone())
                .collect();
            let terminal = ContextEntry {
                transition: ContextTransition::Interrupted {
                    run_id: run_id.clone(),
                    consumed_message_ids,
                },
                payload: messages_payload(delivered.iter().copied())?,
                recorded_at,
            };
            let mut remaining = Vec::with_capacity(usize::from(pending_eval) + 1);
            if pending_eval {
                let generation = self.isolate.generation();
                let error = interrupted_eval_error
                    .clone()
                    .unwrap_or(EvalError::Interrupted {
                        effects_may_have_completed: false,
                        previous_generation: generation,
                        new_generation: generation,
                    });
                let outcome = EvalOutcome::Failure { error };
                remaining.push(ActorEvent::context_appended(eval_entry(
                    run_id,
                    &outcome,
                    recorded_at,
                )?));
            }
            remaining.push(ActorEvent::context_appended(terminal));
            let batch = EventBatch::new(ActorEvent::message_admitted(notice), remaining);

            match append_batch(self.store.as_ref(), &self.actor_id, state, batch).await? {
                AppendAttempt::Appended(next) => {
                    return Ok(InterruptionReceipt {
                        actor_id: self.actor_id.clone(),
                        run_id: run_id.clone(),
                        notice_message_id,
                        revision: next.revision(),
                        isolate_state,
                        interrupted_eval_outcome,
                    });
                }
                AppendAttempt::Conflict(conflicted) => {
                    state = refresh_state(self.store.as_ref(), &self.actor_id, conflicted).await?;
                }
            }
        }
    }

    async fn deliver_eligible(
        &self,
        state: ActorState,
        run_id: &RunId,
        events: Option<&mpsc::Sender<RunEvent>>,
    ) -> Result<ActorState, ActorError> {
        // Refresh from the journal head first: mail admitted since the last
        // append is otherwise invisible until a CAS collision, and this is
        // the boundary where a steer must become model-visible.
        let state = refresh_state(self.store.as_ref(), &self.actor_id, state).await?;
        if state.eligible_messages().next().is_none() {
            return Ok(state);
        }
        let (state, delivered) = self.append_messages(state, run_id).await?;
        emit(
            &self.run_events,
            events,
            RunEvent::MessagesDelivered {
                run_id: run_id.clone(),
                message_ids: delivered,
            },
        );
        Ok(state)
    }

    async fn append_eval_outcomes(
        &self,
        mut state: ActorState,
        run_id: &RunId,
        outcomes: &[EvalOutcome],
    ) -> Result<ActorState, ActorError> {
        let entries = outcomes
            .iter()
            .map(|outcome| eval_entry(run_id, outcome, self.clock.now()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut events = entries.into_iter().map(ActorEvent::context_appended);
        let first = events.next().ok_or_else(|| ActorError::State {
            message: "an eval response produced no outcomes".to_owned(),
        })?;
        let batch = EventBatch::new(first, events.collect());

        loop {
            match append_batch(self.store.as_ref(), &self.actor_id, state, batch.clone()).await? {
                AppendAttempt::Appended(next) => return Ok(next),
                AppendAttempt::Conflict(conflicted) => {
                    state = refresh_state(self.store.as_ref(), &self.actor_id, conflicted).await?;
                }
            }
        }
    }

    async fn append_messages(
        &self,
        mut state: ActorState,
        run_id: &RunId,
    ) -> Result<(ActorState, Vec<MessageId>), ActorError> {
        let recorded_at = self.clock.now();
        loop {
            let eligible = state.eligible_messages().cloned().collect::<Vec<_>>();
            let message_ids = eligible
                .iter()
                .map(|message| message.envelope.message_id().clone())
                .collect::<Vec<_>>();
            if message_ids.is_empty() {
                return Ok((state, message_ids));
            }
            let payload = messages_payload(eligible.iter().map(|message| &message.envelope))?;
            let entry = ContextEntry {
                transition: ContextTransition::Messages {
                    run_id: run_id.clone(),
                    consumed_message_ids: message_ids.clone(),
                },
                payload,
                recorded_at,
            };
            let event = state.plan_context_append(entry).map_err(state_error)?;
            match append_event(self.store.as_ref(), &self.actor_id, state, event).await? {
                AppendAttempt::Appended(next) => return Ok((next, message_ids)),
                AppendAttempt::Conflict(conflicted) => {
                    state = refresh_state(self.store.as_ref(), &self.actor_id, conflicted).await?;
                }
            }
        }
    }

    async fn append_output_candidate(
        &self,
        mut state: ActorState,
        run_id: &RunId,
        response: EncodedPayload,
        recorded_at: Timestamp,
    ) -> Result<OutputAppend, ActorError> {
        loop {
            let terminal =
                model_entry(run_id, RunProgress::Complete, response.clone(), recorded_at);
            match state.plan_context_append(terminal) {
                Ok(event) => {
                    match append_event(self.store.as_ref(), &self.actor_id, state, event).await? {
                        AppendAttempt::Appended(next) => {
                            return Ok(OutputAppend::Completed(next));
                        }
                        AppendAttempt::Conflict(conflicted) => {
                            state = refresh_state(self.store.as_ref(), &self.actor_id, conflicted)
                                .await?;
                        }
                    }
                }
                Err(lam_core::StateError::TerminalWithPendingSteer { .. }) => {
                    let continuing =
                        model_entry(run_id, RunProgress::Continue, response, recorded_at);
                    state = append_context(self.store.as_ref(), &self.actor_id, state, continuing)
                        .await?;
                    let (state, delivered) = self.append_messages(state, run_id).await?;
                    return Ok(OutputAppend::Continued(state, delivered));
                }
                Err(error) => return Err(state_error(error)),
            }
        }
    }
}

fn trace_model_completed(actor_id: &ActorId, run_id: &RunId, metadata: &ModelResponseMetadata) {
    let usage = metadata.usage.as_ref();
    let cost = metadata.cost.as_ref();
    tracing::info!(
        target: "lam::model",
        actor_id = %actor_id,
        run_id = %run_id,
        model = metadata.model.as_deref().unwrap_or("unknown"),
        input_tokens = ?usage.map(|usage| usage.input_tokens),
        cached_input_tokens = ?usage.and_then(|usage| usage.cached_input_tokens),
        output_tokens = ?usage.map(|usage| usage.output_tokens),
        reasoning_tokens = ?usage.and_then(|usage| usage.reasoning_tokens),
        total_tokens = ?usage.map(|usage| usage.total_tokens),
        cost_usd = ?cost.map(|cost| cost.amount_usd),
        cost_source = ?cost.map(|cost| cost.source),
        "model request completed"
    );
}

pub(crate) async fn wait_for_abort(abort: &mut watch::Receiver<bool>) {
    if *abort.borrow() {
        return;
    }
    loop {
        if abort.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        if *abort.borrow_and_update() {
            return;
        }
    }
}

enum OutputAppend {
    Completed(ActorState),
    Continued(ActorState, Vec<MessageId>),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeliveredMessage<'a> {
    message_id: &'a MessageId,
    source: &'a MessageSource,
    payload: &'a EncodedPayload,
}

fn messages_payload<'a>(
    messages: impl IntoIterator<Item = &'a MessageEnvelope>,
) -> Result<EncodedPayload, ActorError> {
    let messages = messages
        .into_iter()
        .map(|message| DeliveredMessage {
            message_id: message.message_id(),
            source: message.source(),
            payload: message.payload(),
        })
        .collect::<Vec<_>>();
    let value = serde_json::to_value(messages).map_err(|error| ActorError::State {
        message: format!("message context could not be encoded: {error}"),
    })?;
    Ok(EncodedPayload::new(codec("lam/messages"), value))
}

fn eval_entry(
    run_id: &RunId,
    outcome: &EvalOutcome,
    recorded_at: Timestamp,
) -> Result<ContextEntry, ActorError> {
    let value = serde_json::to_value(outcome).map_err(|error| ActorError::State {
        message: format!("eval outcome could not be encoded: {error}"),
    })?;
    Ok(ContextEntry {
        transition: ContextTransition::Eval {
            run_id: run_id.clone(),
        },
        payload: EncodedPayload::new(codec("lam/eval"), value),
        recorded_at,
    })
}

fn model_entry(
    run_id: &RunId,
    progress: RunProgress,
    payload: EncodedPayload,
    recorded_at: Timestamp,
) -> ContextEntry {
    ContextEntry {
        transition: ContextTransition::Model {
            run_id: run_id.clone(),
            progress,
        },
        payload,
        recorded_at,
    }
}

fn codec(id: &str) -> CodecRef {
    CodecRef::new(
        CodecId::new(id).expect("Lam's built-in codec ids are valid"),
        1,
    )
}

pub(crate) fn emit(
    actor_events: &mpsc::Sender<RunEvent>,
    events: Option<&mpsc::Sender<RunEvent>>,
    event: RunEvent,
) {
    let _ = actor_events.try_send(event.clone());
    if let Some(events) = events {
        let _ = events.try_send(event);
    }
}
