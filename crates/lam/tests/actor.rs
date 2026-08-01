//! Deterministic end-to-end coverage for the single-actor model loop.

use std::collections::VecDeque;
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use lam::{
    Actor, ActorError, ContextTransition, DeliveryMode, EncodedPayload, EvalRequest, Lam, MemStore,
    Model, ModelCodec, ModelDelta, ModelDirective, ModelEventSink, ModelProvider, Namespace, Never,
    OutputContract, Run, RunEvent, RunProgress,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone)]
struct ScriptedProvider {
    shared: Arc<ScriptedState>,
}

struct ScriptedState {
    steps: Mutex<VecDeque<ScriptedStep>>,
    requests: Mutex<Vec<EncodedPayload>>,
}

struct ScriptedStep {
    response: EncodedPayload,
    deltas: Vec<ModelDelta>,
    gate: Option<Arc<Barrier>>,
}

impl ScriptedProvider {
    fn new(steps: impl IntoIterator<Item = ScriptedStep>) -> Self {
        Self {
            shared: Arc::new(ScriptedState {
                steps: Mutex::new(steps.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            }),
        }
    }

    fn requests(&self) -> Vec<EncodedPayload> {
        self.shared
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ModelProvider for ScriptedProvider {
    type Error = ScriptError;

    fn invoke(
        &self,
        request: EncodedPayload,
        events: ModelEventSink,
    ) -> impl Future<Output = Result<EncodedPayload, Self::Error>> + Send {
        let shared = Arc::clone(&self.shared);
        async move {
            shared
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request);
            let step = shared
                .steps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .ok_or_else(|| ScriptError("scripted provider exhausted".to_owned()))?;
            for delta in step.deltas {
                events.emit(delta);
            }
            if let Some(gate) = step.gate {
                gate.wait();
            }
            Ok(step.response)
        }
    }
}

#[derive(Clone, Copy)]
struct ScriptedCodec;

impl ModelCodec for ScriptedCodec {
    type Error = ScriptError;

    fn encode_request(
        &self,
        context: &[lam::ProjectedContextEntry],
        output: &OutputContract,
    ) -> Result<EncodedPayload, Self::Error> {
        let context = context
            .iter()
            .map(|entry| {
                json!({
                    "transition": &entry.entry.transition,
                    "payload": &entry.entry.payload,
                })
            })
            .collect::<Vec<_>>();
        let output = match output {
            OutputContract::Text => json!({ "kind": "text" }),
            OutputContract::Structured { schema } => {
                json!({ "kind": "structured", "schema": schema })
            }
        };
        Ok(native(json!({ "context": context, "output": output })))
    }

    fn interpret_response(&self, response: &EncodedPayload) -> Result<ModelDirective, Self::Error> {
        if response.codec != scripted_codec() {
            return Err(ScriptError("unexpected response codec".to_owned()));
        }
        match response.value.get("kind").and_then(Value::as_str) {
            Some("eval") => {
                let source = response
                    .value
                    .get("source")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ScriptError("eval response has no source".to_owned()))?;
                let timeout = response
                    .value
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .map(Duration::from_millis);
                Ok(ModelDirective::Eval(EvalRequest {
                    source: source.to_owned(),
                    timeout,
                }))
            }
            Some("output") => Ok(ModelDirective::Output(
                response
                    .value
                    .get("value")
                    .cloned()
                    .ok_or_else(|| ScriptError("output response has no value".to_owned()))?,
            )),
            Some(kind) => Err(ScriptError(format!("unknown directive `{kind}`"))),
            None => Err(ScriptError("response has no directive".to_owned())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct ScriptError(String);

fn scripted_codec() -> lam::CodecRef {
    lam::CodecRef::new(
        lam::CodecId::new("test/scripted").expect("fixture codec is valid"),
        1,
    )
}

fn native(value: Value) -> EncodedPayload {
    EncodedPayload::new(scripted_codec(), value)
}

fn eval(source: &str) -> ScriptedStep {
    ScriptedStep {
        response: native(json!({ "kind": "eval", "source": source })),
        deltas: Vec::new(),
        gate: None,
    }
}

fn output(value: Value) -> ScriptedStep {
    ScriptedStep {
        response: native(json!({ "kind": "output", "value": value })),
        deltas: Vec::new(),
        gate: None,
    }
}

fn gated_output(value: Value, gate: Arc<Barrier>) -> ScriptedStep {
    ScriptedStep {
        response: native(json!({ "kind": "output", "value": value })),
        deltas: Vec::new(),
        gate: Some(gate),
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

async fn wait_for_model_start(run: &mut Run<'_, String>) {
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
