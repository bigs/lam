use std::sync::Arc;

use lam_core::{
    ActorId, ActorState, CompactionArtifact, CompactionOutput, CompactionPlan, CompactionReason,
    CompactionRecord, CompactionRequest, ContextEntry, ContextTransition, EventBatch, JournalStore,
    ModelId, ModelRequestConfig, ModelSelection, OutputContract, Revision, RunId, Timestamp,
    compaction_prefix_len,
};
use tokio::sync::mpsc;

use crate::compaction::{
    CompactionReceipt, compaction_request, estimated_context_tokens, model_context,
};
use crate::control::RunPhase;
use crate::model::RegisteredModel;
use crate::runner::{ActorRunner, emit, wait_for_abort};
use crate::runtime_journal::{
    AppendAttempt, append_batch, append_context, append_event, load_state, refresh_state,
    write_checkpoint,
};
use crate::{ActorError, ModelSwitchPolicy, ModelSwitchReceipt, RunEvent, RuntimeEvent};

impl<S> ActorRunner<S>
where
    S: JournalStore + 'static,
{
    pub(crate) async fn compact_current(
        &mut self,
    ) -> Result<Option<CompactionReceipt>, ActorError> {
        let state = load_state(self.store.as_ref(), &self.actor_id).await?;
        if self.selected_model(&state)?.compactor.is_none() {
            return Err(ActorError::CompactionDisabled);
        }
        let (_state, receipt) = self
            .perform_compaction(state, CompactionReason::Manual, None)
            .await?;
        Ok(receipt)
    }

    pub(crate) async fn maybe_compact(
        &mut self,
        state: ActorState,
        events: Option<&mpsc::Sender<RunEvent>>,
    ) -> Result<ActorState, ActorError> {
        let model = self.selected_model(&state)?.clone();
        if model.compactor.is_none() {
            return Ok(state);
        }
        let compaction_config = model.compaction_config(&self.compaction_config);
        let trigger = compaction_config
            .automatic_trigger_tokens()
            .map_err(compaction_config_error)?;
        let Some(trigger) = trigger else {
            return Ok(state);
        };
        let context = model_context(&state, &model)?;
        let before = estimated_context_tokens(&context, &model)?;
        if before < trigger {
            return Ok(state);
        }
        let (next, receipt) = self
            .perform_compaction(state, CompactionReason::Threshold, events)
            .await?;
        let Some(_receipt) = receipt else {
            return Err(ActorError::Compaction {
                message: "threshold was crossed but no context prefix could be compacted"
                    .to_owned(),
            });
        };
        let context = model_context(&next, &model)?;
        let after = estimated_context_tokens(&context, &model)?;
        if after >= before || after >= trigger {
            return Err(ActorError::Compaction {
                message: format!(
                    "threshold compaction left estimated context at {after} tokens (before {before}, trigger {trigger})"
                ),
            });
        }
        Ok(next)
    }

    pub(crate) async fn perform_compaction(
        &mut self,
        state: ActorState,
        reason: CompactionReason,
        events: Option<&mpsc::Sender<RunEvent>>,
    ) -> Result<(ActorState, Option<CompactionReceipt>), ActorError> {
        let model = self.selected_model(&state)?.clone();
        let Some(compactor) = model.compactor.as_ref().map(Arc::clone) else {
            return match reason {
                CompactionReason::Manual | CompactionReason::ModelSwitch => {
                    Err(ActorError::CompactionDisabled)
                }
                CompactionReason::Threshold => Ok((state, None)),
                CompactionReason::Overflow => Err(ActorError::ContextOverflow),
            };
        };
        let compaction_config = model.compaction_config(&self.compaction_config);
        let retain_tokens = compaction_config
            .retain_tokens()
            .map_err(compaction_config_error)?;
        let request = compaction_request(
            &state,
            reason,
            retain_tokens,
            compaction_config.summary_reserve(),
            &self.system_prompt,
            None,
            |replacement| model.accepts_compaction_replacement(replacement),
        )?;
        if compaction_prefix_len(&request.units, request.retain_tokens).is_none() {
            return Ok((state, None));
        }

        let run_id = state.active_run().cloned();
        let plan = self
            .invoke_compactor(compactor, &request, events, run_id.as_ref(), reason)
            .await?;
        let installation: Result<_, ActorError> = async {
            let prepared = prepare_compaction(
                &request,
                &model,
                plan,
                reason,
                run_id.clone(),
                self.clock.now(),
            )?;
            let marker_revision =
                state
                    .revision()
                    .checked_advance(1)
                    .ok_or_else(|| ActorError::State {
                        message: "actor journal revision space is exhausted".to_owned(),
                    })?;
            let receipt = prepared.receipt(marker_revision);
            let next =
                append_context(self.store.as_ref(), &self.actor_id, state, prepared.entry).await?;
            // Best-effort: a checkpoint lets a later cold load bootstrap from
            // this compaction boundary instead of replaying the journal.
            if let Err(error) = write_checkpoint(self.store.as_ref(), &self.actor_id, &next).await {
                tracing::warn!(
                    target: "lam::compaction",
                    actor_id = %self.actor_id,
                    %error,
                    "checkpoint write failed; cold loads will replay the journal"
                );
            }
            Ok((next, receipt))
        }
        .await;
        let (next, receipt) = match installation {
            Ok(installed) => installed,
            Err(error) => {
                self.emit_compaction_failed(events, run_id.as_ref(), reason, error.to_string());
                return Err(error);
            }
        };
        trace_compaction_completed(&self.actor_id, run_id.as_ref(), &receipt);
        self.emit_compaction_completed(events, run_id.as_ref(), &receipt);
        Ok((next, Some(receipt)))
    }

    pub(crate) async fn switch_model(
        &mut self,
        target_id: ModelId,
        policy: ModelSwitchPolicy,
    ) -> Result<ModelSwitchReceipt, ActorError> {
        let target =
            self.models
                .get(&target_id)
                .cloned()
                .ok_or_else(|| ActorError::UnknownModel {
                    model_id: target_id.clone(),
                })?;

        let mut state = load_state(self.store.as_ref(), &self.actor_id).await?;
        loop {
            let source = self.selected_model(&state)?.clone();
            let previous_id = state
                .selected_model()
                .expect("selected_model validated the durable selection")
                .model_id
                .clone();
            if previous_id == target_id {
                return Ok(ModelSwitchReceipt {
                    previous_model_id: previous_id,
                    selected_model_id: target_id,
                    revision: state.revision(),
                    compaction: None,
                });
            }
            let selection = ModelSelection::new(target_id.clone(), target.descriptor().clone());
            let selection_event = state
                .plan_model_selection(selection)
                .map_err(crate::runtime_journal::state_error)?;

            if policy == ModelSwitchPolicy::ReuseContext {
                let context = model_context(&state, &target)?;
                target
                    .encode_request(
                        &context,
                        &ModelRequestConfig::agent(&OutputContract::Text, &self.system_prompt),
                    )
                    .map_err(|message| ActorError::Codec { message })?;
            } else {
                let request = compaction_request(
                    &state,
                    CompactionReason::ModelSwitch,
                    0,
                    self.compaction_config.summary_reserve(),
                    &self.system_prompt,
                    Some(target.descriptor().clone()),
                    |replacement| source.accepts_compaction_replacement(replacement),
                )?;
                if !request.units.is_empty() {
                    let Some(compactor) = source.compactor.as_ref().map(Arc::clone) else {
                        return Err(ActorError::CompactionDisabled);
                    };
                    let plan = self
                        .invoke_compactor(
                            compactor,
                            &request,
                            None,
                            None,
                            CompactionReason::ModelSwitch,
                        )
                        .await?;
                    let installation: Result<_, ActorError> = async {
                        let prepared = prepare_compaction(
                            &request,
                            &target,
                            plan,
                            CompactionReason::ModelSwitch,
                            None,
                            self.clock.now(),
                        )?;
                        let marker_revision =
                            state.revision().checked_advance(1).ok_or_else(|| {
                                ActorError::State {
                                    message: "actor journal revision space is exhausted".to_owned(),
                                }
                            })?;
                        let compaction = prepared.receipt(marker_revision);
                        let context_event = state
                            .plan_context_append(prepared.entry)
                            .map_err(crate::runtime_journal::state_error)?;
                        let batch = EventBatch::new(context_event, vec![selection_event]);
                        let append =
                            append_batch(self.store.as_ref(), &self.actor_id, state, batch).await?;
                        Ok((append, compaction))
                    }
                    .await;
                    let (append, compaction) = match installation {
                        Ok(installed) => installed,
                        Err(error) => {
                            self.emit_compaction_failed(
                                None,
                                None,
                                CompactionReason::ModelSwitch,
                                error.to_string(),
                            );
                            return Err(error);
                        }
                    };
                    match append {
                        AppendAttempt::Appended(next) => {
                            self.notify_model_selection(&next);
                            if let Err(error) =
                                write_checkpoint(self.store.as_ref(), &self.actor_id, &next).await
                            {
                                tracing::warn!(
                                    target: "lam::compaction",
                                    actor_id = %self.actor_id,
                                    %error,
                                    "checkpoint write failed; cold loads will replay the journal"
                                );
                            }
                            trace_compaction_completed(&self.actor_id, None, &compaction);
                            self.emit_compaction_completed(None, None, &compaction);
                            return Ok(ModelSwitchReceipt {
                                previous_model_id: previous_id,
                                selected_model_id: target_id,
                                revision: next.revision(),
                                compaction: Some(compaction),
                            });
                        }
                        AppendAttempt::Conflict(conflicted) => {
                            state = refresh_state(self.store.as_ref(), &self.actor_id, conflicted)
                                .await?;
                            continue;
                        }
                    }
                }
            }

            match append_event(self.store.as_ref(), &self.actor_id, state, selection_event).await? {
                AppendAttempt::Appended(next) => {
                    self.notify_model_selection(&next);
                    return Ok(ModelSwitchReceipt {
                        previous_model_id: previous_id,
                        selected_model_id: target_id,
                        revision: next.revision(),
                        compaction: None,
                    });
                }
                AppendAttempt::Conflict(conflicted) => {
                    state = refresh_state(self.store.as_ref(), &self.actor_id, conflicted).await?;
                    continue;
                }
            }
        }
    }

    async fn invoke_compactor(
        &mut self,
        compactor: Arc<dyn lam_core::Compactor>,
        request: &CompactionRequest,
        events: Option<&mpsc::Sender<RunEvent>>,
        run_id: Option<&RunId>,
        reason: CompactionReason,
    ) -> Result<CompactionPlan, ActorError> {
        self.emit_compaction_started(events, run_id, reason);
        let mut abort = self.abort.clone();
        let result = if let Some(run_id) = run_id {
            if self.control.set_phase(run_id, RunPhase::Inference)? {
                return Err(ActorError::Interrupted);
            }
            let result = tokio::select! {
                biased;
                _ = wait_for_abort(&mut abort) => return Err(ActorError::Aborted),
                _ = self.control.wait_for_request(run_id) => {
                    return Err(ActorError::Interrupted)
                }
                result = compactor.compact(request) => result,
            };
            if self.control.set_phase(run_id, RunPhase::Boundary)? {
                return Err(ActorError::Interrupted);
            }
            result
        } else {
            tokio::select! {
                biased;
                _ = wait_for_abort(&mut abort) => return Err(ActorError::Aborted),
                result = compactor.compact(request) => result,
            }
        };
        result.map_err(|error| {
            let message = error.to_string();
            self.emit_compaction_failed(events, run_id, reason, message.clone());
            ActorError::Compaction { message }
        })
    }

    fn emit_compaction_started(
        &self,
        events: Option<&mpsc::Sender<RunEvent>>,
        run_id: Option<&RunId>,
        reason: CompactionReason,
    ) {
        let _ = self
            .runtime_events
            .try_send(RuntimeEvent::CompactionStarted {
                run_id: run_id.cloned(),
                reason,
            });
        if let Some(run_id) = run_id {
            emit(
                &self.run_events,
                events,
                RunEvent::CompactionStarted {
                    run_id: run_id.clone(),
                    reason,
                },
            );
        }
    }

    fn emit_compaction_completed(
        &self,
        events: Option<&mpsc::Sender<RunEvent>>,
        run_id: Option<&RunId>,
        receipt: &CompactionReceipt,
    ) {
        let _ = self
            .runtime_events
            .try_send(RuntimeEvent::CompactionCompleted {
                run_id: run_id.cloned(),
                reason: receipt.reason,
                covers_through: receipt.covers_through,
                revision: receipt.revision,
                strategy: receipt.strategy.clone(),
                metadata: receipt.metadata.clone(),
            });
        if let Some(run_id) = run_id {
            emit(
                &self.run_events,
                events,
                RunEvent::CompactionCompleted {
                    run_id: run_id.clone(),
                    reason: receipt.reason,
                    covers_through: receipt.covers_through,
                    metadata: receipt.metadata.clone(),
                },
            );
        }
    }

    fn emit_compaction_failed(
        &self,
        events: Option<&mpsc::Sender<RunEvent>>,
        run_id: Option<&RunId>,
        reason: CompactionReason,
        message: String,
    ) {
        let _ = self
            .runtime_events
            .try_send(RuntimeEvent::CompactionFailed {
                run_id: run_id.cloned(),
                reason,
                message: message.clone(),
            });
        if let Some(run_id) = run_id {
            emit(
                &self.run_events,
                events,
                RunEvent::CompactionFailed {
                    run_id: run_id.clone(),
                    reason,
                    message,
                },
            );
        }
    }
}

