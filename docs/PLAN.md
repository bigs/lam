# Lam architecture and implementation plan

## Status and purpose

This document is the source of truth for Lam's initial architecture. It records
the decisions made before implementation, separates those decisions from
provisional API sketches, and divides the work into slices small enough to
review before each is implemented.

The project is experimental. We expect implementation to teach us things and
we will update this document as it does. We should nevertheless change settled
invariants deliberately rather than letting incidental implementation details
silently redefine them.

The labels used below are:

- **Settled**: an architectural constraint we intend to preserve.
- **Proposed**: the current preferred shape, to be validated in its slice.
- **Deferred**: valuable, but intentionally outside the initial implementation.
- **Open**: requires a focused decision before or during the named slice.

## Product thesis

Lam is a Rust library for constructing coding agents whose only model-visible
tool is:

```text
eval(TypeScript source)
```

The TypeScript executes in a persistent embedded Deno isolate. Everything else
the agent can do—read a file, edit it, invoke a command, send a message, create
a subagent, register a webhook, or call an embedding application's Rust
function—is a library available to that TypeScript program.

The small tool protocol is deliberate. TypeScript supplies control flow,
composition, local state, error handling, concurrency, and abstraction without
requiring the model-facing protocol to grow a tool for every capability.

Lam is library-first:

- An embedding application can construct an actor, send serializable values,
  await text or structured results, and optionally consume runtime events.
- Rust functions can become typed TypeScript builtins.
- Storage, model providers, compaction, capabilities, and scheduling can be
  configured or replaced.
- A coding-agent TUI will be a later consumer of the same library, not a second
  runtime architecture.

## Design principles

### One tiny model interface

**Settled.** The model sees one tool, `eval`. Discoverability and composition
belong inside the TypeScript environment rather than in an ever-growing list of
model tool schemas.

### Powerful host interface

**Settled.** A tiny model interface must not imply an impoverished embedding
API. Rust callers receive typed builders, messages, outputs, namespace
registration, storage interfaces, and event streams.

### Actors are the unit of isolation

**Settled.** Every agent and subagent is a distinct actor with its own mailbox,
model-visible context, and Deno isolate. Actors communicate by sending messages,
not by sharing an interpreter or mutable heap.

### Durable facts are append-only

**Settled.** Mailbox admission and model-visible context are the two critical
durable records. Context history is retained even when a compacted view is used
for a model request.

### Preserve provider truth

**Settled.** Provider-native payloads, including opaque or encrypted reasoning
items, must be retained losslessly. A convenient neutral view must never become
a lossy replacement for the authoritative payload.

### Configuration is capability-oriented

**Settled.** Filesystem, shell, subagents, HTTP, and other standard-library
modules are capabilities that can be registered, configured, or disabled.
Registration and authorization are separate concerns.

### Telemetry is not state

**Settled.** OpenTelemetry traces and spans explain execution. They do not
establish message identity, reply relationships, delivery, recovery, or other
authoritative semantics.

### Build in reviewed slices

**Settled.** Each implementation slice gets an agreed contract and acceptance
tests before it is built. We will not attempt the complete runtime in one pass.

## Runtime choice

### `deno_core`, not a full Deno CLI runtime

**Settled.** Lam embeds `deno_core`.

`deno_core` gives Lam the V8 isolate, JavaScript event loop, module machinery,
and Rust op bridge needed for an in-process library. Lam then decides exactly
which host capabilities exist.

Embedding the full Deno runtime would bring ambient behavior, permissions, and
subsystems that conflict with Lam's capability boundary. Building directly on
V8 would force Lam to recreate too much runtime machinery. The middle ground is
to use `deno_core` and selectively reuse suitable Deno crates or packages where
they save us from rebuilding well-designed pieces.

Bun was considered. Deno was selected because the required in-process isolate
embedding model is the foundation of `deno_core`; no equivalent Bun embedding
contract was established for this design.

### TypeScript execution

**Settled and implemented.** `eval` accepts TypeScript, not merely JavaScript.
Each cell is parsed and transpiled with `deno_ast`, then evaluated as a
persistent REPL cell. Type-only syntax is erased, top-level `await` works, and
the final expression is returned after awaiting it when it is a Promise.

