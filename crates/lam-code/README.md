# `lam-code`

Optional filesystem, editing, and shell capabilities for coding agents built
with [lam](../../README.md).

Installing a `CodingPack` adds familiar host operations without changing the
model's one-tool interface: the model still writes TypeScript for `eval`, and
the pack appears as typed `lam.fs`, `lam.edit`, and `lam.shell` namespaces.

## Basic use

```rust,ignore
use lam::Lam;
use lam_code::{CodingPack, FilesystemAccess, LocalCommandRunner};

let coding = CodingPack::builder("/path/to/project")
    .filesystem_access(FilesystemAccess::ReadWrite)
    .shell(LocalCommandRunner::default())
    .build()?;

let mut actor = Lam::builder(model)
    .namespaces(&coding)
    .build()
    .actor("coder")
    .build()
    .await?;
```

The pack is also usable directly with `lam_deno::Isolate::builder().namespaces`.

## Installed APIs

Capabilities are omitted when their policy disables them. The manifest and
`lam.dir()` always reflect the exact installed surface.

| Function | Purpose |
| --- | --- |
| `lam.fs.read` | Read a numbered, paginated UTF-8 file chunk |
| `lam.fs.list` | List sorted, paginated direct children |
| `lam.edit.apply` | Validate and apply a model-oriented multi-file patch |
| `lam.edit.write` | Create or completely replace a UTF-8 file |
| `lam.shell.run` | Run one command through the injected `CommandRunner` |

`FilesystemAccess::Disabled`, `ReadOnly`, and `ReadWrite` control which
filesystem/editing namespaces exist. Shell is absent unless a runner is
explicitly supplied.

## Filesystem reads

`lam.fs.read` returns bounded chunks with line numbers. The agent can request a
later starting line rather than pulling a large file into model context at
once. `lam.fs.list` uses lexical cursors and returns direct children in stable
order.

Paths are resolved beneath the configured project root. Symlink traversal and
path normalization are checked to prevent API-level escape from that root.
`ReadConfig` and `ListConfig` configure the associated limits.

## Editing

`lam.edit.apply` accepts the `*** Begin Patch` / `*** End Patch` grammar used by
leading coding agents. It supports add, update, move, and delete operations.
Every path and hunk is parsed and validated against the original filesystem
before the first mutation; overlapping parent/child targets are rejected.

If the underlying filesystem changes or fails during commit, the result reports
a partial commit explicitly rather than claiming rollback. The filesystem is
not transactional.

`lam.edit.write` is the simpler whole-file operation. Both edit functions are
absent in read-only mode.

## Command execution

`CommandRunner` is the application extension point:

```rust,ignore
pub trait CommandRunner: Send + Sync + 'static {
    fn run(&self, request: CommandRequest) -> CommandFuture;
}
```

The supplied `LocalCommandRunner` executes through the host shell. It supports
a bounded timeout, optional working directory, independent stdout/stderr
capture, cancellation, and process-tree termination on Unix.

Shell output keeps a bounded tail in the eval result. When a stream is larger,
the complete bytes spill to a pack-owned temporary file which can be paged with
`lam.fs.read` when that namespace is installed. Spill files live only as long
as the `CodingPack`.

`ShellConfig` sets command timeout limits; `CaptureConfig` bounds retained
stdout/stderr and spill behavior. An embedding can inject a remote/container
runner using exactly the same interface.

## Security boundary

The path rules, timeouts, limits, and process cleanup are useful guardrails,
not an operating-system sandbox. `LocalCommandRunner` inherits the authority of
the lam process. A model which can run arbitrary shell commands can generally
reach anything that process can reach.

For untrusted workloads, combine lam with an established OS/container sandbox
or provide a sandboxed `CommandRunner`. Interactive approvals are intentionally
deferred until a UI supplies a real approval consumer and actor/run identity is
available at the interception point.

See the public [`lam`](../lam/README.md) facade,
[`lam-agents`](../lam-agents/README.md) for per-child capability subsets, and
the repository [README](../../README.md).
