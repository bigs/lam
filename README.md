# Lam

Lam is an experimental Rust library for building durable, actor-style coding
agents around one model-visible primitive: evaluating TypeScript in an embedded,
persistent Deno isolate.

The repository is intentionally library-first. A TUI will eventually be built
on the same public API, but it is not part of the initial implementation.

The architecture, settled decisions, open questions, and incremental
implementation slices live in [docs/PLAN.md](docs/PLAN.md).

## Workspace

- `lam`: public facade and builder API
- `lam-core`: actor, model, context, mailbox, and storage abstractions
- `lam-deno`: embedded Deno isolate and typed builtin bridge
- `lam-redb`: durable `redb` state-store implementation

The future TUI will be distributed as an executable named `lam` from a
separately named workspace package, so it does not displace the `lam` library
crate.

The crates are currently scaffolds. The first implementation slice is defined
in the plan and will be reviewed before implementation begins.
