//! Deterministic end-to-end coverage for the single-actor model loop.

mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::task::Poll;
use std::time::{Duration, Instant};

use lam::{
    Actor, ActorError, CompactionArtifact, CompactionConfig, CompactionOutput, CompactionPlan,
    CompactionReason, CompactionRecord, CompactionRequest, Compactor, ContextAmount,
    ContextSequence, ContextTransition, DeliveryMode, EncodedPayload, Lam, MemStore, Model,
    ModelDelta, ModelResponseMetadata, ModelSwitchPolicy, Namespace, Never, Run, RunEvent,
    RunProgress, RuntimeEvent,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use support::{
    RejectingCompactionCodec, ScriptedCodec, ScriptedProvider, ScriptedStep, eval,
    eval_with_rejected_calls, output, output_with_usage, overflow, rejected,
};

fn gated_output(value: Value, gate: Arc<Barrier>) -> ScriptedStep {
    let mut step = output(value);
    step.gate = Some(gate);
    step
}

struct InvalidCutCompactor;

impl Compactor for InvalidCutCompactor {
    fn compact<'a>(&'a self, _request: &'a CompactionRequest) -> lam::CompactionFuture<'a> {
        Box::pin(async {
            Ok(CompactionPlan {
                strategy: "invalid-cut".to_owned(),
                covers_through: ContextSequence::new(2),
                output: CompactionOutput::Artifact(CompactionArtifact::summary("invalid")),
                source: None,
                metadata: ModelResponseMetadata::default(),
            })
        })
    }
}

struct InterruptibleCompactor {
    started: mpsc::Sender<()>,
    dropped: Arc<AtomicBool>,
    calls: AtomicUsize,
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Compactor for InterruptibleCompactor {
    fn compact<'a>(&'a self, request: &'a CompactionRequest) -> lam::CompactionFuture<'a> {
        let first = self.calls.fetch_add(1, Ordering::AcqRel) == 0;
        let covers_through = request.units.last().unwrap().covers_through();
        Box::pin(async move {
            if first {
                let _drop_flag = DropFlag(Arc::clone(&self.dropped));
                let _ = self.started.send(());
                return std::future::pending().await;
            }
            Ok(CompactionPlan {
                strategy: "interruption-retry".to_owned(),
                covers_through,
                output: CompactionOutput::Artifact(CompactionArtifact::summary("resumed")),
                source: None,
                metadata: ModelResponseMetadata::default(),
            })
        })
    }
}

struct ExactCheckpointCompactor;

impl Compactor for ExactCheckpointCompactor {
    fn compact<'a>(&'a self, request: &'a CompactionRequest) -> lam::CompactionFuture<'a> {
        Box::pin(async move {
            let covers_through = request.units.last().unwrap().covers_through();
            Ok(CompactionPlan {
                strategy: "exact-checkpoint".to_owned(),
                covers_through,
                output: CompactionOutput::exact(EncodedPayload::new(
                    lam::CodecRef::new(lam::CodecId::new("test/scripted-compaction").unwrap(), 1),
                    json!({ "native": "opaque" }),
                )),
                source: Some(
                    EncodedPayload::lam_json(json!({
                        "full": "provider response"
                    }))
                    .unwrap(),
                ),
                metadata: ModelResponseMetadata::default(),
            })
        })
    }
}

#[derive(Default)]
struct ExactThenPortableCompactor {
    calls: AtomicUsize,
}

impl Compactor for ExactThenPortableCompactor {
    fn compact<'a>(&'a self, request: &'a CompactionRequest) -> lam::CompactionFuture<'a> {
        Box::pin(async move {
            let covers_through = request.units.last().unwrap().covers_through();
            if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                return Ok(CompactionPlan {
                    strategy: "exact-checkpoint".to_owned(),
                    covers_through,
                    output: CompactionOutput::exact(EncodedPayload::new(
                        lam::CodecRef::new(
                            lam::CodecId::new("test/scripted-compaction").unwrap(),
                            1,
                        ),
                        json!({ "native": "opaque" }),
                    )),
                    source: None,
                    metadata: ModelResponseMetadata::default(),
                });
            }
            if !request.units.iter().any(|unit| {
                unit.entries().iter().any(|entry| {
                    matches!(entry.entry.transition, ContextTransition::Compaction { .. })
                })
            }) {
                return Err(lam::CompactionError::new(
                    "the previous exact checkpoint was omitted",
                ));
            }
            Ok(CompactionPlan {
                strategy: "portable-checkpoint".to_owned(),
                covers_through,
                output: CompactionOutput::Artifact(CompactionArtifact::summary(
                    "portable checkpoint",
                )),
                source: None,
                metadata: ModelResponseMetadata::default(),
            })
        })
    }
}

async fn build_actor(
    provider: ScriptedProvider,
    namespaces: impl IntoIterator<Item = Namespace>,
) -> Actor<MemStore> {
    let mut builder = Lam::builder(Model::new(provider, ScriptedCodec));
    for namespace in namespaces {
        builder = builder.namespace(namespace);
    }
    builder
        .build()
        .actor("main")
        .build()
        .await
        .expect("fixture actor should build")
}

#[tokio::test(flavor = "current_thread")]
async fn new_actor_starts_with_one_durable_model_selection() {
    let actor = Lam::builder(Model::new(ScriptedProvider::new([]), ScriptedCodec))
        .initial_model_id("primary")
        .build()
        .actor("genesis")
        .build()
        .await
        .unwrap();
    let state = actor.actor_ref().state().await.unwrap();
    assert_eq!(state.revision(), lam::Revision::new(1));
    assert!(state.context().is_empty());
    assert_eq!(state.selected_model().unwrap().model_id.as_str(), "primary");
}

async fn wait_for_model_start(run: &mut Run<String>) {
    loop {
        match run.next().await {
            Some(RunEvent::ModelStarted { .. }) => return,
            Some(_) => {}
            None => panic!("run ended before a model request started"),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn model_eval_builtin_result_and_terminal_output_form_one_run() {
    let mut final_step = output(json!("done"));
    final_step.deltas = vec![
        ModelDelta::Reasoning("checking result".to_owned()),
        ModelDelta::Text("done".to_owned()),
    ];
    let provider = ScriptedProvider::new([eval("await acme.math.double(21)"), final_step]);
    let math = Namespace::new("acme.math", "Fixture arithmetic.").function(
        "double",
        "Doubles one integer.",
        |_context, input: i64| async move { Ok::<_, Never>(input * 2) },
    );
    let mut actor = build_actor(provider.clone(), [math]).await;
    let actor_ref = actor.actor_ref();

    let mut run = actor.call(json!({ "task": "double 21" }));
    let mut events = Vec::new();
    while let Some(event) = run.next().await {
        events.push(event);
    }
    let answer = run.await.expect("scripted run should complete");
    assert_eq!(answer, "done");
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::EvalCompleted {
            outcome: lam::EvalOutcome::Success { .. },
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::ModelDelta {
            delta: ModelDelta::Text(text),
            ..
        } if text == "done"
    )));

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let second_request = requests[1].value.to_string();
    assert!(
        second_request.contains("lam/eval") && second_request.contains("42"),
        "the provider must receive the durable eval outcome: {second_request}"
    );

    let state = actor_ref.state().await.expect("state should project");
    assert_eq!(state.context().len(), 4);
    assert!(matches!(
        state.context()[0].entry.transition,
        ContextTransition::Messages { .. }
    ));
    assert!(matches!(
        state.context()[1].entry.transition,
        ContextTransition::Model {
            progress: RunProgress::Continue,
            ..
        }
    ));
    assert!(matches!(
        state.context()[2].entry.transition,
        ContextTransition::Eval { .. }
    ));
    assert!(matches!(
        state.context()[3].entry.transition,
        ContextTransition::Model {
            progress: RunProgress::Complete,
            ..
        }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn parallel_eval_calls_execute_the_first_and_reject_the_rest() {
    let provider = ScriptedProvider::new([
        eval_with_rejected_calls("await acme.math.double(21)", 2),
        output("done"),
    ]);
    let math = Namespace::new("acme.math", "Fixture arithmetic.").function(
        "double",
        "Doubles one integer.",
        |_context, input: i64| async move { Ok::<_, Never>(input * 2) },
    );
    let mut actor = build_actor(provider.clone(), [math]).await;
    let actor_ref = actor.actor_ref();

    let mut run = actor.call("run the calls");
    let mut outcomes = Vec::new();
    while let Some(event) = run.next().await {
        if let RunEvent::EvalCompleted { outcome, .. } = event {
            outcomes.push(outcome);
        }
    }
    assert_eq!(run.await.unwrap(), "done");
    assert!(matches!(outcomes[0], lam::EvalOutcome::Success { .. }));
    assert_eq!(outcomes.len(), 3);
    for outcome in &outcomes[1..] {
        let lam::EvalOutcome::Rejected { message } = outcome else {
            panic!("sibling eval should be rejected: {outcome:?}");
        };
        assert!(message.contains("executes only the first"));
        assert!(message.contains("Promise.all"));
    }

    let state = actor_ref.state().await.unwrap();
    let evals = state
        .context()
        .iter()
        .filter(|entry| matches!(entry.entry.transition, ContextTransition::Eval { .. }))
        .collect::<Vec<_>>();
    assert_eq!(evals.len(), 3);
    assert_eq!(evals[0].entry.payload.value["status"], "success");
    assert_eq!(evals[1].entry.payload.value["status"], "rejected");
    assert_eq!(evals[2].entry.payload.value["status"], "rejected");

    let requests = provider.requests();
    let follow_up = requests[1].value.to_string();
    assert_eq!(follow_up.matches("lam/eval").count(), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_directive_returns_its_reason_and_the_run_continues() {
    let provider = ScriptedProvider::new([
        rejected(
            "This eval call was not executed: eval arguments are invalid: unknown field `timeout_ms`.",
        ),
        output("recovered"),
    ]);
    let mut actor = build_actor(provider.clone(), []).await;
    let actor_ref = actor.actor_ref();

    let mut run = actor.call("do the work");
    let mut outcomes = Vec::new();
    while let Some(event) = run.next().await {
        if let RunEvent::EvalCompleted { outcome, .. } = event {
            outcomes.push(outcome);
        }
    }
    assert_eq!(run.await.unwrap(), "recovered");
    assert_eq!(outcomes.len(), 1);
    let lam::EvalOutcome::Rejected { message } = &outcomes[0] else {
        panic!("the invalid directive should surface as a rejection: {outcomes:?}");
    };
    assert!(message.contains("unknown field"));

    let state = actor_ref.state().await.unwrap();
    let transitions = state
        .context()
        .iter()
        .map(|entry| &entry.entry.transition)
        .collect::<Vec<_>>();
    assert!(matches!(transitions[0], ContextTransition::Messages { .. }));
    assert!(matches!(
        transitions[1],
        ContextTransition::Model {
            progress: RunProgress::Continue,
            ..
        }
    ));
    assert!(matches!(transitions[2], ContextTransition::Eval { .. }));
    assert!(matches!(
        transitions[3],
        ContextTransition::Model {
            progress: RunProgress::Complete,
            ..
        }
    ));
    assert_eq!(state.context()[2].entry.payload.value["status"], "rejected");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let follow_up = requests[1].value.to_string();
    assert!(
        follow_up.contains("unknown field `timeout_ms`"),
        "the model must see why its call was rejected: {follow_up}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn consecutive_rejected_directives_fail_the_run_at_the_cap() {
    let provider = ScriptedProvider::new([
        rejected("first invalid directive"),
        rejected("second invalid directive"),
        rejected("third invalid directive"),
    ]);
    let mut actor = build_actor(provider.clone(), []).await;
    let actor_ref = actor.actor_ref();

    let error = actor.call("spin").await.unwrap_err();
    let ActorError::Codec { message } = &error else {
        panic!("a non-converging model should fail the run: {error:?}");
    };
    assert!(message.contains("3 consecutive"), "{message}");

    // Every rejected response is durable with its paired rejection result.
    let state = actor_ref.state().await.unwrap();
    let evals = state
        .context()
        .iter()
        .filter(|entry| matches!(entry.entry.transition, ContextTransition::Eval { .. }))
        .collect::<Vec<_>>();
    assert_eq!(evals.len(), 3);
    for eval in evals {
        assert_eq!(eval.entry.payload.value["status"], "rejected");
    }
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn manual_compaction_materializes_one_durable_replay_record() {
    let provider = ScriptedProvider::new([
        output("first answer"),
        output("compact state"),
        output("second answer"),
    ]);
    let mut actor = Lam::builder(Model::new(provider.clone(), ScriptedCodec))
        .compaction_config(CompactionConfig::default().retain(ContextAmount::Tokens(0)))
        .build()
        .actor("main")
        .build()
        .await
        .expect("fixture actor should build");
    let actor_ref = actor.actor_ref();
    let mut runtime_events = actor
        .take_runtime_events()
        .expect("event stream is available");

    assert_eq!(actor.call("first task").await.unwrap(), "first answer");
    let receipt = actor
        .compact()
        .await
        .expect("manual compaction succeeds")
        .expect("old context is compactable");
    assert_eq!(receipt.reason, CompactionReason::Manual);
    assert_eq!(receipt.covers_through.get(), 2);

    assert!(matches!(
        runtime_events.next().await,
        Some(RuntimeEvent::CompactionStarted {
            run_id: None,
            reason: CompactionReason::Manual,
        })
    ));
    assert!(matches!(
        runtime_events.next().await,
        Some(RuntimeEvent::CompactionCompleted {
            run_id: None,
            reason: CompactionReason::Manual,
            covers_through,
            ..
        }) if covers_through.get() == 2
    ));

    let state = actor_ref.state().await.expect("state should project");
    assert_eq!(
        state.context().len(),
        1,
        "the projection keeps only the marker; the journal keeps the history"
    );
    let marker = state.context().last().unwrap();
    let record = CompactionRecord::decode(&marker.entry.payload)
        .unwrap()
        .expect("durable compaction record");
    assert_eq!(record.artifact.unwrap().summary, "compact state");
    assert_eq!(record.reason, CompactionReason::Manual);
    assert!(record.source.is_some(), "raw summary response is retained");
    assert_eq!(
        record.replacement.codec.id.as_str(),
        "test/scripted-compaction"
    );

    assert_eq!(actor.call("second task").await.unwrap(), "second answer");
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].value["enableEval"], false);
    assert_eq!(requests[1].value["maxOutputTokens"], 8_192);
    let replay = &requests[2].value["context"];
    assert_eq!(replay.as_array().unwrap().len(), 2);
    assert_eq!(replay[0]["transition"]["kind"], "compaction");
    assert!(
        replay[0]["payload"]["value"].get("source").is_none(),
        "ephemeral replay must not clone the raw summary response"
    );
    assert_eq!(replay[1]["transition"]["kind"], "messages");

    let state = actor_ref.state().await.expect("state should project");
    assert_eq!(
        state.context().len(),
        3,
        "only the post-compaction window is projected"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn exact_compaction_checkpoint_bypasses_neutral_materialization() {
    let provider = ScriptedProvider::new([output("first"), output("second")]);
    let mut actor = Lam::builder(Model::new(provider.clone(), ScriptedCodec))
        .compactor(ExactCheckpointCompactor)
        .compaction_config(CompactionConfig::default().retain(ContextAmount::Tokens(0)))
        .build()
        .actor("exact-compaction")
        .build()
        .await
        .unwrap();
    let actor_ref = actor.actor_ref();

    assert_eq!(actor.call("first").await.unwrap(), "first");
    actor.compact().await.unwrap().unwrap();
    let state = actor_ref.state().await.unwrap();
    let record = CompactionRecord::decode(&state.context().last().unwrap().entry.payload)
        .unwrap()
        .unwrap();
    assert!(record.artifact.is_none());
    assert_eq!(record.replacement.value["native"], "opaque");
    assert_eq!(record.source.unwrap().value["full"], "provider response");

    assert_eq!(actor.call("second").await.unwrap(), "second");
    assert_eq!(
        provider.requests().len(),
        2,
        "exact compaction does no inference"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn model_switch_recompacts_a_marker_only_exact_checkpoint() {
    let source = ScriptedProvider::new([output("source answer")]);
    let target = ScriptedProvider::new([output("target answer")]);
    let mut actor = Lam::builder(Model::new(source, ScriptedCodec))
        .initial_model_id("source")
        .compactor(ExactThenPortableCompactor::default())
        .compaction_config(CompactionConfig::default().retain(ContextAmount::Tokens(0)))
        .model("target", Model::new(target.clone(), ScriptedCodec))
        .build()
        .actor("exact-marker-switch")
        .build()
        .await
        .unwrap();

    assert_eq!(actor.call("first").await.unwrap(), "source answer");
    actor.compact().await.unwrap().unwrap();
    let receipt = actor.switch_model("target").await.unwrap();
    assert_eq!(receipt.compaction.unwrap().strategy, "portable-checkpoint");
    assert_eq!(actor.call("second").await.unwrap(), "target answer");
    assert!(
        target.requests()[0].value["context"][0]
            .to_string()
            .contains("portable checkpoint")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn neutral_recompaction_includes_an_exact_checkpoint_and_its_tail() {
    let provider = ScriptedProvider::new([
        output("first answer"),
        output("second answer"),
        output("third answer"),
    ]);
    let mut actor = Lam::builder(Model::new(provider.clone(), ScriptedCodec))
        .compactor(ExactThenPortableCompactor::default())
        .compaction_config(CompactionConfig::default().retain(ContextAmount::Tokens(0)))
        .build()
        .actor("exact-marker-tail")
        .build()
        .await
        .unwrap();

    assert_eq!(actor.call("first").await.unwrap(), "first answer");
    actor.compact().await.unwrap().unwrap();
    assert_eq!(actor.call("second").await.unwrap(), "second answer");
    let receipt = actor.compact().await.unwrap().unwrap();
    assert_eq!(receipt.strategy, "portable-checkpoint");
    assert_eq!(actor.call("third").await.unwrap(), "third answer");
    assert!(
        provider.requests()[2].value["context"][0]
            .to_string()
            .contains("portable checkpoint")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn model_switch_compacts_then_atomically_selects_the_target() {
    let source = ScriptedProvider::new([output("source answer"), output("portable state")]);
    let target = ScriptedProvider::new([output("target answer")]);
    let mut actor = Lam::builder(Model::new(source.clone(), ScriptedCodec))
        .initial_model_id("source")
        .model("target", Model::new(target.clone(), ScriptedCodec))
        .build()
        .actor("switching")
        .build()
        .await
        .unwrap();
    let actor_ref = actor.actor_ref();

    assert_eq!(actor.call("first").await.unwrap(), "source answer");
    let receipt = actor.switch_model("target").await.unwrap();
    assert_eq!(receipt.previous_model_id.as_str(), "source");
    assert_eq!(receipt.selected_model_id.as_str(), "target");
    let compaction = receipt.compaction.expect("default switch compacts");
    assert_eq!(compaction.reason, CompactionReason::ModelSwitch);
    assert_eq!(compaction.revision.get() + 1, receipt.revision.get());

    let state = actor_ref.state().await.unwrap();
    assert_eq!(state.selected_model().unwrap().model_id.as_str(), "target");
    let marker = state.context().last().unwrap();
    let record = CompactionRecord::decode(&marker.entry.payload)
        .unwrap()
        .unwrap();
    assert_eq!(record.reason, CompactionReason::ModelSwitch);
    assert_eq!(record.artifact.unwrap().summary, "portable state");

    assert_eq!(actor.call("second").await.unwrap(), "target answer");
    assert_eq!(source.requests().len(), 2);
    assert_eq!(source.requests()[1].value["enableEval"], false);
    let target_requests = target.requests();
    assert_eq!(target_requests.len(), 1);
    assert_eq!(
        target_requests[0].value["context"][0]["transition"]["kind"],
        "compaction"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failed_model_switch_leaves_selection_and_context_unchanged() {
    let source = ScriptedProvider::new([output("source answer"), output("portable state")]);
    let target = ScriptedProvider::new([]);
    let mut actor = Lam::builder(Model::new(source, ScriptedCodec))
        .initial_model_id("source")
        .model("target", Model::new(target, RejectingCompactionCodec))
        .build()
        .actor("failed-switch")
        .build()
        .await
        .unwrap();
    let actor_ref = actor.actor_ref();
    assert_eq!(actor.call("first").await.unwrap(), "source answer");
    let before = actor_ref.state().await.unwrap();

    let error = actor.switch_model("target").await.unwrap_err();
    assert!(error.to_string().contains("target model codec rejected"));
    let after = actor_ref.state().await.unwrap();
    assert_eq!(after.revision(), before.revision());
    assert_eq!(after.context(), before.context());
    assert_eq!(after.selected_model(), before.selected_model());
}

#[tokio::test(flavor = "current_thread")]
async fn reuse_context_switch_preflights_without_compaction() {
    let source = ScriptedProvider::new([output("source answer")]);
    let target = ScriptedProvider::new([output("target answer")]);
    let mut actor = Lam::builder(Model::new(source.clone(), ScriptedCodec))
        .initial_model_id("source")
        .model("target", Model::new(target.clone(), ScriptedCodec))
        .build()
        .actor("reuse-switch")
        .build()
        .await
        .unwrap();
    assert_eq!(actor.call("first").await.unwrap(), "source answer");

    let receipt = actor
        .switch_model_with_policy("target", ModelSwitchPolicy::ReuseContext)
        .await
        .unwrap();
    assert!(receipt.compaction.is_none());
    assert_eq!(source.requests().len(), 1);
    assert_eq!(actor.call("second").await.unwrap(), "target answer");
    assert_eq!(target.requests().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn post_compaction_estimate_ignores_usage_from_before_the_marker() {
    let provider = ScriptedProvider::new([
        output_with_usage("first", 1_000),
        output("summary"),
        output("done"),
    ]);
    let config = CompactionConfig::default()
        .context_window_tokens(1_000)
        .trigger_at(ContextAmount::Tokens(500))
        .retain(ContextAmount::Tokens(70))
        .summary_reserve_tokens(100);
    let mut actor = Lam::builder(Model::new(provider.clone(), ScriptedCodec))
        .compaction_config(config)
        .build()
        .actor("main")
        .build()
        .await
        .unwrap();

    assert_eq!(actor.call("one").await.unwrap(), "first");
    assert_eq!(actor.call("two").await.unwrap(), "done");

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].value["enableEval"], false);
    assert_eq!(
        requests[2].value["context"][0]["transition"]["kind"],
        "compaction"
    );
    assert!(
        requests[2].value["context"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["payload"]["value"]["usageTotal"] == 1_000),
        "the pre-marker model response should remain in the exact tail"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn provider_usage_is_authoritative_over_large_durable_payloads() {
    let provider =
        ScriptedProvider::new([output_with_usage("x".repeat(20_000), 100), output("done")]);
    let config = CompactionConfig::default()
        .context_window_tokens(1_000)
        .trigger_at(ContextAmount::Tokens(500))
        .retain(ContextAmount::Tokens(0))
        .summary_reserve_tokens(100);
    let mut actor = Lam::builder(Model::new(provider.clone(), ScriptedCodec))
        .compaction_config(config)
        .build()
        .actor("usage-anchor")
        .build()
        .await
        .unwrap();
    let actor_ref = actor.actor_ref();

    assert_eq!(actor.call("one").await.unwrap(), "x".repeat(20_000));
    assert_eq!(actor.call("two").await.unwrap(), "done");

    assert_eq!(provider.requests().len(), 2);
    assert!(
        actor_ref
            .state()
            .await
            .unwrap()
            .context()
            .iter()
            .all(|entry| {
                !matches!(entry.entry.transition, ContextTransition::Compaction { .. })
            })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn unmetered_suffix_is_added_to_provider_usage() {
    let provider = ScriptedProvider::new([
        output_with_usage("first", 450),
        output("summary"),
        output("done"),
    ]);
    let config = CompactionConfig::default()
        .context_window_tokens(1_000)
        .trigger_at(ContextAmount::Tokens(500))
        .retain(ContextAmount::Tokens(0))
        .summary_reserve_tokens(100);
    let mut actor = Lam::builder(Model::new(provider.clone(), ScriptedCodec))
        .compaction_config(config)
        .build()
        .actor("usage-suffix")
        .build()
        .await
        .unwrap();
    let actor_ref = actor.actor_ref();

    assert_eq!(actor.call("one").await.unwrap(), "first");
    assert_eq!(actor.call("x".repeat(600)).await.unwrap(), "done");

    assert_eq!(provider.requests().len(), 3);
    assert!(
        actor_ref
            .state()
            .await
            .unwrap()
            .context()
            .iter()
            .any(|entry| {
                matches!(entry.entry.transition, ContextTransition::Compaction { .. })
            })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn automatic_compaction_uses_the_selected_models_context_window() {
    let wide = ScriptedProvider::new([output_with_usage("wide first", 500), output("wide second")]);
    let narrow = ScriptedProvider::new([
        output("narrow summary"),
        output_with_usage("narrow answer", 500),
    ]);
    let config = CompactionConfig::default()
        .retain(ContextAmount::Tokens(0))
        .summary_reserve_tokens(50);
    let mut actor =
        Lam::builder(Model::new(wide.clone(), ScriptedCodec).with_context_window_tokens(1_000))
            .initial_model_id("wide")
            .model(
                "narrow",
                Model::new(narrow.clone(), ScriptedCodec).with_context_window_tokens(400),
            )
            .compaction_config(config)
            .build()
            .actor("model-windows")
            .build()
            .await
            .unwrap();
    let actor_ref = actor.actor_ref();

    assert_eq!(actor.call("first").await.unwrap(), "wide first");
    actor
        .switch_model_with_policy("narrow", ModelSwitchPolicy::ReuseContext)
        .await
        .unwrap();
    assert_eq!(actor.call("second").await.unwrap(), "narrow answer");
    actor
        .switch_model_with_policy("wide", ModelSwitchPolicy::ReuseContext)
        .await
        .unwrap();
    assert_eq!(actor.call("third").await.unwrap(), "wide second");

    assert_eq!(narrow.requests().len(), 2);
    assert_eq!(wide.requests().len(), 2);
    assert_eq!(
        actor_ref
            .state()
            .await
            .unwrap()
            .context()
            .iter()
            .filter(|entry| matches!(entry.entry.transition, ContextTransition::Compaction { .. }))
            .count(),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_materialized_replacement_is_never_persisted() {
    let provider = ScriptedProvider::new([output("first"), output("summary")]);
    let mut actor = Lam::builder(Model::new(provider, RejectingCompactionCodec))
        .compaction_config(CompactionConfig::default().retain(ContextAmount::Tokens(0)))
        .build()
        .actor("main")
        .build()
        .await
        .unwrap();
    let actor_ref = actor.actor_ref();

    assert_eq!(actor.call("one").await.unwrap(), "first");
    let error = actor.compact().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("rejected its materialized replacement")
    );
    assert!(
        actor_ref
            .state()
            .await
            .unwrap()
            .context()
            .iter()
            .all(|entry| !matches!(entry.entry.transition, ContextTransition::Compaction { .. }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_compaction_updates_the_summary_and_selects_the_newest_marker() {
    let provider = ScriptedProvider::new([
        output("first"),
        output("summary one"),
        output("second"),
        output("summary two"),
        output("third"),
    ]);
    let mut actor = Lam::builder(Model::new(provider.clone(), ScriptedCodec))
        .compaction_config(CompactionConfig::default().retain(ContextAmount::Tokens(0)))
        .build()
        .actor("main")
        .build()
        .await
        .unwrap();
    let actor_ref = actor.actor_ref();

    assert_eq!(actor.call("one").await.unwrap(), "first");
    actor.compact().await.unwrap().unwrap();
    assert_eq!(actor.call("two").await.unwrap(), "second");
    actor.compact().await.unwrap().unwrap();
    assert_eq!(actor.call("three").await.unwrap(), "third");

    let state = actor_ref.state().await.unwrap();
    let markers = state
        .context()
        .iter()
        .filter(|entry| matches!(entry.entry.transition, ContextTransition::Compaction { .. }))
        .collect::<Vec<_>>();
    assert_eq!(markers.len(), 1, "the covered first marker is truncated");
    let newest = CompactionRecord::decode(&markers[0].entry.payload)
        .unwrap()
        .unwrap();
    assert_eq!(newest.artifact.unwrap().summary, "summary two");
    assert_eq!(
        state.context().len(),
        3,
        "only the newest marker and its tail are projected"
    );

    let requests = provider.requests();
    assert_eq!(
        requests[3].value["context"][0]["transition"]["kind"],
        "compaction"
    );
    assert!(
        requests[3].value["context"][0]
            .to_string()
            .contains("summary one"),
        "iterative summarization receives the prior artifact"
    );
    assert_eq!(requests[4].value["context"].as_array().unwrap().len(), 2);
    assert!(
        requests[4].value["context"][0]
            .to_string()
            .contains("summary two"),
        "normal inference replays only the newest compatible marker"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn threshold_compaction_is_transparent_and_emits_run_events() {
    let provider = ScriptedProvider::new([output("short state"), output("done")]);
    let config = CompactionConfig::default()
        .context_window_tokens(400)
        .trigger_at(ContextAmount::Tokens(100))
        .retain(ContextAmount::Tokens(0))
        .summary_reserve_tokens(50);
    let mut actor = Lam::builder(Model::new(provider.clone(), ScriptedCodec))
        .compaction_config(config)
        .build()
        .actor("main")
        .build()
        .await
        .expect("fixture actor should build");
    let actor_ref = actor.actor_ref();

    let mut run = actor.call("x".repeat(600));
    let mut events = Vec::new();
    while let Some(event) = run.next().await {
        events.push(event);
    }
    assert_eq!(run.await.unwrap(), "done");
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::CompactionStarted {
            reason: CompactionReason::Threshold,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::CompactionCompleted {
            reason: CompactionReason::Threshold,
            covers_through,
            ..
        } if covers_through.get() == 1
    )));

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].value["enableEval"], false);
    assert_eq!(requests[1].value["enableEval"], true);
    let state = actor_ref.state().await.unwrap();
    assert_eq!(state.context().len(), 2, "the covered prefix is truncated");
    assert!(matches!(
        state.context()[0].entry.transition,
        ContextTransition::Compaction { .. }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn interruption_drops_automatic_compaction_and_keeps_the_actor_resumable() {
    let (started_sender, started_receiver) = mpsc::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let provider = ScriptedProvider::new([output("resumed")]);
    let config = CompactionConfig::default()
        .context_window_tokens(400)
        .trigger_at(ContextAmount::Tokens(100))
        .retain(ContextAmount::Tokens(0))
        .summary_reserve_tokens(50);
    let mut actor = Lam::builder(Model::new(provider, ScriptedCodec))
        .compactor(InterruptibleCompactor {
            started: started_sender,
            dropped: Arc::clone(&dropped),
            calls: AtomicUsize::new(0),
        })
        .compaction_config(config)
        .build()
        .actor("compaction-interruption")
        .build()
        .await
        .expect("fixture actor should build");
    let handle = actor.handle();
    let interrupt_thread = std::thread::spawn(move || {
        started_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("automatic compaction should start");
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(handle.interrupt())
            .expect("interruption should become durable")
            .expect("one run should be active")
    });

    let started_at = Instant::now();
    assert_eq!(
        actor.call("x".repeat(600)).await.unwrap_err(),
        ActorError::Interrupted
    );
    let receipt = interrupt_thread
        .join()
        .expect("interrupt thread should finish");
    assert!(started_at.elapsed() < Duration::from_secs(3));
    assert_eq!(receipt.isolate_state, lam::IsolateState::Retained);
    assert!(dropped.load(Ordering::Acquire));

    assert_eq!(actor.call("continue").await.unwrap(), "resumed");
    actor.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn steering_during_compaction_is_delivered_before_agent_inference() {
    let gate = Arc::new(Barrier::new(2));
    let provider = ScriptedProvider::new([
        gated_output(json!("state"), Arc::clone(&gate)),
        output("done"),
    ]);
    let config = CompactionConfig::default()
        .context_window_tokens(400)
        .trigger_at(ContextAmount::Tokens(100))
        .retain(ContextAmount::Tokens(0))
        .summary_reserve_tokens(50);
    let mut actor = Lam::builder(Model::new(provider.clone(), ScriptedCodec))
        .compaction_config(config)
        .build()
        .actor("main")
        .build()
        .await
        .unwrap();
    let actor_ref = actor.actor_ref();

    let mut run = actor.call("x".repeat(600));
    loop {
        if matches!(run.next().await, Some(RunEvent::CompactionStarted { .. })) {
            break;
        }
    }
    actor_ref
        .send("steered while summarizing", DeliveryMode::Steer)
        .await
        .unwrap();
    gate.wait();
    assert_eq!(run.await.unwrap(), "done");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].value["context"]
            .to_string()
            .contains("steered while summarizing"),
        "steering admitted during summary inference must reach the next agent request"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn provider_overflow_compacts_once_then_retries_inference() {
    let provider = ScriptedProvider::new([overflow(), output("overflow state"), output("done")]);
    let mut actor = Lam::builder(Model::new(provider.clone(), ScriptedCodec))
        .compaction_config(
            CompactionConfig::default()
                .retain(ContextAmount::Tokens(0))
                .summary_reserve_tokens(50),
        )
        .build()
        .actor("main")
        .build()
        .await
        .unwrap();

    let mut run = actor.call("large task");
    let mut events = Vec::new();
    while let Some(event) = run.next().await {
        events.push(event);
    }
    assert_eq!(run.await.unwrap(), "done");
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::CompactionCompleted {
            reason: CompactionReason::Overflow,
            ..
        }
    )));
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].value["enableEval"], true);
    assert_eq!(requests[1].value["enableEval"], false);
    assert_eq!(requests[2].value["enableEval"], true);
}

#[tokio::test(flavor = "current_thread")]
async fn disabled_compaction_returns_context_overflow_without_hidden_fallback() {
    let provider = ScriptedProvider::new([overflow()]);
    let mut actor = Lam::builder(Model::new(provider, ScriptedCodec))
        .disable_compaction()
        .build()
        .actor("main")
        .build()
        .await
        .unwrap();

    assert_eq!(
        actor.call("large task").await,
        Err(ActorError::ContextOverflow)
    );
    assert_eq!(actor.compact().await, Err(ActorError::CompactionDisabled));
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_atomic_cut_emits_failure_without_appending_a_marker() {
    let provider = ScriptedProvider::new([eval("1 + 1"), output("done")]);
    let mut actor = Lam::builder(Model::new(provider, ScriptedCodec))
        .compaction_config(CompactionConfig::default().retain(ContextAmount::Tokens(0)))
        .compactor(InvalidCutCompactor)
        .build()
        .actor("main")
        .build()
        .await
        .unwrap();
    let actor_ref = actor.actor_ref();
    let mut runtime_events = actor.take_runtime_events().unwrap();
    assert_eq!(actor.call("compute").await.unwrap(), "done");

    let error = actor.compact().await.unwrap_err();
    assert!(matches!(error, ActorError::Compaction { .. }));
    assert!(matches!(
        runtime_events.next().await,
        Some(RuntimeEvent::CompactionStarted { .. })
    ));
    assert!(matches!(
        runtime_events.next().await,
        Some(RuntimeEvent::CompactionFailed { ref message, .. })
            if message.contains("atomic context boundary")
    ));
    let state = actor_ref.state().await.unwrap();
    assert_eq!(state.context().len(), 4);
    assert!(
        !state
            .context()
            .iter()
            .any(|entry| matches!(entry.entry.transition, ContextTransition::Compaction { .. }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failed_summary_inference_leaves_raw_context_unchanged() {
    let provider = ScriptedProvider::new([output("done"), overflow()]);
    let mut actor = Lam::builder(Model::new(provider, ScriptedCodec))
        .compaction_config(CompactionConfig::default().retain(ContextAmount::Tokens(0)))
        .build()
        .actor("main")
        .build()
        .await
        .unwrap();
    let actor_ref = actor.actor_ref();
    assert_eq!(actor.call("task").await.unwrap(), "done");

    assert!(matches!(
        actor.compact().await,
        Err(ActorError::Compaction { .. })
    ));
    let state = actor_ref.state().await.unwrap();
    assert_eq!(state.context().len(), 2);
    assert!(
        !state
            .context()
            .iter()
            .any(|entry| matches!(entry.entry.transition, ContextTransition::Compaction { .. }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn default_system_prompt_describes_the_installed_manifest() {
    let provider = ScriptedProvider::new([output("done")]);
    let math = Namespace::new("acme.math", "Fixture arithmetic.").function(
        "double",
        "Doubles one integer.\n\nLonger documentation stays discoverable.",
        |_context, input: i64| async move { Ok::<_, Never>(input * 2) },
    );
    let mut actor = build_actor(provider.clone(), [math]).await;

    actor.call("hello").await.expect("scripted call completes");
    let requests = provider.requests();
    let prompt = requests[0].value["systemPrompt"]
        .as_str()
        .expect("scripted codec records the system prompt");
    assert!(prompt.starts_with("You are a coding agent with one tool, `eval`"));
    assert!(prompt.contains("not a general Node.js or Deno runtime"));
    assert!(prompt.contains("Do not import modules or call unlisted platform globals."));
    assert!(prompt.contains("`lam.dir(query?: { path?: string })"));
    assert!(prompt.contains("`lam.result<T extends JsonValue>(value: T): T`"));
    assert!(
        prompt
            .contains("`acme.math.double(input: number): Promise<number>` — Doubles one integer.")
    );
    assert!(!prompt.contains("Longer documentation"));
}

#[tokio::test(flavor = "current_thread")]
async fn actor_builder_can_discard_console_logs_without_changing_eval_results() {
    let provider = ScriptedProvider::new([
        eval("console.log('discarded', { value: 42 }); lam.result(42)"),
        output("done"),
    ]);
    let mut actor = Lam::builder(Model::new(provider.clone(), ScriptedCodec))
        .capture_console(false)
        .build()
        .actor("main")
        .build()
        .await
        .expect("fixture actor should build");

    actor
        .call("compute")
        .await
        .expect("scripted call completes");

    let requests = provider.requests();
    let eval = &requests[1].value["context"][2]["payload"]["value"];
    assert_eq!(
        eval["output"]["result"],
        json!({ "kind": "json", "value": 42 })
    );
    assert_eq!(eval["output"]["logs"], json!([]));
}

#[tokio::test(flavor = "current_thread")]
async fn custom_system_prompt_replaces_the_default_before_annotations() {
    let provider = ScriptedProvider::new([output("done")]);
    let mut actor = Lam::builder(Model::new(provider.clone(), ScriptedCodec))
        .annotate_system_prompt("first annotation")
        .system_prompt("custom instructions")
        .annotate_system_prompt("second annotation")
        .build()
        .actor("main")
        .build()
        .await
        .expect("fixture actor should build");

    actor.call("hello").await.expect("scripted call completes");
    let requests = provider.requests();
    assert_eq!(
        requests[0].value["systemPrompt"],
        "custom instructions\n\nfirst annotation\n\nsecond annotation"
    );
}

#[derive(Debug, Deserialize, JsonSchema, PartialEq)]
struct Review {
    score: u32,
    summary: String,
}

#[tokio::test(flavor = "current_thread")]
async fn structured_output_is_schema_driven_and_deserialized_after_completion() {
    let provider = ScriptedProvider::new([output(json!({
        "score": 9,
        "summary": "tidy",
    }))]);
    let mut actor = build_actor(provider.clone(), []).await;

    let review = actor
        .call("review this")
        .output::<Review>()
        .await
        .expect("structured output should decode");
    assert_eq!(
        review,
        Review {
            score: 9,
            summary: "tidy".to_owned(),
        }
    );
    let requests = provider.requests();
    assert_eq!(requests[0].value["output"]["kind"], "structured");
    assert!(requests[0].value["output"]["schema"].is_object());
}

#[tokio::test(flavor = "current_thread")]
async fn actor_send_returns_after_admission_without_waiting_for_completion() {
    let gate = Arc::new(Barrier::new(2));
    let provider = ScriptedProvider::new([gated_output(json!("done"), Arc::clone(&gate))]);
    let actor = build_actor(provider, []).await;

    let receipt = actor
        .send("start", DeliveryMode::Queue)
        .await
        .expect("send should admit the message");
    let state = actor
        .actor_ref()
        .state()
        .await
        .expect("state should project");
    assert!(state.message(&receipt.message_id).is_some());

    gate.wait();
    let state = wait_for_context(&actor.actor_ref(), 2).await;
    assert!(matches!(
        state.context()[1].entry.transition,
        ContextTransition::Model {
            progress: RunProgress::Complete,
            ..
        }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn send_admits_after_the_actor_owner_is_gone() {
    let actor = build_actor(ScriptedProvider::new([]), []).await;
    let actor_ref = actor.actor_ref();
    drop(actor);

    let receipt = actor_ref
        .send("offline", DeliveryMode::Queue)
        .await
        .expect("send should depend on journal admission, not runner residency");
    let state = actor_ref.state().await.expect("state should project");
    assert!(state.message(&receipt.message_id).is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_structured_output_preserves_completion_and_returns_decode_error() {
    let provider = ScriptedProvider::new([output(json!({ "score": "wrong" }))]);
    let mut actor = build_actor(provider, []).await;
    let actor_ref = actor.actor_ref();

    let error = actor
        .call("review this")
        .output::<Review>()
        .await
        .expect_err("invalid output should fail at the Rust boundary");
    assert!(matches!(error, ActorError::OutputDecode { .. }));
    let state = actor_ref.state().await.expect("state should project");
    assert!(matches!(
        state
            .context()
            .last()
            .expect("terminal context should exist")
            .entry
            .transition,
        ContextTransition::Model {
            progress: RunProgress::Complete,
            ..
        }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn steering_wins_the_terminal_race_without_repeating_the_first_request() {
    let gate = Arc::new(Barrier::new(2));
    let provider = ScriptedProvider::new([
        gated_output(json!("premature"), Arc::clone(&gate)),
        output(json!("final")),
    ]);
    let mut actor = build_actor(provider.clone(), []).await;
    let actor_ref = actor.actor_ref();
    let mut run = actor.call("start");
    wait_for_model_start(&mut run).await;

    actor_ref
        .send("one more thing", DeliveryMode::Steer)
        .await
        .expect("steering should become durable");
    gate.wait();
    let answer = run.await.expect("steered run should complete");
    assert_eq!(answer, "final");
    assert_eq!(provider.requests().len(), 2);

    let state = actor_ref.state().await.expect("state should project");
    let transitions = state
        .context()
        .iter()
        .map(|entry| &entry.entry.transition)
        .collect::<Vec<_>>();
    assert!(matches!(
        transitions[1],
        ContextTransition::Model {
            progress: RunProgress::Continue,
            ..
        }
    ));
    assert!(matches!(transitions[2], ContextTransition::Messages { .. }));
    assert!(matches!(
        transitions[3],
        ContextTransition::Model {
            progress: RunProgress::Complete,
            ..
        }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn steer_admitted_during_an_eval_is_delivered_at_the_tool_result_boundary() {
    let eval_started = Arc::new(tokio::sync::Notify::new());
    let steer_admitted = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new([eval("await acme.sync.block(1)"), output(json!("done"))]);
    let sync = Namespace::new("acme.sync", "Test synchronization.").function(
        "block",
        "Blocks until the steer is admitted.",
        {
            let eval_started = Arc::clone(&eval_started);
            let steer_admitted = Arc::clone(&steer_admitted);
            move |_context, _input: i64| {
                let eval_started = Arc::clone(&eval_started);
                let steer_admitted = Arc::clone(&steer_admitted);
                async move {
                    eval_started.notify_one();
                    steer_admitted.notified().await;
                    Ok::<_, Never>(1)
                }
            }
        },
    );
    let mut actor = build_actor(provider.clone(), [sync]).await;
    let actor_ref = actor.actor_ref();

    let mut run = actor.call("start");
    wait_for_model_start(&mut run).await;
    eval_started.notified().await;
    let receipt = actor_ref
        .send("also do this", DeliveryMode::Steer)
        .await
        .expect("steering should become durable");
    steer_admitted.notify_one();
    assert_eq!(run.await.expect("run should complete"), "done");

    // The steer enters model-visible context directly after the eval
    // outcome, and the next model request already contains it.
    assert_eq!(provider.requests().len(), 2);
    let second_request = provider.requests()[1].value.to_string();
    assert!(
        second_request.contains("also do this"),
        "the request after the tool result must contain the steer: {second_request}"
    );
    let state = actor_ref.state().await.expect("state should project");
    let transitions = state
        .context()
        .iter()
        .map(|entry| &entry.entry.transition)
        .collect::<Vec<_>>();
    assert!(matches!(transitions[0], ContextTransition::Messages { .. }));
    assert!(matches!(
        transitions[1],
        ContextTransition::Model {
            progress: RunProgress::Continue,
            ..
        }
    ));
    assert!(matches!(transitions[2], ContextTransition::Eval { .. }));
    let ContextTransition::Messages {
        consumed_message_ids,
        ..
    } = transitions[3]
    else {
        panic!("the steer must be delivered directly after the eval result: {transitions:?}");
    };
    assert_eq!(
        consumed_message_ids,
        std::slice::from_ref(&receipt.message_id)
    );
    assert!(matches!(
        transitions[4],
        ContextTransition::Model {
            progress: RunProgress::Complete,
            ..
        }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn queued_message_allows_completion_then_starts_later_work() {
    let gate = Arc::new(Barrier::new(2));
    let provider = ScriptedProvider::new([
        gated_output(json!("first"), Arc::clone(&gate)),
        output(json!("queued")),
    ]);
    let mut actor = build_actor(provider.clone(), []).await;
    let actor_ref = actor.actor_ref();
    let mut run = actor.call("start");
    wait_for_model_start(&mut run).await;

    actor_ref
        .send("later", DeliveryMode::Queue)
        .await
        .expect("queued message should become durable");
    gate.wait();
    assert_eq!(run.await.expect("first run should complete"), "first");

    let state = wait_for_context(&actor_ref, 4).await;
    assert_eq!(provider.requests().len(), 2);
    assert!(matches!(
        state.context()[1].entry.transition,
        ContextTransition::Model {
            progress: RunProgress::Complete,
            ..
        }
    ));
    assert!(matches!(
        state.context()[2].entry.transition,
        ContextTransition::Messages { .. }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn graceful_shutdown_finishes_current_work_without_draining_queued_wakes() {
    let gate = Arc::new(Barrier::new(2));
    let provider = ScriptedProvider::new([
        gated_output(json!("first"), Arc::clone(&gate)),
        output(json!("must remain pending")),
    ]);
    let mut actor = build_actor(provider.clone(), []).await;
    let actor_ref = actor.actor_ref();
    let mut run = actor.call("start");
    wait_for_model_start(&mut run).await;
    drop(run);

    let queued = actor_ref
        .send("later", DeliveryMode::Queue)
        .await
        .expect("queued message should become durable");
    let mut shutdown = Box::pin(actor.shutdown());
    std::future::poll_fn(|context| {
        assert!(matches!(shutdown.as_mut().poll(context), Poll::Pending));
        Poll::Ready(())
    })
    .await;
    gate.wait();
    shutdown.await.expect("shutdown should join");

    assert_eq!(provider.requests().len(), 1);
    let state = actor_ref.state().await.expect("state should project");
    assert!(
        state.is_run_completed(
            state.context()[0]
                .entry
                .transition
                .run_id()
                .expect("message context starts a run")
        )
    );
    assert!(
        state
            .message(&queued.message_id)
            .expect("queued message should remain queryable")
            .consumed_at
            .is_none()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_a_run_detaches_without_permitting_an_overlapping_call() {
    let gate = Arc::new(Barrier::new(2));
    let provider = ScriptedProvider::new([gated_output(json!("done"), Arc::clone(&gate))]);
    let mut actor = build_actor(provider, []).await;
    let actor_ref = actor.actor_ref();
    let mut first = actor.call("first");
    wait_for_model_start(&mut first).await;
    drop(first);

    let error = actor
        .call("second")
        .await
        .expect_err("detached work should retain the call lease");
    assert_eq!(error, ActorError::Busy);
    gate.wait();
    let state = wait_for_context(&actor_ref, 2).await;
    assert!(matches!(
        state.context()[1].entry.transition,
        ContextTransition::Model {
            progress: RunProgress::Complete,
            ..
        }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn cloned_actor_handles_share_exclusive_operation_admission() {
    let gate = Arc::new(Barrier::new(2));
    let provider = ScriptedProvider::new([
        gated_output(json!("first"), Arc::clone(&gate)),
        output(json!("second")),
    ]);
    let actor = build_actor(provider, []).await;
    let handle = actor.handle();
    let other = handle.clone();
    let mut first = handle.call("first");
    wait_for_model_start(&mut first).await;

    assert_eq!(
        other.call("overlapping").await.unwrap_err(),
        ActorError::Busy
    );
    assert_eq!(other.compact().await.unwrap_err(), ActorError::Busy);

    gate.wait();
    assert_eq!(first.await.unwrap(), "first");
    assert_eq!(other.call("second").await.unwrap(), "second");
}

async fn wait_for_context(actor: &lam::ActorRef<MemStore>, count: usize) -> lam::ActorState {
    for _ in 0..200 {
        let state = actor.state().await.expect("state should project");
        if state.context().len() >= count {
            return state;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("actor did not reach {count} context entries");
}