struct PreparedCompaction {
    entry: ContextEntry,
    covers_through: lam_core::ContextSequence,
    reason: CompactionReason,
    strategy: String,
    metadata: lam_core::ModelResponseMetadata,
}

impl PreparedCompaction {
    fn receipt(&self, revision: Revision) -> CompactionReceipt {
        CompactionReceipt {
            covers_through: self.covers_through,
            revision,
            reason: self.reason,
            strategy: self.strategy.clone(),
            metadata: self.metadata.clone(),
        }
    }
}

fn prepare_compaction(
    request: &CompactionRequest,
    target: &RegisteredModel,
    plan: CompactionPlan,
    reason: CompactionReason,
    run_id: Option<RunId>,
    recorded_at: Timestamp,
) -> Result<PreparedCompaction, ActorError> {
    if !request
        .units
        .iter()
        .any(|unit| unit.covers_through() == plan.covers_through)
    {
        return Err(ActorError::Compaction {
            message: format!(
                "compactor cutoff {} is not an atomic context boundary",
                plan.covers_through.get()
            ),
        });
    }
    let (artifact, replacement) = match plan.output {
        CompactionOutput::Artifact(artifact) => {
            if artifact.is_empty() {
                return Err(ActorError::Compaction {
                    message: "compactor returned an empty artifact".to_owned(),
                });
            }
            let replacement = target
                .materialize_compaction(&artifact)
                .map_err(|message| ActorError::Compaction { message })?
                .ok_or_else(|| ActorError::Compaction {
                    message: "the target model codec cannot materialize compaction context"
                        .to_owned(),
                })?;
            (Some(artifact), replacement)
        }
        CompactionOutput::Exact {
            replacement,
            artifact,
        } => {
            if artifact.as_ref().is_some_and(CompactionArtifact::is_empty) {
                return Err(ActorError::Compaction {
                    message: "compactor returned an empty companion artifact".to_owned(),
                });
            }
            (artifact, replacement)
        }
    };
    if !target.accepts_compaction_replacement(&replacement) {
        return Err(ActorError::Compaction {
            message: format!(
                "the target model codec rejected its materialized replacement {}@{}",
                replacement.codec.id, replacement.codec.version
            ),
        });
    }
    let strategy = plan.strategy;
    let covers_through = plan.covers_through;
    let metadata = plan.metadata;
    let record = CompactionRecord {
        strategy: strategy.clone(),
        reason,
        source: plan.source,
        artifact,
        replacement,
        metadata: metadata.clone(),
    };
    let payload = record.encode().map_err(|error| ActorError::Compaction {
        message: format!("compaction record could not be encoded: {error}"),
    })?;
    Ok(PreparedCompaction {
        entry: ContextEntry {
            transition: ContextTransition::Compaction {
                covers_through,
                run_id,
            },
            payload,
            recorded_at,
        },
        covers_through,
        reason,
        strategy,
        metadata,
    })
}

fn trace_compaction_completed(
    actor_id: &ActorId,
    run_id: Option<&RunId>,
    receipt: &CompactionReceipt,
) {
    let usage = receipt.metadata.usage.as_ref();
    let cost = receipt.metadata.cost.as_ref();
    tracing::info!(
        target: "lam::compaction",
        actor_id = %actor_id,
        run_id = ?run_id.map(ToString::to_string),
        reason = ?receipt.reason,
        strategy = %receipt.strategy,
        covers_through = receipt.covers_through.get(),
        input_tokens = ?usage.map(|usage| usage.input_tokens),
        output_tokens = ?usage.map(|usage| usage.output_tokens),
        total_tokens = ?usage.map(|usage| usage.total_tokens),
        cost_usd = ?cost.map(|cost| cost.amount_usd),
        "context compaction completed"
    );
}

fn compaction_config_error(error: lam_core::CompactionConfigError) -> ActorError {
    ActorError::Compaction {
        message: error.to_string(),
    }
}