The initial kernel is intentionally module-less: static imports, exports, and
dynamic `import()` are rejected during transpilation. Cells receive a stable
`lam://cell/<id>` source URL, but Slice 1 does not emit source maps. Module
loading should be introduced only alongside an explicit resolution and
capability policy.

### Persistent isolates

**Settled.** Successful evaluations share an actor's isolate state. A program
can define a value or helper in one evaluation and use it in a later evaluation.

The isolate heap is runtime state, not the durable source of truth. After a
process restart, Lam reconstructs the actor from durable records and inserts an
explicit resumption message. Freezing V8 isolates, perhaps at run boundaries,
is an interesting future optimization but is too fragile for the initial
recovery contract.

## The `lam` TypeScript environment

### Namespace

**Settled.** Builtins live under the global `lam` namespace. Larger extensions
receive their own child namespaces:

```ts
lam.fs.read(...)
lam.shell.exec(...)
lam.agents.spawn(...)
lam.http.webhook(...)
acme.search.lookup(...)
```

This prevents a flat global API from becoming incoherent and lets embedders add
domain-specific capabilities without modifying Lam.

### Discoverability

**Settled.** `lam.dir()` exposes the available namespace tree. Its output
includes function names, documentation, input schemas, output schemas, error
schemas, and capability availability.

The system prompt also contains a compact inventory of core modules that were
instantiated for the actor. The model therefore does not need to begin every
session with `lam.dir()`. It can use `lam.dir()` to inspect unfamiliar or
dynamically registered namespaces in detail.

The inventory must reflect the actor's actual capabilities. Disabled or
unauthorized modules must not be advertised as usable.

### Initial standard-library direction

**Proposed.** The coding-agent profile begins with a deliberately small utility
set analogous to `read`, `edit`, and `bash`, represented as namespaced
TypeScript APIs rather than model-visible tools.

Filesystem, shell, subagents, HTTP, and similar modules can be disabled through
the Rust builder. Filesystem roots, command policies, environment access,
network destinations, and other permissions are configured independently from
whether a module has been registered.

**Open.** We still need to settle the exact default capability profile. The
memory state store is unambiguously the default; ambient filesystem or network
authority is not.

### Vanilla TypeScript and Promises

**Settled and implemented.** The kernel API is vanilla TypeScript. Async Rust
builtins return ordinary JavaScript Promises, so model-authored programs use
`await`, `Promise.all`, `try`/`catch`, async functions, and other language
primitives without a framework-specific runtime.

An object-shaped Rust domain failure rejects the Promise with its exact
structured, JSON-serializable error value. TypeScript can catch and inspect
that value directly. Primitive failures use a small tagged wrapper because the
runtime needs object identity to distinguish an unhandled builtin rejection
from an unrelated JavaScript throw. If a builtin failure remains unhandled at
the cell boundary, Lam reports `EvalError::BuiltinFailure` rather than
flattening it into an arbitrary JavaScript exception.

Lam does not depend on Effect, npm, or a JavaScript bundler. An embedding
application can introduce optional TypeScript libraries in a future
module-loading layer, but no third-party async framework is part of the kernel
ABI. Core tracing, cancellation, and lifecycle instrumentation belong on the
Rust side of the runtime.

## Typed Rust extensions

### Namespace registration

**Settled.** The primary extension unit is a namespace containing one or more
functions, not an isolated global function:

```rust,ignore
Lam::builder(model)
    .namespace(acme_namespace)
    .build()
    .await?
```

Registration and permission are separate. An embedding program can register a
namespace implementation while disabling it for a particular actor or
constraining its authority.

### Type inference

**Settled and implemented for the eval kernel.** Rust function types supply the
TypeScript contract:

- one structured input value;
- `()` when no input is required;
- `DeserializeOwned + JsonSchema` for inputs;
- `Serialize + JsonSchema` for successful outputs;
- a serializable/schema-bearing builtin error contract;
- async, thread-safe, `'static` handlers.

The generated schema powers `lam.dir()` now and can power generated TypeScript
declarations and model-facing documentation later. Serde deserialization
independently enforces the typed input boundary at runtime. Authors do not
write a second hand-maintained schema for ordinary functions.

