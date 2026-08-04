use std::num::NonZeroUsize;
use std::sync::Arc;

use lam_core::{
    ActorEvent, ActorEventData, ActorId, ActorState, AdmissionDecision, AdmittedMessage,
    AppendOutcome, ContextEntry, EventBatch, JournalPage, JournalStore, MessageEnvelope,
    ModelSelection, Revision, StateError, StoredEvent,
};
use tokio::sync::Mutex;

use crate::ActorError;
use crate::actor::MessageReceipt;

const JOURNAL_PAGE_SIZE: NonZeroUsize = NonZeroUsize::new(256).expect("256 is nonzero");

pub(crate) enum AppendAttempt {
    Appended(ActorState),
    /// The compare-and-append conflicted. The projection is returned so the
    /// caller can refresh incrementally instead of replaying the journal.
    Conflict(ActorState),
}

pub(crate) async fn load_state<S>(store: &S, actor_id: &ActorId) -> Result<ActorState, ActorError>
where
    S: JournalStore,
{
    refresh_state(store, actor_id, ActorState::new()).await
}

/// Folds any journal events past the state's revision. Unlike [`load_state`],
/// this never replays from revision zero, so it is cheap enough to call at
/// every delivery boundary.
pub(crate) async fn refresh_state<S>(
    store: &S,
    actor_id: &ActorId,
    mut state: ActorState,
) -> Result<ActorState, ActorError>
where
    S: JournalStore,
{
    loop {
        let page = store
            .read(actor_id, state.revision(), JOURNAL_PAGE_SIZE)
            .await
            .map_err(journal_error)?;
        let head = page.head;
        if page.events.is_empty() {
            return Ok(state);
        }
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
            AppendAttempt::Conflict(conflicted) => {
                created = false;
                state = refresh_state(store, actor_id, conflicted).await?;
            }
        }
    }
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
                    AppendAttempt::Conflict(conflicted) => {
                        state = refresh_state(store, actor_id, conflicted).await?
                    }
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
            AppendAttempt::Conflict(conflicted) => {
                state = refresh_state(store, actor_id, conflicted).await?
            }
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
    state.validate_batch(&batch).map_err(state_error)?;
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
        AppendOutcome::Conflict { .. } => Ok(AppendAttempt::Conflict(state)),
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

/// The durable-message slice of actor state the send path needs to admit a
/// message idempotently without replaying the whole journal: the fold head
/// and every admitted envelope. It is seeded once from a folded
/// ActorState and afterwards advances only by scanning newly appended
/// journal events, so each admission is bounded by journal activity since
/// the previous admission rather than by total journal size.
pub(crate) struct AdmissionLedger<S> {
    inner: Mutex<AdmissionView>,
    store: Arc<S>,
    actor_id: ActorId,
}

#[derive(Debug, Default)]
struct AdmissionView {
    /// Journal revision covered by the admitted-message list (fold head).
    revision: Revision,
    /// Every admitted envelope in admission order.
    messages: Vec<AdmittedMessage>,
}

