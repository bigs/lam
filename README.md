# lam

`lam` is an experimental Rust library for building durable coding agents around
one model-visible primitive: evaluate TypeScript in an embedded, persistent
Deno isolate.

The model gets one tool, `eval`. Everything else—filesystem access, shell
execution, application APIs, subagents—is a typed Rust capability surfaced to
that TypeScript environment. This keeps the model interface tiny without
limiting what an embedding can safely choose to provide.

**Status:** Milestone 1, the embeddable agent engine, is complete. The first
interactive `lam` TUI milestone is also available. The project is still
experimental and may change before a stable release.

The complete architecture and decision record lives in
[`docs/PLAN.md`](docs/PLAN.md).

## Why one eval tool?

Tool-rich agents ask the model to repeatedly select from a growing catalog of
small operations. lam instead gives the model a persistent programming
environment:

```text
user input
    ↓
model ──eval(TypeScript)──→ persistent Deno isolate
  ↑                              │
  └──── structured result ───────┘
                                 │
                         typed Rust capabilities
```

A single eval carries a brief user-facing intent alongside its TypeScript. It
can sequence dependent work, use `Promise.all` for independent work, retain
variables between calls, catch structured errors, and return JSON with
`lam.result(value)`. Rust remains authoritative over which capabilities exist
and what types cross the boundary.

The isolate exposes `lam.dir()` for complete runtime discovery. Its compact
system-prompt synopsis is generated from the same manifest, including inferred
input/output schemas and Rust-authored documentation. Adding a Rust builtin does
not require a TypeScript shim.

## What is implemented

- Persistent TypeScript cells with lexical state and top-level `await`.
- Typed, Promise-native Rust namespaces using Serde and JSON Schema inference.
- Explicit JSON results and ordered structured `console` capture.
- Bounded eval timeouts which interrupt and replace poisoned isolates.
- Durable actor mailboxes and model-visible context in append-only journals.
- Linear `call`, durable `send`, steering, queueing, and structured outputs.
- Provider-native context preservation, including encrypted reasoning traces.
- OpenAI Responses and OpenAI-compatible Chat Completions adapters.
- Automatic/manual compaction, model switching, and exact native checkpoints.
- In-memory and `redb` journal implementations.
- A bounded multi-actor runtime with hierarchical subagents and message passing.
- Optional filesystem, patching, and shell capabilities for coding agents.
- Ephemeral run, token, usage, cost, runtime, and actor-lifecycle events.

## Quick start

The workspace is not yet published as a stable crates.io release. For now,
consume the crates by path from a cloned checkout or another Cargo workspace.

This example creates a real OpenAI-backed coding actor. The actor gets only the
capabilities installed by the embedding:

```rust,ignore
use lam::Lam;
use lam_code::{CodingPack, FilesystemAccess, LocalCommandRunner};
use lam_openai::responses::Responses;

let model = Responses::builder("gpt-5.6-luna")
    .api_key(std::env::var("OPENAI_API_KEY")?)
    .build()?;

let coding = CodingPack::builder(".")
    .filesystem_access(FilesystemAccess::ReadWrite)
    .shell(LocalCommandRunner::default())
    .build()?;

let mut actor = Lam::builder(model)
    .namespaces(&coding)
    .context_window_tokens(128_000)
    .annotate_system_prompt("Work only inside the configured project root.")
    .build()
    .actor("main")
    .build()
    .await?;

let answer = actor.call("Find the largest Rust source file.").await?;
println!("{answer}");
actor.shutdown().await?;
```

For the distinctive kernel without a model loop, embed `lam-deno` directly:

```rust,ignore
use lam::{Isolate, Namespace, Never};

let math = Namespace::new("acme.math", "Application arithmetic.").function(
    "double",
    "Doubles an integer.",
    |_context, input: i64| async move { Ok::<_, Never>(input * 2) },
);

let mut isolate = Isolate::builder().namespace(math).build().await?;
let output = isolate
    .eval("lam.result(await acme.math.double(21))")
    .await?;
```

## Crates

| Crate | Responsibility | Use it when |
| --- | --- | --- |
| [`lam`](crates/lam/README.md) | Public single-actor facade, model loop, recovery, compaction, and events | Embedding an agent |
| [`lam-core`](crates/lam-core/README.md) | Provider-independent domain types, append-only journal, projections, and SPIs | Implementing storage, providers, codecs, or compactors |
| [`lam-deno`](crates/lam-deno/README.md) | Persistent Deno isolate and generic typed builtin bridge | Embedding TypeScript without the actor/model layer |
| [`lam-redb`](crates/lam-redb/README.md) | Durable `JournalStore` backed by `redb` | Persisting actor state in one embedded database file |
| [`lam-openai`](crates/lam-openai/README.md) | Lossless Responses and compatible Chat Completions adapters | Connecting OpenAI or a compatible inference provider |
| [`lam-agents`](crates/lam-agents/README.md) | Bounded isolate scheduler, actor addressing, and `lam.agents` capabilities | Hosting roots and subagents together |
| [`lam-code`](crates/lam-code/README.md) | Optional filesystem, editing, and command execution capabilities | Building a coding-agent application |
| [`lam-tui`](crates/lam-tui/README.md) | Ratatui executable, provider configuration, and interactive transcript | Running Lam as a coding agent |

The dependency boundary is intentional: `lam` remains a straightforward
single-actor library; provider, persistence, coding, and multi-agent features
are optional crates layered around it.

