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
the final expression is returned after awaiting it when it is a Promise. The
synchronous `lam.result(value)` identity helper makes that final value explicit
without introducing hidden result state or early-return semantics; bare final
expressions remain valid.

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
explicit resumption message before making the new runtime available. The
notice states that the isolate was reset and, when derivable from the last
native provider response, that an interrupted eval has an unknown outcome.
Freezing V8 isolates, perhaps at run boundaries, is an interesting future
optimization but is too fragile for the initial recovery contract.

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

**Settled and implemented.** `lam.dir()` exposes the available namespace tree. Its output
includes function names, documentation, input schemas, output schemas, error
schemas, and capability availability.

The default system prompt contains a compact inventory of every function
instantiated for the actor. Each line is derived from the same immutable
manifest and includes the callable path, a TypeScript-like input/output shape,
and the first paragraph of its Rust-authored documentation. The synopsis is a
deliberately lossy orientation aid; `lam.dir()` remains authoritative for full
documentation and exact input, output, and error schemas. The model therefore
does not need to begin every session with discovery.

The inventory must reflect the actor's actual capabilities. Disabled or
unauthorized modules must not be advertised as usable.

System instructions are runtime configuration rather than durable context.
The builder can append application instructions to the generated default with
`annotate_system_prompt`, or replace the default and inventory completely with
`system_prompt`. Annotations survive replacement and retain registration order,
making those operations order-independent. Provider codecs encode the one
logical prompt through their native instruction surface.

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

**Settled for Slice 2.** A durable message envelope contains:

- `message_id`;
- authenticated `MessageSource`;
- delivery mode;
- `EncodedPayload`;
- host-observed `received_at`.

**Settled.** `MessageSource` is a closed provenance enum:

- `User { principal: Option<PrincipalId> }`;
- `Host { component: ComponentId }`;
- `Actor { actor_id: ActorId }`.

`Host` includes Lam runtime messages and explicitly identified embedding
components. Provenance does not directly determine the provider-visible message
role. Lam assigns the source from the authenticated call path: ordinary
`send`/`call` input is `User`, trusted host APIs name their component, and
actor messaging records the authenticated sending actor. Model-authored code
cannot impersonate another actor or manufacture a trusted host source.

**Settled.** Recipient identity is not duplicated inside the stored envelope.
The actor journal key supplied to admission is authoritative. Public receipts
and exported history may pair `actor_id` with `message_id` and journal
revision, but a persisted event cannot disagree with the stream containing it.

**Settled.** The core envelope has no `in_reply_to` or `reply_to` field.
Steering means one model step may incorporate several messages, so Lam cannot
deterministically infer that an output answers exactly one input. The durable
core relation is `ContextTransition::Messages.consumed_message_ids`, which
records the entire batch actually inserted into context.

Applications and actor protocols that need request/reply correlation carry an
explicit request identifier in their structured payload. Lam may provide a
higher-level typed request/reply helper later, but the journal does not assign
that semantic relationship.

**Settled.** `received_at` is informational observability data. Lam obtains it
from an injectable clock before the first append attempt and preserves it
unchanged across conditional-append retries. Journal revision remains the sole
authority for admission order and correctness; projections never order or make
delivery decisions by wall-clock time. Sender-declared business timestamps
belong inside the structured payload.

OpenTelemetry trace propagation may be added in the telemetry-owning slice.
It is not part of the initial durable envelope; trace context remains advisory
and never becomes authoritative state.

### Encoded message payload

**Settled for Slice 2.** `EncodedPayload` contains a namespaced `CodecId`, an
integer codec version, and a `serde_json::Value`. `send<T: Serialize>` converts
the input once through the default `lam/json@1` codec before admission. If
serialization fails, no journal event is appended.

