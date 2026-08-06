# AGENTS.md

`lam` is an experimental Rust workspace: a durable coding-agent library whose
one model-visible tool is `eval`, running TypeScript in a persistent Deno
isolate. The model interface, architecture decisions, and contracts are in
`README.md` and `docs/PLAN.md`; open tasks live in `docs/TODO.md`.

## Workspace

- Rust 2024 edition; resolver 3. Workspace crates live under `crates/`.
- `lam` is the single-actor facade; `lam-core` holds domain types and SPIs.
- `lam-deno` is the isolate; `lam-redb` the durable journal; `lam-openai` the
  provider adapters; `lam-agents` the multi-actor runtime; `lam-code` the
  coding capabilities; `lam-tui` produces the `lam-agent` binary.
- Keep the dependency boundary: `lam` stays a single-actor library; provider,
  persistence, coding, and multi-agent features are optional crates.

## Conventions

- Deno/V8-facing dependencies are pinned to exact versions in `Cargo.toml`;
  do not bump them casually. The first build is slow (embedded JS runtime).
- `unsafe_code` is denied workspace-wide; `missing_docs` is a warning. Public
  items get doc comments; journal/protocol formats stay append-only and
  backward-compatible.
- The eval tool is capability-limited: no imports, exports, npm, or ambient
  Node/Deno/process/network APIs. Do not suggest them to the model.

## Verification

Run from the workspace root before considering work complete:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

The default suite is deterministic and offline; live provider tests are
ignored unless explicitly enabled.

## House rules

- `.env` and `target/` are gitignored; never commit credentials or build
  artifacts. TUI provider keys live in `~/.lam/providers.toml`.
- Commit and push only when asked. Group related changes; write commit
  messages that name the crate or surface they touch.