## Core contracts

### Persistent capability-oriented TypeScript

Successful evals share lexical and global state. Async Rust functions appear as
ordinary promises. Each namespace function accepts one typed input and returns
a typed output or typed error. `lam.dir()` exposes the exact installed manifest;
`lam.result(value)` makes the final JSON value explicit.

A bare isolate has no filesystem, process, network, npm, or ambient `Deno`
authority. The kernel-owned TypeScript bootstrap is installed through Deno's
extension lifecycle and transpiled with the same pinned `deno_ast` used for eval
cells. There is no npm install, bundling step, or generated runtime artifact.

### Durable actors

Each actor owns one append-only journal containing model selection, admitted
messages, full provider-native context, and compaction records. A
`JournalStore` needs only ordered reads and compare-and-append batches. Pure
projections derive pending work and the effective context.

`ActorHandle::call` returns an owned run for one tool-calling loop, while the
linear `Actor` retains runtime lifecycle and event-stream ownership.
`ActorRef::send` returns after durable admission. Steering messages join the
active run at its next boundary; queued messages wait for that run to finish.
`ActorHandle::interrupt` recoverably closes live work with an atomic,
model-visible terminal boundary while leaving the actor usable for a new run.
Recovery creates a fresh isolate, preserves the complete journal, and inserts
a model-visible notice when an interrupted eval may have had partial effects.

A recoverable interruption drops incomplete provider or compactor output,
replaces an isolate interrupted during eval, and records an explicit failed
eval result when needed. A completion already committed at the journal
boundary wins the race.

### Provider-native history

Provider codecs translate between the journal and a wire API, but the original
provider payload remains authoritative. This preserves encrypted reasoning,
signatures, unknown extension fields, and exact tool-call structure. Computed
views are used for model-visible replay rather than normalizing away native
data.

OpenAI Responses always uses `store: false` and manually replays complete native
output items. Chat Completions stores the native SSE chunks because compatible
servers do not return a second completed response object after streaming.

### Compaction and model switching

Compaction is configurable and enabled by default at 90% of the declared model
context window. The portable default produces a summary plus an exact recent
tail; deterministic truncation and custom `Compactor` implementations are also
available. The raw journal is never discarded.

Model switching compacts by default, then atomically records a target-compatible
checkpoint and model selection. Explicit context reuse is available when the
embedding accepts the compatibility risk. OpenAI's native Responses compaction
is opt-in and can fall back to the portable strategy.

### Multiple actors

`lam-agents` hosts several thread-affine isolates on a fixed pool of
current-thread executors. Awaiting async work parks one isolate so a sibling on
the same worker can progress; synchronous CPU-bound JavaScript occupies that
worker until it returns or is interrupted.

Actors have canonical paths such as `/root/researcher`. `lam.agents.call`
creates a persistent child and waits directly for its initial task.
`lam.agents.spawn` returns after durable admission and later steers a typed
outcome into the parent's mailbox. Addressed sends are always actor-authenticated
and steering. Direct-child stop recursively retires a subtree. Host-side tree
interruption fans out recoverable run boundaries, retires descendants, and
leaves the addressed root available for later input.

## Capability and safety model

lam's default is absence of authority. Capabilities are installed explicitly on
builders and can be omitted or replaced by application-specific namespaces.

`lam-code` applies path validation, limits, and process cleanup, but these are
API guardrails—not an operating-system sandbox. Its supplied local command
runner inherits the embedding process's host authority. A production embedding
should run lam inside an appropriate OS/container sandbox or inject a sandboxed
`CommandRunner` when executing untrusted programs.

Eval timeout does not roll back host effects which completed before
interruption. lam reports the isolate replacement and possible partial effects
rather than pretending execution was transactional.

## Events and observability

Run streams expose model starts/completions, token deltas, eval calls/results,
message delivery, compaction, and terminal outcomes. Provider completion events
include normalized token usage, untouched native usage JSON, and optional
embedding-supplied cost estimates. The same metadata is emitted through Rust
`tracing`.

Multi-actor embeddings can take one addressed system event stream containing
hosted/retired actors, existing run/runtime events, and child outcomes. These
events are ephemeral UI/observability data; journals and mailboxes remain the
durable authority.

## Development

The workspace uses Rust 2024 and pins the Deno/V8-facing dependency versions.
The first build can take a while because it compiles the embedded JavaScript
runtime stack.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

Add `--offline` to the clippy, test, and doc commands when dependencies are
already fetched.

The default suite is deterministic and does not require network access. Ignored
live tests use `OPENAI_API_KEY` or `FIREWORKS_API_KEY` and make bounded provider
requests; they must be enabled explicitly.

## Interactive TUI

The separate `lam-tui` package produces the `lam` executable while depending
on the library crate of the same name. It loads `~/.lam/providers.toml`, starts
the coding and multi-agent capability packs in the current directory, and
renders a responsive conversation with expandable eval and lifecycle rows.
See [`crates/lam-tui/README.md`](crates/lam-tui/README.md) for configuration and
key bindings.

## Roadmap

Independent follow-ups include HTTP/webhook capabilities, async monitors,
interactive approvals, agent-writable storage and content-addressed blobs,
additional providers, durable topology reconstruction, overload queues,
rebalancing, and isolate snapshots. They remain outside the minimal core until
a concrete consumer demonstrates the need.