Registration is manifest-driven. `Namespace::function` contributes both an
erased Rust handler and its inferred descriptor to one immutable registry. A
Deno extension installs that registry into isolate state before its TypeScript
ESM entry point runs. The entry point obtains the configured namespace tree
through `op_lam_manifest`, materializes all namespace objects and functions,
and routes every ordinary invocation through `op_lam_call`. `lam.dir()` queries
the same Rust manifest, so discoverability cannot drift from the callable
surface. Adding an ordinary Rust builtin never requires a TypeScript shim.

The bridge deliberately standardizes JSON input, Promise output, and structured
failure semantics. Reusable protocols can later cover resources, streams, or
subscriptions. An explicitly configured custom facade remains a possible
escape hatch for unusual JavaScript semantics, but it is not the ordinary
extension path. A procedural macro may eventually reduce Rust boilerplate
after the builder API has proved stable.

The initial `OperationContext` exposes the isolate generation. Actor identity,
cancellation, tracing context, and explicitly granted host handles can be added
when their owning slices establish concrete requirements; they are not
speculative fields in Slice 1.

## Actor model

### Identity and isolation

**Settled.** Each actor owns:

- a stable actor identifier;
- a durable inbox;
- an append-only model-context stream;
- one resident Deno isolate while scheduled;
- model and compaction configuration;
- its enabled namespace/capability set.

A subagent is a full actor. It does not share its parent's isolate. This makes
message passing and durable recovery explicit and permits actors to move
between scheduler threads when they are not resident.

### Push, not polling

**Settled.** Messages wake actors and drive inference. An LLM is passive, so
making model code poll an Erlang-style mailbox would add latency and ceremony
without adding useful semantics.

Mailbox inspection may later be exposed for specialized applications, but the
normal delivery path inserts eligible messages into model context and triggers
or continues a run.

### Message envelope

**Proposed.** A durable message envelope contains at least:

- `message_id`;
- recipient actor;
- sender identity and sender kind;
- delivery mode;
- structured payload and its Lam codec;
- admission sequence/time;
- optional `in_reply_to`;
- optional propagated telemetry context.

`in_reply_to` is a durable semantic edge to the specific message being
answered. It is useful for subagent replies and request/reply navigation.
It is not a tracing span and it does not imply that every message belongs to a
single global correlation group.

OpenTelemetry trace and span context may travel with the envelope, but is
advisory telemetry and may be absent or sampled away.

### Message kinds

**Settled direction.** The mailbox can carry user, system, library/runtime, and
other-actor messages. Payloads are structured Lam values rather than strings
prematurely flattened into prompts.

### Steering and queueing

**Settled.** User-facing callers can choose between:

- `Steer`: make the message eligible at the next safe model-request boundary;
- `Queue`: hold the message until the current run has reached a terminal
  result, then start or join subsequent work.

Messages sent by one agent to another always steer.

A safe steering boundary is after the currently executing provider response or
eval has produced the result needed for a coherent context entry and before the
next provider request. Lam does not splice text into an in-flight provider
request or interrupt JavaScript at an arbitrary instruction.

Multiple newly eligible messages are batched in mailbox order. Before Lam
accepts a candidate terminal result, it checks for steering messages admitted
during the final model step. If any exist, they are added to context and the
same run continues. This closes the race in which an actor could otherwise
finish immediately after acknowledging a steering receipt.

The precise atomic store operation used for that race is intentionally left to
the state-store slice; the externally visible semantics are settled.

## Runs and the public Rust API

### Run

**Settled.** A run is one activation/tool-calling loop: it begins from admitted
input, may contain any number of provider and `eval` steps, incorporates
steering messages at safe boundaries, and ends when the model produces a
terminal result with no eligible steer pending.

Every context entry produced during an activation is tagged with its `run_id`.
That gives diagnostics, history tools, and future rewind facilities a natural
boundary without making the run a separate source of truth.

### Builder

**Proposed public shape:**

