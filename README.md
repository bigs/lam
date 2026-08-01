# Lam

Lam is an experimental Rust library for building durable, actor-style coding
agents around one model-visible primitive: evaluating TypeScript in an embedded,
persistent Deno isolate.

The repository is intentionally library-first. A TUI will eventually be built
on the same public API, but it is not part of the initial implementation.

The architecture, settled decisions, open questions, and incremental
implementation slices live in [docs/PLAN.md](docs/PLAN.md).

## Current kernel

The first three implementation slices are usable through the public `lam` crate.
They provide:

- persistent, serial TypeScript cells with top-level `await`;
- ordinary JavaScript `Promise` values for asynchronous Rust builtins;
- typed Rust namespace registration with inferred JSON Schemas;
- synchronous capability discovery through `lam.dir()`;
- JSON-only results and structured console capture;
- host-bounded timeouts that discard and replace a poisoned isolate;
- no ambient filesystem, process, network, or `Deno` authority;
- one append-only, actor-local journal containing admitted messages and
  model-visible context;
- a pluggable typed `JournalStore` contract and pure-Rust `MemStore`;
- pure projections for pending delivery, context history, run completion, and
  compaction markers;
- compare-and-append semantics that deterministically resolve steering against
  run finalization;
- provider-neutral model and codec contracts that preserve native response
  payloads in context before acting on them;
- a dedicated single-actor runner with durable `send`, linear `call`, steering,
  and queueing semantics; and
- detachable run-event streams plus text and JSON Schema-derived structured
  outputs.

The isolate bootstrap is kernel-owned TypeScript installed through Deno's
extension lifecycle. At startup it reads a Rust-generated builtin manifest,
materializes every configured namespace and function, and routes calls through
one generic Rust op. Extension authors only use `Namespace::function`; they do
not maintain TypeScript shims. The bootstrap is transpiled by the same pinned
`deno_ast` used for eval cells, so Lam has no npm install, JavaScript bundling
step, checked-in generated runtime, or dependency on Effect.

```rust,ignore
use lam::{Isolate, Namespace, Never};

let math = Namespace::new("acme.math", "Application arithmetic.").function(
    "double",
    "Doubles an integer.",
    |_context, input: i64| async move { Ok::<_, Never>(input * 2) },
);

let mut isolate = Isolate::builder().namespace(math).build().await?;
let output = isolate.eval("await acme.math.double(21)").await?;
```

For now, a system thread may own only one live Lam isolate. The builder rejects
a second isolate on the same thread; the scheduler design will revisit
residency after we establish a safe lifecycle contract with `rusty_v8`.

## Workspace

- `lam`: public facade and single-actor model runner
- `lam-core`: actor journal, mailbox, context, and storage contracts
- `lam-deno`: embedded Deno isolate and typed builtin bridge
- `lam-redb`: reserved for the future durable `redb` adapter

The future TUI will be distributed as an executable named `lam` from a
separately named workspace package, so it does not displace the `lam` library
crate.
