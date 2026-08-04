# `lam`

The public single-actor facade for the [lam](../../README.md) coding-agent
runtime.

This is the crate most embedding applications start with. It combines the
persistent TypeScript kernel from `lam-deno`, the durable domain model from
`lam-core`, and a provider-neutral model/tool loop behind `Lam::builder`.

## Responsibilities

- Build and recover durable actors.
- Run the model → eval → model loop.
- Provide durable `send` and linear `call` APIs.
- Render the default system prompt from the installed capability manifest.
- Coordinate steering, queueing, structured output, and model switching.
- Trigger manual, threshold, and overflow compaction.
- Expose run-scoped and actor-wide ephemeral events.
- Re-export the public `lam-core` and `lam-deno` types needed by embeddings.

Concrete providers, durable storage, coding capabilities, and multi-actor
scheduling live in separate optional crates.

## Basic actor

`Lam::builder` accepts any `Model<P, C>` whose provider and codec implement the
`lam-core` contracts. Provider crates such as `lam-openai` return this type
directly.

```rust,ignore
use lam::{DeliveryMode, Lam};

let runtime = Lam::builder(model)
    .context_window_tokens(128_000)
    .namespace(application_api)
    .annotate_system_prompt("Follow the application's data-handling policy.")
    .build();

let mut actor = runtime.actor("assistant").build().await?;

// Durable admission without waiting for the model loop.
let receipt = actor
    .actor_ref()
    .send("A queued update", DeliveryMode::Queue)
    .await?;

// One serialized tool-calling loop, returning terminal text.
let answer = actor.call("Handle the current task").await?;

actor.shutdown().await?;
```

The default store is the pure-Rust in-memory `MemStore`. Install a different
`JournalStore` before calling `build()`:

```rust,ignore
let runtime = Lam::builder(model)
    .state_store(durable_store)
    .build();
```

## Builder layers

The construction API deliberately separates shared runtime configuration from
one actor identity:

1. `Lam::builder(model)` selects models, storage, capabilities, prompt policy,
   compaction, eval limits, and console capture.
2. `LamBuilder::build()` freezes that configuration into `LamRuntime`.
3. `LamRuntime::actor(id)` selects the durable actor journal.
4. `ActorBuilder::build()` creates a dedicated actor thread, while
   `build_task()` exposes the non-`Send` task for a scheduler such as
   `lam-agents`.

## Handles and control flow

- `Actor` is the linear runtime owner used for event-stream ownership, graceful
  shutdown, and joined abort. Its operation methods delegate to its handle.
- `ActorHandle` is cloneable authority for correlated calls, compaction, model
  switching, state projection, durable mailbox delivery, and recoverable run
  interruption.
- `ActorRef` is cloneable send-only mailbox authority. `send` returns after the
  message is durably admitted.
- `AbortHandle` is cloneable out-of-band cancellation authority and can
  interrupt a blocked model request or active JavaScript without waiting for
  the active correlated operation.
- `Run<T>` is the owned, lazy, streamable form of a call. It exposes progress
  events and can decode schema-constrained output.

Calls, compaction, and model switches are mutually exclusive per actor. A
conflicting operation returns `ActorError::Busy`. Additional input should use
the mailbox: steering joins the active run at its next boundary, while
queueing waits for that run to finish.

`ActorHandle::interrupt` is deliberately separate from abort. It cancels
in-flight provider, compactor, or eval work, discards incomplete model deltas,
and atomically records a model-visible runtime notice plus any required eval
failure. The active run becomes permanently interrupted, the actor remains
resident, and a later call starts a new run from the durable boundary. If an
eval had begun, its isolate is replaced before interruption completes.

## Context and recovery

The actor journal is append-only and retains the complete provider-native
history. Reopening the same actor ID with the same model registry rebuilds the
projection and starts a fresh isolate. If execution may have stopped between a
model's eval request and its durable result, recovery admits a structured,
model-visible notice declaring that the isolate was reset and the effect
outcome is unknown.

A live recoverable interruption is more precise than crash recovery: when a
durable eval request has no result, Lam records an explicit interrupted eval
failure in the same journal batch as the terminal interruption notice.

Process-local call waiters are not durable jobs. Recovered work continues
through the actor mailbox and journal rather than trying to resurrect a Rust
future from a previous process.

## Compaction and models

Every registered model has a stable host-defined `ModelId` and an immutable
provider/model/codec descriptor. The initial model defaults to ID `default`;
additional models are registered with `model` or `model_with_compactor`.

Compaction is enabled by default. Its portable strategy asks the selected model
for a summary and retains an exact recent tail. Embeddings can configure
thresholds, select deterministic truncation, install any `Compactor`, compose
fallback compactors, or disable compaction entirely. Raw journal history is
never deleted.

Switching models compacts by default and atomically commits a compatible
checkpoint with the new selection. `ModelSwitchPolicy::ReuseContext` is an
explicit opt-in for embeddings willing to reuse the current history unchanged.

## Events

`RunEvent` covers model/eval progress, token deltas, message delivery,
compaction, usage, and terminal outcomes. A correlated `Run` owns one stream;
`Actor::take_run_events` covers all runs, including ordinary mailbox wakes.
`RuntimeEvents` carries actor-wide recovery and compaction information.

These streams are best-effort observability surfaces. The journal is the
durable source of truth.

## Related crates

- [`lam-core`](../lam-core/README.md): domain types and extension contracts.
- [`lam-deno`](../lam-deno/README.md): persistent TypeScript kernel.
- [`lam-openai`](../lam-openai/README.md): real model adapters.
- [`lam-redb`](../lam-redb/README.md): durable journal storage.
- [`lam-agents`](../lam-agents/README.md): bounded multi-actor hosting.
- [`lam-code`](../lam-code/README.md): optional coding capabilities.

See the repository [README](../../README.md) for a project overview and
[`docs/PLAN.md`](../../docs/PLAN.md) for the full design record.
