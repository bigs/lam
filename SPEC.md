# Addressed Messaging and Durable Spawn Ownership

## Status

This document specifies the user-visible and runtime contracts for messaging a
displayed agent and for cancellation around `lam.agents.spawn`. The terms
**must**, **must not**, **should**, and **may** are normative.

The durable journal is authoritative. Process-local notifications, TUI event
streams, and result channels are observation mechanisms and must not be used as
durable ownership or residency evidence.

## Terminology

- **Displayed agent**: the actor whose conversation is currently shown in the
  TUI conversation pane.
- **Direct parent**: the actor that invoked `lam.agents.spawn` for a child.
- **Admission**: the successful durable append of the child's initial task as a
  `MessageAdmitted` journal event.
- **Foreground observer**: the spawn invocation waiting to return a
  `SpawnReceipt` to its caller.
- **Detached spawn work**: an admitted initial task whose foreground observer
  has gone away while the child remains owned by the agent system.
- **Retiring actor**: a resident actor for which a stop reason has been chosen,
  whether or not its runner has fully exited.

## TUI addressed-message contract

### Target selection

Submitting ordinary input must target the displayed agent. The command carries
the canonical `ActorAddress` explicitly; the runtime must not infer `/root` or
read the selected pane later. Submission must leave the displayed pane
unchanged.

A restored or retired conversation remains browseable. The TUI may optimistically
submit from that pane, but it must not decide residency from `AgentSystemEvent`:
that event stream is lossy. `AgentSystem::agent` is the authoritative
process-local lookup, and a stale target is reported asynchronously as a failed
addressed message.

### Pending steers

Pending steers are owned by the target conversation, not by `/root` and not by
the currently visible pane at receipt time.

1. Submission creates an optimistic pending steer in the target conversation.
2. A durable receipt attaches the message identity to that same conversation.
3. Only a projector fold for the target actor may clear the steer as consumed.
4. Switching panes stores and restores each conversation's pending-steer list.
5. If the target run is active, an unconsumed receipt may report that delivery
   is queued for the next boundary. A receipt already reported as consumed must
   not display a queued status.

A failed send removes the matching target conversation's optimistic steer. It
may restore the text into the editor only when that target is still displayed
and the editor is empty. Otherwise it preserves the unsent text in the target's
error row and must not overwrite another pane's draft.

### Result routing and session replacement

Addressed message receipts and errors are routed by the target stored in the
result. Root-only operations—including compaction, model selection, effort
selection, and interruption errors—must update `/root` even if the user changes
panes while the operation is in flight.

Before replacing a session runtime, the TUI must quiesce all command producers
and discard queued results from the old runtime. No result from one session may
be applied to the app for its successor.

## Spawn ownership contract

### Ownership boundary

Durable initial-task admission is the ownership boundary for `spawn`:

- If foreground cancellation settles before admission, the child subtree must
  be cancelled and retired, and its residency capacity must eventually be
  released.
- If admission settles first, the child task is owned by the agent system. Loss
  of the foreground `SpawnReceipt` observer must not cancel it.
- A committed admission must win even when cancellation arrives before the
  admission receipt can be returned.

The runner coordinates this race. A cancellation signal received before an
append begins prevents admission. If cancellation interrupts an in-flight
append, durable journal state settles whether admission committed. The caller
must not make the decision from a stale, independently timed journal snapshot.

The task is registered as spawned before the foreground receipt is sent, so an
admitted detached task remains observable through `lam.agents.wait`.

### Operation ownership matrix

| Operation | Cancellation ownership | Required effect |
| --- | --- | --- |
| `spawn`, before durable admission | Foreground caller | Cancel and retire the new child subtree |
| `spawn`, after durable admission | Agent system | Detach the caller; continue child work |
| `call` | Caller | Cancel and retire the call-owned child subtree |
| `wait` | Agent system | Detach only the observer; never cancel child work |

A completed foreground call must release its exclusive operation lease before
publishing completion, so observing completion means another operation can be
admitted immediately.

## Terminal outcome delivery

Every admitted spawned task produces one terminal `AgentOutcome` for its direct
parent. Completed and failed outcomes follow normal actor-to-actor admission and
wake behavior. An interrupted detached spawn produces
`AgentOutcome::Cancelled`; it must not be silently discarded.

Cancellation delivery follows this order:

1. Durably admit the actor-sourced cancelled outcome to the direct parent's
   journal without waking it.
2. Order a possible wake against parent retirement using the same lifecycle
   gate as stop initiation.
3. Wake only if retirement has not started. If retirement already started, keep
   the durable outcome without starting new model work.

This ordering prevents a cancellation outcome from resurrecting an intermediate
actor while a subtree is being retired. Normal availability checks treat an
actor already known to be retiring as unavailable rather than waiting for its
runner to exit; cancellation-outcome wake ordering remains the lifecycle-linearized
guarantee specified here.

The in-memory spawned-task registry retains receipts for successful delivery but
must retain a full terminal outcome only for cancellation, where structured wait
failure needs it. Large completed outputs must not be duplicated indefinitely.

## Wait semantics

Dropping or cancelling `lam.agents.wait` detaches only that wait observer. A later
wait by the same direct parent can still observe terminal delivery.

For completed or failed child work whose terminal outcome is durable in the
parent inbox, `wait` returns the normal `WaitReceipt`. For a cancelled child,
`wait` fails with `WaitError::Cancelled` containing:

- the child address;
- the structured `AgentOutcome::Cancelled`;
- the durable parent-inbox message identity; and
- the parent journal revision containing that outcome.

The serialized error code is `cancelled`, and the nested outcome status is
`cancelled`. The durable actor-sourced outcome must occur exactly once in the
direct parent's journal.

## Durability and compatibility

No existing journal event or protocol record may be rewritten. New behavior is
implemented through existing append-only admissions and new API result variants.
Historical journals remain replayable. The addition of `WaitError::Cancelled` is
an intentional pre-release API extension; consumers using exhaustive Rust
matches must add the new branch.

## Acceptance scenarios

The implementation must cover at least these deterministic regressions:

1. Sending while a child pane is displayed targets that child and does not snap
   back to `/root`.
2. A child's pending steer survives a root fold and clears on the child's fold.
3. A late child receipt or failure is routed correctly after switching panes.
4. A late root-only command result cannot mutate the selected child pane.
5. Results queued by a quiesced old runtime cannot cross session replacement.
6. Cancellation while admission is blocked before commit retires the child and
   releases capacity.
7. Cancellation after commit but before receipt leaves the spawned child
   resident and allows its terminal outcome to reach the direct parent.
8. Cancelling `call` retires its subtree; cancelling `wait` does not.
9. Interrupting detached spawned work yields exactly one durable cancelled
   outcome and a later structured cancelled wait failure.
10. Cloned actor handles can begin a new exclusive operation immediately after
    observing completion of the prior one.