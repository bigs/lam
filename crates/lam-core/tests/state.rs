//! Deterministic projection and actor-state race coverage.

use std::num::NonZeroUsize;

use lam_core::{
    ActorEvent, ActorId, ActorState, AdmissionDecision, AppendOutcome, CodecId, CodecRef,
    ContextEntry, ContextSequence, ContextTransition, DeliveryMode, EncodedPayload, EventBatch,
    JournalPage, JournalStore, MemStore, MessageEnvelope, MessageId, MessageSource,
    ModelDescriptor, ModelId, ModelSelection, Revision, RunId, RunProgress, StateError,
    StoredEvent, Timestamp,
};
use serde_json::json;

fn actor_id() -> ActorId {
    ActorId::new("actor").expect("fixture id is valid")
}

fn message(id: &str, delivery: DeliveryMode, value: &str, time: i64) -> MessageEnvelope {
    MessageEnvelope::new(
        MessageId::new(id).expect("fixture id is valid"),
        MessageSource::User { principal: None },
        delivery,
        EncodedPayload::lam_json(json!({ "message": value })).expect("fixture JSON is valid"),
        Timestamp::from_unix_millis(time),
    )
    .expect("fixture envelope is valid")
}

fn host_message(id: &str, value: &str, time: i64) -> MessageEnvelope {
    MessageEnvelope::new(
        MessageId::new(id).expect("fixture id is valid"),
        MessageSource::Host {
            component: lam_core::ComponentId::new("lam/runtime")
                .expect("fixture component is valid"),
        },
        DeliveryMode::Steer,
        EncodedPayload::lam_json(json!({ "notice": value })).expect("fixture JSON is valid"),
        Timestamp::from_unix_millis(time),
    )
    .expect("fixture envelope is valid")
}

fn message_ids(ids: &[&str]) -> Vec<MessageId> {
    ids.iter()
        .map(|id| MessageId::new(*id).expect("fixture id is valid"))
        .collect()
}

fn context(transition: ContextTransition, value: serde_json::Value, time: i64) -> ContextEntry {
    ContextEntry {
        transition,
        payload: EncodedPayload::lam_json(value).expect("fixture JSON is valid"),
        recorded_at: Timestamp::from_unix_millis(time),
    }
}

fn model_selection(id: &str, model: &str) -> ModelSelection {
    ModelSelection::new(
        ModelId::new(id).unwrap(),
        ModelDescriptor::new("test", model, "test/codec").unwrap(),
    )
}

async fn append(
    store: &MemStore,
    actor: &ActorId,
    expected: Revision,
    event: ActorEvent,
) -> AppendOutcome {
    store
        .append(actor, expected, EventBatch::one(event))
        .await
        .expect("memory append is infallible")
}

async fn append_successfully(
    store: &MemStore,
    actor: &ActorId,
    expected: Revision,
    event: ActorEvent,
) {
    assert!(matches!(
        append(store, actor, expected, event).await,
        AppendOutcome::Appended { .. }
    ));
}