The stored JSON value is authoritative. It preserves every provider-native
field and opaque encrypted string, but not irrelevant wire formatting, object
key order, or numeric spelling. Computed codec views interpret this single
value; Lam does not store a second normalized copy. A raw-wire representation
can be introduced only if an actual provider demonstrates that byte identity,
rather than JSON-value identity, is required.

### Message kinds

**Settled direction.** The mailbox can carry user, host/runtime, and
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

**Settled for the state model.** A candidate terminal context append is
conditional on the actor-journal revision from which the decision was made. If
a message admission wins the race, it advances that revision, the completion
append conflicts, and the actor refolds before deciding whether to continue.
An eligible steer therefore continues the same run; a queued message may allow
the terminal append to be retried and then begins later work. The successful
terminal context append is the durable run-completion linearization point.

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

The ergonomic builder method may remain named `state_store`, but it accepts an
implementation of the lower-level `JournalStore` trait. There is no separate
public `StateStore` trait in the initial design.

### `send`

**Settled.**

```rust,ignore
let receipt = actor.send(value, DeliveryMode::Steer).await?;
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

**Settled for Slice 3.** `Actor` is a non-cloneable linear owner, while
`ActorRef` is a cloneable mailbox address exposing `send`. `Actor::call`
requires `&mut self` and returns a `Run<'_, T>` which holds that mutable borrow,
preventing overlapping calls through safe Rust while cloned references remain
available for steering.

`Run<T>` is both a `Future<Output = Result<T, _>>` and a stream of ephemeral
runtime events. Ignoring the stream never blocks the actor. Text is the default
output; `.output::<T>()` requires `DeserializeOwned + JsonSchema`. Runs start
when first polled. Dropping an unpolled run prevents it from starting; dropping
a started run detaches that observer without cancelling actor work. Until the
detached run finishes, another `call` reports that the actor is busy.

## Model-visible context

### Per-actor append-only stream

**Settled.** Each actor has an ordered, append-only context stream. This is the
valuable long-term conversation record and must be retained in full.

This is a logical stream projected from the actor's authoritative journal, not
a separately mutable store. Each `ContextAppended` event contributes one entry
in context order.

**Settled for Slice 2.** Every `ContextEntry` contains:

- one `ContextTransition`;
- `EncodedPayload`;
- host-observed `recorded_at`.

`recorded_at` is informational and comes from the same injectable clock policy
as message `received_at`. Actor identity is the journal key. Context sequence
is derived from `ContextAppended` order, and authorship is projected from
consumed messages or the payload codec. Provider/model identifiers remain in
native payload or codec metadata; trace identifiers remain telemetry.

### Context transitions

**Settled.** `ContextTransition` has four structurally valid variants:

- `Messages { run_id, consumed_message_ids }`;
- `Model { run_id, progress: Continue | Complete }`;
- `Eval { run_id }`;
- `Compaction { covers_through, run_id: Option<RunId> }`.

The unified transition prevents invalid kind/run combinations from being
constructed. A `Model { progress: Complete }` transition is the durable
run-completion linearization point.

`Messages` is one mailbox-ordered batch and requires the exact nonempty set of
currently eligible messages. `Model` retains untouched provider-native output,
including reasoning and the model's eval request. `Eval` is the model-visible
result of executing Lam's single tool. The request remains in the preceding
model payload; its evaluated outcome is a first-class context transition.
`Compaction` contributes a replacement view covering entries through the
specified context sequence.

`Messages` is the only transition that can open a run; later steering batches
continue that same run. `Model` and `Eval` require an already active run.
`Model` is the only initial transition that may complete a run, while `Eval` is
always continuing. A compaction marker may be outside a run or associated with
the already active run, but it cannot start or terminate one.

Lam deliberately has no generic `Tool`, `ToolCall`, or provider-role context
kind. Filesystem, shell, HTTP, subagent, and application functions are
capabilities invoked from TypeScript inside the one eval tool. Provider roles,
reasoning-item types, eval-call identifiers, and other wire details remain in
codec-tagged payloads and computed views.

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

