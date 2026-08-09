# `lam-agents`

Optional bounded multi-actor scheduling and subagent capabilities for
[lam](../../README.md).

The public `lam` crate intentionally remains a straightforward single-actor
library. `lam-agents` adds a fixed pool of current-thread executors, several
parked thread-affine isolates per worker, canonical actor addressing, and a
manifest-generated `lam.agents` namespace.

## Hosting a root

```rust,ignore
use lam::{Lam, MemStore};
use lam_agents::{AgentSystem, SubagentConfig};

let system = AgentSystem::builder(MemStore::new())
    .worker_threads(2)
    .max_agents(16)
    .build()?;

let children: SubagentConfig<MemStore> = SubagentConfig::builder(model.clone(), "high")
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
system.wait().await?;
system.shutdown().await?;
```

`host` starts an ordinary root without the subagent namespace.
`host_with_subagents` derives the authoritative sender identity from the actor
builder and installs the configured child policy.

## Actor addresses

Every hosted actor has one canonical Unix-style path:

```text
/root
/root/researcher
/root/researcher/parser
```

Spawn requests supply exactly one child-name segment. A live or previously
durable address is never silently reused. `list()` without an argument returns
the current actor's direct resident children; an explicit path lists that
namespace's direct children.

## Model-visible API

The exact installed functions are manifest-discoverable through `lam.dir()`:

| Function | Semantics |
| --- | --- |
| `lam.agents.identity()` | Return the current address and automatic parent |
| `lam.agents.list(request?)` | List direct resident children |
| `lam.agents.spawn(request)` | Create a detached persistent child and return after initial task admission |
| `lam.agents.wait({ addresses })` | Await spawned direct children without steering them |
| `lam.agents.call(request)` | Create a persistent child and wait directly for its initial task outcome |
| `lam.agents.send({ to, message })` | Durably send an authenticated steering message to any resident address |
| `lam.agents.stop({ address })` | Stop one direct child and its descendants, waiting for residency release |

Every child request must select an allowed `{ provider, model }` and `effort`,
along with any replacement system prompt, appended instructions, and exact
subset of registered namespace paths. `SubagentConfig` registers fixed
model/effort combinations plus namespaces, host-required instructions, nesting
depth, and eval limits. There is no implicit spawn/call model default.

Unless the user or project instructions request another combination, agents
should pass the selection from `lam.dir({ path: "lam" })` when the embedding
reports one with an effort and `lam.agents.models()` lists that combination.
Otherwise they choose a model and effort explicitly from that catalog.

## `call` and `spawn`

Both operations admit an actor-sourced, always-steering initial task and use the
same tagged `AgentOutcome`:

- `completed` carries the child address, message ID, and terminal text;
- `failed` carries the address, message ID, and error;
- `cancelled` carries the address, message ID, and optional reason.

`call` parks the parent's eval promise and resolves the Rust waiter directly;
it does not insert a duplicate result into the parent mailbox. Dropping the
call stops its owned child subtree.

`spawn` returns only after the task is durable. The child runs independently,
then sends one actor-authenticated outcome into the parent's durable mailbox,
waking or steering the parent. A detached child remains addressable after its
initial run completes.

`wait` accepts one or more direct-child addresses returned by `spawn` and waits
for all of their initial tasks without sending, interrupting, or otherwise
steering those children. It resolves only after every terminal outcome is
durably admitted to the caller's inbox. At the next model boundary, the wait
receipt and those inbox messages are therefore visible together in the same
continuation. Cancelling or timing out the surrounding eval does not cancel the
spawned work; its eventual outcomes are still delivered.

## Scheduler model

Each worker is one OS thread with a Tokio current-thread runtime and `LocalSet`.
Isolates never migrate between workers. Async provider requests and builtins
yield normally, allowing sibling actors on the same worker to progress.
Synchronous JavaScript or blocking Rust code occupies the worker until it
returns or is interrupted.

`max_agents` bounds actors being built plus resident actors. Launch reservations
prevent duplicate addresses. Capacity is released only after the outer actor
task retires—not merely when its inner runner observes cancellation.

## Embedded control and events

`Agent` is a cloneable embedded handle with `call`, structured
`call_structured`, explicit compaction, model switching, durable `send`, state
projection, recoverable tree interruption, and out-of-band abort. Correlated
operations use Lam's shared actor operation lease; a conflict returns
`ActorError::Busy` rather than waiting behind the resident lifecycle owner.

`AgentSystem::wait()` waits for quiescence: no active host operations,
reservations, actor runs, or eligible mailbox work. It does not retire idle
actors. `shutdown()` stops admission, gracefully retires actors, and joins all
workers; `abort()` interrupts active work first. Administrative `stop(address)`
retires an addressed subtree.

`Agent::interrupt(scope)` (or `AgentSystem::interrupt(address, scope)`) can
interrupt only the addressed actor or its complete resident subtree. Actor
scope keeps descendants running and their detached outcomes deliverable.
Subtree scope gives each active run its own durable interruption boundary,
then retires descendants and releases their capacity while the addressed root
remains available. A model or eval completion already committed at the
boundary wins normally. Cancelled detached tasks publish one structured
`AgentOutcome::Cancelled` to their direct parent's durable mailbox; `wait`
surfaces that outcome as a structured cancellation error rather than normal
completion. Outcomes never bubble beyond the direct parent.

`take_events()` yields one single-consumer, addressed, ephemeral stream:

- actors hosted and retired with `StopReason`;
- existing `RunEvent` and `RuntimeEvent` values tagged by address;
- child task outcomes.

Runtime and run events may describe the same operation at different scopes;
both are forwarded unchanged. Journals and mailboxes remain the durable
authority.

## Deliberately deferred

- Durable reconstruction of the live topology after process restart.
- Overload queues beyond explicit capacity failure.
- Dynamic rebalancing or isolate migration.

See the public [`lam`](../lam/README.md) facade, optional
[`lam-code`](../lam-code/README.md) capabilities, and repository
[README](../../README.md).
