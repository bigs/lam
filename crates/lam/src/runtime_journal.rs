use std::num::NonZeroUsize;

use lam_core::{
    ActorEvent, ActorId, ActorState, AdmissionDecision, AppendOutcome, ContextEntry, EventBatch,
    JournalPage, JournalStore, MessageEnvelope, ModelSelection, StoredEvent,
};

use crate::ActorError;
use crate::actor::MessageReceipt;

const JOURNAL_PAGE_SIZE: NonZeroUsize = NonZeroUsize::new(256).expect("256 is nonzero");

pub(crate) enum AppendAttempt {
    Appended(ActorState),
    Conflict,
}

pub(crate) async fn load_state<S>(store: &S, actor_id: &ActorId) -> Result<ActorState, ActorError>
where
    S: JournalStore,
{
    let mut state = ActorState::new();
    loop {
        let page = store
            .read(actor_id, state.revision(), JOURNAL_PAGE_SIZE)
            .await
            .map_err(journal_error)?;
        let head = page.head;
        state = state.fold_page(page).map_err(state_error)?;
        if state.revision() == head {
            return Ok(state);
        }
    }
}

/// Establishes the first durable model selection, or returns the selection
/// already authoritative for a reopened actor.
pub(crate) async fn ensure_model_selection<S>(
    store: &S,
    actor_id: &ActorId,
    initial: ModelSelection,
) -> Result<(ActorState, bool), ActorError>
where
    S: JournalStore,
{
    let mut state = load_state(store, actor_id).await?;
    let mut created = state.revision() == lam_core::Revision::ZERO;
    loop {
        if state.selected_model().is_some() {
            return Ok((state, false));
        }
        let event = state
            .plan_model_selection(initial.clone())
            .map_err(state_error)?;
        match append_event(store, actor_id, state, event).await? {
            AppendAttempt::Appended(next) => return Ok((next, created)),
            AppendAttempt::Conflict => {
                created = false;
                state = load_state(store, actor_id).await?;
            }
        }
    }
}

pub(crate) async fn admit_message<S>(
    store: &S,
    actor_id: &ActorId,
    message: MessageEnvelope,
) -> Result<MessageReceipt, ActorError>
where
    S: JournalStore,
{
    let state = load_state(store, actor_id).await?;
    admit_message_from_state(store, actor_id, state, message)
        .await
        .map(|(receipt, _state)| receipt)
}

pub(crate) async fn admit_message_from_state<S>(
    store: &S,
    actor_id: &ActorId,
    mut state: ActorState,
    message: MessageEnvelope,
) -> Result<(MessageReceipt, ActorState), ActorError>
where
    S: JournalStore,
{
    loop {
        match state.plan_admission(message.clone()).map_err(state_error)? {
            AdmissionDecision::Existing { revision } => {
                return Ok((
                    MessageReceipt {
                        actor_id: actor_id.clone(),
                        message_id: message.message_id().clone(),
                        revision,
                    },
                    state,
                ));
            }
            AdmissionDecision::Append(event) => {
                match append_event(store, actor_id, state, event).await? {
                    AppendAttempt::Appended(next) => {
                        let receipt = MessageReceipt {
                            actor_id: actor_id.clone(),
                            message_id: message.message_id().clone(),
                            revision: next.revision(),
                        };
                        return Ok((receipt, next));
                    }
                    AppendAttempt::Conflict => state = load_state(store, actor_id).await?,
                }
            }
        }
    }
}

pub(crate) async fn append_context<S>(
    store: &S,
    actor_id: &ActorId,
    mut state: ActorState,
    entry: ContextEntry,
) -> Result<ActorState, ActorError>
where
    S: JournalStore,
{
    loop {
        let event = state
            .plan_context_append(entry.clone())
            .map_err(state_error)?;
        match append_event(store, actor_id, state, event).await? {
            AppendAttempt::Appended(next) => return Ok(next),
            AppendAttempt::Conflict => state = load_state(store, actor_id).await?,
        }
    }
}

pub(crate) async fn append_event<S>(
    store: &S,
    actor_id: &ActorId,
    state: ActorState,
    event: ActorEvent,
) -> Result<AppendAttempt, ActorError>
where
    S: JournalStore,
{
    append_batch(store, actor_id, state, EventBatch::one(event)).await
}

pub(crate) async fn append_batch<S>(
    store: &S,
    actor_id: &ActorId,
    state: ActorState,
    batch: EventBatch,
) -> Result<AppendAttempt, ActorError>
where
    S: JournalStore,
{
    let expected = state.revision();
    let mut revision = expected;
    let events = batch
        .iter()
        .cloned()
        .map(|event| {
            revision = revision
                .checked_advance(1)
                .ok_or_else(|| ActorError::State {
                    message: "actor journal revision space is exhausted".to_owned(),
                })?;
            Ok(StoredEvent { revision, event })
        })
        .collect::<Result<Vec<_>, ActorError>>()?;
    match store
        .append(actor_id, expected, batch)
        .await
        .map_err(journal_error)?
    {
        AppendOutcome::Appended { head } => {
            let state = state
                .fold_page(JournalPage { head, events })
                .map_err(state_error)?;
            Ok(AppendAttempt::Appended(state))
        }
        AppendOutcome::Conflict { .. } => Ok(AppendAttempt::Conflict),
    }
}

fn journal_error<E>(error: lam_core::JournalError<E>) -> ActorError
where
    E: std::error::Error,
{
    ActorError::Journal {
        message: error.to_string(),
    }
}

pub(crate) fn state_error(error: lam_core::StateError) -> ActorError {
    ActorError::State {
        message: error.to_string(),
    }
}
