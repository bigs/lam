use lam_core::{
    ActorId, ActorState, CodecId, CodecRef, ComponentId, ContextTransition, DeliveryMode,
    EncodedPayload, JournalStore, MessageEnvelope, MessageSource, ModelDirective,
};

use crate::actor::{Clock, RuntimeIds};
use crate::model::RegisteredModel;
use crate::runtime_journal::{admit_message_from_state, load_state};
use crate::{
    ActorError, InterruptedEvalOutcome, IsolateState, RUNTIME_COMPONENT_ID, RuntimeEvent,
    SYSTEM_NOTICE_CODEC_ID, SYSTEM_NOTICE_CODEC_VERSION, SystemNotice,
};

pub(crate) struct StartupRecovery {
    pub(crate) wake: bool,
    pub(crate) event: Option<RuntimeEvent>,
}

pub(crate) async fn recover_actor<S>(
    actor_id: &ActorId,
    store: &S,
    model: &RegisteredModel,
    clock: &dyn Clock,
    ids: &RuntimeIds,
    announce: bool,
) -> Result<StartupRecovery, ActorError>
where
    S: JournalStore,
{
    let state = load_state(store, actor_id).await?;
    if !announce {
        return Ok(StartupRecovery {
            wake: false,
            event: None,
        });
    }

    let resumed_run_id = state.active_run().cloned();
    let interrupted_eval_outcome = interrupted_eval_outcome(&state, model);
    let notice = SystemNotice::runtime_resumed(resumed_run_id.clone(), interrupted_eval_outcome);
    let payload = EncodedPayload::new(
        system_notice_codec(),
        serde_json::to_value(notice).map_err(|error| ActorError::State {
            message: format!("runtime resumption notice could not be encoded: {error}"),
        })?,
    );
    let message_id = ids.message_id();
    let delivery = if resumed_run_id.is_some() {
        DeliveryMode::Steer
    } else {
        DeliveryMode::Queue
    };
    let message = MessageEnvelope::new(
        message_id.clone(),
        MessageSource::Host {
            component: ComponentId::new(RUNTIME_COMPONENT_ID)
                .expect("Lam's runtime component id is valid"),
        },
        delivery,
        payload,
        clock.now(),
    )
    .map_err(|error| ActorError::State {
        message: error.to_string(),
    })?;
    let (receipt, recovered) = admit_message_from_state(store, actor_id, state, message).await?;
    let wake = has_recoverable_work(&recovered);

    Ok(StartupRecovery {
        wake,
        event: Some(RuntimeEvent::RuntimeResumed {
            message_id,
            revision: receipt.revision,
            isolate_state: IsolateState::Reset,
            resumed_run_id,
            interrupted_eval_outcome,
        }),
    })
}

pub(crate) fn has_recoverable_work(state: &ActorState) -> bool {
    state.active_run().is_some()
        || state
            .pending_messages()
            .any(|message| !is_runtime_resumption(&message.envelope))
}

fn is_runtime_resumption(message: &MessageEnvelope) -> bool {
    let MessageSource::Host { component } = message.source() else {
        return false;
    };
    component.as_str() == RUNTIME_COMPONENT_ID
        && message.payload().codec == system_notice_codec()
        && matches!(
            message.payload().decode::<SystemNotice>(),
            Ok(SystemNotice::RuntimeResumed { .. })
        )
}

fn interrupted_eval_outcome(
    state: &ActorState,
    model: &RegisteredModel,
) -> Option<InterruptedEvalOutcome> {
    let active_run = state.active_run()?;
    let last_step = state.context().iter().rev().find(|projected| {
        !matches!(
            projected.entry.transition,
            ContextTransition::Compaction { .. }
        ) && projected.entry.transition.run_id() == Some(active_run)
    })?;
    let ContextTransition::Model { .. } = &last_step.entry.transition else {
        return None;
    };
    matches!(
        model.interpret_response(&last_step.entry.payload),
        Ok(ModelDirective::Eval(_))
    )
    .then_some(InterruptedEvalOutcome::Unknown)
}

fn system_notice_codec() -> CodecRef {
    CodecRef::new(
        CodecId::new(SYSTEM_NOTICE_CODEC_ID).expect("Lam's system notice codec id is valid"),
        SYSTEM_NOTICE_CODEC_VERSION,
    )
}