```rust,ignore
let lam = Lam::builder(model)
    .state_store(MemStore::new())
    .configure_stdlib(|stdlib| {
        stdlib
            .filesystem(false)
            .subagents(false)
    })
    .namespace(acme_namespace)
    .build()
    .await?;

let actor = lam.actor("main").build().await?;
```

The builder has useful defaults, exposes ordinary immutable-style
configuration transformations, and permits custom implementations of the
major interfaces. Runtime capabilities are primarily runtime configuration,
not a combinatorial set of Cargo features.

### `send`

**Settled.**

```rust,ignore
let receipt = actor.send(value, Delivery::Steer).await?;
```

`value` can be any serializable Rust value. `send` returns a
`MessageReceipt` once the configured state store has admitted the message.
"Durable" is relative to that configured store: memory admission survives
concurrent runtime work, while `redb` admission additionally survives a process
restart.

`send` does not wait for a model run or return its eventual answer.

### `call`

**Settled.**

```rust,ignore
let answer: String = actor.call(input).await?;
let structured: Review = actor.call(input).output::<Review>().await?;
```

`call` starts a new correlated run and waits for its tool-calling loop to
finish. It supports text and schema-constrained structured output.

**Proposed.** The full return is a `Run<T>` handle that can be consumed as an
event stream or simply joined/awaited for `T`. The exact Rust ergonomics will be
settled in the public-API slice; the semantic distinction from `send` will not.

## Model-visible context

### Per-actor append-only stream

**Settled.** Each actor has an ordered, append-only context stream. This is the
valuable long-term conversation record and must be retained in full.

A conceptual context entry contains:

- actor-local sequence;
- optional `run_id`;
- entry kind and authorship metadata;
- codec identifier and codec version;
- lossless encoded payload;
- minimal Lam metadata needed for projection and recovery.

This is a conceptual contract, not a commitment to a particular Rust enum or
serialization format.

### Native and Lam codecs

**Settled.** Context is heterogeneous:

- User, TUI, library, and actor-generated inputs can be stored in a structured
  Lam-native codec.
- Provider-generated messages and reasoning items are stored in that provider's
  native format without normalizing away fields.
- Compaction results are stored in the codec that produced them.

Provider reasoning models may return encrypted or otherwise opaque reasoning
traces that must be replayed exactly. Lam must not deserialize such a payload
into a smaller common enum and then synthesize a supposedly equivalent native
message.

**Proposed.** A `ModelCodec` supplies computed, read-only views:

- encode Lam-native messages for a provider request;
- expose useful neutral metadata for UIs and diagnostics;
- pass compatible native payloads through unchanged;
- translate or compact incompatible history when switching providers.

Views cut both ways: Lam-native input must be encoded for providers, while
provider-native output can be inspected through a general view. There remains
one authoritative payload, not a native copy plus a neutral copy that can
diverge.

## Context compaction

### Logical compaction

**Settled.** Context compaction changes the in-scope model view; it does not
delete historical entries.

A compaction marker is itself an append-only context entry. Conceptually it
records:

- the context sequence it `covers_through`;
- its output codec/version;
- provider/model compatibility;
- the compacted native or Lam payload;
- strategy metadata useful for inspection.

To construct a request, Lam selects the newest marker compatible with the
target model/codec and appends the eligible tail after `covers_through`. If no
compatible marker exists, it starts from the beginning or creates a new
compaction.

An implementation may maintain a per-actor/per-codec watermark pointing to the
latest compatible marker. That watermark is a rebuildable optimization. The
append-only marker is authoritative, so a missing watermark can be recovered
by seeking backward through the ordered context stream.

Markers for other codecs remain history and are ignored when projecting a
request for which they are not compatible.

### When compaction occurs

**Settled direction.** Compaction is enabled by default and is transparent to
`call`: the actor continues until completion rather than returning a
"compaction required" condition to the embedding application.

Compaction may be triggered by:

- approaching a model context limit;
- switching to an incompatible provider or model;
- an explicit caller request;
- later, a background materialization policy.

### Strategy chain

**Proposed default order:**

1. a provider-native compaction facility when one exists;
2. a Lam-managed summary with a verbatim/recent tail;
3. deterministic emergency truncation as a last resort.