Slice 2 uses the simplest correct implementation: its pure projection folds
the available history and remembers the newest compatible marker. Store-level
reverse seeks, watermarks, and other materializations remain deferred until
recovery measurements justify them.

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

**Settled.** The initial durable state is one authoritative append-only journal
per actor. It begins with exactly two domain event kinds:

1. `MessageAdmitted`, containing the durable message envelope;
2. `ContextAppended`, containing one model-visible `ContextEntry`. Its
   `Messages` transition carries any inbox message identifiers atomically
   consumed into that entry.

The pending inbox is the pure projection of admitted messages not yet consumed
by a context append. The append-only context stream is the projection of
`ContextAppended` events. A completing model transition records run completion,
and a compaction marker is a context transition rather than a separate storage
facility.

We are not starting with a generic actor event-sourcing framework, effects
ledger, CQRS bus, or transaction DSL. The event vocabulary is closed and
domain-specific. Additional durable facts must earn their place through a
recovery or product requirement.

### `JournalStore`

**Settled.** Storage is behind a public interface so embedders can provide their
own implementation. The sole initial backend SPI is named `JournalStore`.

The underlying persistence primitive is an ordered read plus an atomic,
conditional batch append at an expected actor revision. Lam owns event
semantics, folding, and retries; a backend owns ordered storage and the
compare-and-append guarantee.

`JournalStore` reads and appends `ActorEvent` directly. It is public so
embedders can provide a backend, but deliberately has no generic or associated
event language: Lam's actor journal is the one closed contract. `MemStore`
keeps these typed values directly without a pointless serialization round
trip. Durable and custom stores choose their physical Serde encoding
internally.

This is deliberately not a generic `append<T: Serialize>` method. Narrowing
the trait to Lam's actual event type prevents the storage seam from becoming a
premature event-sourcing framework while preserving custom backend support.

`ActorEvent` owns explicit schema/version compatibility, and its context
payload wrapper preserves provider-native data without requiring the journal
backend to understand it. Backends may serialize the event, but they do not
interpret `MessageEnvelope`, `ContextEntry`, or individual `ActorEvent`
variants. This prevents the backend trait from growing into a transaction DSL.

Higher-level actor-state operations remain concrete domain logic over a
`JournalStore`. We will introduce another public abstraction only if a real
embedding use case demonstrates that it is needed.

The interface must preserve:

- one isolated ordered journal per actor;
- ordered actor-local appends;
- admission-before-receipt for messages;
- consistent delivery progress;
- append-only context;
- the steering/finalization race semantics;
- isolation between system state and future agent-writable data.

The semantic surface is only ordered `read` and conditional `append`. Slice 2
uses return-position `impl Future + Send`, allowing implementations to write
ordinary `async fn` methods without an `async-trait` dependency or mandatory
heap allocation. Consumers use static dispatch for now. A type-erasing adapter
can be added later if a concrete embedding use case needs heterogeneous stores;
it will not change the storage semantics.

### Revisions and paging

**Settled.** `Revision::ZERO` represents an empty or nonexistent actor
journal. The first successful append at expected revision zero implicitly
creates it; there is no separate journal-creation operation.

`read(actor, after, limit)` treats `after` as exclusive and returns a
`JournalPage` containing contiguous `StoredEvent` values plus the journal head
observed in the same consistent store view. Every stored event carries its own
revision. The caller-provided nonzero event limit lets Lam bound recovery
memory independently of backend configuration.

An atomic batch of `N` events appended at revision `R` receives consecutive
revisions `R + 1` through `R + N`, and success returns the new head. The events
remain immutable after commit. Reads do not hold one backend snapshot open
across pages: a projection may observe a later head while catching up, and its
subsequent conditional append detects any writer that intervened.

### Append outcomes and errors

