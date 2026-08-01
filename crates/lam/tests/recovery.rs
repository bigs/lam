//! Recovery and explicit actor-lifecycle coverage over the durable backend.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use lam::{
    Actor, ActorError, ActorEvent, ActorId, ActorRef, AppendOutcome, ContextEntry,
    ContextTransition, DeliveryMode, EncodedPayload, EventBatch, InterruptedEvalOutcome,
    IsolateState, JournalStore, Lam, MessageEnvelope, MessageId, MessageSource, Model,
    ModelDescriptor, ModelEventSink, ModelProvider, Namespace, Never, Revision, RunId, RunProgress,
    RuntimeEvent, SYSTEM_NOTICE_CODEC_ID, SYSTEM_NOTICE_CODEC_VERSION, SystemNotice, Timestamp,
};
use lam_redb::RedbStore;
use serde_json::json;

use support::{ScriptError, ScriptedCodec, ScriptedProvider, eval, output};

struct PendingProvider {
    started: mpsc::Sender<()>,
    dropped: Arc<AtomicBool>,
}

impl ModelProvider for PendingProvider {
    type Error = ScriptError;

    fn invoke(
        &self,
        _request: EncodedPayload,
        _events: ModelEventSink,
    ) -> impl Future<Output = Result<EncodedPayload, Self::Error>> + Send {
        let started = self.started.clone();
        let dropped = Arc::clone(&self.dropped);
        async move {
            let _drop_flag = DropFlag(dropped);
            let _ = started.send(());
            std::future::pending::<Result<EncodedPayload, ScriptError>>().await
        }
    }
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

async fn build_actor(
    store: RedbStore,
    provider: ScriptedProvider,
    namespaces: impl IntoIterator<Item = Namespace>,
) -> Actor<RedbStore> {
    let mut builder = Lam::builder(Model::new(provider, ScriptedCodec))
        .state_store(store)
        .max_eval_timeout(Duration::from_secs(10));
    for namespace in namespaces {
        builder = builder.namespace(namespace);
    }
    builder
        .build()
        .actor("durable")
        .build()
        .await
        .expect("fixture actor should build")
}

#[tokio::test(flavor = "current_thread")]
async fn active_version_one_run_gains_its_first_model_selection_on_reopen() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("legacy-active.redb");
    let store = RedbStore::create(&path).expect("store should open");
    let actor_id = ActorId::new("durable").unwrap();
    let message_id = MessageId::new("legacy-message").unwrap();
    let run_id = RunId::new("legacy-run").unwrap();
    let message = MessageEnvelope::new(
        message_id.clone(),
        MessageSource::User { principal: None },
        DeliveryMode::Steer,
        EncodedPayload::lam_json(json!({ "message": "legacy" })).unwrap(),
        Timestamp::from_unix_millis(1),
    )
    .unwrap();
    let messages = ContextEntry {
        transition: ContextTransition::Messages {
            run_id: run_id.clone(),
            consumed_message_ids: vec![message_id],
        },
        payload: EncodedPayload::lam_json(json!([{ "message": "legacy" }])).unwrap(),
        recorded_at: Timestamp::from_unix_millis(2),
    };
    let version_one = |event: ActorEvent| {
        let mut value = serde_json::to_value(event).unwrap();
        value["schemaVersion"] = json!(1);
        serde_json::from_value(value).unwrap()
    };
    let outcome = store
        .append(
            &actor_id,
            Revision::ZERO,
            EventBatch::new(
                version_one(ActorEvent::message_admitted(message)),
                vec![version_one(ActorEvent::context_appended(messages))],
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        AppendOutcome::Appended {
            head: Revision::new(2)
        }
    );

    let provider = ScriptedProvider::new([output("recovered")]);
    let actor = build_actor(store, provider.clone(), []).await;
    let state = wait_for_context(&actor.actor_ref(), 3).await;
    assert_eq!(state.active_run(), None);
    assert!(state.is_run_completed(&run_id));
    assert!(state.selected_model().is_some());
    assert_eq!(provider.requests().len(), 1);
    actor.shutdown().await.expect("shutdown should join");
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_graceful_restarts_wait_until_a_call_batches_the_notices() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("quiescent.redb");
    let mut actor = build_actor(
        RedbStore::create(&path).expect("store should open"),
        ScriptedProvider::new([output("first")]),
        [],
    )
    .await;
    assert_eq!(
        actor.call("first").await.expect("first run should finish"),
        "first"
    );
    actor.shutdown().await.expect("shutdown should join");

    let dormant_provider = ScriptedProvider::new([]);
    let mut actor = build_actor(
        RedbStore::open(&path).expect("store should reopen"),
        dormant_provider.clone(),
        [],
    )
    .await;
    let mut runtime_events = actor
        .take_runtime_events()
        .expect("runtime events should have one consumer");
    let state = actor
        .actor_ref()
        .state()
        .await
        .expect("state should project");
    assert!(
        dormant_provider.requests().is_empty(),
        "a resumption notice alone must not wake a quiescent actor"
    );
    let first_notice_id = state
        .pending_messages()
        .next()
        .expect("first resumption notice should remain pending")
        .envelope
        .message_id()
        .clone();
    assert_runtime_event(
        runtime_events
            .next()
            .await
            .expect("first resumption event should be buffered"),
        &first_notice_id,
        None,
        None,
    );
    actor.shutdown().await.expect("shutdown should join");

    let provider = ScriptedProvider::new([output("second")]);
    let mut actor = build_actor(
        RedbStore::open(&path).expect("store should reopen"),
        provider.clone(),
        [],
    )
    .await;
    let mut runtime_events = actor
        .take_runtime_events()
        .expect("runtime events should have one consumer");
    assert!(
        provider.requests().is_empty(),
        "a resumption notice alone must not wake a quiescent actor"
    );

    let state = actor
        .actor_ref()
        .state()
        .await
        .expect("state should project");
    let notices = state.pending_messages().collect::<Vec<_>>();
    assert_eq!(notices.len(), 2, "one notice per resumed runtime");
    assert_eq!(notices[0].envelope.message_id(), &first_notice_id);
    assert_runtime_notice(&notices[0].envelope, None, None);
    assert_runtime_notice(&notices[1].envelope, None, None);
    let notice_id = notices[1].envelope.message_id().clone();
    assert_runtime_event(
        runtime_events
            .next()
            .await
            .expect("resumption event should be buffered"),
        &notice_id,
        None,
        None,
    );

    assert_eq!(
        actor
            .call("second")
            .await
            .expect("second run should finish"),
        "second"
    );
    let state = actor
        .actor_ref()
        .state()
        .await
        .expect("state should project");
    assert_eq!(state.context().len(), 4);
    let ContextTransition::Messages {
        consumed_message_ids,
        ..
    } = &state.context()[2].entry.transition
    else {
        panic!("the second run should start with one message batch");
    };
    assert_eq!(consumed_message_ids.len(), 3);
    assert_eq!(consumed_message_ids[0], first_notice_id);
    assert_eq!(consumed_message_ids[1], notice_id);
    actor.shutdown().await.expect("shutdown should join");
}

#[tokio::test(flavor = "current_thread")]
async fn reopen_resolves_the_durable_model_selection_from_the_registry() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("selected-model.redb");
    let source = ScriptedProvider::new([output("source answer"), output("portable state")]);
    let target = ScriptedProvider::new([]);
    let mut actor = Lam::builder(Model::new(source, ScriptedCodec))
        .initial_model_id("source")
        .model("target", Model::new(target, ScriptedCodec))
        .state_store(RedbStore::create(&path).expect("store should open"))
        .build()
        .actor("durable-selection")
        .build()
        .await
        .unwrap();
    assert_eq!(actor.call("first").await.unwrap(), "source answer");
    actor.switch_model("target").await.unwrap();
    actor.shutdown().await.unwrap();