impl<S> AdmissionLedger<S>
where
    S: JournalStore + 'static,
{
    /// Seeds the ledger from an already-folded actor state at the journal
    /// head. After seeding, the ledger only ever advances incrementally.
    pub(crate) fn seed(store: Arc<S>, actor_id: ActorId, state: &ActorState) -> Self {
        Self {
            inner: Mutex::new(AdmissionView {
                revision: state.revision(),
                messages: state.messages().to_vec(),
            }),
            store,
            actor_id,
        }
    }

    /// Durably admits one envelope under the same idempotency contract as
    /// ActorState::plan_admission, without materializing the full actor
    /// projection. Conflict retries refresh the view incrementally.
    pub(crate) async fn admit(
        &self,
        message: MessageEnvelope,
    ) -> Result<MessageReceipt, ActorError> {
        let message_id = message.message_id().clone();
        let mut view = self.inner.lock().await;
        self.refresh(&mut view).await?;
        loop {
            match plan_view_admission(&view, message.clone()).map_err(state_error)? {
                AdmissionDecision::Existing { revision } => {
                    return Ok(MessageReceipt {
                        actor_id: self.actor_id.clone(),
                        message_id,
                        revision,
                    });
                }
                AdmissionDecision::Append(event) => {
                    let expected = view.revision;
                    match self
                        .store
                        .append(&self.actor_id, expected, EventBatch::one(event))
                        .await
                        .map_err(journal_error)?
                    {
                        AppendOutcome::Appended { head } => {
                            view.revision = head;
                            view.messages.push(AdmittedMessage {
                                revision: head,
                                envelope: message,
                                consumed_at: None,
                            });
                            return Ok(MessageReceipt {
                                actor_id: self.actor_id.clone(),
                                message_id,
                                revision: head,
                            });
                        }
                        AppendOutcome::Conflict { .. } => self.refresh(&mut view).await?,
                    }
                }
            }
        }
    }

    /// Folds newly appended journal events into the view, collecting only
    /// message admissions. Context appends advance the revision without
    /// growing the view.
    async fn refresh(&self, view: &mut AdmissionView) -> Result<(), ActorError> {
        loop {
            let page = self
                .store
                .read(&self.actor_id, view.revision, JOURNAL_PAGE_SIZE)
                .await
                .map_err(journal_error)?;
            let head = page.head;
            if page.events.is_empty() {
                return Ok(());
            }
            for stored in &page.events {
                if let ActorEventData::MessageAdmitted { message } = stored.event.data() {
                    view.messages.push(AdmittedMessage {
                        revision: stored.revision,
                        envelope: message.clone(),
                        consumed_at: None,
                    });
                }
            }
            view.revision = page
                .events
                .last()
                .expect("a non-empty page has a last event")
                .revision;
            if view.revision == head {
                return Ok(());
            }
        }
    }
}