**Settled.** A compare-and-append conflict is ordinary concurrency control, not
a backend failure. `append` returns either `Appended { head }` or
`Conflict { expected, actual }`. Domain logic refolds and retries after a
conflict.

An `EventBatch` contains one required first event and zero or more remaining
events, making an empty append unrepresentable. Every `JournalStore` supplies
an associated backend error implementing `Error + Send + Sync + 'static`;
`MemStore` uses `Infallible`.

The common journal error wrapper has only `Backend(error)` and
`RevisionExhausted`. Invalid ordering, gaps, or inconsistent page metadata are
store-contract violations detected by `lam-core` while folding, rather than
variants every backend must manufacture.

### Implementations

**Settled.**

- `MemStore` is the default pure-Rust `JournalStore`
  implementation and stores typed values directly.
- `lam-redb` is the first durable `JournalStore` implementation and performs
  Serde encoding behind the trait boundary.
- Custom implementations are first-class and can enable `lam-core`'s dedicated
  `test-support` feature to run the shared storage conformance suite.

`redb`'s ordered tables and transactions are a good match for per-actor
journals. Its exact schema belongs to the durable-adapter slice.

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

**Settled for Slice 3.** The initial single-actor runner owns one dedicated
system thread with a current-thread async runtime, actor projection, model loop,
and persistent isolate. Callers interact through `Actor` and `ActorRef`; the
thread assignment is not part of their public semantics.

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
- per-request token usage and optional cost metadata;
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

Actor-wide lifecycle events are separate from a correlated run's events.
`Actor::take_runtime_events` returns a buffered, single-consumer stream; this
allows `RuntimeEvent::RuntimeResumed` to remain observable even though its
authoritative notice is admitted before `ActorBuilder::build` returns.

### No initial effects ledger

**Settled.** We will not initially persist a third stream recording every
external effect or `ModelStepStarted`/`Completed` event. The recovery semantics
for ambiguous in-flight external effects are real, but an effects ledger adds
substantial machinery before we have a demonstrated need.

Lifecycle information otherwise exists as runtime events and OpenTelemetry
spans. Recovery does not need a third ledger: the minimum authoritative fact is
a structured `lam/system-notice@1` mailbox message derived from the durable
context when a fresh runtime starts.

Approval behavior belongs primarily in capability implementations and their
TypeScript/Rust bridge rather than in a universal event-ledger abstraction.

### OpenTelemetry

**Settled.** Runs, model calls, evals, builtins, compactions, and message
delivery should produce OpenTelemetry spans from the Rust runtime. A future
TypeScript library may create child spans through an explicitly registered
capability, but tracing does not require a framework inside the isolate.

Trace identifiers are never authoritative state. Durable IDs such as
`message_id`, `actor_id`, consumed-message references, context sequence, and
`run_id` retain their meaning when tracing is disabled or sampled. Explicit
application protocol identifiers do as well.

Completed model calls also produce a best-effort metadata view for runtime
events and tracing: provider-reported model identity, normalized token counts,
the untouched native usage object, and optional cost. Usage extraction cannot
fail or change a run. Cost is provider-reported when available or explicitly
marked as an estimate derived from embedding-supplied rates; Lam does not bake a
mutable provider price catalog into core state.

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

**Settled control-flow decomposition for Slice 3.**

- A model provider performs requests and exposes native response streams.
- A codec converts Lam-native entries to provider input and recognizes
  compatible provider-native entries.
- An output contract describes text or structured completion.
- A compactor produces an append-only marker compatible with a target codec.

The abstraction must be provider-neutral at the control-flow level while
remaining provider-lossless at the data level.

The codec is pure: it encodes current context, runtime system instructions, and
an output contract into one native request payload, then interprets the
completed native response as either one eval request or an output candidate.
The provider performs the external request and returns the untouched completed
native payload. Only that payload is durable; the system instructions are
configuration and the interpreted directive is a recomputable view.