Strategies are configurable and replaceable. Provider-native compaction
results remain provider-native context payloads. We will verify actual OpenAI,
Anthropic, and other provider capabilities when their adapters are built rather
than encoding assumptions now.

Physical database compaction, archival, and content-addressed offloading are
separate storage concerns. Logical context compaction never grants permission
to discard the user's full history.

## Persistence

### Critical state

**Settled.** The initial durable state is deliberately small:

1. the per-actor inbox and its delivery progress;
2. the per-actor append-only context stream and compaction markers.

We are not starting with a generic actor event-sourcing framework, effects
ledger, CQRS bus, or transaction DSL. Additional durable facts must earn their
place through a recovery or product requirement.

### `StateStore`

**Settled.** Storage is behind a public interface so embedders can provide their
own implementation.

The interface must preserve:

- ordered actor-local appends;
- admission-before-receipt for messages;
- consistent delivery progress;
- append-only context;
- the steering/finalization race semantics;
- isolation between system state and future agent-writable data.

**Open for the state-store slice.** We will derive the smallest atomic API from
these invariants and deterministic race tests. We will avoid both an
underpowered collection of unrelated KV calls and an open-ended mutation DSL.

### Implementations

**Settled.**

- `MemStore` is the default and pure-Rust reference implementation.
- `lam-redb` is the first durable implementation.
- Custom implementations are first-class and must be testable with a shared
  conformance suite.

`redb`'s ordered tables and transactions are a good match for per-actor inbox
and context keyspaces. The exact schema is part of the state-store slice.

SQLite is not in the initial plan. It can be added later as another adapter
without changing actor semantics.

### Future storage facilities

**Deferred.**

- A separate, explicitly namespaced agent-writable KV store can be exposed as a
  builtin. It must not permit access to Lam's reserved system state.
- Large payloads can move to a content-addressed blob store while logs retain
  their hashes and metadata.
- LSM-style materialized projections or snapshots can accelerate recovery.
- Retention and archival policies can control physical growth without changing
  logical history semantics.

Cross-actor operations are not globally atomic. That is expected in the actor
model; workflows needing coordination use messages and explicit protocols.

## Scheduling and concurrency

### Isolate residency

**Settled current safety constraint.** A system thread may own at most one live
Lam isolate. `rusty_v8::OwnedIsolate` remains entered for its lifetime, so
constructing another resident isolate and interleaving the two on one thread is
not a supported safe lifecycle. Slice 1 enforces this with a thread-local
builder permit instead of allowing misuse to become a native crash.

An isolate is also local to its construction thread and is not `Send`. Timeout
replacement occurs on that same thread and retains the residency permit.

**Deferred to the scheduler slice.** The original fixed-size design assumed one
single-threaded executor could host multiple resident isolates. Implementation
invalidated that assumption. The simplest safe scheduler is a bounded set of
threads with one resident isolate per thread. Before choosing it, we should
evaluate its resource and actor-residency behavior and determine whether
`deno_core` exposes a reviewed activation mechanism that safely permits
multiple isolates per thread. Actor isolation and message passing do not depend
on either scheduling choice.

### Actor serialization

**Settled.** One actor processes state transitions serially. Provider calls,
evals, and mailbox admission can be concurrent at the system level, but the
actor applies their results at explicit boundaries.

This serialization plus store-level atomic admission/finalization semantics is
what makes steering deterministic.

## Runtime events and observability

### Consumer event stream

**Settled direction.** A run can expose ephemeral events useful to a TUI,
debugger, or approval UI:

- token/reasoning deltas suitable for display;
- model-request lifecycle;
- eval start and completion;
- builtin start and completion;
- messages steered or queued;
- compaction activity;
- terminal output or failure.

Token deltas are important for a responsive TUI but are UX data, not durable
context. A completed provider payload is appended durably; its transient chunks
need not be.

Embedded consumers that only need input and output can simply await `call`
without draining every event manually. The event stream must not make the
simple path awkward.

### No initial effects ledger

**Settled.** We will not initially persist a third stream recording every
external effect or `ModelStepStarted`/`Completed` event. The recovery semantics
for ambiguous in-flight external effects are real, but an effects ledger adds
substantial machinery before we have a demonstrated need.

