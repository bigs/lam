//! Explicitly ignored, credentialed multi-agent smoke tests.

mod support;

use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lam::{
    ActorEventData, EncodedPayload, JournalStore, Lam, MemStore, MessageSource, Model,
    ModelDescriptor, ModelEventSink, ModelProvider, Revision,
};
use lam_agents::{AgentSystem, SubagentConfig};
use lam_openai::ModelPricing;
use lam_openai::chat_completions::ChatCompletions;
use serde_json::json;

use support::RoundTripGate;

const FIREWORKS_MODEL: &str = "accounts/fireworks/models/deepseek-v4-flash-0731";
const FIREWORKS_BASE_URL: &str = "https://api.fireworks.ai/inference/v1";
const MAX_MODEL_REQUESTS: usize = 8;
const MAX_OUTPUT_TOKENS: u64 = 4_096;

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires FIREWORKS_API_KEY and makes bounded live API calls"]
async fn fireworks_child_to_parent_round_trip() {
    let (provider, codec) = ChatCompletions::builder(FIREWORKS_MODEL)
        .api_key(required_env("FIREWORKS_API_KEY"))
        .base_url(FIREWORKS_BASE_URL)
        .pricing(ModelPricing::new(0.14, 0.28).cached_input(0.028))
        .extra_body(json!({
            "max_tokens": MAX_OUTPUT_TOKENS,
            "reasoning_effort": "low",
            "reasoning_history": "preserved",
            "perf_metrics_in_response": true
        }))
        .build_parts()
        .expect("valid Fireworks configuration");
    let provider = BoundedProvider::new(provider, MAX_MODEL_REQUESTS);
    let request_count = Arc::clone(&provider.requests);
    let model = Model::new(provider, codec).with_descriptor(
        ModelDescriptor::new(
            "openai-compatible",
            FIREWORKS_MODEL,
            "openai/chat-completions",
        )
        .unwrap(),
    );
    let gate = RoundTripGate::new();
    let system = AgentSystem::builder(MemStore::new())
        .worker_threads(1)
        .max_agents(2)
        .build()
        .unwrap();
    let children: SubagentConfig<MemStore> = SubagentConfig::builder(model.clone(), "low")
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
                .actor("/root"),
            children,
        )
        .await
        .expect("root actor starts");

    let task = r#"Create exactly one subagent named worker with lam.agents.spawn, explicitly selecting model { provider: "openai-compatible", model: "accounts/fireworks/models/deepseek-v4-flash-0731" } and effort "low". Give it this task: inspect lam.agents.identity, use lam.agents.send to send {"token":"LAM_CHILD_OK"} to its parent address, then call test.roundtrip.signal. After spawning it, call lam.agents.list with no arguments and verify /root/worker is present, then call test.roundtrip.wait yourself. Do not call signal from the parent. Only after the child message is visible, reply with exactly LAM_CHILD_OK."#;
    let output = match tokio::time::timeout(Duration::from_secs(120), root.call(task)).await {
        Ok(result) => result.expect("live parent call succeeds"),
        Err(_elapsed) => {
            system.abort().await.expect("timed-out system aborts");
            panic!("live parent call exceeded 120 seconds");
        }
    };

    let store = system.state_store();
    let page = store
        .read(
            root.actor_id(),
            Revision::ZERO,
            NonZeroUsize::new(256).unwrap(),
        )
        .await
        .expect("root journal is readable");
    let child_message = page.events.iter().find_map(|stored| {
        let ActorEventData::MessageAdmitted { message } = stored.event.data() else {
            return None;
        };
        match message.source() {
            MessageSource::Actor { actor_id }
                if message.payload().value.to_string().contains("LAM_CHILD_OK") =>
            {
                Some(actor_id.to_string())
            }
            MessageSource::User { .. }
            | MessageSource::Host { .. }
            | MessageSource::Actor { .. } => None,
        }
    });

    println!("live.roundtrip.output={output:?}");
    let requests = request_count.load(Ordering::Acquire);
    println!("live.roundtrip.requests={requests}");
    println!("live.roundtrip.child={child_message:?}");
    assert_eq!(output.trim(), "LAM_CHILD_OK");
    assert!(
        child_message.as_deref() == Some("/root/worker"),
        "no durable addressed child message was found: {child_message:?}"
    );
    assert!(requests <= MAX_MODEL_REQUESTS);
    system.shutdown().await.expect("live system shuts down");
}

struct BoundedProvider<P> {
    inner: P,
    requests: Arc<AtomicUsize>,
    limit: usize,
}

impl<P> BoundedProvider<P> {
    fn new(inner: P, limit: usize) -> Self {
        Self {
            inner,
            requests: Arc::new(AtomicUsize::new(0)),
            limit,
        }
    }
}

impl<P> ModelProvider for BoundedProvider<P>
where
    P: ModelProvider,
{
    type Error = BoundedProviderError<P::Error>;

    async fn invoke(
        &self,
        request: EncodedPayload,
        events: ModelEventSink,
    ) -> Result<EncodedPayload, Self::Error> {
        let request_number = self.requests.fetch_add(1, Ordering::AcqRel) + 1;
        if request_number > self.limit {
            return Err(BoundedProviderError::Limit { limit: self.limit });
        }
        self.inner
            .invoke(request, events)
            .await
            .map_err(BoundedProviderError::Provider)
    }

    fn is_context_overflow(&self, error: &Self::Error) -> bool {
        match error {
            BoundedProviderError::Provider(error) => self.inner.is_context_overflow(error),
            BoundedProviderError::Limit { .. } => false,
        }
    }
}

#[derive(Debug)]
enum BoundedProviderError<E> {
    Provider(E),
    Limit { limit: usize },
}

impl<E> fmt::Display for BoundedProviderError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provider(error) => error.fmt(formatter),
            Self::Limit { limit } => write!(formatter, "live request limit {limit} exceeded"),
        }
    }
}

impl<E> Error for BoundedProviderError<E> where E: Error + 'static {}

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}