A model response may request exactly one eval. Sequential and parallel work
belong inside that single TypeScript program, including `Promise.all` when
appropriate. Provider parallel-tool-call controls are disabled where available;
multiple sibling eval calls are a protocol error.

The native model response is appended before Lam acts on it. An eval response
is recorded as a continuing model entry, then the program executes exactly once
within that live actor attempt. Its complete `lam/eval@1` success or failure is
held in memory across compare-and-append retries and appended before steering
messages enter context. CAS retries never rerun a completed provider request or
eval.

An output is only a terminal candidate. If a queued message wins the append
race, Lam refolds and retries completion. If a steering message wins, Lam
appends the same native response as continuing, consumes the steer, and makes
another provider request. A process crash may repeat inference. A crash after
an effecting eval begins but before its result is durable remains outcome
unknown; Slice 3 adds no effects ledger or exactly-once claim.

Initial actor-loop tests will use a deterministic scripted provider. Real
OpenAI or Anthropic integration should not be needed to prove mailbox,
context, eval, or run semantics.

## Workspace structure

The workspace currently contains:

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
    ├── lam-openai/
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

The durable `JournalStore` adapter and its conformance tests. It depends on
`lam-core`; `lam-core` does not depend on it.

### `lam-openai`

The first real model-adapter crate. It contains distinct public builders for
OpenAI's Responses API and the generic OpenAI-compatible Chat Completions
protocol. They share only HTTP, SSE, and Lam-native context helpers; each keeps
its own wire contract and replay rules. The crate depends on the public `lam`
facade so its builders can return a ready-to-use `Model`.

### Later crates

Additional provider families, standard-library capability packs, and the TUI
may become separate crates when their boundaries are demonstrated. We will not
create empty crates for speculative boundaries now.

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
- `EvalOutput.result` is either `EvalValue::Undefined` or JSON;
- `console.debug`, `log`, `info`, `warn`, and `error` become ordered entries in
  `EvalOutput.logs`; JSON-compatible arguments retain their structure and
  position, while unsupported values receive a textual fallback;
- console capture is enabled by default and can be disabled with
  `IsolateBuilder::capture_console` without removing the JavaScript global;
- non-JSON values and cycles produce `ResultNotSerializable`;
- JavaScript exceptions retain their native CDP details;
- typed builtin rejections are catchable as structured values in TypeScript and
  remain `BuiltinFailure` when unhandled.

Namespace functions take one typed input and return
`Future<Output = Result<O, E>>`. Serde performs the actual boundary conversion,
while `schemars` derives discoverable input, output, and error schemas from the
same Rust types. The kernel's only built-in namespace functions are synchronous
`lam.dir` and `lam.result`; embedding applications register everything else
explicitly.

The runtime bootstrap is kernel-owned TypeScript and is the only checked-in
source for that layer. It is an embedded Deno extension ESM entry point,
transpiled through `RuntimeOptions::extension_transpiler` with the already
pinned `deno_ast` dependency. Extension state installs the Rust registry,
console buffer, and isolate generation before the ESM runs. The bootstrap calls
`op_lam_manifest` once to materialize every configured namespace, routes
ordinary functions through `op_lam_call`, implements synchronous `lam.dir()`
and `lam.result()` facades, captures console output, and then removes the
ambient `Deno` bootstrap object. There is no manual `execute_script` injection,
extension-specific TypeScript shim, npm toolchain, build script, bundle, or
generated runtime file. A bare isolate has no filesystem, network, process,
URL/fetch, npm, or third-party framework runtime.

Each eval has a host timeout bounded by the builder's maximum. When it fires,
Lam interrupts V8, considers that isolate poisoned, drops it and pending Rust
op futures, constructs the next generation, and only then returns `TimedOut`.
The variant guarantees that heap state was lost and already-completed external
side effects may remain; these invariants are not duplicated as boolean
fields. `TimedOut` carries both generations and means replacement succeeded.
If replacement fails, the actor-facing layer will observe `RestartFailed` or
`Poisoned` rather than continuing on the interrupted heap.

