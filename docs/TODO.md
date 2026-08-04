# TODO

## Agent runtime

- [ ] Implement world state for dynamic agent state.
- [x] Esc to recoverably interrupt all running agents.
  - Scope is always `/root` and its complete active descendant tree, regardless of the selected `/agents` view or focused pane.
  - Escape has this singular purpose: it never clears draft input or changes pane focus, and it does nothing when no root work is active.
  - Borrow the deliberate double-Escape UX: the first physical key press arms interruption and flashes `Press Esc again to stop the current run` immediately above the input; a second physical press within 1.5 seconds triggers it. Key-repeat events must not count as the second press.
  - Any intervening non-Escape key disarms interruption. Clear the armed state and remove the warning immediately when disarmed, when its deadline expires, when the active run ends, or when interruption begins.
  - Model this as a first-class user interruption, not `Actor::abort`, `AgentSystem::stop`, process shutdown, or actor retirement. The root must remain usable for the next prompt.
  - Fan cancellation out before waiting so active provider requests, evals, and descendant work stop concurrently.
  - Give each affected actor an atomic durable terminal boundary. A completion already committed before interruption wins; interruption must never overwrite a durable model or eval outcome.
  - If an eval request is durable but its result is not, append an explicit interrupted eval failure. Record that external effects may have completed, the prior isolate state was lost, and the isolate was restarted.
  - If model generation is in progress, discard every incomplete delta: partial assistant text, reasoning, and partial tool-call JSON. Do not persist a partial model message or fabricate a valid provider-native response from incomplete SSE data.
  - Add a model-visible `lam/runtime` system notice describing the user interruption and close the active run explicitly so the next prompt cannot accidentally resume it.
  - On interruption, remove transient streaming rows from the TUI and replace them with the durable system-notice row so live and restored history agree.
  - A fully committed eval request is not a partial tool call: retain its tool-call row and pair it with the durable interrupted eval failure.
  - Resolve cancellation races at the journal append boundary and document that external tool side effects are not rolled back.
  - After committing their interruption records, descendants retire and release residency. Their durable journals remain browseable in `/agents`, their addresses remain non-reusable, and `/root` alone remains resident for later conversation.
  - Interruption permanently closes the active run. A later user message starts a new run from the retained context and interruption notice; cancelled provider requests, evals, and tool loops are never restarted automatically.
- [x] Ctrl+C clears nonempty draft input first; when the input is empty, it quits through the existing system-abort cleanup.
- [x] Streaming Markdown rendering for expanded user, assistant, and reasoning messages; collapsed previews remain plain.
- [ ] Bound console capture and spill oversized serialized eval results through embedding-provided storage.
- [x] Clarify the eval tool description so the model passes structured values to `lam.result` directly instead of pre-stringifying.
  - Root cause: the double JSON encoding was model behavior, not a harness defect. The provider APIs force exactly one stringification at the wire (`function_call_output.output` is a string); the second appeared only when the model called `JSON.stringify` before `lam.result`.
  - Added `Pass structured values directly to lam.result without JSON.stringify; the runtime handles encoding` to `EVAL_TOOL_DESCRIPTION`.
- [ ] Offer interleaved (merged, temporal-order) shell output as an option alongside the current separate stdout/stderr captures.
  - Observed from the model side: separate streams are usually right, but when a command fails the merged ordering is what explains how stdout and stderr relate.
  - Constraint: Unix pipes record no per-write timestamps and no cross-stream ordering. Once stdout and stderr are separate pipes, true interleave cannot be reconstructed post-hoc; any merge is a best-effort guess.
  - Design fork, unresolved:
    - One pipe (`stderr(Stdio::stdout())`): kernel-guaranteed write-order, but output is not labeled by stream. Honest trade; best fit for the failure-diagnosis case.
    - Two pipes + read-lock or chunk timestamps: keeps stream labels, but ordering is heuristic (timestamps would measure read/arrival time, not the child's write time) and a read-lock risks blocking the child on a full pipe buffer. Worst of both.
    - Status quo (two pipes, separate fields): exact labels, no claimed cross-stream order.
  - Deferred pending a decision on whether guaranteed ordering (one pipe, unlabeled) is worth giving up attribution.
