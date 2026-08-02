# `lam-core`

Provider-independent domain logic for [lam](../../README.md).

`lam-core` owns the append-only actor language, pure projections, model/codec
contracts, compaction contracts, and storage SPI. It deliberately has no V8,
Deno, HTTP, or concrete database dependency.

Most applications use these types through the public `lam` re-exports. Depend
on `lam-core` directly when implementing infrastructure or testing domain
logic without constructing an isolate.

## Domain model

Each actor has one ordered journal of versioned `ActorEvent` values. The event
language includes durable model selection, admitted mailbox messages, appended
model-visible context, and compaction records. `ActorState` folds those events
into derived views such as:

- admission-ordered pending and eligible messages;
- the complete raw context history;
- active/completed run progress;
- the selected model and latest compatible compaction boundary.

Context transitions consume their source messages atomically. There is no
second mutable inbox table or delivery record to reconcile.

## `JournalStore`

Storage backends implement two operations:

- `read(actor, after, limit)` returns an ordered page and the actor-local head;
- `append(actor, expected, events)` atomically appends a non-empty batch only
  when the current head equals `expected`.

That compare-and-append boundary is the concurrency primitive. Projection and
retry logic live above it, so a backend does not need to understand actor state
or invent a transaction DSL.

The included `MemStore` is a pure-Rust, in-memory reference implementation:

```rust,ignore
use std::num::NonZeroUsize;

use lam_core::{ActorId, JournalStore, MemStore, Revision};

let store = MemStore::new();
let actor = ActorId::new("example")?;
let page = store
    .read(&actor, Revision::ZERO, NonZeroUsize::new(128).unwrap())
    .await?;
assert_eq!(page.head, Revision::ZERO);
```

Enable the `test-support` feature to run the reusable store conformance suite
against another implementation. It verifies actor isolation, ordering, paging,
atomic batches, and compare-and-append conflicts. Projection/race tests remain
separate because they test lam's event language rather than backend behavior.

## Model boundary

`ModelProvider` performs one inference request and emits ephemeral deltas.
`ModelCodec` projects durable native context into a request, interprets the
native response as an eval or terminal directive, and exposes non-authoritative
usage metadata.

The split keeps provider-native payloads authoritative. A codec can compute
useful views without forcing encrypted reasoning, signatures, tool calls, or
unknown extension fields through a lossy common message schema.

The principal types are:

- `ModelProvider` and `ModelEventSink` for transport/inference;
- `ModelCodec`, `ModelDirective`, and `OutputContract` for protocol semantics;
- `EncodedPayload` and `CodecRef` for versioned native JSON;
- `ModelDescriptor`, `ModelSelection`, and `ModelId` for durable registry
  identity;
- `TokenUsage`, `ModelCost`, and `ModelResponseMetadata` for observability.

## Compaction boundary

The `Compactor` trait receives atomic context units and produces either a
portable `CompactionArtifact` or an exact provider-native replacement.
`CompactionRecord` retains the source response, materialized replay payload,
optional display artifact, compatibility metadata, and usage/cost view.

Helpers group model/eval pairs into indivisible cut units and estimate token
sizes when exact provider usage is unavailable. The full raw journal remains
append-only regardless of the materialized effective context.

## Stability boundary

`ActorEvent` is a closed, versioned language because stores must persist it
losslessly. Provider payloads remain open JSON inside codec-tagged envelopes.
Identifier constructors validate strings before they become durable keys.

This crate does not own:

- JavaScript execution (`lam-deno`);
- actor threads or tool loops (`lam`);
- a concrete durable database (`lam-redb`);
- provider HTTP clients (`lam-openai`);
- multi-actor scheduling (`lam-agents`).

See the repository [README](../../README.md) and
[`docs/PLAN.md`](../../docs/PLAN.md) for the complete architecture.