Acceptance coverage proves TypeScript persistence and top-level await, absence
of ambient authority, Promise composition, typed error catch and propagation,
schema discovery, automatic materialization of a deeply nested application
namespace registered only in Rust, isolation across generations, timeout
restart, cancellation of a pending Rust builtin, namespace validation, and the
one-live-isolate-per-thread guard.

Explicitly out of scope:

- model APIs and the tool-calling loop;
- inboxes, steering, queueing, and `run_id`;
- `JournalStore`, `redb`, and context compaction;
- filesystem, shell, HTTP, and subagents;
- the worker-pool scheduler;
- the final public `Lam` builder.

### Slice 2: append-only state model

**Implemented; pending API review.**

Implement one authoritative journal per actor with the two initial event kinds
`MessageAdmitted` and `ContextAppended`. Define the message envelope, context
entry, identifiers, revisions, codec-tagged JSON payload wrapper, and the
minimal public ordered-read/conditional-append storage contract.
`JournalStore` operates directly on Lam's closed `ActorEvent` language.

Implement `MemStore` as the pure-Rust reference backend. Build pure projections
for pending inbox order, full context history, run completion, and the newest
compatible compaction marker. Admission-ordered messages and their consumption
status are the mailbox source of truth; pending and eligible messages are
derived views. `ContextTransition` encodes the valid run/kind combinations,
and a message transition consumes its source identifiers atomically. No
separate mutable delivery record exists.

The feature-gated storage conformance suite proves ordered isolation between
actors, conditional-append conflicts, atomic batches, and paging. Projection
tests separately prove admission-before-receipt, atomic message consumption,
deterministic replay, and the steering/finalization race. `redb`, snapshots,
store-specific indexes, blobs, generic event-sourcing machinery, and the model
loop remain out of scope.

### Slice 3: actor and scripted model loop

**Implemented.**

The eval kernel and memory store are combined behind provider-neutral model and
codec contracts, with deterministic scripted-provider coverage. Runs, durable
`send`, linear `call`, steering batches, queueing, text output, structured
output, and simple/streaming consumption are implemented. Provider-native
responses are appended before their directives are acted upon, and eval
outcomes use the `lam/eval@1` context codec.

The slice has one actor and one dedicated runner thread. It does not include
subagents, actor-to-actor routing, child lifecycle, or the eventual scheduler.
`Actor` is linear for calls; cloneable `ActorRef` values remain available for
mailbox delivery. Dropping a started run only detaches its consumer.

The essential end-to-end test is:

```text
input → model requests eval → persistent TypeScript executes a typed builtin
      → model receives eval result → terminal typed output
```

### Slice 4: `redb` durability and recovery

#### Slice 4A: durable journal adapter

**Implemented.**

`lam-redb` implements `JournalStore` with versioned actor-head and
revision-addressed event tables. One redb write transaction compares the
actor-local head, appends a consecutive event batch, and advances the head.
Read transactions observe the head and bounded event page from one snapshot.
Actor events use their versioned Serde JSON representation as the authoritative
stored value.

The shared store suite covers the backend contract. A close/reopen test rebuilds
the same projection from bounded pages and proves that pending inbox messages,
completed runs, compaction watermarks, and raw context survive.

#### Slice 4B: actor recovery policy

**Implemented.**

Actor startup builds a fresh isolate, folds the complete actor journal, and
then admits one host-authored `lam/system-notice@1` message before returning the
actor. A never-before-used actor receives no notice. The structured
`runtimeResumed` payload records `isolateState: "reset"`, the active run when
one exists, and `interruptedEvalOutcome: "unknown"` only when the last native
model response can be interpreted as an eval request without a following
durable eval result. Its message ID identifies that runtime occurrence.

Admission and activation are deliberately separate:

- an active run receives the notice as `Steer` and wakes;
- pending substantive mail receives a queued notice and wakes;
- quiescent history receives a queued notice without waking;
- pending resumption notices alone never start a model run;
- a new `call` is admitted before notice-only mail is processed, so both enter
  context in one batch.

A startup activation continues across run boundaries until no substantive
durable work remains. This prevents queued mail from being stranded when
recovery must first finish an already-active run.

The durable notice is the source of truth. A matching buffered
`RuntimeEvent::RuntimeResumed` gives a TUI an immediate display event. Real
provider codecs should represent the notice with a native system/developer
construct where appropriate, or use clearly delimited fallback markup such as
`<lam_system_notice>`; eval results remain distinct `lam/eval@1` context items.

`Actor::shutdown` consumes the linear owner, waits for the current command,
and joins the dedicated thread without discarding pending mail. `Actor::abort`
instead signals out of band, drops a cancellable provider future, interrupts
active V8 execution, and joins the thread. A separately cloneable
`AbortHandle` carries that explicit kill authority while `ActorRef` remains a
send-only mailbox address. Abort does not roll back host effects, append a
shutdown marker, or fabricate an eval result. A missing result is reported as
outcome unknown after restart. Process-local `call` waiters are not recoverable
after process death, so autonomously recovered work uses the ordinary text
output contract. Durable attachable jobs with persisted output contracts remain
a possible future API.

### Slice 5: real provider codec

**Implemented and live-validated for both protocols.**

`lam-openai` implements both OpenAI's Responses API and the broadly supported
OpenAI-compatible Chat Completions protocol. They are separate public builders
rather than one mode flag, but share a small `reqwest`/SSE transport. Both
builders support an embedding-provided HTTP client, configurable base URL,
bearer authentication, and an extra JSON request object for provider-specific
settings. Lam overwrites the fields required for its control-flow invariants.

The Responses adapter is deliberately stateless:

- every request sets `store: false` and manually replays the in-scope context;
- `reasoning.encrypted_content` is always requested and every native response
  `output` item is replayed unchanged;
- the completed provider response object is retained untouched inside a small
  Lam envelope carrying only requested model and output-kind metadata;
- `parallel_tool_calls` is false and the only declared function is `eval`;
- structured outputs use the Responses `text.format` JSON Schema contract.

The Chat Completions adapter targets compatible providers such as Fireworks:

- its base URL is configurable; Fireworks uses
  `https://api.fireworks.ai/inference/v1`;
- provider extensions such as `reasoning_effort` and `reasoning_history` pass
  through `extra_body`;
- every native SSE JSON chunk is the authoritative stored response because a
  streaming Chat Completions call has no separate completed response object;
- the assistant-message replay is a computed view over those chunks and keeps
  `reasoning_content`, encrypted/signature fields, indexed reasoning details,
  tool calls, and unknown extension fields rather than decoding through a
  closed common message type;
- non-streaming JSON returned by a nominally compatible endpoint is also
  accepted and preserved losslessly;
- structured outputs use the compatible `response_format.json_schema`
  contract.

Both protocols project visible text/reasoning deltas into ephemeral
`ModelDelta` events, preserve only completed native payloads durably, and emit
Rust `tracing` spans with HTTP status and provider request ID when available.
Their codecs compute a non-authoritative `ModelResponseMetadata` view from each
completed native response. `RunEvent::ModelCompleted` and a Rust tracing event
expose model identity, input/cached-input/output/reasoning/total token counts,
the untouched native usage object, and optional USD cost. Cost estimates require
embedding-supplied per-model prices and are labeled `estimated`, avoiding a
stale built-in price catalog. Chat Completions asks for streaming usage by
default through `stream_options.include_usage`, with an explicit compatibility
opt-out; Fireworks also includes usage in its final streaming chunk.
A resumed run with a pending eval call projects the durable
`runtimeResumed`/unknown-outcome notice into the required provider tool-result
slot and also presents the distinct, delimited system notice to the model.