/// Mirrors ActorState::plan_admission against a lightweight admission view.
/// Keep the two in sync: the ledger must enforce the same idempotency and
/// collision rules as the full projection.
fn plan_view_admission(
    view: &AdmissionView,
    message: MessageEnvelope,
) -> Result<AdmissionDecision, StateError> {
    message.validate()?;
    if let Some(existing) = view
        .messages
        .iter()
        .find(|admitted| admitted.envelope.message_id() == message.message_id())
    {
        if message.is_idempotent_retry_of(&existing.envelope) {
            return Ok(AdmissionDecision::Existing {
                revision: existing.revision,
            });
        }
        return Err(StateError::MessageIdCollision {
            message_id: message.message_id().clone(),
        });
    }
    Ok(AdmissionDecision::Append(ActorEvent::message_admitted(
        message,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lam_core::{
        ContextEntry, ContextTransition, DeliveryMode, EncodedPayload, MemStore, MessageId,
        MessageSource, RunId, RunProgress, Timestamp,
    };

    fn envelope(id: &str, text: &str) -> MessageEnvelope {
        MessageEnvelope::new(
            MessageId::new(id).expect("message id"),
            MessageSource::User { principal: None },
            DeliveryMode::Steer,
            EncodedPayload::lam_json(text.to_owned()).expect("payload"),
            Timestamp::from_unix_millis(1),
        )
        .expect("envelope")
    }

    fn root() -> ActorId {
        ActorId::new("/root").expect("actor id")
    }

    #[tokio::test]
    async fn ledger_admits_messages_and_dedups_idempotent_retries() {
        let store = Arc::new(MemStore::new());
        let ledger = AdmissionLedger::seed(Arc::clone(&store), root(), &ActorState::default());

        let first = envelope("m1", "hello");
        let receipt = ledger.admit(first.clone()).await.expect("admit");
        assert_eq!(receipt.revision, Revision::new(1));

        // Re-admitting the identical envelope is an idempotent retry: the
        // original revision is returned and no new event is appended.
        let retry = ledger.admit(first).await.expect("idempotent retry");
        assert_eq!(retry.revision, Revision::new(1));

        // A different envelope reusing the id collides instead of appending.
        assert!(
            ledger
                .admit(envelope("m1", "different payload"))
                .await
                .is_err()
        );

        // A fresh message advances the head.
        let second = ledger.admit(envelope("m2", "world")).await.expect("admit");
        assert_eq!(second.revision, Revision::new(2));

        let state = load_state(store.as_ref(), &root()).await.expect("load");
        assert_eq!(state.messages().len(), 2);
    }

    #[tokio::test]
    async fn ledger_refresh_sees_external_admissions() {
        let store = Arc::new(MemStore::new());
        let actor = root();
        let ledger =
            AdmissionLedger::seed(Arc::clone(&store), actor.clone(), &ActorState::default());

        // Another path admits directly through the store, as the runner does
        // for notices and call inputs.
        let external = envelope("x1", "external");
        let _ = store
            .append(
                &actor,
                Revision::ZERO,
                EventBatch::one(ActorEvent::message_admitted(external.clone())),
            )
            .await
            .expect("append");

        // The ledger dedups against the external admission without replaying
        // the journal from zero.
        let retry = ledger.admit(external).await.expect("idempotent retry");
        assert_eq!(retry.revision, Revision::new(1));

        let state = load_state(store.as_ref(), &actor).await.expect("load");
        assert_eq!(state.messages().len(), 1);
    }

    #[tokio::test]
    async fn ledger_lands_behind_external_activity_without_conflicts() {
        let store = Arc::new(MemStore::new());
        let actor = root();
        let ledger =
            AdmissionLedger::seed(Arc::clone(&store), actor.clone(), &ActorState::default());

        // A valid run sequence: admit m0, start run r1 by consuming it, then
        // continue with a model entry. Context appends advance the head
        // without growing the admitted set.
        let m0 = envelope("m0", "seed");
        let _ = store
            .append(
                &actor,
                Revision::ZERO,
                EventBatch::one(ActorEvent::message_admitted(m0.clone())),
            )
            .await
            .expect("append");
        let messages = ContextEntry {
            transition: ContextTransition::Messages {
                run_id: RunId::new("r1").expect("run id"),
                consumed_message_ids: vec![m0.message_id().clone()],
            },
            payload: EncodedPayload::lam_json("payload").expect("payload"),
            recorded_at: Timestamp::from_unix_millis(1),
        };
        let _ = store
            .append(
                &actor,
                Revision::new(1),
                EventBatch::one(ActorEvent::context_appended(messages)),
            )
            .await
            .expect("append");
        let model = ContextEntry {
            transition: ContextTransition::Model {
                run_id: RunId::new("r1").expect("run id"),
                progress: RunProgress::Continue,
            },
            payload: EncodedPayload::lam_json("payload").expect("payload"),
            recorded_at: Timestamp::from_unix_millis(2),
        };
        let _ = store
            .append(
                &actor,
                Revision::new(2),
                EventBatch::one(ActorEvent::context_appended(model)),
            )
            .await
            .expect("append");

        // The admission refreshes past the context events and lands at rev 4.
        let admitted = ledger.admit(envelope("m1", "mine")).await.expect("admit");
        assert_eq!(admitted.revision, Revision::new(4));

        let state = load_state(store.as_ref(), &actor).await.expect("load");
        assert_eq!(state.messages().len(), 2);
        assert_eq!(state.revision(), Revision::new(4));
    }

    #[tokio::test]
    async fn conflicted_append_returns_the_state_for_incremental_refresh() {
        let store = Arc::new(MemStore::new());
        let actor = root();
        let seed = envelope("m1", "seed");
        let _ = store
            .append(
                &actor,
                Revision::ZERO,
                EventBatch::one(ActorEvent::message_admitted(seed)),
            )
            .await
            .expect("append");
        let state = load_state(store.as_ref(), &actor).await.expect("load");

        // Another path advances the head behind the projected state.
        let external = envelope("x1", "external");
        let _ = store
            .append(
                &actor,
                state.revision(),
                EventBatch::one(ActorEvent::message_admitted(external.clone())),
            )
            .await
            .expect("append");

        // Appending with the stale state conflicts and hands the projection
        // back so the retry refreshes incrementally instead of replaying.
        let batch = EventBatch::one(ActorEvent::message_admitted(envelope("m2", "mine")));
        let attempt = append_batch(store.as_ref(), &actor, state, batch)
            .await
            .expect("append");
        let state = match attempt {
            AppendAttempt::Conflict(state) => state,
            AppendAttempt::Appended(_) => panic!("a stale append must conflict"),
        };
        let state = refresh_state(store.as_ref(), &actor, state)
            .await
            .expect("refresh");
        let retry = envelope("m2", "mine");
        match append_batch(
            store.as_ref(),
            &actor,
            state,
            EventBatch::one(ActorEvent::message_admitted(retry.clone())),
        )
        .await
        .expect("append")
        {
            AppendAttempt::Appended(state) => {
                assert_eq!(state.revision(), Revision::new(3));
                assert!(state.message(external.message_id()).is_some());
                assert!(state.message(retry.message_id()).is_some());
            }
            AppendAttempt::Conflict(_) => panic!("a refreshed retry must append"),
        }
    }
}
