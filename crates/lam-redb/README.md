# `lam-redb`

A durable [`JournalStore`](../lam-core/README.md) implementation for
[lam](../../README.md), backed by the pure-Rust embedded `redb` database.

Use this crate when actor mailboxes, provider-native context, model selection,
and compaction markers should survive process restart. Use `MemStore` from
`lam-core` when persistence is unnecessary.

## Basic use

```rust,ignore
use lam::Lam;
use lam_redb::RedbStore;

let store = RedbStore::create("state.lam.redb")?;
let mut actor = Lam::builder(model)
    .state_store(store)
    .build()
    .actor("assistant")
    .build()
    .await?;

let answer = actor.call("Continue the durable task").await?;
actor.shutdown().await?;
```

`RedbStore::create` creates a database when absent or opens it when present.
`RedbStore::open` requires an existing database.

## Storage layout

The database contains two versioned tables:

- actor ID → actor-local head revision;
- `(actor ID, revision)` → serialized `ActorEvent`.

Actor events are stored as their versioned Serde JSON representation. A read
observes the head and event page from one redb snapshot. An append uses one
write transaction to compare the expected actor-local head, write a consecutive
batch, and advance the head.

Different actors share one database without sharing revision sequences. The
backend does not maintain mutable projections, indexes, mailbox tables, or
model-specific schemas; those remain pure logic in `lam-core`.

## Concurrency and errors

A head mismatch returns `AppendOutcome::Conflict` rather than partially writing
the batch. Revision exhaustion and backend failures use the generic
`JournalError` boundary. `RedbStoreError` distinguishes database failures from
actor-event serialization failures and invalid stored actor IDs.

The `JournalStore` methods are async for backend interchangeability, while the
current redb operations complete synchronously inside each call.

## Recovery

Reopening an actor through `lam` folds its complete journal, validates the
durable model selection against the supplied registry, creates a new isolate,
and admits a resumption notice when prior execution may have been interrupted.
The database does not freeze or restore V8 heap state.

The conformance tests run the same ordered-read/conditional-append suite as the
in-memory store, plus close/reopen projection coverage.

## Scope

This crate stores lam's system journal only. It is not an agent-writable KV or
document database, and it does not yet provide snapshots, indexes,
content-addressed blobs, or compaction of old journal records.

See [`lam-core`](../lam-core/README.md), the public
[`lam`](../lam/README.md) facade, and the repository
[README](../../README.md).
