use std::sync::Arc;

use lam_core::{
    ActorId, ActorState, CompactionReason, CompactionRecord, JournalStore, ModelCodec,
    ModelProvider, RunId, compaction_prefix_len,
};
use tokio::sync::mpsc;

use crate::compaction::{
    CompactionReceipt, compaction_request, estimated_context_tokens, model_context,
};
use crate::runner::{ActorRunner, emit, wait_for_abort};
use crate::runtime_journal::{append_context, load_state};
use crate::{ActorError, RunEvent, RuntimeEvent};

impl<P, C, S> ActorRunner<P, C, S>
where
    P: ModelProvider,
    C: ModelCodec,
    S: JournalStore + 'static,
{
    pub(crate) async fn compact_current(
        &mut self,
    ) -> Result<Option<CompactionReceipt>, ActorError> {
        if self.compactor.is_none() {
            return Err(ActorError::CompactionDisabled);
        }
        let state = load_state(self.store.as_ref(), &self.actor_id).await?;
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
        if self.compactor.is_none() {
            return Ok(state);
        }
        let trigger = self
            .compaction_config
            .automatic_trigger_tokens()
            .map_err(compaction_config_error)?;
        let Some(trigger) = trigger else {
            return Ok(state);
        };
        let context = model_context(&state, self.codec.as_ref())?;
        let before = estimated_context_tokens(&context, self.codec.as_ref())?;
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
        let context = model_context(&next, self.codec.as_ref())?;
        let after = estimated_context_tokens(&context, self.codec.as_ref())?;
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
        let Some(compactor) = self.compactor.as_ref().map(Arc::clone) else {
            return match reason {
                CompactionReason::Manual => Err(ActorError::CompactionDisabled),
                CompactionReason::Threshold => Ok((state, None)),
                CompactionReason::Overflow => Err(ActorError::ContextOverflow),
            };
        };
        let retain_tokens = self
            .compaction_config
            .retain_tokens()
            .map_err(compaction_config_error)?;
        let request = compaction_request(
            &state,
            reason,
            retain_tokens,
            self.compaction_config.summary_reserve(),
            |replacement| self.codec.accepts_compaction_replacement(replacement),
        )?;
        if compaction_prefix_len(&request.units, request.retain_tokens).is_none() {
            return Ok((state, None));
        }

        let run_id = state.active_run().cloned();
        self.emit_compaction_started(events, run_id.as_ref(), reason);
        let mut abort = self.abort.clone();
        let plan = tokio::select! {
            biased;
            _ = wait_for_abort(&mut abort) => return Err(ActorError::Aborted),
            result = compactor.compact(&request) => result,
        };
        let plan = match plan {
            Ok(plan) => plan,
            Err(error) => {
                self.emit_compaction_failed(events, run_id.as_ref(), reason, error.to_string());
                return Err(ActorError::Compaction {
                    message: error.to_string(),
                });
            }
        };
        let installation: Result<_, ActorError> = async {
            if plan.artifact.is_empty() {
                return Err(ActorError::Compaction {
                    message: "compactor returned an empty artifact".to_owned(),
                });
            }
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
            let replacement = self
                .codec
                .materialize_compaction(&plan.artifact)
                .map_err(|error| ActorError::Compaction {
                    message: error.to_string(),
                })?
                .ok_or_else(|| ActorError::Compaction {
                    message: "the active model codec cannot materialize compaction context"
                        .to_owned(),
                })?;
            if !self.codec.accepts_compaction_replacement(&replacement) {
                return Err(ActorError::Compaction {
                    message: format!(
                        "the active model codec rejected its materialized replacement {}@{}",
                        replacement.codec.id, replacement.codec.version
                    ),
                });
            }
            let record = CompactionRecord {
                strategy: plan.strategy.clone(),
                reason,
                source: plan.source,
                artifact: plan.artifact,
                replacement,
                metadata: plan.metadata.clone(),
            };
            let payload = record.encode().map_err(|error| ActorError::Compaction {
                message: format!("compaction record could not be encoded: {error}"),
            })?;
            let entry = lam_core::ContextEntry {
                transition: lam_core::ContextTransition::Compaction {
                    covers_through: plan.covers_through,
                    run_id: run_id.clone(),
                },
                payload,
                recorded_at: self.clock.now(),
            };
            let next = append_context(self.store.as_ref(), &self.actor_id, state, entry).await?;
            let receipt = CompactionReceipt {
                covers_through: plan.covers_through,
                revision: next.revision(),
                reason,
                strategy: plan.strategy,
                metadata: plan.metadata,
            };
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