Lifecycle information can first exist as runtime events and OpenTelemetry
spans. If crash recovery requires durable outcome-unknown records later, we can
add the minimum facts then.

Approval behavior belongs primarily in capability implementations and their
TypeScript/Rust bridge rather than in a universal event-ledger abstraction.

### OpenTelemetry

**Settled.** Runs, model calls, evals, builtins, compactions, and message
delivery should produce OpenTelemetry spans from the Rust runtime. A future
TypeScript library may create child spans through an explicitly registered
capability, but tracing does not require a framework inside the isolate.

Trace identifiers are never authoritative state. Durable IDs such as
`message_id`, `in_reply_to`, `actor_id`, context sequence, and `run_id` retain
their meaning when tracing is disabled or sampled.

## Security and capability boundaries

### No ambient host authority

**Settled.** A bare isolate receives no filesystem, process, shell, or network
authority merely because it embeds Deno. Host access exists only through
registered Lam builtins and their policies.

The Rust op boundary enforces authority. Prompt instructions and TypeScript
wrappers improve ergonomics but are not the security boundary.

### Policy dimensions

**Proposed.** Capability configuration will eventually cover:

- readable and writable filesystem roots;
- allowed command execution and environment variables;
- network destinations and listener policy;
- subagent creation limits;
- time, memory, output, and cancellation limits;
- access to embedding-application handles.

These policies will be designed with their corresponding standard-library
slices rather than all at once.

## HTTP, monitors, and subagents

### Webhooks

**Deferred but architecturally supported.** A future `lam.http` capability can
register a webhook as an intentional side effect and return its externally
usable URL. Each request becomes a structured mailbox message and wakes the
actor, allowing external events to enter context without model polling.

Listener lifecycle, authentication, routing, and public exposure are policies
of the HTTP capability, not ambient isolate behavior.

### Monitors

**Deferred.** Async generators can model native monitors. Each emitted item
becomes a message to the owning actor. Backpressure, coalescing, cancellation,
restart behavior, and durability need a focused design before implementation.

### Subagents

**Deferred until the base actor works.** A subagent is constructed through the
same public actor machinery, gets its own store records and isolate, and
communicates through always-steering messages. The subagent standard library
can be disabled by the builder.

## Provider abstraction

**Proposed decomposition.**

- A model provider performs requests and exposes native response streams.
- A codec converts Lam-native entries to provider input and recognizes
  compatible provider-native entries.
- An output contract describes text or structured completion.
- A compactor produces an append-only marker compatible with a target codec.

The abstraction must be provider-neutral at the control-flow level while
remaining provider-lossless at the data level.

Initial actor-loop tests will use a deterministic scripted provider. Real
OpenAI or Anthropic integration should not be needed to prove mailbox,
context, eval, or run semantics.

## Workspace structure

The initial workspace contains:

```text
.
├── Cargo.toml
├── README.md
├── docs/
│   └── PLAN.md
└── crates/
    ├── lam/
    ├── lam-core/
    ├── lam-deno/
    └── lam-redb/
```

### `lam`

The public facade. It will expose `Lam`, builders, actor handles, `send`,
`call`, output contracts, and the most useful extension points without forcing
users to assemble internal crates manually.

### `lam-core`

Provider-independent domain logic: identifiers, inbox/context types, run and
delivery semantics, provider/codec/store traits, projections, `MemStore`, actor
state machine, scheduler-facing contracts, and deterministic tests.

It must not depend on V8 or `deno_core`.

### `lam-deno`

The embedded TypeScript runtime: isolate lifecycle, transpilation, eval,
namespace registry, schema discovery, Rust op bridge, cancellation, and runtime
limits.

It depends inward on the minimal contracts it needs from `lam-core`.

### `lam-redb`

The durable `StateStore` adapter and its conformance tests. It depends on
`lam-core`; `lam-core` does not depend on it.

### Later crates

Provider adapters, standard-library capability packs, and the TUI may become
separate crates when their boundaries are demonstrated. We will not create
empty crates for speculative boundaries now.

### TUI package and executable naming

**Settled.** The user-facing TUI executable will be named `lam`.

