# `lam-deno`

Persistent, capability-oriented TypeScript execution for
[lam](../../README.md).

This crate embeds `deno_core`, transpiles TypeScript with `deno_ast`, and
materializes typed Rust functions as Promise-native JavaScript namespaces. It
is useful on its own as a small code-interpreter primitive; the higher-level
`lam` crate adds actors, models, journals, recovery, and compaction.

## Basic use

```rust,ignore
use lam_deno::{Isolate, Namespace, Never};

let math = Namespace::new("acme.math", "Application arithmetic.").function(
    "double",
    "Doubles an integer.",
    |_context, input: i64| async move { Ok::<_, Never>(input * 2) },
);

let mut isolate = Isolate::builder()
    .namespace(math)
    .capture_console(true)
    .build()
    .await?;

let first = isolate
    .eval("const answer = await acme.math.double(21); lam.result(answer)")
    .await?;
let second = isolate.eval("lam.result(answer + 1)").await?;
```

Successful cells share lexical and global state. Top-level `await` works, and
a final promise is awaited even when the source did not spell `await`.

## Typed builtins

`Namespace::function` accepts one typed input and returns a future containing a
typed output or typed error. Serde performs boundary serialization and
`schemars` derives discoverability metadata from the same Rust types.

At isolate startup, one extension bootstrap reads the Rust-generated manifest,
creates every configured namespace/function, and routes calls through a generic
op. Extension authors do not maintain TypeScript facades. Builtin failures are
catchable as structured JavaScript values and remain structured
`EvalError::BuiltinFailure` values when unhandled.

Two kernel-owned helpers are always available:

- `lam.dir(query?)` synchronously returns matching documentation and schemas;
- `lam.result(value)` marks and returns the final JSON-compatible value.

`Isolate::api_inventory()` returns the concise manifest synopsis used by the
higher-level actor system prompt.

## Eval contract

`EvalOutput` separates the final `EvalValue` from ordered `ConsoleEntry` values.
The result must be `undefined` or JSON-compatible. Unsupported/cyclic final
values fail explicitly; unsupported console arguments degrade individually to
safe text.

Cells may not use imports or exports. The bare runtime intentionally contains
no filesystem, process, network, fetch, npm, Node compatibility layer, or
ambient `Deno` object. Those capabilities must be registered by the embedding.

## Timeouts and replacement

`eval_with` can select a timeout within the builder's configured maximum. When
the watchdog fires, lam interrupts V8, treats the isolate as poisoned, drops
pending Rust op futures, and constructs a fresh generation before returning.

`EvalError::TimedOut` means replacement succeeded and reports the previous and
new generation. `RestartFailed` or `Poisoned` means no healthy continuation is
available. Host effects completed before interruption are not rolled back.

`IsolateInterrupt` provides out-of-band cancellation without waiting for the
mutable isolate owner. Its termination signal also covers the handoff from a
completed async builtin back into JavaScript. After the eval future has been
dropped, `Isolate::restart_after_interruption` discards the poisoned heap and
installs a fresh generation. Deliberate interruption, like timeout, does not
roll back host effects which may already have completed.

## Thread affinity

V8 isolates are thread-affine and `Isolate` is deliberately not `Send`. The
future returned by an async eval can yield, allowing multiple isolates to be
polled on one current-thread executor, but each isolate must always be polled
and destroyed on its owning thread. Synchronous CPU-bound JavaScript occupies
that thread until it returns or is interrupted.

`lam-agents` owns the audited scheduling/lifecycle layer for parking several
isolates on a fixed worker pool. This crate does not implement a scheduler.

## Non-goals

- Reproducing the full Deno CLI or standard library.
- Resolving npm packages or modules.
- Providing ambient host authority.
- Owning model/provider or durable actor state.

See the repository [README](../../README.md), the public
[`lam`](../lam/README.md) facade, and [`docs/PLAN.md`](../../docs/PLAN.md).
