//! End-to-end scheduler and manifest-spawn coverage.

mod support;

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use lam::{
    ActorEventData, ActorId, AppendOutcome, CodecId, CodecRef, EncodedPayload, EvalRequest,
    EventBatch, JournalError, JournalPage, JournalStore, Lam, MemStore, MessageSource, Model,
    ModelCodec, ModelDescriptor, ModelDirective, ModelEventSink, ModelProvider, ModelRequestConfig,
    OutputContract, Revision,
};
use lam_agents::{
    ActorAddress, AgentOutcome, AgentSystem, AgentSystemError, AgentSystemEvent, StopReason,
    SubagentConfig, SubagentConfigError,
};
use serde_json::{Value, json};
use tokio::sync::Notify;

use support::RoundTripGate;

#[derive(Clone)]
struct RoutingProvider {
    shared: Arc<ProviderState>,
    scenario: RoutingScenario,
}

#[derive(Clone, Copy)]
enum RoutingScenario {
    General,
    ChildCall,
    BackgroundOutcome,
    SpawnWait,
    DirectStop,
    CancelCall,
    UnauthorizedStop,
}

struct ProviderState {
    requests: Mutex<Vec<EncodedPayload>>,
    request_notify: Notify,
    thread_names: Mutex<Vec<String>>,
    child_seen: AtomicBool,
    child_notify: Notify,
}

#[derive(Clone)]
struct AdmissionGateStore {
    inner: Arc<MemStore>,
    gate: Arc<AdmissionGate>,
}

struct AdmissionGate {
    actor_id: ActorId,
    armed: AtomicBool,
    entered: AtomicBool,
    entered_notify: Notify,
    release: Notify,
}

impl AdmissionGateStore {
    fn new(actor_id: &str) -> Self {
        Self {
            inner: Arc::new(MemStore::new()),
            gate: Arc::new(AdmissionGate {
                actor_id: ActorId::new(actor_id).unwrap(),
                armed: AtomicBool::new(true),
                entered: AtomicBool::new(false),
                entered_notify: Notify::new(),
                release: Notify::new(),
            }),
        }
    }

    async fn wait_until_blocked(&self) {
        loop {
            let notified = self.gate.entered_notify.notified();
            if self.gate.entered.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn release(&self) {
        self.gate.release.notify_waiters();
    }
}

impl JournalStore for AdmissionGateStore {
    type Error = Infallible;

    async fn read(
        &self,
        actor: &ActorId,
        after: Revision,
        limit: NonZeroUsize,
    ) -> Result<JournalPage, JournalError<Self::Error>> {
        self.inner.read(actor, after, limit).await
    }

    async fn append(
        &self,
        actor: &ActorId,
        expected: Revision,
        events: EventBatch,
    ) -> Result<AppendOutcome, JournalError<Self::Error>> {
        let is_admission = events
            .iter()
            .any(|event| matches!(event.data(), ActorEventData::MessageAdmitted { .. }));
        if actor == &self.gate.actor_id
            && is_admission
            && self.gate.armed.swap(false, Ordering::AcqRel)
        {
            self.gate.entered.store(true, Ordering::Release);
            self.gate.entered_notify.notify_waiters();
            self.gate.release.notified().await;
        }
        self.inner.append(actor, expected, events).await
    }
}

impl RoutingProvider {
    fn new() -> Self {
        Self::for_scenario(RoutingScenario::General)
    }

    fn for_scenario(scenario: RoutingScenario) -> Self {
        Self {
            shared: Arc::new(ProviderState {
                requests: Mutex::new(Vec::new()),
                request_notify: Notify::new(),
                thread_names: Mutex::new(Vec::new()),
                child_seen: AtomicBool::new(false),
                child_notify: Notify::new(),
            }),
            scenario,
        }
    }

    async fn wait_for_child(&self) {
        loop {
            let notified = self.shared.child_notify.notified();
            if self.shared.child_seen.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn requests(&self) -> Vec<EncodedPayload> {
        lock(&self.shared.requests).clone()
    }

    async fn wait_for_request(&self, text: &str) {
        loop {
            let notified = self.shared.request_notify.notified();
            if lock(&self.shared.requests)
                .iter()
                .any(|request| request.value.to_string().contains(text))
            {
                return;
            }
            notified.await;
        }
    }

    fn thread_names(&self) -> Vec<String> {
        lock(&self.shared.thread_names).clone()
    }
}

impl ModelProvider for RoutingProvider {
    type Error = Infallible;

    async fn invoke(
        &self,
        request: EncodedPayload,
        _events: ModelEventSink,
    ) -> Result<EncodedPayload, Self::Error> {
        lock(&self.shared.thread_names).push(
            std::thread::current()
                .name()
                .unwrap_or("unnamed")
                .to_owned(),
        );
        lock(&self.shared.requests).push(request.clone());
        self.shared.request_notify.notify_waiters();
        if let Some(response) = scenario_response(self.scenario, &request.value) {
            return Ok(native(response));
        }
        let rendered = request.value.to_string();
        if has_message(&request.value, "cancel spawn task", Some("user")) {
            if has_transition(&request.value, "eval") {
                return Ok(native(
                    json!({ "kind": "output", "value": "spawn cancelled" }),
                ));
            }
            return Ok(native(json!({
                "kind": "eval",
                "source": r#"await lam.agents.spawn({
  name: "cancelled",
  task: "must not run",
  namespaces: []
});
lam.result("unexpected")"#
            })));
        }
        if has_message(&request.value, "duplicate child name task", Some("user")) {
            if has_transition(&request.value, "eval") {
                return Ok(native(
                    json!({ "kind": "output", "value": "duplicate observed" }),
                ));
            }
            return Ok(native(json!({
                "kind": "eval",
                "source": r#"const first = await lam.agents.spawn({
  name: "same",
  task: "duplicate leaf task",
  namespaces: []
});
let duplicate;
try {
  await lam.agents.spawn({ name: "same", task: "must not run", namespaces: [] });
} catch (error) {
  duplicate = error;
}
lam.result({ first, duplicate })"#
            })));
        }
        if has_message(&request.value, "blocking task", Some("user")) {
            return Ok(native(json!({
                "kind": "eval",
                "source": "await new Promise(() => {})"
            })));
        }
        if has_message(&request.value, "LAM_CHILD_OK", Some("actor")) {
            return Ok(native(json!({ "kind": "output", "value": "LAM_CHILD_OK" })));
        }
        if has_message(&request.value, "roundtrip root task", Some("user")) {
            if find_string_field(&request.value, "address").is_some() {
                return Ok(native(json!({
                    "kind": "eval",
                    "source": "await test.roundtrip.wait({}); lam.result(\"waiting complete\")"
                })));
            }
            return Ok(native(json!({
                "kind": "eval",
                "source": r#"const child = await lam.agents.spawn({
  name: "child",
  task: "roundtrip child task",
  namespaces: ["lam.agents", "test.roundtrip"]
});
const children = await lam.agents.list();
const listed = children.some(({ address }) => address === "/roundtrip-root/child")
  ? "LIST_OK"
  : "LIST_BAD";
lam.result({ child, children, listed })"#
            })));
        }
        if has_message(&request.value, "roundtrip child task", Some("actor")) {
            if has_transition(&request.value, "eval") {
                return Ok(native(
                    json!({ "kind": "output", "value": "child complete" }),
                ));
            }
            return Ok(native(json!({
                "kind": "eval",
                "source": r#"const identity = await lam.agents.identity();
await lam.agents.send({ to: identity.parent, message: { token: "LAM_CHILD_OK" } });
await test.roundtrip.signal({});
lam.result("sent")"#
            })));
        }
        if rendered.contains("root task") {
            if let Some(address) = find_string_field(&request.value, "address") {
                return Ok(native(json!({ "kind": "output", "value": address })));
            }
            return Ok(native(json!({
                "kind": "eval",
                "source": r#"const child = await lam.agents.spawn({
  name: "worker",
  task: "child task",
  model: { provider: "test", model: "model-a" },
  namespaces: [],
  systemPrompt: "child base",
  instructions: "child instruction"
});
lam.result(child)"#
            })));
        }
        if rendered.contains("child task") {
            self.shared.child_seen.store(true, Ordering::Release);
            self.shared.child_notify.notify_waiters();
            return Ok(native(
                json!({ "kind": "output", "value": "child complete" }),
            ));
        }
        Ok(native(
            json!({ "kind": "output", "value": "plain complete" }),
        ))
    }
}

