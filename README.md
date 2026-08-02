# Lam

Lam is an experimental Rust library for building durable, actor-style coding
agents around one model-visible primitive: evaluating TypeScript in an embedded,
persistent Deno isolate.

The repository is intentionally library-first. A TUI will eventually be built
on the same public API, but it is not part of the initial implementation.

The architecture, settled decisions, open questions, and incremental
implementation slices live in [docs/PLAN.md](docs/PLAN.md).

## Current kernel

The implemented slices are usable through the public `lam` crate and its
adapter crates. They provide:

- persistent, serial TypeScript cells with top-level `await`;
- ordinary JavaScript `Promise` values for asynchronous Rust builtins;
- typed Rust namespace registration with inferred JSON Schemas;
- synchronous capability discovery through `lam.dir()`;
- a compact manifest-derived default system prompt, with builder methods to
  replace it or append application instructions;
- JSON-only final results, including the explicit `lam.result(value)` helper;
- ordered, structured `console` capture which can be disabled by the builder;
- host-bounded timeouts that discard and replace a poisoned isolate;
- no ambient filesystem, process, network, or `Deno` authority;
- one append-only, actor-local journal containing model selection, admitted
  messages, and model-visible context;
- a pluggable typed `JournalStore` contract and pure-Rust `MemStore`;
- pure projections for pending delivery, context history, run completion, and
  compaction markers;
- compare-and-append semantics that deterministically resolve steering against
  run finalization;
- provider-neutral model and codec contracts that preserve native response
  payloads in context before acting on them;
- a dedicated single-actor runner with durable `send`, linear `call`, steering,
  and queueing semantics;
- detachable run-event streams plus text and JSON Schema-derived structured
  outputs;
- restart recovery with a durable, model-visible isolate-reset notice;
- configurable 90%-by-default context compaction with a model-generated
  summary plus exact tail, deterministic truncation, manual triggering, and a
  public `Compactor` extension point;
- append-only compaction records containing the raw source response, an
  optional neutral artifact, the exact replay checkpoint, and usage/cost
  metadata;
- a heterogeneous model registry with durable selection, compact-by-default
  switching, an explicit context-reuse policy, and atomic checkpoint/selection
  commits;
- an optional bounded `lam-agents` executor pool whose manifest-generated
  `lam.agents` capability creates hierarchically addressed child actors with
  explicit model, namespace, prompt, and depth policy;
- a pure-Rust `redb` journal backend; and
- lossless OpenAI Responses and OpenAI-compatible Chat Completions adapters,
  including explicitly configured native OpenAI Responses compaction, with
  token/reasoning streaming, native usage preservation, normalized usage
  events, and opt-in cost estimates.

The isolate bootstrap is kernel-owned TypeScript installed through Deno's
extension lifecycle. At startup it reads a Rust-generated builtin manifest,
materializes every configured namespace and function, and routes calls through
one generic Rust op. Extension authors only use `Namespace::function`; they do
not maintain TypeScript shims. The bootstrap is transpiled by the same pinned
`deno_ast` used for eval cells, so Lam has no npm install, JavaScript bundling
step, checked-in generated runtime, or dependency on Effect.

The actor renders its model instructions from the same builtin manifest. The
default names every installed function with its inferred input/output shape and
the first paragraph of its Rust-authored documentation; `lam.dir()` retains the
complete schemas and documentation. Embeddings can append focused instructions
with `annotate_system_prompt` or replace the generated prompt entirely with
`system_prompt`.

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

OpenAI Responses is stateless by construction: Lam always sends `store: false`,
requests encrypted reasoning, and manually replays complete native output
items:

```rust,ignore
use lam::Lam;
use lam_openai::responses::Responses;

let model = Responses::builder("gpt-5.6-luna")
    .api_key(std::env::var("OPENAI_API_KEY")?)
    .build()?;
let mut actor = Lam::builder(model)
    .context_window_tokens(128_000)
    .annotate_system_prompt("Work only inside the configured project root.")
    .build()
    .actor("main")
    .build()
    .await?;
let answer: String = actor.call("Inspect this project").await?;
```

Models are registered under stable host-defined identities. Every new actor
writes its initial `ModelSelected` event before any messages; reopening resolves
that durable identity against the supplied registry and rejects missing or
rebound entries. Switching compacts the complete effective history by default,
then atomically installs the target-compatible checkpoint and selection.
Compatible histories may opt into a preflighted reuse instead:

```rust,ignore
use lam::ModelSwitchPolicy;

let receipt = actor.switch_model("fast").await?;
let receipt = actor
    .switch_model_with_policy("deep", ModelSwitchPolicy::ReuseContext)
    .await?;
```

OpenAI native compaction is opt-in and composes with the universal summary-tail
fallback without a capability table or model-name heuristic:

```rust,ignore
use lam::{FallbackCompactor, Lam, SummaryTailCompactor};
use lam_openai::responses::{OpenAiResponsesCompactor, Responses};

let model = Responses::builder("gpt-5.6-luna")
    .api_key(std::env::var("OPENAI_API_KEY")?)
    .build()?;
let compactor = FallbackCompactor::new(
    OpenAiResponsesCompactor::new(&model),
    SummaryTailCompactor::new(model.clone()),
);
let runtime = Lam::builder(model).compactor(compactor).build();
```

The separate Chat Completions builder accepts compatible provider extensions
without narrowing their response messages. For Fireworks:

```rust,ignore
use lam_openai::{ModelPricing, chat_completions::ChatCompletions};
use serde_json::json;

let model = ChatCompletions::builder("accounts/fireworks/models/MODEL_ID")
    .api_key(std::env::var("FIREWORKS_API_KEY")?)
    .base_url("https://api.fireworks.ai/inference/v1")
    .pricing(ModelPricing::new(0.14, 0.28).cached_input(0.028))
    .extra_body(json!({
        "reasoning_effort": "high",
        "reasoning_history": "preserved"
    }))
    .build()?;
```

Every completed model request emits `RunEvent::ModelCompleted` with the model,
normalized input/cached-input/output/reasoning/total token counts, the untouched
provider usage object, and an optional cost. The same fields are emitted through
Rust `tracing`. Costs are marked `estimated` and appear only when the embedding
supplies current USD-per-million-token rates; Lam deliberately does not ship a
price catalog that can silently become stale. Chat Completions requests
`stream_options.include_usage` by default and can disable it for a compatible
server that rejects the option.

Lam isolates are thread-affine and deliberately not `Send`, but a local runtime
may host more than one. Each V8 isolate is parked between future polls and
entered only while Lam polls or tears down its kernel. Asynchronous evals can
therefore interleave on one system thread; synchronous CPU-bound JavaScript
occupies that thread until it returns or its watchdog interrupts it.

`lam-agents` keeps that scheduling policy optional. A root and its children can
share a bounded executor pool while retaining independent isolates, journals,
and mailboxes. Child models use direct provider/model identity, and requested
namespace strings are exact manifest paths rather than profile aliases:

```rust,ignore
use lam::{Lam, MemStore};
use lam_agents::{AgentSystem, SubagentConfig};

let system = AgentSystem::builder(MemStore::new())
    .worker_threads(2)
    .max_agents(16)
    .build()?;
let children: SubagentConfig<MemStore> = SubagentConfig::builder(model.clone())
    .namespace(read_only_files)
    .required_instructions("Stay within the configured project root.")
    .max_depth(2)
    .build()?;
let root = system
    .host_with_subagents(
        Lam::builder(model)
            .state_store(system.state_store())
            .build()
            .actor("/root"),
        children,
    )
    .await?;
let answer = root.call("Delegate a read-only investigation").await?;
system.shutdown().await?;
```

Every hosted actor has a canonical Unix-style address. `spawn` requires one
child-name segment and derives an address such as `/root/researcher`; an
existing or previously durable child address is never silently reused. It
returns only after the child's initial, always-steering task is durable.
`lam.agents.identity()` returns the current address and parent, while
`lam.agents.list()` lists direct resident children of the current actor (or of
an explicit `path`). `lam.agents.send({ to, message })` routes to any resident
address in the same system, durably records authenticated actor provenance,
and steers an active recipient run. Completion and wait APIs remain separate
follow-up work.

## Workspace

- `lam`: public facade and single-actor model runner
- `lam-agents`: bounded multi-actor scheduler and subagent capability pack
- `lam-core`: actor journal, mailbox, context, and storage contracts
- `lam-deno`: embedded Deno isolate and typed builtin bridge
- `lam-openai`: Responses and compatible Chat Completions providers/codecs
- `lam-redb`: durable `JournalStore` adapter with versioned redb tables

The future TUI will be distributed as an executable named `lam` from a
separately named workspace package, so it does not displace the `lam` library
crate.