The Rust library remains the `lam` package and library crate. The future TUI
will live in a separately named Cargo package—provisionally `lam-tui`—with an
explicit binary target:

```toml
[package]
name = "lam-tui"

[[bin]]
name = "lam"
path = "src/main.rs"

[dependencies]
lam = { path = "../lam" }
```

Cargo package names must be unique, but a binary target can have the same name
as a library crate. The executable and library are distinct artifacts, and the
binary can import the library as `lam`. This does not require renaming
`lam-core`, `lam-deno`, or the public library.

If the packages are later published, users would install a package such as
`lam-tui` or `lam-cli` and receive an executable named `lam`. Whether to optimize
the crates.io command, a release installer, or package-manager distribution is
a packaging decision for the TUI slice; it should not couple TUI dependencies
into the core library now.

## Implementation slices

### Slice 0: documentation and workspace scaffold

**This document and the skeletal workspace.**

Acceptance:

- the settled architecture is recorded;
- uncertain details are marked rather than silently decided;
- all four initial crates build without implementation placeholders pretending
  to be stable APIs;
- formatting, metadata, and tests pass;
- Slice 1 is explicit enough to review before coding.

### Slice 1: persistent typed eval kernel

**Implemented.**

This slice validates the distinctive technical primitive without involving a
model provider, mailbox, scheduler pool, or durable database.

The Rust surface is `Isolate::builder()`, typed `Namespace` registration,
`eval`, and `eval_with`. The public `lam` crate re-exports that kernel while the
full actor-level `Lam` builder remains deferred.

The eval contract is:

- a cell is TypeScript transpiled with `deno_ast`, with imports and exports
  rejected;
- cells execute serially and successful cells share lexical and global state;
- top-level `await` works, and a final Promise is awaited even when the source
  did not spell `await`;
- a successful result is either `EvalValue::Undefined` or JSON;
- `console.debug`, `log`, `info`, `warn`, and `error` become structured entries
  on `EvalOutput`;
- non-JSON values and cycles produce `ResultNotSerializable`;
- JavaScript exceptions retain their native CDP details;
- typed builtin rejections are catchable as structured values in TypeScript and
  remain `BuiltinFailure` when unhandled.

Namespace functions take one typed input and return
`Future<Output = Result<O, E>>`. Serde performs the actual boundary conversion,
while `schemars` derives discoverable input, output, and error schemas from the
same Rust types. The kernel's only built-in namespace function is synchronous
`lam.dir`; embedding applications register everything else explicitly.

The runtime bootstrap is kernel-owned TypeScript and is the only checked-in
source for that layer. It is an embedded Deno extension ESM entry point,
transpiled through `RuntimeOptions::extension_transpiler` with the already
pinned `deno_ast` dependency. Extension state installs the Rust registry,
console buffer, and isolate generation before the ESM runs. The bootstrap calls
`op_lam_manifest` once to materialize every configured namespace, routes
ordinary functions through `op_lam_call`, implements `lam.dir()` against the
same manifest op, captures console output, and then removes the ambient `Deno`
bootstrap object. There is no manual `execute_script` injection, extension-
specific TypeScript shim, npm toolchain, build script, bundle, or generated
runtime file. A bare isolate has no filesystem, network, process, URL/fetch,
npm, or third-party framework runtime.

Each eval has a host timeout bounded by the builder's maximum. When it fires,
Lam interrupts V8, considers that isolate poisoned, drops it and pending Rust
op futures, constructs the next generation, and only then returns `TimedOut`.
The error explicitly reports that heap state was lost and already-completed
external side effects may remain. If replacement fails, the actor-facing layer
will be able to observe `RestartFailed` or `Poisoned` rather than continuing on
the interrupted heap.

Acceptance coverage proves TypeScript persistence and top-level await, absence
of ambient authority, Promise composition, typed error catch and propagation,
schema discovery, automatic materialization of a deeply nested application
namespace registered only in Rust, isolation across generations, timeout
restart, cancellation of a pending Rust builtin, namespace validation, and the
one-live-isolate-per-thread guard.

Explicitly out of scope:

- model APIs and the tool-calling loop;
- inboxes, steering, queueing, and `run_id`;
- `StateStore`, `redb`, and context compaction;
- filesystem, shell, HTTP, and subagents;
- the worker-pool scheduler;
- the final public `Lam` builder.

### Slice 2: append-only state model

Define context/inbox records, `MemStore`, the minimal `StateStore` atomic
surface, pure projections, compaction-marker lookup, and a backend conformance
suite. Prove steering/finalization race behavior with deterministic tests
before adding `redb`.

### Slice 3: actor and scripted model loop

Combine the eval kernel and memory store with a deterministic fake provider.
Implement runs, `send`, `call`, steering batches, queueing, text output,
structured output, and simple/streaming consumption.

The essential end-to-end test is:

```text
input → model requests eval → persistent TypeScript executes a typed builtin
      → model receives eval result → terminal typed output
```

### Slice 4: `redb` durability and recovery

Implement the ordered key schema and transaction boundaries in `lam-redb`.
Run the shared store suite, restart actors from disk, preserve admitted inbox
messages, rebuild context projections and compaction watermarks, and inject an
explicit resumption message.

### Slice 5: real provider codec

Implement one real provider end to end while retaining its native payloads.
Add token streaming, provider tracing, schema-constrained output, and compatible
context replay. The provider will be selected immediately before the slice so
we can use current API behavior.

### Slice 6: compaction strategies

Implement transparent threshold/model-switch compaction, a Lam summary plus
verbatim tail, emergency truncation, and provider-native compaction where the
selected adapter supports it. Verify that raw history remains queryable.

### Slice 7: scheduler and multiple actors

Introduce bounded actor residency, cross-actor steering, and scheduler limits.
The slice must first settle whether safe residency means one isolate thread per
slot or whether an upstream-supported activation mechanism permits multiple
independent isolates per thread. Only then add subagent construction.

### Slice 8: coding-agent capability pack

Build the initial filesystem/edit/shell namespaces with explicit policies and
prompt inventory. Approval mechanisms live at these capability boundaries.

### Follow-up: TUI

Build the TUI as a consumer of the `lam` library and emit the `lam` executable.
It uses the same `send`, `call`, history, and run-event streams as any other
embedding application. Token deltas remain ephemeral but are displayed
immediately for a responsive experience.

HTTP webhooks, monitors, agent-writable KV, content-addressed blobs, additional
providers, and isolate snapshots follow as independent reviewed slices.

## Testing strategy

### Determinism first

State-machine and race tests use a scripted provider, controlled boundaries,
and an in-memory store. They must not depend on timing sleeps or a real API.

### Shared conformance suites

Every `StateStore` implementation runs the same behavioral suite. Typed
namespace implementations should likewise be testable without a real model.

### Failure injection

Later storage/actor slices will inject interruption:

- after inbox admission but before delivery;
- after delivery selection but before context append;
- while a provider request is outstanding;
- after eval completion but before the next model request;
- while a compaction marker or terminal result is committed.

The expected behavior must be explicit: retry, resume, report outcome unknown,
or fail the actor. We will add durable machinery only when a test demonstrates
the fact needed for recovery.

### No network in core tests

The default workspace test suite should be deterministic and offline. Provider
contract tests requiring credentials or network access remain opt-in.

## Open decisions index

These questions are intentionally unresolved and assigned to slices:

- default stdlib capability profile — coding capability slice;
- exact atomic `StateStore` surface — Slice 2;
- context and inbox serialization format/versioning — Slice 2;
- final `Run<T>` Rust ergonomics — Slice 3;
- first real model provider — Slice 5;
- context thresholds and default compaction strategy — Slice 6;
- safe isolate activation, scheduler sizing, and actor residency policy —
  Slice 7.

## Working agreement

Before beginning a slice:

1. review its scope and open questions;
2. turn the acceptance list into concrete tests;
3. agree on any public types the slice introduces.

After completing a slice:

1. run formatting, linting, and relevant tests;
2. update this document with what implementation taught us;
3. record newly settled decisions and deliberately deferred work;
4. review the resulting API before beginning the next slice.

This keeps the implementation and the shared mental model synchronized while
allowing Lam to remain experimental.