fn scenario_response(scenario: RoutingScenario, request: &Value) -> Option<Value> {
    match scenario {
        RoutingScenario::General => None,
        RoutingScenario::ChildCall => Some(child_call_response(request)),
        RoutingScenario::BackgroundOutcome => Some(background_outcome_response(request)),
        RoutingScenario::SpawnWait => Some(spawn_wait_response(request)),
        RoutingScenario::DirectStop => Some(direct_stop_response(request)),
        RoutingScenario::CancelCall => Some(cancel_call_response(request)),
        RoutingScenario::UnauthorizedStop => Some(unauthorized_stop_response(request)),
    }
}

fn child_call_response(request: &Value) -> Value {
    if has_message(request, "sync child task", Some("actor")) {
        return json!({ "kind": "output", "value": "sync child complete" });
    }
    if transition_payload_contains(request, "eval", "sync child complete") {
        return json!({ "kind": "output", "value": "SYNC_OK" });
    }
    if has_message(request, "sync call root", Some("user")) {
        return json!({
            "kind": "eval",
            "source": r#"const outcome = await lam.agents.call({
  name: "sync",
  task: "sync child task",
  namespaces: []
});
lam.result(outcome)"#
        });
    }
    json!({ "kind": "output", "value": "plain complete" })
}

fn background_outcome_response(request: &Value) -> Value {
    if has_message(request, "background child task", Some("actor")) {
        return json!({ "kind": "output", "value": "background child complete" });
    }
    if has_message(request, "background child complete", Some("actor")) {
        return json!({ "kind": "output", "value": "BACKGROUND_OK" });
    }
    if has_message(request, "background outcome root", Some("user")) {
        if has_transition(request, "eval") {
            return json!({ "kind": "output", "value": "SPAWN_RETURNED" });
        }
        return json!({
            "kind": "eval",
            "source": r#"await lam.agents.spawn({
  name: "background",
  task: "background child task",
  namespaces: []
});
lam.result("spawned")"#
        });
    }
    json!({ "kind": "output", "value": "plain complete" })
}

