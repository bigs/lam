//! Explicitly ignored, credentialed smoke tests for live provider behavior.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lam::{
    Actor, ActorState, Lam, MemStore, Model, ModelCodec, ModelProvider, ModelResponseMetadata,
    Namespace, RunEvent,
};
use lam_openai::ModelPricing;
use lam_openai::chat_completions::ChatCompletions;
use lam_openai::responses::Responses;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const FIREWORKS_MODEL: &str = "accounts/fireworks/models/deepseek-v4-flash-0731";
const FIREWORKS_BASE_URL: &str = "https://api.fireworks.ai/inference/v1";
const OPENAI_MODEL: &str = "gpt-5-mini";
const MAX_MODEL_REQUESTS: usize = 6;

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires OPENAI_API_KEY and makes bounded live API calls"]
async fn openai_responses_smoke() {
    let model = Responses::builder(OPENAI_MODEL)
        .api_key(required_env("OPENAI_API_KEY"))
        .pricing(ModelPricing::new(0.25, 2.0).cached_input(0.025))
        .extra_body(json!({
            "max_output_tokens": 512,
            "reasoning": { "effort": "low" }
        }))
        .build()
        .expect("valid OpenAI model configuration");
    fixture_smoke(model, "openai-live").await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires FIREWORKS_API_KEY and makes bounded live API calls"]
async fn fireworks_chat_completions_smoke() {
    let model = ChatCompletions::builder(FIREWORKS_MODEL)
        .api_key(required_env("FIREWORKS_API_KEY"))
        .base_url(FIREWORKS_BASE_URL)
        .pricing(ModelPricing::new(0.14, 0.28).cached_input(0.028))
        .extra_body(json!({
            "max_tokens": 2048,
            "reasoning_effort": "low",
            "reasoning_history": "preserved",
            "perf_metrics_in_response": true
        }))
        .build()
        .expect("valid Fireworks model configuration");
    project_navigation_smoke(model).await;
}

async fn fixture_smoke<P, C>(model: Model<P, C>, actor_id: &str)
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

#[derive(Deserialize, JsonSchema)]
struct ListDirectoryInput {
    #[serde(default)]
    path: String,
}

#[derive(JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectEntry {
    path: String,
    kind: String,
    size_bytes: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LargestFile {
    path: String,
    size_bytes: u64,
}

async fn project_navigation_smoke<P, C>(model: Model<P, C>)
where
    P: ModelProvider,
    C: ModelCodec,
{
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root exists");
    let expected = find_largest_file(&root)
        .expect("project view is readable")
        .expect("project contains a regular file");
    let namespace_root = root.clone();
    let filesystem = Namespace::new(
        "project.fs",
        "Read-only metadata access to the local project used by the live test.",
    )
    .function(
        "list",
        "Lists one project directory read-only. Use an empty path for the root; entries contain relative paths, kinds, and byte sizes for regular files. Root .git and target directories are omitted, and symlinks are never followed.",
        move |_context, input: ListDirectoryInput| {
            let root = namespace_root.clone();
            async move { list_project_directory(&root, &input.path) }
        },
    );
    let mut actor = Lam::builder(model)
        .namespace(filesystem)
        .default_eval_timeout(Duration::from_secs(30))
        .max_eval_timeout(Duration::from_secs(45))
        .build()
        .actor("fireworks-project-navigation")
        .build()
        .await
        .expect("live actor starts");

    let observation = timed_call(
        &mut actor,
        "Recursively navigate the local project using the registered read-only APIs and identify the largest regular file in the exposed tree. Report its relative path and exact size in bytes. Do not guess.",
    )
    .await;
    let state = actor
        .actor_ref()
        .state()
        .await
        .expect("live context projects");
    let capture_path = capture_run(&root, &state, &observation, &expected);
    actor.shutdown().await.expect("live actor shuts down");

    println!("run.capture {}", capture_path.display());
    assert!(observation.eval_count > 0, "project task never used eval");
    assert!(
        observation.output.contains(&expected.path),
        "output did not name expected largest file {}: {}",
        expected.path,
        observation.output
    );
    let normalized_output = observation.output.replace([',', '_'], "");
    assert!(
        normalized_output.contains(&expected.size_bytes.to_string()),
        "output did not include expected byte size {}: {}",
        expected.size_bytes,
        observation.output
    );
    assert_metered("project-navigation", &observation.metadata);
}

fn list_project_directory(root: &Path, requested: &str) -> Result<Vec<ProjectEntry>, String> {
    let directory = resolve_project_directory(root, requested)?;
    let mut paths = fs::read_dir(&directory)
        .map_err(|error| {
            format!(
                "cannot list {}: {error}",
                display_relative(root, &directory)
            )
        })?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();

    let mut entries = Vec::new();
    for path in paths {
        if directory == root && is_excluded_root_entry(&path) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        let file_type = metadata.file_type();
        let (kind, size_bytes) = if file_type.is_file() {
            ("file", Some(metadata.len()))
        } else if file_type.is_dir() {
            ("directory", None)
        } else if file_type.is_symlink() {
            ("symlink", None)
        } else {
            ("other", None)
        };
        entries.push(ProjectEntry {
            path: display_relative(root, &path),
            kind: kind.to_owned(),
            size_bytes,
        });
    }
    Ok(entries)
}

fn resolve_project_directory(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let mut directory = root.to_path_buf();
    for component in Path::new(requested).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(segment) => {
                if directory == root && matches!(segment.to_str(), Some(".git" | "target")) {
                    return Err("that root directory is outside the live-test view".to_owned());
                }
                directory.push(segment);
                let metadata = fs::symlink_metadata(&directory)
                    .map_err(|error| format!("cannot inspect {requested:?}: {error}"))?;
                if metadata.file_type().is_symlink() {
                    return Err("symlinks are not followed".to_owned());
                }
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("path must be relative and cannot contain parent traversal".to_owned());
            }
        }
    }
    if !directory.is_dir() {
        return Err(format!("{} is not a directory", requested));
    }
    Ok(directory)
}