    let reopened_source = ScriptedProvider::new([]);
    let reopened_target = ScriptedProvider::new([output("target answer")]);
    let mut actor = Lam::builder(Model::new(reopened_source.clone(), ScriptedCodec))
        .initial_model_id("source")
        .model("target", Model::new(reopened_target.clone(), ScriptedCodec))
        .state_store(RedbStore::open(&path).expect("store should reopen"))
        .build()
        .actor("durable-selection")
        .build()
        .await
        .unwrap();
    assert_eq!(
        actor
            .actor_ref()
            .state()
            .await
            .unwrap()
            .selected_model()
            .unwrap()
            .model_id
            .as_str(),
        "target"
    );
    assert_eq!(actor.call("second").await.unwrap(), "target answer");
    assert!(reopened_source.requests().is_empty());
    assert_eq!(reopened_target.requests().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn reopen_rejects_a_missing_or_rebound_durable_model() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("registry-mismatch.redb");
    let source = ScriptedProvider::new([output("source answer"), output("portable state")]);
    let mut actor = Lam::builder(Model::new(source, ScriptedCodec))
        .initial_model_id("source")
        .model(
            "target",
            Model::new(ScriptedProvider::new([]), ScriptedCodec),
        )
        .state_store(RedbStore::create(&path).unwrap())
        .build()
        .actor("registry-mismatch")
        .build()
        .await
        .unwrap();
    assert_eq!(actor.call("first").await.unwrap(), "source answer");
    actor.switch_model("target").await.unwrap();
    actor.shutdown().await.unwrap();

    let missing = Lam::builder(Model::new(ScriptedProvider::new([]), ScriptedCodec))
        .initial_model_id("source")
        .state_store(RedbStore::open(&path).unwrap())
        .build()
        .actor("registry-mismatch")
        .build()
        .await;
    let Err(missing) = missing else {
        panic!("reopen should reject a missing durable model");
    };
    assert!(
        missing
            .to_string()
            .contains("not present in the runtime registry")
    );

    let rebound = Model::new(ScriptedProvider::new([]), ScriptedCodec)
        .with_descriptor(ModelDescriptor::new("different", "different", "different").unwrap());
    let mismatch = Lam::builder(Model::new(ScriptedProvider::new([]), ScriptedCodec))
        .initial_model_id("source")
        .model("target", rebound)
        .state_store(RedbStore::open(&path).unwrap())
        .build()
        .actor("registry-mismatch")
        .build()
        .await;
    let Err(mismatch) = mismatch else {
        panic!("reopen should reject a rebound durable model");
    };
    assert!(mismatch.to_string().contains("descriptor does not match"));
}

#[tokio::test(flavor = "current_thread")]
async fn startup_wakes_substantive_durable_mail() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("pending.redb");
    let actor = build_actor(
        RedbStore::create(&path).expect("store should open"),
        ScriptedProvider::new([]),
        [],
    )
    .await;
    let actor_ref = actor.actor_ref();
    actor.shutdown().await.expect("shutdown should join");
    actor_ref
        .send("offline", DeliveryMode::Queue)
        .await
        .expect("offline admission should succeed");
    drop(actor_ref);