fn spawn_wait_response(request: &Value) -> Value {
    if has_message(request, "waited child task", Some("actor")) {
        return json!({ "kind": "output", "value": "waited child complete" });
    }
    if has_message(request, "spawn wait root", Some("user")) {
        if transition_payload_contains(request, "eval", "inboxMessageId")
            && has_message(request, "waited child complete", Some("actor"))
        {
            return json!({ "kind": "output", "value": "WAIT_OK" });
        }
        if has_transition(request, "eval") {
            return json!({ "kind": "output", "value": "WAIT_INCOMPLETE" });
        }
        return json!({
            "kind": "eval",
            "source": r#"const child = await lam.agents.spawn({
  name: "waited",
  task: "waited child task",
  namespaces: []
});
const receipt = await lam.agents.wait({ addresses: [child.address] });
lam.result(receipt)"#
        });
    }
    json!({ "kind": "output", "value": "plain complete" })
}

fn direct_stop_response(request: &Value) -> Value {
    if has_message(request, "stop child background", Some("actor")) {
        return json!({ "kind": "eval", "source": "await new Promise(() => {})" });
    }
    if has_message(request, "cancelled", Some("actor"))
        || transition_payload_contains(request, "eval", "stopped")
    {
        return json!({ "kind": "output", "value": "STOP_OK" });
    }
    if has_message(request, "stop child root", Some("user")) {
        if transition_payload_contains(request, "eval", "/stop-root/worker") {
            return json!({
                "kind": "eval",
                "source": r#"await lam.agents.stop({ address: "/stop-root/worker" });
            lam.result("stopped")"#
            });
        }
        return json!({
            "kind": "eval",
            "source": r#"await lam.agents.spawn({
  name: "worker",
  task: "stop child background",
  namespaces: []
});
lam.result("spawned")"#
        });
    }
    json!({ "kind": "output", "value": "plain complete" })
}

fn cancel_call_response(request: &Value) -> Value {
    if has_message(request, "blocking call child", Some("actor")) {
        return json!({ "kind": "eval", "source": "await new Promise(() => {})" });
    }
    if has_message(request, "cancelled", Some("actor"))
        || transition_payload_contains(request, "eval", "stopped")
    {
        return json!({ "kind": "output", "value": "STOP_OK" });
    }
    if has_message(request, "cancel child call root", Some("user")) {
        if has_transition(request, "eval") {
            return json!({ "kind": "output", "value": "CALL_CANCELLED" });
        }
        return json!({
            "kind": "eval",
            "source": r#"await lam.agents.call({
  name: "cancelled-call",
  task: "blocking call child",
  namespaces: []
});
lam.result("unexpected")"#
        });
    }
    json!({ "kind": "output", "value": "plain complete" })
}

fn unauthorized_stop_response(request: &Value) -> Value {
    if has_message(request, "reject stop root", Some("user")) {
        if transition_payload_contains(request, "eval", "notDirectChild") {
            return json!({ "kind": "output", "value": "STOP_REJECTED" });
        }
        return json!({
            "kind": "eval",
            "source": r#"let rejected;
try {
  await lam.agents.stop({ address: "/someone-else" });
} catch (error) {
  rejected = error;
}
lam.result(rejected)"#
        });
    }
    json!({ "kind": "output", "value": "plain complete" })
}

#[derive(Clone, Copy)]
struct TestCodec;

impl ModelCodec for TestCodec {
    type Error = TestCodecError;

    fn encode_request(
        &self,
        context: &[lam::ProjectedContextEntry],
        config: &ModelRequestConfig<'_>,
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
        let output = match config.output {
            OutputContract::Text => json!({ "kind": "text" }),
            OutputContract::Structured { schema } => {
                json!({ "kind": "structured", "schema": schema })
            }
        };
        Ok(native(json!({
            "context": context,
            "output": output,
            "systemPrompt": config.system_prompt,
        })))
    }

