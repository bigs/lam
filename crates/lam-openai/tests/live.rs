//! Explicitly ignored, credentialed smoke tests for live provider behavior.

use std::fs;
use std::time::Duration;

use lam::{
    Actor, Lam, MemStore, Model, ModelCodec, ModelProvider, ModelResponseMetadata, Namespace,
    RunEvent,
};
use lam_openai::ModelPricing;
use lam_openai::chat_completions::ChatCompletions;
use lam_openai::responses::Responses;
use serde_json::{Value, json};

const FIREWORKS_MODEL: &str = "accounts/fireworks/models/deepseek-v4-flash-0731";
const FIREWORKS_BASE_URL: &str = "https://api.fireworks.ai/inference/v1";
const OPENAI_MODEL: &str = "gpt-5-mini";

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires OPENAI_API_KEY and makes bounded live API calls"]
async fn openai_responses_smoke() {
    let model = Responses::builder(OPENAI_MODEL)
        .api_key(required_env("OPENAI_API_KEY"))
        .pricing(ModelPricing::new(0.25, 2.0).cached_input(0.025))
        .extra_body(json!({
            "max_output_tokens": 512,
            "reasoning": { "effort": "low" },
            "instructions": "You are a Lam coding agent. The eval function shown in your tools is available and executes TypeScript in your persistent isolate. Use it when the user asks you to inspect or compute with a registered Lam namespace."
        }))
        .build()
        .expect("valid OpenAI model configuration");
    smoke(model, "openai-live").await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires FIREWORKS_API_KEY and makes bounded live API calls"]
async fn fireworks_chat_completions_smoke() {
    let model = ChatCompletions::builder(FIREWORKS_MODEL)
        .api_key(required_env("FIREWORKS_API_KEY"))
        .base_url(FIREWORKS_BASE_URL)
        .pricing(ModelPricing::new(0.14, 0.28).cached_input(0.028))
        .extra_body(json!({
            "max_tokens": 512,
            "reasoning_effort": "low",
            "reasoning_history": "preserved",
            "perf_metrics_in_response": true
        }))
        .build()
        .expect("valid Fireworks model configuration");
    smoke(model, "fireworks-live").await;
}

async fn smoke<P, C>(model: Model<P, C>, actor_id: &str)
where
    P: ModelProvider,
    C: ModelCodec,
{
    let fixture = tempfile::tempdir().expect("create bounded fixture directory");
    for name in ["alpha.txt", "beta.txt", "gamma.txt"] {
        fs::write(fixture.path().join(name), name).expect("write fixture file");
    }
    let root = fixture.path().to_path_buf();
    let filesystem = Namespace::new(
        "smoke.fs",
        "Read-only access to the bounded live-test fixture directory.",
    )
    .function(
        "list",
        "Lists file names in the fixed fixture directory. Pass an empty object.",
        move |_context, _input: Value| {
            let root = root.clone();
            async move {
                let entries = fs::read_dir(root).map_err(|error| error.to_string())?;
                let mut names = entries
                    .map(|entry| {
                        entry.map_err(|error| error.to_string()).and_then(|entry| {
                            entry
                                .file_name()
                                .into_string()
                                .map_err(|name| format!("non-UTF-8 fixture file name: {name:?}"))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                names.sort();
                Ok::<_, String>(names)
            }
        },
    );
    let mut actor = Lam::builder(model)
        .namespace(filesystem)
        .default_eval_timeout(Duration::from_secs(10))
        .max_eval_timeout(Duration::from_secs(15))
        .build()
        .actor(actor_id)
        .build()
        .await
        .expect("live actor starts");

    let hello = timed_call(
        &mut actor,
        "This is a bounded smoke test. Reply with exactly: Hello, world! Do not call eval.",
    )
    .await;
    assert!(hello.output.to_lowercase().contains("hello, world"));
    assert_eq!(hello.eval_count, 0, "hello test unexpectedly used eval");
    assert_metered("hello", &hello.metadata);

    let directory = timed_call(
        &mut actor,
        "Use eval exactly once with source exactly `await smoke.fs.list({})` and timeoutMs null. Count the returned file names, then report the count and all names concisely.",
    )
    .await;
    assert_eq!(directory.eval_count, 1, "directory test eval count");
    assert!(directory.output.contains('3'));
    for name in ["alpha.txt", "beta.txt", "gamma.txt"] {
        assert!(directory.output.contains(name), "missing {name} in output");
    }
    assert_metered("directory", &directory.metadata);

    actor.shutdown().await.expect("live actor shuts down");
}

struct Observation {
    output: String,
    metadata: Vec<ModelResponseMetadata>,
    eval_count: usize,
}

async fn timed_call(actor: &mut Actor<MemStore>, prompt: &str) -> Observation {
    tokio::time::timeout(Duration::from_secs(120), observe_call(actor, prompt))
        .await
        .expect("live call exceeded 120 seconds")
}

async fn observe_call(actor: &mut Actor<MemStore>, prompt: &str) -> Observation {
    let mut run = actor.call(prompt);
    let mut metadata = Vec::new();
    let mut eval_count = 0;
    while let Some(event) = run.next().await {
        match event {
            RunEvent::ModelCompleted {
                metadata: completed,
                ..
            } => {
                println!(
                    "model.completed {}",
                    serde_json::to_string(&completed).expect("metadata is serializable")
                );
                metadata.push(completed);
            }
            RunEvent::EvalCompleted { outcome, .. } => {
                eval_count += 1;
                println!(
                    "eval.completed {}",
                    serde_json::to_string(&outcome).expect("eval outcome is serializable")
                );
            }
            _ => {}
        }
    }
    let output = run.await.expect("live run completes");
    println!("run.output {output}");
    Observation {
        output,
        metadata,
        eval_count,
    }
}

fn assert_metered(label: &str, metadata: &[ModelResponseMetadata]) {
    assert!(!metadata.is_empty(), "{label} made no model requests");
    assert!(
        metadata.iter().all(|metadata| metadata.usage.is_some()),
        "{label} response omitted token usage"
    );
    assert!(
        metadata.iter().all(|metadata| metadata.cost.is_some()),
        "{label} response omitted the configured cost estimate"
    );
    let total_tokens = metadata
        .iter()
        .filter_map(|metadata| metadata.usage.as_ref())
        .map(|usage| usage.total_tokens)
        .sum::<u64>();
    let cost_usd = metadata
        .iter()
        .filter_map(|metadata| metadata.cost.as_ref())
        .map(|cost| cost.amount_usd)
        .sum::<f64>();
    println!(
        "run.usage label={label} total_tokens={total_tokens} estimated_cost_usd={cost_usd:.8}"
    );
}

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}
