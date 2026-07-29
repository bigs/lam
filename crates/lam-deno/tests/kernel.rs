//! Acceptance tests for the first persistent-isolate kernel slice.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use lam_deno::{
    ConsoleEntry, ConsoleLevel, EvalError, EvalOptions, EvalValue, Isolate, IsolateBuildError,
    Namespace, Never,
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
        "Returns its input and the invoking isolate generation.",
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
        json_result(hidden.value),
        json!(["undefined", "undefined", "undefined"])
    );

    let declaration = isolate
        .eval("let base: number = 40;")
        .await
        .expect("typed declaration should transpile");
    assert_eq!(declaration.value, EvalValue::Undefined);

    let result = isolate
        .eval("await Promise.resolve(base + 2)")
        .await
        .expect("state and top-level await should work");
    assert_eq!(json_result(result.value), json!(42));

    let console = isolate
        .eval(r#"console.info("answer", { value: base + 2 }); base"#)
        .await
        .expect("console output should be captured");
    assert_eq!(json_result(console.value), json!(40));
    assert_eq!(
        console.console,
        vec![ConsoleEntry {
            level: ConsoleLevel::Info,
            message: r#"answer {"value":42}"#.to_owned(),
        }]
    );

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
    let discovered = json_result(discovered.value);
    assert_eq!(discovered[0]["path"], json!("lam.math"));
    assert_eq!(discovered[0]["functions"][0]["name"], json!("add"));
    assert!(discovered[0]["functions"][0]["inputSchema"]["properties"]["left"].is_object());
    assert_eq!(discovered[0]["functions"][1]["name"], json!("divide"));

    let kernel_contract = isolate
        .eval("lam.dir({ path: 'lam.dir' })")
        .await
        .expect("the discovery function should describe itself");
    let kernel_contract = json_result(kernel_contract.value).to_string();
    assert!(kernel_contract.contains(r#""inputSchema""#));
    assert!(!kernel_contract.contains(r#""input_schema""#));

    let promise = isolate
        .eval("lam.math.add({ left: 20, right: 22 }) instanceof Promise")
        .await
        .expect("Rust builtins should expose ordinary JavaScript Promises");
    assert_eq!(json_result(promise.value), json!(true));

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
        json_result(sum.value),
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
        json_result(caught.value),
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
        json_result(result.value),
        json!({
            "text": "hello",
            "isolateGeneration": 1,
        })
    );

    let discovered = isolate
        .eval("lam.dir({ path: 'acme.catalog.text.echo' })")
        .await
        .expect("lam.dir should project the same Rust manifest");
    let discovered = json_result(discovered.value);
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
    assert_eq!(json_result(authoritative.value), json!("acme.catalog.text"));
}

#[tokio::test(flavor = "current_thread")]
async fn isolates_do_not_share_javascript_state() {
    let mut first = test_isolate().await;

    first
        .eval("globalThis.onlyInFirst = 42")
        .await
        .expect("first isolate should accept state");
    drop(first);

    let mut second = test_isolate().await;
    let value = second
        .eval("typeof globalThis.onlyInFirst")
        .await
        .expect("second isolate should remain independent");

    assert_eq!(json_result(value.value), json!("undefined"));
}

#[tokio::test(flavor = "current_thread")]
async fn timeout_discards_and_replaces_the_isolate() {
    let mut isolate = Isolate::builder()
        .namespace(math_namespace())
        .default_timeout(Duration::from_millis(200))
        .max_timeout(Duration::from_millis(200))
        .build()
        .await
        .expect("test isolate should initialize");

    isolate
        .eval("const survivor: number = 42;")
        .await
        .expect("pre-timeout state should exist");
    let previous_generation = isolate.generation();

    let error = isolate
        .eval_with(
            "while (true) {}",
            EvalOptions::default().timeout(Duration::from_millis(20)),
        )
        .await
        .expect_err("infinite execution should be interrupted");
    assert_eq!(
        error,
        EvalError::TimedOut {
            timeout_ms: 20,
            previous_generation,
            new_generation: previous_generation + 1,
            isolate_restarted: true,
            state_lost: true,
            partial_effects_possible: true,
        }
    );
    assert_eq!(isolate.generation(), previous_generation + 1);

    let state = isolate
        .eval("typeof survivor")
        .await
        .expect("fresh generation should be ready before timeout is returned");
    assert_eq!(json_result(state.value), json!("undefined"));

    let builtin = isolate
        .eval("lam.math.add({ left: 40, right: 2 })")
        .await
        .expect("registered capabilities should be restored");
    assert_eq!(json_result(builtin.value), json!({ "sum": 42 }));
}

#[tokio::test(flavor = "current_thread")]
async fn timeout_cancels_pending_rust_builtin_work() {
    let dropped = Arc::new(AtomicBool::new(false));
    let handler_dropped = Arc::clone(&dropped);
    let pending_namespace = Namespace::new("lam.wait", "Cancellation probe.").function(
        "forever",
        "Waits forever so isolate replacement must drop this operation.",
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
        .default_timeout(Duration::from_millis(200))
        .max_timeout(Duration::from_millis(200))
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
        .expect_err("a pending Rust op must still respect the eval deadline");

    assert!(matches!(
        error,
        EvalError::TimedOut {
            previous_generation: generation,
            ..
        } if generation == previous_generation
    ));
    assert!(
        dropped.load(Ordering::SeqCst),
        "dropping the poisoned runtime should cancel its pending op future"
    );
    assert_eq!(isolate.generation(), previous_generation + 1);
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

    let first = test_isolate().await;
    let occupied = match Isolate::builder().build().await {
        Ok(_) => panic!("a second live isolate on one system thread is unsafe"),
        Err(error) => error,
    };
    assert!(matches!(
        occupied,
        IsolateBuildError::ThreadAlreadyOwnsIsolate
    ));
    drop(first);
}