Offline tests cover exact request bodies, encrypted and plaintext reasoning
replay, unknown Chat Completions extension fields, schema-constrained output,
SSE framing, usage/cost projection, downstream model-completion events, and
complete two-request Lam actor/eval loops for both protocols.
Explicitly ignored live tests validate a plain completion plus a real
Rust-backed directory-list/count eval loop. On 2026-08-01 they passed against
OpenAI Responses with `gpt-5-mini` (resolved to
`gpt-5-mini-2025-08-07`) and Fireworks Chat Completions with
`accounts/fireworks/models/deepseek-v4-flash-0731`. Both returned native usage,
including cached-input detail where applicable, and the configured price view
produced bounded USD estimates. These tests validate current provider behavior;
secrets and network-dependent tests remain outside the default suite.

### Slice 5B: model instructions and manifest synopsis

**Implemented.**

The actor supplies one concise, provider-neutral system prompt on every model
request. Its default is deliberately minimal:

```text
You are a coding agent with one tool, `eval`, which runs TypeScript in a persistent Deno isolate. Use registered APIs for host interaction and `lam.dir()` for their complete documentation and schemas.

Available APIs:
{manifest-derived synopsis}
```

There are no examples or generic tool-loop exhortations. The `eval` declaration
itself explains top-level await, persistent top-level state, final-expression
results, registered APIs, sequencing in one program, and `Promise.all` for
independent work.

The API synopsis is generated after isolate construction from precisely the
installed registry. Ordinary functions are shown as
`path(input: Shape): Promise<Output>` followed by the first documentation
paragraph. `lam.dir()` has its synchronous optional-query signature. Full
schemas, typed errors, and remaining documentation stay available through
`lam.dir()` rather than bloating every request. The synopsis also advertises
`lam.result<T extends JsonValue>(value: T): T`, so models can make their final
eval value explicit without an example in the system prompt.

`LamBuilder::annotate_system_prompt` appends embedding-specific instructions to
the generated default. `LamBuilder::system_prompt` replaces the default and API
inventory completely; annotations remain ordered regardless of when the
replacement is configured. The logical prompt is not appended to durable actor
context. The Responses codec sends it as top-level `instructions`; Chat
Completions prepends one `system` message, combining the prompt with any
structured-output instruction when necessary. Provider-specific `extra_body`
cannot override these runtime-owned fields.

### Slice 5C: explicit eval results and structured console capture

**Implemented.**

The persistent TypeScript runtime exposes `lam.result(value)` as a synchronous
identity function. It is manifest-described, appears in the generated API
synopsis, and is intended as the final expression of an eval. It neither stores
mutable state nor changes control flow, and existing bare final expressions
continue to work.

Successful `EvalOutput` values have two distinct surfaces: `result` contains
the final JSON value (or `undefined`), while `logs` contains `console.debug`,
`log`, `info`, `warn`, and `error` calls in emission order. Each log retains its
level and ordered argument array. JSON-compatible arguments cross structurally;
arguments which JSON cannot represent degrade individually to safe text rather
than flattening the whole call.

Console capture defaults on. `IsolateBuilder::capture_console` and
`LamBuilder::capture_console` can disable collection for an embedding, while
the familiar JavaScript `console` global remains callable and its entries are
discarded. The eval tool description briefly identifies `lam.result`; the
manifest remains the authoritative documentation surface.

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

Every `JournalStore` implementation can enable the dedicated `test-support`
feature and run the same storage-level behavioral suite. Actor projection and
race semantics remain ordinary `lam-core` tests rather than part of the backend
SPI. Typed namespace implementations should likewise be testable without a
real model.

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
- `JournalStore` async and object-erasure mechanics — Slice 2;
- context and inbox serialization format/versioning — Slice 2;
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