    let provider = ScriptedProvider::new([output("processed")]);
    let mut actor = build_actor(
        RedbStore::open(&path).expect("store should reopen"),
        provider.clone(),
        [],
    )
    .await;
    let mut runtime_events = actor
        .take_runtime_events()
        .expect("runtime events should have one consumer");
    let state = wait_for_context(&actor.actor_ref(), 2).await;
    assert_eq!(provider.requests().len(), 1);
    let ContextTransition::Messages {
        consumed_message_ids,
        ..
    } = &state.context()[0].entry.transition
    else {
        panic!("recovered mail should start a run");
    };
    assert_eq!(consumed_message_ids.len(), 2, "mail and notice batch");
    assert!(matches!(
        state.context()[1].entry.transition,
        ContextTransition::Model {
            progress: RunProgress::Complete,
            ..
        }
    ));
    assert!(matches!(
        runtime_events.next().await,
        Some(RuntimeEvent::RuntimeResumed { .. })
    ));
    actor.shutdown().await.expect("shutdown should join");
}

#[tokio::test(flavor = "current_thread")]
async fn abort_drops_an_in_flight_provider_future() {
    let (started_sender, started_receiver) = mpsc::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let provider = PendingProvider {
        started: started_sender,
        dropped: Arc::clone(&dropped),
    };
    let mut actor = Lam::builder(Model::new(provider, ScriptedCodec))
        .build()
        .actor("provider-abort")
        .build()
        .await
        .expect("fixture actor should build");
    let abort_handle = actor.abort_handle();
    let abort_thread = std::thread::spawn(move || {
        started_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("provider request should start");
        abort_handle.abort();
    });

    assert_eq!(
        actor
            .call("begin")
            .await
            .expect_err("the active call should observe its abort"),
        ActorError::Aborted
    );
    abort_thread.join().expect("abort thread should finish");
    actor.abort().await.expect("abort should join the runner");
    assert!(
        dropped.load(Ordering::Acquire),
        "cancellation must drop the provider future"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn abort_interrupts_v8_and_recovery_reports_unknown_eval_outcome() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("aborted.redb");
    let (started_sender, started_receiver) = mpsc::channel();
    let started = Namespace::new("test.control", "Recovery synchronization.").function(
        "started",
        "Signals that eval entered the isolate.",
        move |_context, (): ()| {
            let started_sender = started_sender.clone();
            async move {
                let _ = started_sender.send(());
                Ok::<(), Never>(())
            }
        },
    );
    let mut actor = build_actor(
        RedbStore::create(&path).expect("store should open"),
        ScriptedProvider::new([eval("await test.control.started(); while (true) {}")]),
        [started],
    )
    .await;
    let actor_ref = actor.actor_ref();
    let queued_ref = actor_ref.clone();
    let abort_handle = actor.abort_handle();
    let abort_thread = std::thread::spawn(move || {
        started_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("eval should enter the isolate");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("orchestration runtime should build");
        let queued = runtime
            .block_on(queued_ref.send("later", DeliveryMode::Queue))
            .expect("queued mail should become durable before abort");
        abort_handle.abort();
        queued
    });

    let abort_started = Instant::now();
    let error = actor
        .call("begin")
        .await
        .expect_err("the active call should observe its abort");
    assert_eq!(error, ActorError::Aborted);
    let queued = abort_thread.join().expect("abort thread should finish");
    actor.abort().await.expect("abort should join the runner");
    assert!(
        abort_started.elapsed() < Duration::from_secs(3),
        "abort should not wait for the eval timeout"
    );
    let interrupted = actor_ref.state().await.expect("state should project");
    assert_eq!(interrupted.context().len(), 2);
    assert!(matches!(
        interrupted.context()[1].entry.transition,
        ContextTransition::Model {
            progress: RunProgress::Continue,
            ..
        }
    ));
    assert!(
        interrupted
            .context()
            .iter()
            .all(|entry| !matches!(entry.entry.transition, ContextTransition::Eval { .. })),
        "abort must not invent a durable eval outcome"
    );
    assert!(
        interrupted
            .message(&queued.message_id)
            .expect("queued message should survive abort")
            .consumed_at
            .is_none()
    );
    let active_run = interrupted
        .active_run()
        .expect("interrupted run should remain active")
        .clone();
    drop(actor_ref);

    let provider = ScriptedProvider::new([output("recovered"), output("queued")]);
    let mut actor = build_actor(
        RedbStore::open(&path).expect("store should reopen"),
        provider.clone(),
        [],
    )
    .await;
    let mut runtime_events = actor
        .take_runtime_events()
        .expect("runtime events should have one consumer");
    let state = wait_for_context(&actor.actor_ref(), 6).await;
    assert!(state.is_run_completed(&active_run));
    assert_eq!(provider.requests().len(), 2);
    assert!(state.pending_messages().next().is_none());
    let resumed_messages = &state.context()[2].entry;
    let ContextTransition::Messages {
        consumed_message_ids,
        run_id,
    } = &resumed_messages.transition
    else {
        panic!("the resumption notice should steer the interrupted run");
    };
    assert_eq!(run_id, &active_run);
    assert_eq!(consumed_message_ids.len(), 1);
    let notice = state
        .message(&consumed_message_ids[0])
        .expect("consumed notice should remain queryable");
    assert_runtime_notice(
        &notice.envelope,
        Some(&active_run),
        Some(InterruptedEvalOutcome::Unknown),
    );
    assert_runtime_event(
        runtime_events
            .next()
            .await
            .expect("resumption event should be buffered"),
        notice.envelope.message_id(),
        Some(&active_run),
        Some(InterruptedEvalOutcome::Unknown),
    );
    let recovery_request = provider.requests()[0].value.to_string();
    assert!(
        recovery_request.contains(SYSTEM_NOTICE_CODEC_ID)
            && recovery_request.contains("interruptedEvalOutcome")
            && recovery_request.contains("unknown"),
        "the model must receive the structured outcome-unknown notice: {recovery_request}"
    );
    assert!(
        provider.requests()[1].value.to_string().contains("later"),
        "queued durable mail should start after the recovered run completes"
    );
    actor.shutdown().await.expect("shutdown should join");
}

fn assert_runtime_notice(
    message: &lam::MessageEnvelope,
    resumed_run_id: Option<&lam::RunId>,
    interrupted_eval_outcome: Option<InterruptedEvalOutcome>,
) {
    assert!(matches!(
        message.source(),
        MessageSource::Host { component } if component.as_str() == lam::RUNTIME_COMPONENT_ID
    ));
    assert_eq!(message.payload().codec.id.as_str(), SYSTEM_NOTICE_CODEC_ID);
    assert_eq!(message.payload().codec.version, SYSTEM_NOTICE_CODEC_VERSION);
    assert_eq!(
        message.delivery(),
        if resumed_run_id.is_some() {
            DeliveryMode::Steer
        } else {
            DeliveryMode::Queue
        }
    );
    assert_eq!(message.payload().value["type"], "runtimeResumed");
    assert_eq!(message.payload().value["isolateState"], "reset");
    assert!(message.payload().value.get("isolate_state").is_none());
    assert_eq!(
        message
            .payload()
            .decode::<SystemNotice>()
            .expect("notice payload should decode"),
        SystemNotice::RuntimeResumed {
            isolate_state: IsolateState::Reset,
            resumed_run_id: resumed_run_id.cloned(),
            interrupted_eval_outcome,
        }
    );
}

fn assert_runtime_event(
    event: RuntimeEvent,
    message_id: &lam::MessageId,
    resumed_run_id: Option<&lam::RunId>,
    interrupted_eval_outcome: Option<InterruptedEvalOutcome>,
) {
    let RuntimeEvent::RuntimeResumed {
        message_id: actual_message_id,
        revision,
        isolate_state,
        resumed_run_id: actual_run_id,
        interrupted_eval_outcome: actual_outcome,
    } = event
    else {
        panic!("expected runtime resumption event")
    };
    assert_eq!(&actual_message_id, message_id);
    assert!(revision > lam::Revision::ZERO);
    assert_eq!(isolate_state, IsolateState::Reset);
    assert_eq!(actual_run_id.as_ref(), resumed_run_id);
    assert_eq!(actual_outcome, interrupted_eval_outcome);
}

async fn wait_for_context(actor: &ActorRef<RedbStore>, count: usize) -> lam::ActorState {
    for _ in 0..500 {
        let state = actor.state().await.expect("state should project");
        if state.context().len() >= count {
            return state;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("actor did not reach {count} context entries");
}
