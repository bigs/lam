//! Shared contract tests for custom [`JournalStore`] backends.

use std::num::NonZeroUsize;

use serde_json::json;

use crate::{
    ActorEvent, ActorId, AppendOutcome, DeliveryMode, EncodedPayload, EventBatch, JournalStore,
    MessageEnvelope, MessageId, MessageSource, Revision, Timestamp,
};

/// Runs the storage contract against a pristine actor journal.
///
/// Backend crates can enable the `test-support` feature and call this from one
/// of their tests. The suite covers empty reads, compare-and-append conflicts,
/// atomic batches, ordered paging, event round trips, and actor isolation.
///
/// # Panics
///
/// Panics when the store violates the [`JournalStore`] contract or a backend
/// operation fails.
pub async fn assert_actor_journal_conformance<S>(store: &S)
where
    S: JournalStore,
{
    let actor_a = ActorId::new("conformance-a").expect("fixture id is valid");
    let actor_b = ActorId::new("conformance-b").expect("fixture id is valid");
    let one = NonZeroUsize::MIN;
    let two = NonZeroUsize::new(2).expect("two is nonzero");
    let first = fixture_event("message-1", 1);
    let second = fixture_event("message-2", 2);
    let third = fixture_event("message-3", 3);

    let empty = store
        .read(&actor_a, Revision::ZERO, one)
        .await
        .expect("empty read should succeed");
    assert_eq!(empty.head, Revision::ZERO);
    assert!(empty.events.is_empty());

    assert_eq!(
        store
            .append(&actor_a, Revision::ZERO, EventBatch::one(first.clone()))
            .await
            .expect("first append should succeed"),
        AppendOutcome::Appended {
            head: Revision::new(1)
        }
    );

    let admitted = store
        .read(&actor_a, Revision::ZERO, one)
        .await
        .expect("successful append must be immediately readable");
    assert_eq!(admitted.head, Revision::new(1));
    assert_eq!(admitted.events.len(), 1);
    assert_eq!(admitted.events[0].event, first);

    assert_eq!(
        store
            .append(&actor_a, Revision::ZERO, EventBatch::one(second.clone()))
            .await
            .expect("a stale append is an ordinary store outcome"),
        AppendOutcome::Conflict {
            expected: Revision::ZERO,
            actual: Revision::new(1),
        }
    );

    assert_eq!(
        store
            .append(
                &actor_a,
                Revision::new(1),
                EventBatch::new(second.clone(), vec![third.clone()]),
            )
            .await
            .expect("batch append should succeed"),
        AppendOutcome::Appended {
            head: Revision::new(3)
        }
    );

    let first_page = store
        .read(&actor_a, Revision::ZERO, two)
        .await
        .expect("first page should load");
    assert_eq!(first_page.head, Revision::new(3));
    assert_eq!(
        first_page
            .events
            .iter()
            .map(|stored| stored.revision)
            .collect::<Vec<_>>(),
        vec![Revision::new(1), Revision::new(2)]
    );
    assert_eq!(first_page.events[0].event, first);
    assert_eq!(first_page.events[1].event, second);

    let second_page = store
        .read(&actor_a, Revision::new(2), two)
        .await
        .expect("second page should load");
    assert_eq!(second_page.head, Revision::new(3));
    assert_eq!(second_page.events.len(), 1);
    assert_eq!(second_page.events[0].revision, Revision::new(3));
    assert_eq!(second_page.events[0].event, third);

    let other_actor = store
        .read(&actor_b, Revision::ZERO, two)
        .await
        .expect("actor streams should be isolated");
    assert_eq!(other_actor.head, Revision::ZERO);
    assert!(other_actor.events.is_empty());

    assert!(matches!(
        store
            .append(
                &actor_b,
                Revision::ZERO,
                EventBatch::one(fixture_event("other-message", 4)),
            )
            .await
            .expect("other actor append should succeed"),
        AppendOutcome::Appended { .. }
    ));

    let actor_a_unchanged = store
        .read(&actor_a, Revision::new(3), one)
        .await
        .expect("first actor should remain readable");
    assert_eq!(actor_a_unchanged.head, Revision::new(3));
    assert!(actor_a_unchanged.events.is_empty());
}

fn fixture_event(message_id: &str, value: u64) -> ActorEvent {
    let message = MessageEnvelope::new(
        MessageId::new(message_id).expect("fixture id is valid"),
        MessageSource::User { principal: None },
        DeliveryMode::Steer,
        EncodedPayload::lam_json(json!({ "value": value })).expect("fixture JSON is valid"),
        Timestamp::from_unix_millis(i64::try_from(value).expect("fixture time fits")),
    )
    .expect("fixture envelope is valid");
    ActorEvent::message_admitted(message)
}

#[cfg(test)]
mod tests {
    use super::assert_actor_journal_conformance;
    use crate::MemStore;

    #[tokio::test(flavor = "current_thread")]
    async fn memory_store_obeys_the_journal_contract() {
        assert_actor_journal_conformance(&MemStore::new()).await;
    }
}