    fn interpret_response(&self, response: &EncodedPayload) -> Result<ModelDirective, Self::Error> {
        match response.value.get("kind").and_then(Value::as_str) {
            Some("eval") => Ok(ModelDirective::Eval(EvalRequest {
                intent: response
                    .value
                    .get("intent")
                    .and_then(Value::as_str)
                    .unwrap_or("Evaluate TypeScript")
                    .to_owned(),
                source: response
                    .value
                    .get("source")
                    .and_then(Value::as_str)
                    .ok_or(TestCodecError)?
                    .to_owned(),
                timeout: None,
            })),
            Some("output") => Ok(ModelDirective::Output(
                response.value.get("value").cloned().ok_or(TestCodecError)?,
            )),
            _ => Err(TestCodecError),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid test model payload")]
struct TestCodecError;

#[tokio::test(flavor = "current_thread")]
async fn one_worker_hosts_root_and_manifest_spawned_child() {
    let provider = RoutingProvider::new();
    let model = Model::new(provider.clone(), TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new())
        .worker_threads(1)
        .max_agents(4)
        .build()
        .unwrap();
    let subagents: SubagentConfig<MemStore> = SubagentConfig::builder(model.clone())
        .required_instructions("host invariant")
        .agent_namespace(false)
        .build()
        .unwrap();
    let root = system
        .host_with_subagents(
            Lam::builder(model)
                .state_store(system.state_store())
                .build()
                .actor("/root"),
            subagents,
        )
        .await
        .unwrap();

    let child_id = root.call("root task").await.unwrap();
    assert_eq!(child_id, "/root/worker");
    tokio::time::timeout(std::time::Duration::from_secs(2), provider.wait_for_child())
        .await
        .expect("the child should run without blocking its parent worker");

    let requests = provider.requests();
    let root_request = requests
        .iter()
        .find(|request| request.value.to_string().contains("root task"))
        .unwrap();
    let root_prompt = root_request.value["systemPrompt"].as_str().unwrap();
    assert!(root_prompt.contains("lam.agents.spawn"));
    assert!(root_prompt.contains("test/model-a"));
    assert!(root_prompt.contains("Agent identity: /root"));
    let child_request = requests
        .iter()
        .find(|request| request.value.to_string().contains("child task"))
        .unwrap();
    let child_prompt = child_request.value["systemPrompt"].as_str().unwrap();
    assert!(child_prompt.contains("child base"));
    assert!(child_prompt.contains("host invariant"));
    assert!(child_prompt.contains("child instruction"));
    assert!(child_prompt.contains("Agent identity: /root/worker"));
    assert!(child_prompt.contains("Parent agent: /root"));
    let child_instruction = child_prompt.find("child instruction").unwrap();
    let host_invariant = child_prompt.find("host invariant").unwrap();
    let identity = child_prompt.find("Agent identity: /root/worker").unwrap();
    assert!(child_instruction < host_invariant && host_invariant < identity);
    assert!(
        provider
            .thread_names()
            .iter()
            .all(|name| name == "lam-agent-worker-0")
    );
    let child_actor_id = ActorId::new(child_id).unwrap();
    let store = system.state_store();
    let page = store
        .read(
            &child_actor_id,
            Revision::ZERO,
            NonZeroUsize::new(64).unwrap(),
        )
        .await
        .unwrap();
    let source = page
        .events
        .iter()
        .find_map(|stored| match stored.event.data() {
            ActorEventData::MessageAdmitted { message } => Some(message.source()),
            _ => None,
        });
    assert!(matches!(
        source,
        Some(MessageSource::Actor { actor_id }) if actor_id.as_str() == "/root"
    ));

    tokio::time::timeout(std::time::Duration::from_secs(2), system.shutdown())
        .await
        .expect("the system should stop every resident actor")
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn child_call_returns_one_outcome_without_a_parent_mailbox_copy() {
    let provider = RoutingProvider::for_scenario(RoutingScenario::ChildCall);
    let model = Model::new(provider, TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new())
        .max_agents(2)
        .build()
        .unwrap();
    let mut events = system.take_events().unwrap();
    let subagents: SubagentConfig<MemStore> = SubagentConfig::builder(model.clone())
        .agent_namespace(false)
        .build()
        .unwrap();
    let root = system
        .host_with_subagents(
            Lam::builder(model)
                .state_store(system.state_store())
                .build()
                .actor("/call-root"),
            subagents,
        )
        .await
        .unwrap();

    assert_eq!(root.call("sync call root").await.unwrap(), "SYNC_OK");

    let mut saw_child_run = false;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut outcome = None;
        loop {
            match events.next().await.unwrap() {
                AgentSystemEvent::Run { address, .. } if address.as_str() == "/call-root/sync" => {
                    saw_child_run = true;
                }
                AgentSystemEvent::Outcome { outcome: observed } => outcome = Some(observed),
                _ => {}
            }
            if saw_child_run && let Some(outcome) = outcome.take() {
                break outcome;
            }
        }
    })
    .await
    .expect("the child outcome should be observable");
    assert!(saw_child_run);
    assert!(matches!(
        outcome,
        AgentOutcome::Completed {
            address,
            ref output,
            ..
        } if address.as_str() == "/call-root/sync" && output == "sync child complete"
    ));

    let page = system
        .state_store()
        .read(
            root.actor_id(),
            Revision::ZERO,
            NonZeroUsize::new(128).unwrap(),
        )
        .await
        .unwrap();
    assert!(!page.events.iter().any(|stored| {
        let ActorEventData::MessageAdmitted { message } = stored.event.data() else {
            return false;
        };
        matches!(
            message.source(),
            MessageSource::Actor { actor_id } if actor_id.as_str() == "/call-root/sync"
        )
    }));

    system
        .stop(&ActorAddress::new("/call-root/sync").unwrap())
        .await
        .unwrap();
    system.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn background_outcome_steers_the_parent_and_system_waits_for_quiescence() {
    let provider = RoutingProvider::for_scenario(RoutingScenario::BackgroundOutcome);
    let model = Model::new(provider.clone(), TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new())
        .max_agents(2)
        .build()
        .unwrap();
    let subagents: SubagentConfig<MemStore> = SubagentConfig::builder(model.clone())
        .agent_namespace(false)
        .build()
        .unwrap();
    let root = system
        .host_with_subagents(
            Lam::builder(model)
                .state_store(system.state_store())
                .build()
                .actor("/background-root"),
            subagents,
        )
        .await
        .unwrap();

    let output = root.call("background outcome root").await.unwrap();
    assert!(output == "SPAWN_RETURNED" || output == "BACKGROUND_OK");
    tokio::time::timeout(std::time::Duration::from_secs(2), system.wait())
        .await
        .expect("background work should reach quiescence")
        .unwrap();
    assert!(provider.requests().iter().any(|request| {
        has_message(&request.value, "background child complete", Some("actor"))
    }));

    system.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn spawn_wait_returns_with_the_durable_outcome_in_the_same_model_turn() {
    let provider = RoutingProvider::for_scenario(RoutingScenario::SpawnWait);
    let model = Model::new(provider.clone(), TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new())
        .max_agents(2)
        .build()
        .unwrap();
    let subagents: SubagentConfig<MemStore> = SubagentConfig::builder(model.clone())
        .agent_namespace(false)
        .build()
        .unwrap();
    let root = system
        .host_with_subagents(
            Lam::builder(model)
                .state_store(system.state_store())
                .build()
                .actor("/wait-root"),
            subagents,
        )
        .await
        .unwrap();

    assert_eq!(root.call("spawn wait root").await.unwrap(), "WAIT_OK");

    let requests = provider.requests();
    let continuation = requests
        .iter()
        .find(|request| {
            transition_payload_contains(&request.value, "eval", "inboxMessageId")
                && has_message(&request.value, "waited child complete", Some("actor"))
        })
        .expect("one continuation must contain both the wait result and child outcome");
    let context = continuation.value["context"]
        .as_array()
        .expect("test codec context should be an array");
    let wait_result = context
        .iter()
        .position(|entry| transition_payload_contains(entry, "eval", "inboxMessageId"))
        .expect("wait result should be model-visible");
    let outcome = context
        .iter()
        .position(|entry| has_message(entry, "waited child complete", Some("actor")))
        .expect("durable child outcome should be model-visible");
    assert!(
        wait_result < outcome,
        "the wait result must precede the synchronously drained inbox outcome"
    );
    assert!(
        requests.iter().any(|request| {
            request.value["systemPrompt"]
                .as_str()
                .is_some_and(|prompt| {
                    prompt.contains("lam.agents.wait")
                        && prompt.contains("durably admitted")
                        && prompt.contains("does not message, interrupt, stop")
                })
        }),
        "generated API documentation should explain the wait contract"
    );

    system.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn direct_child_stop_cancels_work_and_releases_the_subtree_capacity() {
    let provider = RoutingProvider::for_scenario(RoutingScenario::DirectStop);
    let model = Model::new(provider, TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new())
        .max_agents(2)
        .build()
        .unwrap();
    let subagents: SubagentConfig<MemStore> = SubagentConfig::builder(model.clone())
        .agent_namespace(false)
        .build()
        .unwrap();
    let root = system
        .host_with_subagents(
            Lam::builder(model.clone())
                .state_store(system.state_store())
                .build()
                .actor("/stop-root"),
            subagents,
        )
        .await
        .unwrap();

    assert_eq!(root.call("stop child root").await.unwrap(), "STOP_OK");
    let replacement = system
        .host(
            Lam::builder(model)
                .state_store(system.state_store())
                .build()
                .actor("/replacement"),
        )
        .await
        .expect("stopping the child should release its residency slot");
    assert_eq!(replacement.address().as_str(), "/replacement");
    system.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_child_call_retires_the_owned_subtree() {
    let provider = RoutingProvider::for_scenario(RoutingScenario::CancelCall);
    let model = Model::new(provider, TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new())
        .max_agents(2)
        .build()
        .unwrap();
    let mut events = system.take_events().unwrap();
    let subagents: SubagentConfig<MemStore> = SubagentConfig::builder(model.clone())
        .agent_namespace(false)
        .build()
        .unwrap();
    let root = system
        .host_with_subagents(
            Lam::builder(model.clone())
                .state_store(system.state_store())
                .default_eval_timeout(std::time::Duration::from_millis(50))
                .build()
                .actor("/cancel-call-root"),
            subagents,
        )
        .await
        .unwrap();

    assert_eq!(
        root.call("cancel child call root").await.unwrap(),
        "CALL_CANCELLED"
    );
    let reason = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let AgentSystemEvent::Retired { address, reason } = events.next().await.unwrap()
                && address.as_str() == "/cancel-call-root/cancelled-call"
            {
                break reason;
            }
        }
    })
    .await
    .expect("call cancellation should retire its child");
    assert_eq!(reason, StopReason::Cancelled);
    tokio::time::timeout(std::time::Duration::from_secs(2), system.wait())
        .await
        .unwrap()
        .unwrap();

    let replacement = system
        .host(
            Lam::builder(model)
                .state_store(system.state_store())
                .build()
                .actor("/replacement"),
        )
        .await
        .expect("cancelled call should release child capacity");
    assert_eq!(replacement.address().as_str(), "/replacement");
    system.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_mailbox_wakes_are_visible_and_wait_reaches_quiescence() {
    let provider = RoutingProvider::new();
    let model = Model::new(provider, TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new()).build().unwrap();
    let mut events = system.take_events().unwrap();
    let root = system
        .host(
            Lam::builder(model)
                .state_store(system.state_store())
                .build()
                .actor("/wake-root"),
        )
        .await
        .unwrap();

    root.send("ordinary wake", lam::DeliveryMode::Steer)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), system.wait())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if matches!(
                events.next().await,
                Some(AgentSystemEvent::Run { address, .. })
                    if address.as_str() == "/wake-root"
            ) {
                break;
            }
        }
    })
    .await
    .expect("ordinary wakes should emit addressed run progress");
    system.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn agents_cannot_stop_actors_outside_their_direct_children() {
    let provider = RoutingProvider::for_scenario(RoutingScenario::UnauthorizedStop);
    let model = Model::new(provider, TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new()).build().unwrap();
    let subagents: SubagentConfig<MemStore> = SubagentConfig::builder(model.clone())
        .agent_namespace(false)
        .build()
        .unwrap();
    let root = system
        .host_with_subagents(
            Lam::builder(model)
                .state_store(system.state_store())
                .build()
                .actor("/stop-policy-root"),
            subagents,
        )
        .await
        .unwrap();

    assert_eq!(
        root.call("reject stop root").await.unwrap(),
        "STOP_REJECTED"
    );
    system.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn child_message_steers_an_active_parent_on_the_same_worker() {
    let provider = RoutingProvider::new();
    let model = Model::new(provider.clone(), TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let gate = RoundTripGate::new();
    let system = AgentSystem::builder(MemStore::new())
        .worker_threads(1)
        .max_agents(2)
        .build()
        .unwrap();
    let subagents: SubagentConfig<MemStore> = SubagentConfig::builder(model.clone())
        .namespace(gate.signal_namespace())
        .max_depth(1)
        .build()
        .unwrap();
    let root = system
        .host_with_subagents(
            Lam::builder(model)
                .state_store(system.state_store())
                .namespace(gate.wait_namespace())
                .build()
                .actor("/roundtrip-root"),
            subagents,
        )
        .await
        .unwrap();

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        root.call("roundtrip root task"),
    )
    .await
    .expect("the parent and child should not deadlock on one worker")
    .unwrap();
    assert_eq!(output, "LAM_CHILD_OK");

    let store = system.state_store();
    let page = store
        .read(
            root.actor_id(),
            Revision::ZERO,
            NonZeroUsize::new(128).unwrap(),
        )
        .await
        .unwrap();
    let child_sender = page.events.iter().find_map(|stored| {
        let ActorEventData::MessageAdmitted { message } = stored.event.data() else {
            return None;
        };
        match message.source() {
            MessageSource::Actor { actor_id } => Some(actor_id.clone()),
            MessageSource::User { .. } | MessageSource::Host { .. } => None,
        }
    });
    assert!(child_sender.is_some(), "the child message must be durable");
    assert_eq!(
        child_sender.as_ref().map(ActorId::as_str),
        Some("/roundtrip-root/child")
    );

    let requests = provider.requests();
    let child_request = requests
        .iter()
        .find(|request| has_message(&request.value, "roundtrip child task", Some("actor")))
        .expect("the child should receive its task");
    let child_prompt = child_request.value["systemPrompt"].as_str().unwrap();
    assert!(child_prompt.contains("lam.agents.send"));
    assert!(!child_prompt.contains("lam.agents.spawn"));
    assert!(child_prompt.contains("Agent identity: /roundtrip-root/child"));
    assert!(child_prompt.contains("Parent agent: /roundtrip-root"));
    assert!(
        requests
            .iter()
            .any(|request| { request.value.to_string().contains("/roundtrip-root/child") })
    );
    assert!(requests.iter().any(|request| transition_payload_contains(
        &request.value,
        "eval",
        "LIST_OK"
    )));

    system.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_child_names_fail_with_the_canonical_address() {
    let provider = RoutingProvider::new();
    let model = Model::new(provider.clone(), TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new())
        .max_agents(3)
        .build()
        .unwrap();
    let subagents: SubagentConfig<MemStore> = SubagentConfig::builder(model.clone())
        .agent_namespace(false)
        .build()
        .unwrap();
    let root = system
        .host_with_subagents(
            Lam::builder(model)
                .state_store(system.state_store())
                .build()
                .actor("/root"),
            subagents,
        )
        .await
        .unwrap();

    assert_eq!(
        root.call("duplicate child name task").await.unwrap(),
        "duplicate observed"
    );
    assert!(provider.requests().iter().any(|request| {
        transition_payload_contains(&request.value, "eval", "addressInUse")
            && transition_payload_contains(&request.value, "eval", "/root/same")
    }));

    system.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn create_only_start_rejects_a_durable_identity_after_shutdown() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let model = Model::new(RoutingProvider::new(), TestCodec)
                .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
            let store = Arc::new(MemStore::new());
            let (actor, task) = Lam::builder(model.clone())
                .state_store(Arc::clone(&store))
                .build()
                .actor("/root/once")
                .build_new_task()
                .await
                .unwrap();
            let runner = tokio::task::spawn_local(task);
            actor.shutdown().await.unwrap();
            runner.await.unwrap();

            let result = Lam::builder(model)
                .state_store(store)
                .build()
                .actor("/root/once")
                .build_new_task()
                .await;
            assert!(matches!(
                result,
                Err(lam::ActorBuildError::ActorAlreadyExists { actor_id })
                    if actor_id.as_str() == "/root/once"
            ));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_spawn_retires_the_child_before_releasing_capacity() {
    let provider = RoutingProvider::new();
    let model = Model::new(provider, TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let store = AdmissionGateStore::new("/root/cancelled");
    let system = AgentSystem::builder(store.clone())
        .max_agents(2)
        .build()
        .unwrap();
    let subagents: SubagentConfig<AdmissionGateStore> = SubagentConfig::builder(model.clone())
        .agent_namespace(false)
        .build()
        .unwrap();
    let root = system
        .host_with_subagents(
            Lam::builder(model.clone())
                .state_store(system.state_store())
                .default_eval_timeout(std::time::Duration::from_millis(50))
                .build()
                .actor("/root"),
            subagents,
        )
        .await
        .unwrap();
    let caller = root.clone();
    let call = tokio::spawn(async move { caller.call("cancel spawn task").await });

    if tokio::time::timeout(
        std::time::Duration::from_secs(2),
        store.wait_until_blocked(),
    )
    .await
    .is_err()
    {
        store.release();
        system.abort().await.unwrap();
        panic!("spawn never reached initial task admission");
    }
    let output = match tokio::time::timeout(std::time::Duration::from_secs(2), call).await {
        Ok(result) => result.unwrap().unwrap(),
        Err(_) => {
            store.release();
            system.abort().await.unwrap();
            panic!("the eval timeout did not cancel spawn admission");
        }
    };
    assert_eq!(output, "spawn cancelled");

    let replacement = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match system
                .host(
                    Lam::builder(model.clone())
                        .state_store(system.state_store())
                        .build()
                        .actor("/replacement"),
                )
                .await
            {
                Ok(actor) => break actor,
                Err(AgentSystemError::Capacity { .. }) => tokio::task::yield_now().await,
                Err(error) => panic!("replacement actor failed: {error}"),
            }
        }
    })
    .await
    .expect("cancelled child must release capacity after its runner exits");
    assert_eq!(replacement.address().as_str(), "/replacement");
    system.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn residency_bound_rejects_a_second_live_actor_before_building_it() {
    let provider = RoutingProvider::new();
    let model = Model::new(provider, TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new())
        .max_agents(1)
        .build()
        .unwrap();
    let first = system
        .host(
            Lam::builder(model.clone())
                .state_store(system.state_store())
                .build()
                .actor("/first"),
        )
        .await
        .unwrap();
    let second = system
        .host(
            Lam::builder(model)
                .state_store(system.state_store())
                .build()
                .actor("/second"),
        )
        .await;
    assert!(matches!(
        second,
        Err(AgentSystemError::Capacity { max_agents: 1 })
    ));
    drop(first);
    system.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn agent_system_rejects_noncanonical_host_addresses() {
    let provider = RoutingProvider::new();
    let model = Model::new(provider, TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new()).build().unwrap();
    let result = system
        .host(
            Lam::builder(model)
                .state_store(system.state_store())
                .build()
                .actor("root"),
        )
        .await;
    assert!(matches!(
        result,
        Err(AgentSystemError::InvalidAddress { address, .. }) if address == "root"
    ));
    system.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn conflicting_agent_operations_do_not_wait_for_active_calls() {
    let provider = RoutingProvider::new();
    let model = Model::new(provider.clone(), TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new()).build().unwrap();
    let root = system
        .host(
            Lam::builder(model)
                .state_store(system.state_store())
                .build()
                .actor("/blocked"),
        )
        .await
        .unwrap();
    let caller = root.clone();
    let call = tokio::spawn(async move { caller.call("blocking task").await });
    provider.wait_for_request("blocking task").await;

    let error = tokio::time::timeout(std::time::Duration::from_secs(2), root.compact())
        .await
        .expect("a conflicting operation should not wait for inference")
        .expect_err("compaction should conflict with the active call");
    assert!(matches!(
        error,
        AgentSystemError::Actor(lam::ActorError::Busy)
    ));

    root.abort_handle().abort();
    assert!(matches!(
        call.await.unwrap(),
        Err(AgentSystemError::Actor(lam::ActorError::Aborted))
    ));
    system.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn abort_retires_a_resident_with_an_active_owned_run() {
    let provider = RoutingProvider::new();
    let model = Model::new(provider.clone(), TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new())
        .max_agents(1)
        .build()
        .unwrap();
    let root = system
        .host(
            Lam::builder(model.clone())
                .state_store(system.state_store())
                .build()
                .actor("/blocked"),
        )
        .await
        .unwrap();
    let caller = root.clone();
    let call = tokio::spawn(async move { caller.call("blocking task").await });
    provider.wait_for_request("blocking task").await;

    root.abort_handle().abort();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), call)
        .await
        .expect("abort should not wait for the active run")
        .unwrap();
    assert!(matches!(
        result,
        Err(AgentSystemError::Actor(lam::ActorError::Aborted))
    ));

    let replacement = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match system
                .host(
                    Lam::builder(model.clone())
                        .state_store(system.state_store())
                        .build()
                        .actor("/replacement"),
                )
                .await
            {
                Ok(actor) => break actor,
                Err(AgentSystemError::Capacity { .. }) => tokio::task::yield_now().await,
                Err(error) => panic!("replacement actor failed: {error}"),
            }
        }
    })
    .await
    .expect("the stopped resident should release its capacity slot");
    assert_eq!(
        replacement.call("plain replacement").await.unwrap(),
        "plain complete"
    );
    system.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn abort_escalates_an_in_progress_graceful_shutdown() {
    let provider = RoutingProvider::new();
    let model = Model::new(provider.clone(), TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new()).build().unwrap();
    let root = system
        .host(
            Lam::builder(model.clone())
                .state_store(system.state_store())
                .build()
                .actor("/blocked"),
        )
        .await
        .unwrap();
    let caller = root.clone();
    let call = tokio::spawn(async move { caller.call("blocking task").await });
    provider.wait_for_request("blocking task").await;

    let graceful_system = system.clone();
    let graceful = tokio::spawn(async move { graceful_system.shutdown().await });
    loop {
        match system
            .host(
                Lam::builder(model.clone())
                    .state_store(system.state_store())
                    .build()
                    .actor("/blocked"),
            )
            .await
        {
            Err(AgentSystemError::AddressInUse { .. }) => tokio::task::yield_now().await,
            Err(AgentSystemError::ShuttingDown) => break,
            Ok(_) => panic!("duplicate actor address was accepted"),
            Err(error) => panic!("unexpected shutdown probe error: {error}"),
        }
    }

    tokio::time::timeout(std::time::Duration::from_secs(2), system.abort())
        .await
        .expect("abort should interrupt the call blocking graceful shutdown")
        .unwrap();
    assert!(matches!(
        call.await.unwrap(),
        Err(AgentSystemError::Actor(lam::ActorError::Aborted))
    ));
    graceful.await.unwrap().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn roots_are_assigned_round_robin_across_configured_workers() {
    let provider = RoutingProvider::new();
    let model = Model::new(provider.clone(), TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let system = AgentSystem::builder(MemStore::new())
        .worker_threads(2)
        .max_agents(2)
        .build()
        .unwrap();
    let first = system
        .host(
            Lam::builder(model.clone())
                .state_store(system.state_store())
                .build()
                .actor("/first"),
        )
        .await
        .unwrap();
    let second = system
        .host(
            Lam::builder(model)
                .state_store(system.state_store())
                .build()
                .actor("/second"),
        )
        .await
        .unwrap();
    let (first_output, second_output) =
        tokio::join!(first.call("plain task one"), second.call("plain task two"));
    assert_eq!(first_output.unwrap(), "plain complete");
    assert_eq!(second_output.unwrap(), "plain complete");
    assert_eq!(
        provider.thread_names().into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "lam-agent-worker-0".to_owned(),
            "lam-agent-worker-1".to_owned(),
        ])
    );
    system.shutdown().await.unwrap();
}

#[test]
fn explicit_subagent_policy_rejects_ambiguous_registrations() {
    let provider = RoutingProvider::new();
    let model = Model::new(provider, TestCodec)
        .with_descriptor(ModelDescriptor::new("test", "model-a", "test/messages").unwrap());
    let duplicate_model: Result<SubagentConfig<MemStore>, _> =
        SubagentConfig::builder(model.clone())
            .model(model.clone())
            .build();
    assert!(matches!(
        duplicate_model,
        Err(SubagentConfigError::DuplicateModel { .. })
    ));

    let duplicate_namespace: Result<SubagentConfig<MemStore>, _> =
        SubagentConfig::builder(model.clone())
            .namespace(lam::Namespace::new("acme.read", "read"))
            .namespace(lam::Namespace::new("acme.read", "duplicate"))
            .build();
    assert!(matches!(
        duplicate_namespace,
        Err(SubagentConfigError::DuplicateNamespace { .. })
    ));

    let reserved_namespace: Result<SubagentConfig<MemStore>, _> = SubagentConfig::builder(model)
        .namespace(lam::Namespace::new("lam.agents", "collision"))
        .build();
    assert!(matches!(
        reserved_namespace,
        Err(SubagentConfigError::ReservedNamespace { .. })
    ));
}

fn native(value: Value) -> EncodedPayload {
    EncodedPayload::new(
        CodecRef::new(CodecId::new("test/native").unwrap(), 1),
        value,
    )
}

fn find_string_field(value: &Value, field: &str) -> Option<String> {
    match value {
        Value::Object(object) => object
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                object
                    .values()
                    .find_map(|value| find_string_field(value, field))
            }),
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_string_field(value, field)),
        _ => None,
    }
}

