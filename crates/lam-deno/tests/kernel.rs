//! Acceptance tests for the first persistent-isolate kernel slice.

use std::future::poll_fn;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::task::Poll;
use std::time::{Duration, Instant};

use lam_deno::{
    ConsoleEntry, ConsoleLevel, DirectorySelection, DirectorySelectionSource, EvalError,
    EvalOptions, EvalValue, Isolate, IsolateBuildError, Namespace, Never,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize, JsonSchema)]
struct AddInput {
    left: f64,
    right: f64,
}

#[derive(Serialize, JsonSchema)]
struct AddOutput {
    sum: f64,
}

#[derive(Deserialize, JsonSchema)]
struct EchoInput {
    text: String,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct EchoOutput {
    text: String,
    isolate_generation: u64,
}

#[derive(Deserialize, JsonSchema)]
struct DivideInput {
    dividend: f64,
    divisor: f64,
}

#[derive(Serialize, JsonSchema)]
struct DivideOutput {
    quotient: f64,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
enum DivisionError {
    DivisionByZero { dividend: f64 },
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn math_namespace() -> Namespace {
    Namespace::new("lam.math", "Small arithmetic test kernel.")
        .function(
            "add",
            "Adds two numbers across an asynchronous Rust op boundary.",
            |_context, input: AddInput| async move {
                tokio::task::yield_now().await;
                Ok::<_, Never>(AddOutput {
                    sum: input.left + input.right,
                })
            },
        )
        .function(
            "divide",
            "Divides two numbers with a typed zero-divisor failure.",
            |_context, input: DivideInput| async move {
                if input.divisor == 0.0 {
                    Err(DivisionError::DivisionByZero {
                        dividend: input.dividend,
                    })
                } else {
                    Ok(DivideOutput {
                        quotient: input.dividend / input.divisor,
                    })
                }
            },
        )
}

fn application_namespace() -> Namespace {
    Namespace::new(
        "acme.catalog.text",
        "Application-defined functions outside the Lam namespace.",
    )
    .function(
        "echo",
        "Returns its input and the invoking isolate generation.\n\nThis second paragraph remains available through lam.dir.",
        |context, input: EchoInput| async move {
            Ok::<_, Never>(EchoOutput {
                text: input.text,
                isolate_generation: context.isolate_generation(),
            })
        },
    )
}

async fn test_isolate() -> Isolate {
    Isolate::builder()
        .namespace(math_namespace())
        .namespace(application_namespace())
        .build()
        .await
        .expect("test isolate should initialize")
}

fn json_result(value: EvalValue) -> Value {
    match value {
        EvalValue::Json(value) => value,
        EvalValue::Undefined => panic!("expected a JSON evaluation result"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn persistent_typescript_repl_contract() {
    let mut isolate = test_isolate().await;

    let hidden = isolate
        .eval("[typeof Deno, typeof fetch, typeof process]")
        .await
        .expect("ambient host APIs should be hidden");
    assert_eq!(
        json_result(hidden.result),
        json!(["undefined", "undefined", "undefined"])
    );

    let declaration = isolate
        .eval("let base: number = 40;")
        .await
        .expect("typed declaration should transpile");
    assert_eq!(declaration.result, EvalValue::Undefined);

    let result = isolate
        .eval("await Promise.resolve(base + 2)")
        .await
        .expect("state and top-level await should work");
    assert_eq!(json_result(result.result), json!(42));

    let console = isolate
        .eval(r#"console.info("answer", { value: base + 2 }, undefined, 1n); base"#)
        .await
        .expect("console output should be captured");
    assert_eq!(json_result(console.result), json!(40));
    assert_eq!(
        console.logs,
        vec![ConsoleEntry {
            level: ConsoleLevel::Info,
            args: vec![
                json!("answer"),
                json!({ "value": 42 }),
                json!("undefined"),
                json!("1"),
            ],
        }]
    );

    let explicit = isolate
        .eval("lam.result({ answer: base + 2 })")
        .await
        .expect("lam.result should make the final value explicit");
    assert_eq!(json_result(explicit.result), json!({ "answer": 42 }));

    let non_json = isolate
        .eval("1n")
        .await
        .expect_err("bigint must not cross the JSON boundary");
    assert!(matches!(non_json, EvalError::ResultNotSerializable { .. }));

    let runtime = isolate
        .eval("throw new Error('boom')")
        .await
        .expect_err("unhandled exceptions should be structured");
    match runtime {
        EvalError::Runtime { exception } => {
            assert!(exception.message.contains("boom"));
            assert!(!exception.details.is_null());
        }
        other => panic!("expected runtime error, got {other:?}"),
    }

    let import = isolate
        .eval("await import('file:///not-allowed.ts')")
        .await
        .expect_err("imports are intentionally outside slice one");
    assert!(matches!(import, EvalError::Transpile { .. }));
}

#[tokio::test(flavor = "current_thread")]
async fn promise_native_builtins_are_typed_and_discoverable() {
    let mut isolate = test_isolate().await;

    let discovered = isolate
        .eval("lam.dir({ path: 'lam.math' })")
        .await
        .expect("lam.dir should be synchronous and serializable");
    let discovered = json_result(discovered.result);
    assert_eq!(discovered[0]["path"], json!("lam.math"));
    assert_eq!(discovered[0]["functions"][0]["name"], json!("add"));
    assert!(discovered[0]["functions"][0]["inputSchema"]["properties"]["left"].is_object());
    assert_eq!(discovered[0]["functions"][1]["name"], json!("divide"));

    let kernel_contract = isolate
        .eval("lam.dir({ path: 'lam.dir' })")
        .await
        .expect("the discovery function should describe itself");
    let kernel_contract = json_result(kernel_contract.result).to_string();
    assert!(kernel_contract.contains(r#""inputSchema""#));
    assert!(!kernel_contract.contains(r#""input_schema""#));

    let result_contract = isolate
        .eval("lam.dir({ path: 'lam.result' })")
        .await
        .expect("the explicit result helper should describe itself");
    let result_contract = json_result(result_contract.result);
    assert_eq!(result_contract[0]["functions"][0]["name"], "result");

    let promise = isolate
        .eval("lam.math.add({ left: 20, right: 22 }) instanceof Promise")
        .await
        .expect("Rust builtins should expose ordinary JavaScript Promises");
    assert_eq!(json_result(promise.result), json!(true));

    let sum = isolate
        .eval(
            "await Promise.all([\
               lam.math.add({ left: 20, right: 22 }),\
               lam.math.divide({ dividend: 84, divisor: 2 })\
             ])",
        )
        .await
        .expect("ordinary Promise composition should work");
    assert_eq!(
        json_result(sum.result),
        json!([{ "sum": 42 }, { "quotient": 42 }])
    );

    let caught = isolate
        .eval(
            "await (async () => {\
               try {\
                 return { value: await lam.math.divide({ dividend: 42, divisor: 0 }) };\
               } catch (error) {\
                 return { caught: error };\
               }\
             })()",
        )
        .await
        .expect("typed builtin rejections should be catchable as their raw value");
    assert_eq!(
        json_result(caught.result),
        json!({
            "caught": {
                "type": "divisionByZero",
                "dividend": 42
            }
        })
    );

    let unhandled = isolate
        .eval("lam.math.divide({ dividend: 42, divisor: 0 })")
        .await
        .expect_err("an unhandled builtin rejection should retain its category");
    assert_eq!(
        unhandled,
        EvalError::BuiltinFailure {
            error: json!({
                "type": "divisionByZero",
                "dividend": 42
            })
        }
    );

    let invalid = isolate
        .eval("lam.math.add({ left: 'forty', right: 2 })")
        .await
        .expect_err("invalid input should fail at the Rust type boundary");
    match invalid {
        EvalError::Runtime { exception } => {
            assert!(exception.message.contains("builtin input"));
        }
        other => panic!("expected a host bridge runtime error, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn rust_registration_materializes_nested_namespaces_from_the_manifest() {
    let mut isolate = test_isolate().await;

    let result = isolate
        .eval("await acme.catalog.text.echo({ text: 'hello' })")
        .await
        .expect("a Rust-only registration should become callable TypeScript");
    assert_eq!(
        json_result(result.result),
        json!({
            "text": "hello",
            "isolateGeneration": 1,
        })
    );

    let discovered = isolate
        .eval("lam.dir({ path: 'acme.catalog.text.echo' })")
        .await
        .expect("lam.dir should project the same Rust manifest");
    let discovered = json_result(discovered.result);
    assert_eq!(discovered[0]["path"], json!("acme.catalog.text"));
    assert_eq!(discovered[0]["functions"][0]["name"], json!("echo"));
    assert!(
        discovered[0]["functions"][0]["outputSchema"]["properties"]["isolateGeneration"]
            .is_object()
    );

    let authoritative = isolate
        .eval(
            "const local = lam.dir({ path: 'acme.catalog.text' });\
             local[0].path = 'tampered';\
             lam.dir({ path: 'acme.catalog.text' })[0].path",
        )
        .await
        .expect("JavaScript mutations must not alter the Rust manifest");
    assert_eq!(
        json_result(authoritative.result),
        json!("acme.catalog.text")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn model_api_inventory_is_a_compact_view_of_the_manifest() {
    let isolate = test_isolate().await;
    let inventory = isolate.api_inventory();

    assert!(inventory.contains(
        "- `lam.dir(query?: { path?: string }): NamespaceDescriptor[]` — Discover installed namespaces, functions, and inferred schemas. When the embedding exposes a current model selection, the unfiltered result and `lam` path query include it as `currentSelection` on the `lam` namespace descriptor."
    ));
    assert!(inventory.contains(
        "- `lam.result<T extends JsonValue>(value: T): T` — Returns a JSON-serializable value unchanged, making the eval's final result explicit. Use it as the last expression."
    ));
    assert!(inventory.contains(
        "- `lam.math.add(input: { left: number; right: number }): Promise<{ sum: number }>` — Adds two numbers across an asynchronous Rust op boundary."
    ));
    assert!(inventory.contains(
        "- `acme.catalog.text.echo(input: { text: string }): Promise<{ text: string; isolateGeneration: number }>` — Returns its input and the invoking isolate generation."
    ), "unexpected inventory:\n{inventory}");
    assert!(!inventory.contains("This second paragraph"));
}

#[tokio::test(flavor = "current_thread")]
async fn dir_reports_the_embeddings_live_model_selection() {
    let selected = Arc::new(RwLock::new(DirectorySelection {
        provider: "openai".to_owned(),
        model: "gpt-first".to_owned(),
        effort: Some("high".to_owned()),
    }));
    let source = DirectorySelectionSource::new({
        let selected = Arc::clone(&selected);
        move || selected.read().unwrap().clone()
    });
    let mut isolate = Isolate::builder()
        .directory_selection(source)
        .build()
        .await
        .expect("selection-aware isolate should build");

    let first = isolate
        .eval("lam.dir({ path: 'lam' })[0].currentSelection")
        .await
        .expect("selection should be discoverable");
    assert_eq!(
        json_result(first.result),
        json!({ "provider": "openai", "model": "gpt-first", "effort": "high" })
    );

    *selected.write().unwrap() = DirectorySelection {
        provider: "fireworks".to_owned(),
        model: "deepseek".to_owned(),
        effort: Some("low".to_owned()),
    };
    let second = isolate
        .eval("lam.dir({ path: 'lam' })[0].currentSelection")
        .await
        .expect("updated selection should be discoverable");
    assert_eq!(
        json_result(second.result),
        json!({ "provider": "fireworks", "model": "deepseek", "effort": "low" })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn multiple_isolates_share_one_thread_without_sharing_state() {
    let (mut first, mut second) = tokio::join!(test_isolate(), test_isolate());

    first
        .eval("globalThis.identity = 'first'; globalThis.turns = 0")
        .await
        .expect("first isolate should accept state");
    second
        .eval("globalThis.identity = 'second'; globalThis.turns = 40")
        .await
        .expect("second isolate should accept independent state");

    let first_eval = first.eval(
        "const added = await lam.math.add({ left: 1, right: 1 });\
         globalThis.turns += added.sum;\
         lam.result({ identity, turns: globalThis.turns })",
    );
    let second_eval = second.eval(
        "const added = await lam.math.add({ left: 1, right: 2 });\
         globalThis.turns += added.sum;\
         lam.result({ identity, turns: globalThis.turns })",
    );
    let (first_output, second_output) = tokio::join!(first_eval, second_eval);

    assert_eq!(
        json_result(first_output.expect("first eval should complete").result),
        json!({ "identity": "first", "turns": 2 })
    );
    assert_eq!(
        json_result(second_output.expect("second eval should complete").result),
        json!({ "identity": "second", "turns": 43 })
    );

    // Isolates can be destroyed out of construction order because neither one
    // remains entered while parked.
    drop(first);
    let surviving = second
        .eval("lam.result({ identity, turns: globalThis.turns })")
        .await
        .expect("the second isolate should survive first-isolate teardown");
    assert_eq!(
        json_result(surviving.result),
        json!({ "identity": "second", "turns": 43 })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn default_eval_has_no_wall_deadline() {
    let pending_namespace = Namespace::new("lam.wait", "No wall deadline probe.").function(
        "briefly",
        "Resolves after the configured execution limit would have elapsed.",
        |_context, (): ()| async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            Ok::<_, Never>(())
        },
    );
    let mut isolate = Isolate::builder()
        .namespace(pending_namespace)
        .execution_timeout(Duration::from_millis(20))
        .build()
        .await
        .expect("test isolate should initialize");

    let started_at = Instant::now();
    isolate
        .eval("await lam.wait.briefly(); lam.result(1)")
        .await
        .expect("pending builtin waits must not consume the execution limit");
    assert!(
        started_at.elapsed() >= Duration::from_millis(80),
        "the builtin should have waited past the execution limit"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn pending_builtin_can_outlive_execution_limit_and_succeed() {
    let pending_namespace = Namespace::new("lam.wait", "Execution limit probe.").function(
        "later",
        "Stays pending longer than the continuous execution limit.",
        |_context, (): ()| async move {
            tokio::time::sleep(Duration::from_millis(75)).await;
            Ok::<_, Never>("ok")
        },
    );
    let mut isolate = Isolate::builder()
        .namespace(pending_namespace)
        .execution_timeout(Duration::from_millis(15))
        .build()
        .await
        .expect("test isolate should initialize");
    let generation = isolate.generation();

    let output = isolate
        .eval("lam.result(await lam.wait.later())")
        .await
        .expect("async Pending must not trip the execution watchdog");
    assert_eq!(json_result(output.result), json!("ok"));
    assert_eq!(isolate.generation(), generation);
}

#[tokio::test(flavor = "current_thread")]
async fn synchronous_infinite_javascript_hits_execution_timeout() {
    let mut isolate = Isolate::builder()
        .namespace(math_namespace())
        .execution_timeout(Duration::from_millis(20))
        .build()
        .await
        .expect("test isolate should initialize");
    let mut sibling = test_isolate().await;
    sibling
        .eval("globalThis.siblingState = 42")
        .await
        .expect("sibling state should initialize");

    isolate
        .eval("const survivor: number = 42;")
        .await
        .expect("pre-timeout state should exist");
    let previous_generation = isolate.generation();

    let error = isolate
        .eval("while (true) {}")
        .await
        .expect_err("infinite execution should hit the continuous limit");
    assert_eq!(
        error,
        EvalError::ExecutionTimedOut {
            timeout_ms: 20,
            previous_generation,
            new_generation: previous_generation + 1,
        }
    );
    assert_eq!(isolate.generation(), previous_generation + 1);

    let state = isolate
        .eval("typeof survivor")
        .await
        .expect("fresh generation should be ready before the error is returned");
    assert_eq!(json_result(state.result), json!("undefined"));

    let builtin = isolate
        .eval("lam.math.add({ left: 40, right: 2 })")
        .await
        .expect("registered capabilities should be restored");
    assert_eq!(json_result(builtin.result), json!({ "sum": 42 }));

    let sibling_state = sibling
        .eval("siblingState")
        .await
        .expect("replacing one isolate must not disturb its sibling");
    assert_eq!(json_result(sibling_state.result), json!(42));
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_wall_deadline_times_out_pending_builtin() {
    let dropped = Arc::new(AtomicBool::new(false));
    let handler_dropped = Arc::clone(&dropped);
    let pending_namespace = Namespace::new("lam.wait", "Wall deadline probe.").function(
        "forever",
        "Waits forever so the wall deadline must cancel this operation.",
        move |_context, (): ()| {
            let handler_dropped = Arc::clone(&handler_dropped);
            async move {
                let _drop_flag = DropFlag(handler_dropped);
                std::future::pending::<()>().await;
                Ok::<(), Never>(())
            }
        },
    );
    let mut isolate = Isolate::builder()
        .namespace(pending_namespace)
        .execution_timeout(Duration::from_secs(5))
        .build()
        .await
        .expect("test isolate should initialize");
    let previous_generation = isolate.generation();

    let error = isolate
        .eval_with(
            "lam.wait.forever()",
            EvalOptions::default().timeout(Duration::from_millis(20)),
        )
        .await
        .expect_err("a pending Rust op must still respect an explicit wall deadline");

    assert_eq!(
        error,
        EvalError::TimedOut {
            timeout_ms: 20,
            previous_generation,
            new_generation: previous_generation + 1,
        }
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "dropping the poisoned runtime should cancel its pending op future"
    );
    assert_eq!(isolate.generation(), previous_generation + 1);
}

#[tokio::test(flavor = "current_thread")]
async fn host_interruption_crosses_an_async_builtin_handoff() {
    let (started_sender, started_receiver) = mpsc::channel();
    let started = Namespace::new("test.control", "Interruption synchronization.").function(
        "started",
        "Signals that JavaScript is about to resume.",
        move |_context, (): ()| {
            let started_sender = started_sender.clone();
            async move {
                let _ = started_sender.send(());
                Ok::<(), Never>(())
            }
        },
    );
    let mut isolate = Isolate::builder()
        .namespace(started)
        .execution_timeout(Duration::from_secs(5))
        .build()
        .await
        .expect("test isolate should initialize");
    let interrupt = isolate.interrupt_handle();
    let stopper = std::thread::spawn(move || {
        started_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("builtin should signal before JavaScript resumes");
        interrupt.terminate();
    });

    let started_at = Instant::now();
    isolate
        .eval("await test.control.started(); while (true) {}")
        .await
        .expect_err("the host interruption should stop resumed JavaScript");
    stopper.join().expect("interrupt thread should finish");
    assert!(
        started_at.elapsed() < Duration::from_secs(2),
        "host interruption should not wait for an ambient wall deadline"
    );

    let previous_generation = isolate.generation();
    assert_eq!(
        isolate
            .restart_after_interruption()
            .expect("a fresh isolate should start"),
        previous_generation + 1
    );
    assert_eq!(
        json_result(
            isolate
                .eval("lam.result(42)")
                .await
                .expect("the replacement isolate should be usable")
                .result
        ),
        json!(42)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_and_dropping_an_eval_preserves_a_sibling_isolate() {
    let pending_namespace = Namespace::new("lam.wait", "Cancellation probe.").function(
        "forever",
        "Never resolves.",
        |_context, (): ()| async move {
            std::future::pending::<()>().await;
            Ok::<(), Never>(())
        },
    );
    let mut pending = Isolate::builder()
        .namespace(pending_namespace)
        .build()
        .await
        .expect("pending isolate should initialize");
    let mut sibling = test_isolate().await;

    let mut evaluation = Box::pin(pending.eval("lam.wait.forever()"));
    poll_fn(|cx| match evaluation.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(result) => panic!("pending eval completed unexpectedly: {result:?}"),
    })
    .await;
    drop(evaluation);
    drop(pending);

    let result = sibling
        .eval("6 * 7")
        .await
        .expect("cancelling a parked sibling must not damage this isolate");
    assert_eq!(json_result(result.result), json!(42));
}

#[tokio::test(flavor = "current_thread")]
async fn console_capture_can_be_disabled_without_removing_console() {
    let mut isolate = Isolate::builder()
        .capture_console(false)
        .build()
        .await
        .expect("test isolate should initialize");

    let output = isolate
        .eval("console.log('discarded', { value: 42 }); lam.result(typeof console.log)")
        .await
        .expect("console should remain callable when capture is disabled");

    assert_eq!(json_result(output.result), json!("function"));
    assert!(output.logs.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn builder_default_timeout_applies_as_wall_deadline() {
    let pending_namespace = Namespace::new("lam.wait", "Default wall probe.").function(
        "forever",
        "Never resolves.",
        |_context, (): ()| async move {
            std::future::pending::<()>().await;
            Ok::<(), Never>(())
        },
    );
    let mut isolate = Isolate::builder()
        .namespace(pending_namespace)
        .default_timeout(Duration::from_millis(25))
        .execution_timeout(Duration::from_secs(5))
        .build()
        .await
        .expect("test isolate should initialize");
    let previous_generation = isolate.generation();

    let error = isolate
        .eval("lam.wait.forever()")
        .await
        .expect_err("builder default wall deadline should bound pending work");
    assert!(matches!(
        error,
        EvalError::TimedOut {
            timeout_ms: 25,
            previous_generation: generation,
            new_generation,
        } if generation == previous_generation && new_generation == previous_generation + 1
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn builder_rejects_ambiguous_or_invalid_names() {
    let duplicate = match Isolate::builder()
        .namespace(Namespace::new("acme.search", "one"))
        .namespace(Namespace::new("acme.search", "two"))
        .build()
        .await
    {
        Ok(_) => panic!("duplicate namespaces should fail before V8 starts"),
        Err(error) => error,
    };
    assert!(matches!(
        duplicate,
        IsolateBuildError::DuplicateNamespace { .. }
    ));

    let invalid = match Isolate::builder()
        .namespace(Namespace::new("acme.bad-name", "invalid"))
        .build()
        .await
    {
        Ok(_) => panic!("paths should remain dot-accessible identifiers"),
        Err(error) => error,
    };
    assert!(matches!(invalid, IsolateBuildError::InvalidName { .. }));

    let conflict = match Isolate::builder()
        .namespace(Namespace::new("acme", "Function parent.").function(
            "search",
            "Occupies the path required by the child namespace.",
            |_context, (): ()| async move { Ok::<_, Never>(()) },
        ))
        .namespace(Namespace::new("acme.search.index", "Conflicting child."))
        .build()
        .await
    {
        Ok(_) => panic!("function paths cannot also be namespace parents"),
        Err(error) => error,
    };
    assert!(matches!(
        conflict,
        IsolateBuildError::NamespaceFunctionConflict { .. }
    ));

    let global_collision = match Isolate::builder()
        .namespace(Namespace::new(
            "Object.tools",
            "Must not replace an existing JavaScript global.",
        ))
        .build()
        .await
    {
        Ok(_) => panic!("namespace roots cannot replace JavaScript globals"),
        Err(error) => error,
    };
    assert!(matches!(
        global_collision,
        IsolateBuildError::RuntimeInitialization { .. }
    ));
}