fn find_largest_file(root: &Path) -> Result<Option<LargestFile>, String> {
    fn visit(
        root: &Path,
        directory: &str,
        largest: &mut Option<LargestFile>,
    ) -> Result<(), String> {
        for entry in list_project_directory(root, directory)? {
            if entry.kind == "directory" {
                visit(root, &entry.path, largest)?;
            } else if entry.kind == "file" {
                let candidate = LargestFile {
                    path: entry.path,
                    size_bytes: entry.size_bytes.unwrap_or(0),
                };
                if largest.as_ref().is_none_or(|current| {
                    candidate.size_bytes > current.size_bytes
                        || (candidate.size_bytes == current.size_bytes
                            && candidate.path < current.path)
                }) {
                    *largest = Some(candidate);
                }
            }
        }
        Ok(())
    }

    let mut largest = None;
    visit(root, "", &mut largest)?;
    Ok(largest)
}

fn is_excluded_root_entry(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(".git" | "target")
    )
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

struct Observation {
    output: String,
    metadata: Vec<ModelResponseMetadata>,
    eval_count: usize,
    phase_events: Vec<String>,
}

async fn timed_call(actor: &mut Actor<MemStore>, prompt: &str) -> Observation {
    tokio::time::timeout(Duration::from_secs(180), observe_call(actor, prompt))
        .await
        .expect("live call exceeded 180 seconds")
}

async fn observe_call(actor: &mut Actor<MemStore>, prompt: &str) -> Observation {
    let abort = actor.abort_handle();
    let mut run = actor.call(prompt);
    let mut metadata = Vec::new();
    let mut eval_count = 0;
    let mut model_count = 0;
    let mut phase_events = Vec::new();
    while let Some(event) = run.next().await {
        match event {
            RunEvent::Started { ref run_id } => {
                phase_events.push(format!("run {run_id} started"));
            }
            RunEvent::MessagesDelivered {
                ref run_id,
                ref message_ids,
            } => {
                phase_events.push(format!(
                    "run {run_id} delivered messages {}",
                    message_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            RunEvent::ModelStarted { ref run_id } => {
                model_count += 1;
                phase_events.push(format!("run {run_id} started model request {model_count}"));
                if model_count > MAX_MODEL_REQUESTS {
                    abort.abort();
                }
            }
            RunEvent::ModelCompleted {
                ref run_id,
                metadata: completed,
            } => {
                println!(
                    "model.completed {}",
                    serde_json::to_string(&completed).expect("metadata is serializable")
                );
                phase_events.push(format!(
                    "run {run_id} completed model request {model_count}"
                ));
                metadata.push(completed);
            }
            RunEvent::EvalStarted { ref run_id } => {
                phase_events.push(format!("run {run_id} started eval {}", eval_count + 1));
            }
            RunEvent::EvalCompleted {
                ref run_id,
                ref outcome,
            } => {
                eval_count += 1;
                println!(
                    "eval.completed {}",
                    serde_json::to_string(outcome).expect("eval outcome is serializable")
                );
                phase_events.push(format!("run {run_id} completed eval {eval_count}"));
            }
            RunEvent::Completed { ref run_id } => {
                phase_events.push(format!("run {run_id} completed"));
            }
            RunEvent::Failed { ref message } => {
                phase_events.push(format!("run failed: {message}"));
            }
            RunEvent::ModelDelta { .. } => {}
        }
    }
    assert!(
        model_count <= MAX_MODEL_REQUESTS,
        "live run exceeded {MAX_MODEL_REQUESTS} model requests"
    );
    let output = run.await.expect("live run completes");
    println!("run.output {output}");
    Observation {
        output,
        metadata,
        eval_count,
        phase_events,
    }
}

fn capture_run(
    root: &Path,
    state: &ActorState,
    observation: &Observation,
    expected: &LargestFile,
) -> PathBuf {
    let context = state
        .context()
        .iter()
        .map(|projected| {
            json!({
                "sequence": projected.sequence.get(),
                "journalRevision": projected.revision.get(),
                "entry": &projected.entry,
            })
        })
        .collect::<Vec<_>>();
    let captured_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time follows Unix epoch")
        .as_millis();
    let capture = json!({
        "capturedAtUnixMs": captured_at,
        "actorId": "fireworks-project-navigation",
        "requestedModel": FIREWORKS_MODEL,
        "projectRoot": root,
        "scope": { "excludedRootEntries": [".git", "target"], "followsSymlinks": false },
        "expectedLargestFile": expected,
        "finalOutput": observation.output,
        "phaseEvents": observation.phase_events,
        "responseMetadata": observation.metadata,
        "journal": {
            "revision": state.revision().get(),
            "contextEntries": context,
        }
    });
    let directory = root.join("target/live-reports");
    fs::create_dir_all(&directory).expect("create live report directory");
    let path = directory.join("deepseek-console-result-run.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&capture).expect("capture is serializable"),
    )
    .expect("write live run capture");
    path
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