async fn catch_up(store: &MemStore, actor: &ActorId, mut state: ActorState) -> ActorState {
    loop {
        let page = store
            .read(
                actor,
                state.revision(),
                NonZeroUsize::new(2).expect("two is nonzero"),
            )
            .await
            .expect("memory read is infallible");
        let observed_head = page.head;
        state = state.fold_page(page).expect("journal should project");
        if state.revision() == observed_head {
            return state;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn admission_is_idempotent_and_context_is_a_pure_projection() {
    let store = MemStore::new();
    let actor = actor_id();
    let mut state = ActorState::new();
    let original = message("m1", DeliveryMode::Steer, "hello", 1);

    let AdmissionDecision::Append(event) = state
        .plan_admission(original.clone())
        .expect("new message should append")
    else {
        panic!("new message was unexpectedly already present");
    };
    assert_eq!(
        append(&store, &actor, state.revision(), event).await,
        AppendOutcome::Appended {
            head: Revision::new(1)
        }
    );
    state = catch_up(&store, &actor, state).await;

    let retry_with_new_observation_time = message("m1", DeliveryMode::Steer, "hello", 99);
    assert_eq!(
        state
            .plan_admission(retry_with_new_observation_time)
            .expect("identical semantic content should be idempotent"),
        AdmissionDecision::Existing {
            revision: Revision::new(1)
        }
    );
    assert!(matches!(
        state.plan_admission(message("m1", DeliveryMode::Steer, "different", 1)),
        Err(StateError::MessageIdCollision { .. })
    ));

    let run = RunId::new("run-1").expect("fixture id is valid");
    let event = state
        .plan_context_append(context(
            ContextTransition::Messages {
                run_id: run.clone(),
                consumed_message_ids: message_ids(&["m1"]),
            },
            json!([{ "message": "hello" }]),
            2,
        ))
        .expect("eligible message should enter context");
    append_successfully(&store, &actor, state.revision(), event).await;
    state = catch_up(&store, &actor, state).await;

    assert_eq!(state.context_sequence().get(), 1);
    assert!(state.pending_messages().next().is_none());
    assert_eq!(state.active_run(), Some(&run));
    assert_eq!(
        state
            .message(&MessageId::new("m1").expect("fixture id is valid"))
            .and_then(|message| message.consumed_at)
            .expect("message should be consumed")
            .get(),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn steering_message_wins_against_stale_terminal_append() {
    let store = MemStore::new();
    let actor = actor_id();
    let run = RunId::new("run-race").expect("fixture id is valid");
    let mut state = ActorState::new();

    let AdmissionDecision::Append(initial) = state
        .plan_admission(message("initial", DeliveryMode::Steer, "start", 1))
        .expect("initial message should append")
    else {
        panic!("initial message was unexpectedly present");
    };
    append_successfully(&store, &actor, state.revision(), initial).await;
    state = catch_up(&store, &actor, state).await;

    let input = state
        .plan_context_append(context(
            ContextTransition::Messages {
                run_id: run.clone(),
                consumed_message_ids: message_ids(&["initial"]),
            },
            json!([{ "message": "start" }]),
            2,
        ))
        .expect("initial message should start the run");
    append_successfully(&store, &actor, state.revision(), input).await;
    state = catch_up(&store, &actor, state).await;

    let terminal_candidate = state
        .plan_context_append(context(
            ContextTransition::Model {
                run_id: run.clone(),
                progress: RunProgress::Complete,
            },
            json!({ "text": "done" }),
            3,
        ))
        .expect("terminal candidate is valid before the race");
    let stale_revision = state.revision();

    let steering =
        ActorEvent::message_admitted(message("steer", DeliveryMode::Steer, "one more thing", 4));
    assert!(matches!(
        append(&store, &actor, stale_revision, steering).await,
        AppendOutcome::Appended { .. }
    ));
    assert_eq!(
        append(&store, &actor, stale_revision, terminal_candidate).await,
        AppendOutcome::Conflict {
            expected: stale_revision,
            actual: Revision::new(stale_revision.get() + 1),
        }
    );

    state = catch_up(&store, &actor, state).await;
    assert!(matches!(
        state.plan_context_append(context(
            ContextTransition::Model {
                run_id: run.clone(),
                progress: RunProgress::Complete,
            },
            json!({ "text": "done" }),
            5,
        )),
        Err(StateError::TerminalWithPendingSteer { .. })
    ));

    let steered_input = state
        .plan_context_append(context(
            ContextTransition::Messages {
                run_id: run.clone(),
                consumed_message_ids: message_ids(&["steer"]),
            },
            json!([{ "message": "one more thing" }]),
            6,
        ))
        .expect("steering batch should continue the same run");
    append_successfully(&store, &actor, state.revision(), steered_input).await;
    state = catch_up(&store, &actor, state).await;

    let terminal = state
        .plan_context_append(context(
            ContextTransition::Model {
                run_id: run.clone(),
                progress: RunProgress::Complete,
            },
            json!({ "text": "now done" }),
            7,
        ))
        .expect("run may terminate after consuming steering");
    append_successfully(&store, &actor, state.revision(), terminal).await;
    state = catch_up(&store, &actor, state).await;

    assert!(state.is_run_completed(&run));
    assert!(state.active_run().is_none());
}

#[test]
fn interruption_batch_records_pending_eval_and_closes_the_run_atomically() {
    let run = RunId::new("run-interrupted").expect("fixture id is valid");
    let initial = message("initial", DeliveryMode::Steer, "start", 1);
    let pending = message("pending", DeliveryMode::Steer, "more", 3);
    let notice = host_message("notice", "interrupted", 4);
    let state = ActorState::new()
        .fold_page(JournalPage {
            head: Revision::new(3),
            events: vec![
                StoredEvent {
                    revision: Revision::new(1),
                    event: ActorEvent::message_admitted(initial),
                },
                StoredEvent {
                    revision: Revision::new(2),
                    event: ActorEvent::context_appended(context(
                        ContextTransition::Messages {
                            run_id: run.clone(),
                            consumed_message_ids: message_ids(&["initial"]),
                        },
                        json!([{ "message": "start" }]),
                        2,
                    )),
                },
                StoredEvent {
                    revision: Revision::new(3),
                    event: ActorEvent::message_admitted(pending),
                },
            ],
        })
        .expect("active run should project");

    let batch = EventBatch::new(
        ActorEvent::message_admitted(notice),
        vec![
            ActorEvent::context_appended(context(
                ContextTransition::Eval {
                    run_id: run.clone(),
                },
                json!({ "status": "failure", "kind": "interrupted" }),
                5,
            )),
            ActorEvent::context_appended(context(
                ContextTransition::Interrupted {
                    run_id: run.clone(),
                    consumed_message_ids: message_ids(&["pending", "notice"]),
                },
                json!([{ "notice": "interrupted" }]),
                6,
            )),
        ],
    );
    state
        .validate_batch(&batch)
        .expect("later events may depend on earlier batch state");
    let events = batch
        .into_vec()
        .into_iter()
        .enumerate()
        .map(|(index, event)| StoredEvent {
            revision: Revision::new(4 + u64::try_from(index).unwrap()),
            event,
        })
        .collect();
    let state = state
        .fold_page(JournalPage {
            head: Revision::new(6),
            events,
        })
        .expect("interruption batch should project");

    assert!(state.active_run().is_none());
    assert!(state.is_run_interrupted(&run));
    assert!(!state.is_run_completed(&run));
    assert!(state.pending_messages().next().is_none());
    assert!(matches!(
        state.plan_context_append(context(
            ContextTransition::Eval {
                run_id: run.clone(),
            },
            json!({}),
            7,
        )),
        Err(StateError::RunAlreadyInterrupted { .. })
    ));
}

#[test]
fn committed_completion_wins_over_a_late_interruption_batch() {
    let run = RunId::new("run-completed").expect("fixture id is valid");
    let state = ActorState::new()
        .fold_page(JournalPage {
            head: Revision::new(3),
            events: vec![
                StoredEvent {
                    revision: Revision::new(1),
                    event: ActorEvent::message_admitted(message(
                        "initial",
                        DeliveryMode::Steer,
                        "start",
                        1,
                    )),
                },
                StoredEvent {
                    revision: Revision::new(2),
                    event: ActorEvent::context_appended(context(
                        ContextTransition::Messages {
                            run_id: run.clone(),
                            consumed_message_ids: message_ids(&["initial"]),
                        },
                        json!([{ "message": "start" }]),
                        2,
                    )),
                },
                StoredEvent {
                    revision: Revision::new(3),
                    event: ActorEvent::context_appended(context(
                        ContextTransition::Model {
                            run_id: run.clone(),
                            progress: RunProgress::Complete,
                        },
                        json!({ "output": "done" }),
                        3,
                    )),
                },
            ],
        })
        .expect("completed run should project");
    let notice = host_message("notice", "interrupted", 4);
    let batch = EventBatch::new(
        ActorEvent::message_admitted(notice),
        vec![ActorEvent::context_appended(context(
            ContextTransition::Interrupted {
                run_id: run.clone(),
                consumed_message_ids: message_ids(&["notice"]),
            },
            json!([{ "notice": "interrupted" }]),
            5,
        ))],
    );

    assert!(state.is_run_completed(&run));
    assert!(matches!(
        state.validate_batch(&batch),
        Err(StateError::TerminalWithoutActiveRun { .. })
    ));
    assert!(!state.is_run_interrupted(&run));
}

#[tokio::test(flavor = "current_thread")]
async fn queued_message_does_not_block_current_run_completion() {
    let store = MemStore::new();
    let actor = actor_id();
    let run = RunId::new("run-queue").expect("fixture id is valid");
    let mut state = ActorState::new();

    let events = EventBatch::new(
        ActorEvent::message_admitted(message("initial", DeliveryMode::Steer, "start", 1)),
        vec![ActorEvent::context_appended(context(
            ContextTransition::Messages {
                run_id: run.clone(),
                consumed_message_ids: message_ids(&["initial"]),
            },
            json!([{ "message": "start" }]),
            2,
        ))],
    );
    assert!(matches!(
        store
            .append(&actor, Revision::ZERO, events)
            .await
            .expect("memory append is infallible"),
        AppendOutcome::Appended { .. }
    ));
    state = catch_up(&store, &actor, state).await;

    let stale_revision = state.revision();
    append_successfully(
        &store,
        &actor,
        stale_revision,
        ActorEvent::message_admitted(message("queued", DeliveryMode::Queue, "later", 3)),
    )
    .await;

    let stale_terminal = ActorEvent::context_appended(context(
        ContextTransition::Model {
            run_id: run.clone(),
            progress: RunProgress::Complete,
        },
        json!({ "text": "done" }),
        4,
    ));
    assert!(matches!(
        append(&store, &actor, stale_revision, stale_terminal).await,
        AppendOutcome::Conflict { .. }
    ));

    state = catch_up(&store, &actor, state).await;
    let retry = state
        .plan_context_append(context(
            ContextTransition::Model {
                run_id: run.clone(),
                progress: RunProgress::Complete,
            },
            json!({ "text": "done" }),
            4,
        ))
        .expect("queued work should not block completion");
    append_successfully(&store, &actor, state.revision(), retry).await;
    state = catch_up(&store, &actor, state).await;

    assert!(state.is_run_completed(&run));
    assert_eq!(
        state
            .eligible_messages()
            .map(|message| message.envelope.message_id().as_str())
            .collect::<Vec<_>>(),
        vec!["queued"]
    );
}

#[test]
fn newest_compatible_compaction_is_projected_without_discarding_history() {
    let mut state = ActorState::new();
    let codec_a = CodecRef::new(CodecId::new("provider/a").expect("fixture id is valid"), 1);
    let codec_b = CodecRef::new(CodecId::new("provider/b").expect("fixture id is valid"), 1);
    let entries = [
        ContextEntry {
            transition: ContextTransition::Compaction {
                covers_through: ContextSequence::ZERO,
                run_id: None,
            },
            payload: EncodedPayload::new(codec_a.clone(), json!({ "summary": "old" })),
            recorded_at: Timestamp::from_unix_millis(1),
        },
        ContextEntry {
            transition: ContextTransition::Compaction {
                covers_through: ContextSequence::new(1),
                run_id: None,
            },
            payload: EncodedPayload::new(codec_b, json!({ "summary": "other" })),
            recorded_at: Timestamp::from_unix_millis(2),
        },
        ContextEntry {
            transition: ContextTransition::Compaction {
                covers_through: ContextSequence::new(2),
                run_id: None,
            },
            payload: EncodedPayload::new(codec_a.clone(), json!({ "summary": "new" })),
            recorded_at: Timestamp::from_unix_millis(3),
        },
    ];

    for (index, entry) in entries.into_iter().enumerate() {
        let revision = Revision::new(u64::try_from(index + 1).expect("fixture revision fits"));
        state = state
            .fold_page(JournalPage {
                head: revision,
                events: vec![StoredEvent {
                    revision,
                    event: ActorEvent::context_appended(entry),
                }],
            })
            .expect("compaction marker should project");
    }

    let latest = state
        .latest_compaction_matching(|entry| entry.payload.codec == codec_a)
        .expect("compatible marker should exist");
    assert_eq!(latest.sequence.get(), 3);
    assert_eq!(latest.entry.payload.value, json!({ "summary": "new" }));
    assert_eq!(state.context().len(), 3, "raw history remains intact");
}

#[test]
fn actor_events_have_a_stable_versioned_serde_shape() {
    let event =
        ActorEvent::message_admitted(message("serialized", DeliveryMode::Steer, "hello", 1));
    let encoded = serde_json::to_value(&event).expect("event should serialize");
    assert_eq!(encoded["schemaVersion"], json!(3));
    assert_eq!(encoded["event"]["type"], json!("messageAdmitted"));
    assert_eq!(
        serde_json::from_value::<ActorEvent>(encoded).expect("event should deserialize"),
        event
    );
}

#[test]
fn model_selection_is_durable_and_cannot_change_during_a_run() {
    let initial = model_selection("primary", "model-a");
    let state = ActorState::new()
        .fold_page(JournalPage {
            head: Revision::new(1),
            events: vec![StoredEvent {
                revision: Revision::new(1),
                event: ActorEvent::model_selected(initial.clone()),
            }],
        })
        .unwrap();
    assert_eq!(state.selected_model(), Some(&initial));

    let message = message("run-message", DeliveryMode::Steer, "start", 1);
    let state = state
        .fold_page(JournalPage {
            head: Revision::new(3),
            events: vec![
                StoredEvent {
                    revision: Revision::new(2),
                    event: ActorEvent::message_admitted(message),
                },
                StoredEvent {
                    revision: Revision::new(3),
                    event: ActorEvent::context_appended(context(
                        ContextTransition::Messages {
                            run_id: RunId::new("active").unwrap(),
                            consumed_message_ids: message_ids(&["run-message"]),
                        },
                        json!([]),
                        2,
                    )),
                },
            ],
        })
        .unwrap();
    assert!(matches!(
        state.plan_model_selection(model_selection("other", "model-b")),
        Err(StateError::ModelSwitchDuringRun { .. })
    ));
}

#[test]
fn first_model_selection_can_label_an_active_legacy_run() {
    let message = message("legacy-run-message", DeliveryMode::Steer, "start", 1);
    let run_id = RunId::new("legacy-active").unwrap();
    let state = ActorState::new()
        .fold_page(JournalPage {
            head: Revision::new(2),
            events: vec![
                StoredEvent {
                    revision: Revision::new(1),
                    event: ActorEvent::message_admitted(message),
                },
                StoredEvent {
                    revision: Revision::new(2),
                    event: ActorEvent::context_appended(context(
                        ContextTransition::Messages {
                            run_id: run_id.clone(),
                            consumed_message_ids: message_ids(&["legacy-run-message"]),
                        },
                        json!([]),
                        2,
                    )),
                },
            ],
        })
        .unwrap();

    let selection = model_selection("primary", "legacy-model");
    let event = state
        .plan_model_selection(selection.clone())
        .expect("migration may establish the first selection during an active run");
    let state = state
        .fold_page(JournalPage {
            head: Revision::new(3),
            events: vec![StoredEvent {
                revision: Revision::new(3),
                event,
            }],
        })
        .unwrap();

    assert_eq!(state.selected_model(), Some(&selection));
    assert_eq!(state.active_run(), Some(&run_id));
    assert!(matches!(
        state.plan_model_selection(model_selection("other", "other-model")),
        Err(StateError::ModelSwitchDuringRun { .. })
    ));
}

#[test]
fn model_identity_cannot_be_rebound_and_v1_events_still_project() {
    let selected = model_selection("stable", "model-a");
    let state = ActorState::new()
        .fold_page(JournalPage {
            head: Revision::new(1),
            events: vec![StoredEvent {
                revision: Revision::new(1),
                event: ActorEvent::model_selected(selected),
            }],
        })
        .unwrap();
    assert!(matches!(
        state.plan_model_selection(model_selection("stable", "model-b")),
        Err(StateError::ModelDescriptorChanged { .. })
    ));

    let mut encoded = serde_json::to_value(ActorEvent::message_admitted(message(
        "legacy",
        DeliveryMode::Steer,
        "old",
        1,
    )))
    .unwrap();
    encoded["schemaVersion"] = json!(1);
    let legacy: ActorEvent = serde_json::from_value(encoded).unwrap();
    let projected = ActorState::new()
        .fold_page(JournalPage {
            head: Revision::new(1),
            events: vec![StoredEvent {
                revision: Revision::new(1),
                event: legacy,
            }],
        })
        .unwrap();
    assert_eq!(projected.pending_messages().count(), 1);
}

#[test]
fn projection_is_independent_of_page_boundaries() {
    let run = RunId::new("replayed-run").expect("fixture id is valid");
    let events = vec![
        ActorEvent::message_admitted(message("replayed", DeliveryMode::Steer, "hello", 1)),
        ActorEvent::context_appended(context(
            ContextTransition::Messages {
                run_id: run.clone(),
                consumed_message_ids: message_ids(&["replayed"]),
            },
            json!([{ "message": "hello" }]),
            2,
        )),
        ActorEvent::context_appended(context(
            ContextTransition::Model {
                run_id: run,
                progress: RunProgress::Complete,
            },
            json!({ "text": "done" }),
            3,
        )),
    ];
    let stored = events
        .into_iter()
        .enumerate()
        .map(|(index, event)| StoredEvent {
            revision: Revision::new(u64::try_from(index + 1).expect("fixture revision fits")),
            event,
        })
        .collect::<Vec<_>>();

    let whole = ActorState::new()
        .fold_page(JournalPage {
            head: Revision::new(3),
            events: stored.clone(),
        })
        .expect("whole journal should project");
    let paged = ActorState::new()
        .fold_page(JournalPage {
            head: Revision::new(3),
            events: stored[..1].to_vec(),
        })
        .expect("first page should project")
        .fold_page(JournalPage {
            head: Revision::new(3),
            events: stored[1..].to_vec(),
        })
        .expect("second page should project");

    assert_eq!(paged, whole);
}

#[test]
fn only_message_transitions_can_start_a_run() {
    let state = ActorState::new();
    let run = RunId::new("orphaned-run").expect("fixture id is valid");
    let transitions = [
        ContextTransition::Model {
            run_id: run.clone(),
            progress: RunProgress::Continue,
        },
        ContextTransition::Eval {
            run_id: run.clone(),
        },
    ];

    for transition in transitions {
        assert!(matches!(
            state.plan_context_append(context(transition, json!({}), 1)),
            Err(StateError::RunNotActive { .. })
        ));
    }
}

#[test]
fn actor_messages_are_always_steering() {
    let queued_actor_message = MessageEnvelope::new(
        MessageId::new("actor-message").expect("fixture id is valid"),
        MessageSource::Actor {
            actor_id: ActorId::new("sender").expect("fixture id is valid"),
        },
        DeliveryMode::Queue,
        EncodedPayload::lam_json(json!({ "message": "hello" })).expect("fixture JSON is valid"),
        Timestamp::from_unix_millis(1),
    );

    assert!(matches!(
        queued_actor_message,
        Err(lam_core::MessageError::ActorMustSteer)
    ));
}

#[tokio::test]
async fn single_event_batches_validate_without_the_full_preview_clone() {
    let store = MemStore::new();
    let actor = actor_id();
    let first = message("m1", DeliveryMode::Steer, "first", 1);
    append_successfully(
        &store,
        &actor,
        Revision::ZERO,
        ActorEvent::message_admitted(first),
    )
    .await;
    let state = catch_up(&store, &actor, ActorState::default()).await;

    // A fresh single-event admission validates on the fast path.
    let fresh = message("m2", DeliveryMode::Steer, "second", 2);
    state
        .validate_batch(&EventBatch::one(ActorEvent::message_admitted(fresh)))
        .expect("a fresh single-event admission validates");

    // The duplicate rejection the preview path enforces is preserved.
    let duplicate = message("m1", DeliveryMode::Steer, "first", 1);
    let error = state
        .validate_batch(&EventBatch::one(ActorEvent::message_admitted(duplicate)))
        .expect_err("a duplicate single-event admission is rejected");
    assert!(matches!(error, StateError::DuplicateMessage { .. }));

    // Context validation still runs on the fast path: a Messages transition
    // must consume exactly the eligible set.
    let orphan = context(
        ContextTransition::Messages {
            run_id: RunId::new("r1").expect("fixture run id is valid"),
            consumed_message_ids: Vec::new(),
        },
        json!({ "messages": [] }),
        3,
    );
    let error = state
        .validate_batch(&EventBatch::one(ActorEvent::context_appended(orphan)))
        .expect_err("an empty Messages transition is rejected");
    assert!(matches!(error, StateError::MessageBatchMismatch { .. }));
}