fn has_message(value: &Value, text: &str, source_kind: Option<&str>) -> bool {
    match value {
        Value::Object(object) => {
            let matching_source = object
                .get("source")
                .and_then(Value::as_object)
                .and_then(|source| source.get("kind"))
                .and_then(Value::as_str)
                .is_some_and(|kind| source_kind.is_none_or(|expected| kind == expected));
            let matching_payload = object
                .get("payload")
                .is_some_and(|payload| payload.to_string().contains(text));
            (matching_source && matching_payload)
                || object
                    .values()
                    .any(|value| has_message(value, text, source_kind))
        }
        Value::Array(values) => values
            .iter()
            .any(|value| has_message(value, text, source_kind)),
        _ => false,
    }
}

fn has_transition(value: &Value, kind: &str) -> bool {
    match value {
        Value::Object(object) => {
            object
                .get("transition")
                .and_then(Value::as_object)
                .and_then(|transition| transition.get("kind"))
                .and_then(Value::as_str)
                .is_some_and(|found| found == kind)
                || object.values().any(|value| has_transition(value, kind))
        }
        Value::Array(values) => values.iter().any(|value| has_transition(value, kind)),
        _ => false,
    }
}

fn transition_payload_contains(value: &Value, kind: &str, text: &str) -> bool {
    match value {
        Value::Object(object) => {
            let matches = object
                .get("transition")
                .and_then(Value::as_object)
                .and_then(|transition| transition.get("kind"))
                .and_then(Value::as_str)
                .is_some_and(|found| found == kind)
                && object
                    .get("payload")
                    .is_some_and(|payload| payload.to_string().contains(text));
            matches
                || object
                    .values()
                    .any(|value| transition_payload_contains(value, kind, text))
        }
        Value::Array(values) => values
            .iter()
            .any(|value| transition_payload_contains(value, kind, text)),
        _ => false,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
