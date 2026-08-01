//! Durable-store conformance and reopen coverage.

use std::num::NonZeroUsize;

use lam_core::{
    ActorEvent, ActorId, ActorState, AppendOutcome, CodecId, CodecRef, ContextEntry,
    ContextSequence, ContextTransition, DeliveryMode, EncodedPayload, EventBatch, JournalStore,
    MessageEnvelope, MessageId, MessageSource, Revision, RunId, RunProgress, Timestamp,
};
use lam_redb::RedbStore;
use serde_json::json;

#[tokio::test(flavor = "current_thread")]
async fn redb_store_obeys_the_journal_contract() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let store = RedbStore::create(directory.path().join("conformance.redb"))
        .expect("redb store should open");

    lam_core::test_support::assert_actor_journal_conformance(&store).await;
}

#[tokio::test(flavor = "current_thread")]
async fn actor_projection_survives_database_reopen() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("reopen.redb");
    let actor = ActorId::new("persistent-actor").expect("fixture id is valid");
    let initial_id = MessageId::new("initial").expect("fixture id is valid");
    let queued_id = MessageId::new("queued").expect("fixture id is valid");
    let run_id = RunId::new("run-1").expect("fixture id is valid");
    let codec = CodecRef::new(
        CodecId::new("test/summary").expect("fixture id is valid"),
        1,
    );

    {
        let store = RedbStore::create(&path).expect("redb store should open");
        let batch = EventBatch::new(
            ActorEvent::message_admitted(message(initial_id.clone(), DeliveryMode::Steer, 1)),
            vec![
                ActorEvent::context_appended(context(
                    ContextTransition::Messages {
                        run_id: run_id.clone(),
                        consumed_message_ids: vec![initial_id],
                    },
                    EncodedPayload::lam_json(json!([{ "message": "start" }]))
                        .expect("fixture JSON is valid"),
                    2,
                )),
                ActorEvent::context_appended(context(
                    ContextTransition::Model {
                        run_id: run_id.clone(),
                        progress: RunProgress::Complete,
                    },
                    EncodedPayload::lam_json(json!({ "text": "done" }))
                        .expect("fixture JSON is valid"),
                    3,
                )),
                ActorEvent::message_admitted(message(queued_id.clone(), DeliveryMode::Queue, 4)),
                ActorEvent::context_appended(context(
                    ContextTransition::Compaction {
                        covers_through: ContextSequence::new(2),
                        run_id: None,
                    },
                    EncodedPayload::new(codec.clone(), json!({ "summary": "complete" })),
                    5,
                )),
            ],
        );
        let outcome = store
            .append(&actor, Revision::ZERO, batch)
            .await
            .expect("append should succeed");
        assert_eq!(
            outcome,
            AppendOutcome::Appended {
                head: Revision::new(5)
            }
        );
    }

    let store = RedbStore::open(&path).expect("existing database should reopen");
    let state = rebuild(&store, &actor).await;
    assert_eq!(state.revision(), Revision::new(5));
    assert!(state.is_run_completed(&run_id));
    assert_eq!(
        state
            .pending_messages()
            .map(|message| message.envelope.message_id())
            .collect::<Vec<_>>(),
        vec![&queued_id]
    );
    let compaction = state
        .latest_compaction_matching(|entry| entry.payload.codec == codec)
        .expect("compaction marker should survive reopen");
    assert_eq!(
        compaction.entry.payload.value,
        json!({ "summary": "complete" })
    );
    assert_eq!(state.context().len(), 3, "raw context remains available");
}

fn message(id: MessageId, delivery: DeliveryMode, time: i64) -> MessageEnvelope {
    MessageEnvelope::new(
        id,
        MessageSource::User { principal: None },
        delivery,
        EncodedPayload::lam_json(json!({ "message": "fixture" })).expect("fixture JSON is valid"),
        Timestamp::from_unix_millis(time),
    )
    .expect("fixture message is valid")
}

fn context(transition: ContextTransition, payload: EncodedPayload, time: i64) -> ContextEntry {
    ContextEntry {
        transition,
        payload,
        recorded_at: Timestamp::from_unix_millis(time),
    }
}

async fn rebuild(store: &RedbStore, actor: &ActorId) -> ActorState {
    let mut state = ActorState::new();
    loop {
        let page = store
            .read(
                actor,
                state.revision(),
                NonZeroUsize::new(2).expect("two is nonzero"),
            )
            .await
            .expect("read should succeed");
        let head = page.head;
        state = state.fold_page(page).expect("journal should project");
        if state.revision() == head {
            return state;
        }
    }
}
