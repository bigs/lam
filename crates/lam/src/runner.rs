use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lam_core::{
    ActorId, ActorState, CodecId, CodecRef, CompactionConfig, CompactionReason, ContextEntry,
    ContextTransition, EncodedPayload, JournalStore, MessageId, MessageSource, ModelDirective,
    ModelEventSink, ModelId, ModelRequestConfig, ModelResponseMetadata, OutputContract, RunId,
    RunProgress, Timestamp,
};
use lam_deno::{EvalOptions, Isolate};
use serde::Serialize;
use tokio::sync::{mpsc, watch};

use crate::actor::{Clock, RuntimeIds};
use crate::command::{CallRequest, RunnerCommand};
use crate::compaction::{estimated_context_tokens, model_context};
use crate::model::RegisteredModel;
use crate::recovery::has_recoverable_work;
use crate::runtime_journal::{
    AppendAttempt, admit_message, append_context, append_event, load_state, state_error,
};
use crate::{ActorError, EvalOutcome, RunEvent, RuntimeEvent};

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
            let state = load_state(self.store.as_ref(), &self.actor_id).await?;
            if !has_recoverable_work(&state) {
                return Ok(());
            }
        }
    }

    async fn handle_call(&mut self, mut call: CallRequest) -> bool {
        let result = self.prepare_call(&mut call).await;
        let aborted = matches!(result, Err(ActorError::Aborted));
        match result {
            Ok(value) => {
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
            let state = load_state(self.store.as_ref(), &self.actor_id).await?;
            if !has_recoverable_work(&state) {
                break;
            }
            self.drive_one(OutputContract::Text, None).await?;
        }

        let mut abort = self.abort.clone();
        let receipt = tokio::select! {
            biased;
            _ = wait_for_abort(&mut abort) => return Err(ActorError::Aborted),
            receipt = admit_message(self.store.as_ref(), &self.actor_id, call.message.clone()) => {
                receipt?
            }
        };
        call.admitted(receipt);
        self.drive_one(call.output.clone(), Some(&call.events))
            .await?
            .ok_or(ActorError::State {
                message: "call input did not start a run".to_owned(),
            })
    }

    async fn drive_one(
        &mut self,
        output: OutputContract,
        events: Option<&mpsc::Sender<RunEvent>>,
    ) -> Result<Option<serde_json::Value>, ActorError> {
        let mut state = load_state(self.store.as_ref(), &self.actor_id).await?;
        let model = self.selected_model(&state)?.clone();
        let run_id = match state.active_run() {
            Some(run_id) => run_id.clone(),
            None => {
                if state.eligible_messages().next().is_none() {
                    return Ok(None);
                }
                self.ids.run_id()
            }
        };
        emit(
            &self.run_events,
            events,
            RunEvent::Started {
                run_id: run_id.clone(),
            },
        );
        loop {
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
                let result = tokio::select! {
                    biased;
                    _ = wait_for_abort(&mut abort) => return Err(ActorError::Aborted),
                    result = model.invoke(request, sink) => result,
                };
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
            let directive = model
                .interpret_response(&response)
                .map_err(|message| ActorError::Codec { message })?;
            let recorded_at = self.clock.now();

            match directive {
                ModelDirective::Eval(request) => {
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
                    let result = tokio::select! {
                        biased;
                        _ = wait_for_abort(&mut abort) => return Err(ActorError::Aborted),
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
                    let outcome = match result {
                        Ok(output) => EvalOutcome::Success { output },
                        Err(error) => EvalOutcome::Failure { error },
                    };
                    emit(
                        &self.run_events,
                        events,
                        RunEvent::EvalCompleted {
                            run_id: run_id.clone(),
                            outcome: outcome.clone(),
                        },
                    );
                    let entry = eval_entry(&run_id, &outcome, self.clock.now())?;
                    state =
                        append_context(self.store.as_ref(), &self.actor_id, state, entry).await?;
                }
                ModelDirective::Output(value) => {
                    match self
                        .append_output_candidate(state, &run_id, response, recorded_at)
                        .await?
                    {
                        OutputAppend::Completed => {
                            emit(&self.run_events, events, RunEvent::Completed { run_id });
                            return Ok(Some(value));
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
            }
        }
    }

    async fn deliver_eligible(
        &self,
        state: ActorState,
        run_id: &RunId,
        events: Option<&mpsc::Sender<RunEvent>>,
    ) -> Result<ActorState, ActorError> {
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
            let payload = messages_payload(&eligible)?;
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
                AppendAttempt::Conflict => {
                    state = load_state(self.store.as_ref(), &self.actor_id).await?;
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
                        AppendAttempt::Appended(_next) => return Ok(OutputAppend::Completed),
                        AppendAttempt::Conflict => {
                            state = load_state(self.store.as_ref(), &self.actor_id).await?;
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
    Completed,
    Continued(ActorState, Vec<MessageId>),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeliveredMessage<'a> {
    message_id: &'a MessageId,
    source: &'a MessageSource,
    payload: &'a EncodedPayload,
}

fn messages_payload(messages: &[lam_core::AdmittedMessage]) -> Result<EncodedPayload, ActorError> {
    let messages = messages
        .iter()
        .map(|message| DeliveredMessage {
            message_id: message.envelope.message_id(),
            source: message.envelope.source(),
            payload: message.envelope.payload(),
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
