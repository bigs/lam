# Lam Development Report

Status of the current engagement: committed work on main, a large uncommitted
batch in the working tree awaiting user verification. The cold-boot
investigation is resolved: all five ranked structural fixes are implemented,
plus two findings that revise the earlier analysis (see "Corrections to the
earlier analysis"). A second batch hardens boot/teardown further:
quick-repair commits, teardown checkpoints, a checkpoint-size regression
guard, and session deletion in the TUI (see "Second batch").

## Git state

- HEAD: f86dc78 (main == origin/main)
- Working tree: ~20 files changed, uncommitted.
- Workflow: commit/push only after the user has verified a change.

## Committed on main

### f86dc78 - tui: quiesce the command runtime before teardown to fix Ctrl+C panic

- Root cause: dropping a multi-thread tokio runtime performs a blocking join,
  illegal inside the current-thread async context of tokio_main, so quitting
  with Ctrl+C panicked.
- Fix: Runtime::quiesce() drains in-flight command tasks (bounded by
  COMMAND_DRAIN_TIMEOUT) and drops the multi-thread runtime on a plain thread
  where blocking is allowed. Applied to the quit and session-switch paths.
- User-verified (clean Ctrl+C quit).

### 4825c8d - core: bootstrap actor projections from compaction checkpoints

- Cold loads bootstrap from the newest compaction checkpoint (Checkpoint
  from_state/into_state, serde_json) instead of replaying the journal from
  revision zero. Both the actor load_state path and the TUI projector
  bootstrap use it.

## Uncommitted working tree (pending verification)

### 1. Boot-phase instrumentation (--debug-log)

- New crates/lam-tui/src/boot.rs: phase()/phase_sync() emit JSONL
  boot.phase events with elapsed_ms when --debug-log is enabled.
- Timed phases in main.rs (config load, catalog open, resume, session
  choices, per-session previews, open_session, terminal start, boot.complete
  total) and runtime.rs Runtime::build (configured_models, coding pack, redb
  open, agent system, host root actor, per-actor projector bootstrap,
  command runtime).
- lam/src/actor.rs start() timed: isolate build, state load, startup recovery.
- Diagnostics filter extended to all lam_* crate targets at TRACE.
- NEW: the quit path is timed too (shutdown_abort, shutdown_quiesce,
  shutdown_background_compactions, shutdown_compact_store), and background
  session compaction logs a session.compaction event with elapsed_ms.

### 2. Journal constructed model responses instead of raw SSE chunks

- The chat-completions codec previously persisted the full raw SSE chunk
  stream per model turn ({outputKind, model, chunks}); a long session stored
  674,531 chunks, ~351 bytes each, ~226 MiB of mostly JSON envelope overhead.
- Streaming now folds the chunks at the block terminator (terminal
  finish_reason chunk) via the existing assemble_message into the native
  response shape ({outputKind, model, response: {model, choices, usage}})
  and journals that. The Responses codec already stored the reconstructed
  native response.
- Legacy chunk payloads remain decodable (the readers dispatch on the chunks
  vs response field); no version bump, no migration.
- New ProviderError::Codec variant.

### 3. Compressed actor checkpoints

- Checkpoint::encode()/decode() in lam-core: LAMC magic prefix + zlib
  (flate2, pure-Rust backend) compressed JSON.
- decode() dispatches on the marker: compressed blobs inflate + parse, legacy
  JSON blobs parse directly. No migration; old checkpoints stay readable.
- Both readers (lam load_state, lam-tui projector bootstrap) call
  Checkpoint::decode, centralizing the format in one place.
- bincode was investigated and rejected: checkpoints embed codec-tagged
  serde_json::Value payloads, which bincode cannot deserialize (no
  deserialize_any support).
- Deps added: flate2 1.1.9, simd-adler32 (miniz_oxide already in the lock).

### 4. Journal compaction at teardown, now gated on measured waste

- RedbStore::compact(): thin wrapper over redb in-place compaction.
- NEW RedbStore::footprint(): measures file length against allocated pages
  (StoreFootprint { file_bytes, reclaimable_bytes }).
- Runtime::into_store(): consumes the runtime, releases the agent system,
  and returns the journal store for teardown maintenance.
- Quit path: compacts only when reclaimable >= 64 MiB AND >= 25% of the
  file; otherwise quit is instant. Prints reclaimable size and duration.
  If the store Arc is still shared at quit (should not happen after abort +
  quiesce), that is now reported instead of silently skipped — an unclosed
  Database is exactly what makes the next boot repair the whole file.
- Session-switch path: the torn-down old session is compacted on a
  background thread under the same gate; quit joins these threads.

### 5. Read-only session previews

- lam_redb::ReadOnlyStore: read-only open + paged reads, skipping redb's
  write-path open setup.
- first_user_message uses it, and now caps previews at 300 chars.

### 6. Projection truncates covered context on compaction (the big one)

- lam-core ActorState::apply_context: folding a Compaction marker now drops
  every projected entry with sequence <= covers_through before pushing the
  marker. The journal keeps the full history durably; the projection (and
  therefore every checkpoint) holds only the post-compaction window.
- Checkpoint::into_state applies the same rule, so legacy bloated
  checkpoints rebuild the same truncated projection a fresh fold produces.
- Checkpoint::has_covered_context() / Checkpoint::blob_is_current() expose
  staleness so loaders can detect legacy blobs.
- Consequences (accepted by design):
  - Cold-booted transcripts start at the newest compaction window;
    pre-compaction rows are no longer rendered from the checkpoint. A full
    transcript can still be rebuilt from the journal if we ever want a
    "load full history" affordance.
  - Older compaction markers covered by a newer marker leave the
    projection, so the cross-codec fallback (selected_compaction walking to
    an older compatible marker) can no longer find them. The newest marker
    is always compatible with the model that wrote it; a ReuseContext model
    switch across codecs now sees only the surviving tail.
- Tests updated to the new invariant: lam-core/tests/state.rs,
  lam/tests/actor.rs (3 tests), lam-redb/tests/store.rs, lam-openai
  tests/live.rs. New tests: truncation-on-fold + sequence monotonicity,
  legacy checkpoint normalization.

### 7. Stale checkpoints self-heal at load

- lam load_state: when the checkpoint blob is legacy-format or still carries
  covered entries, the normalized state is written back as a fresh
  compressed checkpoint immediately after the (single) expensive decode.
- Because the root actor's state load runs before the TUI's projector
  bootstrap in Runtime::build, the TUI's second decode reads the rewritten
  small blob. The double-decode problem disappears without cross-crate state
  sharing: after the first boot both decodes are of the small checkpoint.
- New test: load_state_rewrites_stale_checkpoints_in_the_current_form.

### 8. Session previews are normalized into the catalog index

- SessionRecord gains an optional `preview` field (serde default, no schema
  bump; old records decode with None).
- SessionCatalog::list returns SessionListing { session, preview };
  SessionCatalog::store_preview caches a preview once discovered. A first
  user message never changes, so the cache never invalidates.
- Boot reads previews from the single index read; only sessions without a
  cached preview are scanned (read-only, once, then backfilled). Initial
  load no longer opens every session journal in the workspace.
- New test: previews_are_cached_in_the_index_once_stored.

### 9. RedbStore::open probes the migration read-only

- The CHECKPOINTS-table migration shim now checks with a read transaction
  and only runs the write transaction when the table is actually missing.
  Ordinary opens are write-free.

### 10. Dev-profile optimization for journal-hot dependencies

- Workspace Cargo.toml: [profile.dev.package] opt-level = 3 for redb,
  flate2, miniz_oxide, simd-adler32.
- Measured (130 MiB journal, M-series): clean RW open 947 ms in plain dev,
  56 ms with the override, 18 ms in release. This alone removes most of the
  "seconds per open" pain during development.

## Corrections to the earlier analysis (measured)

Two claims in the previous version of this report were wrong or incomplete:

1. **"redb RW open costs 8-10 ms per MiB" conflated debug builds and
   repair.** Measured with redb 4.1: a *cleanly closed* database opens via
   the allocator-state fast path in ~0.14 ms/MiB (release) — near-constant
   in practice. The multi-ms/MiB cost appears when (a) the previous process
   never dropped the Database (crash, kill, panic — redb skips its cleanup
   when unwinding), forcing a full repair, and/or (b) the binary is an
   unoptimized dev build (~50x slower on this CPU-bound path). The historic
   Ctrl+C panic (fixed in f86dc78) meant every panicked quit poisoned the
   next boot with a full repair, which is where the consistent slow opens
   came from.

2. **"Quit-time compaction persists recovery_required with no commit at
   exit" is only true if the store never drops.** Verified by test: compact()
   followed by a clean drop leaves the file fully readable by the cheap
   read-only path (read_only_open_succeeds_after_compaction_and_clean_close).
   The poisoned state exists only in the window between compact()'s 2-phase
   commits and the Database drop — snapshotting the file there does
   reproduce RepairAborted. The durable fixes are: always drop the store
   before exit (now loudly reported if impossible), and gate compaction so
   the window is rarely entered at all.

## Where the 34 s boot went

| Phase (was) | Cause | Fix |
|---|---|---|
| projector_bootstrap 14.8 s | 284 MB checkpoint JSON decode + fold | truncated checkpoints (#6), self-heal rewrite (#7) |
| state_load 12.0 s | same decode again | second decode now reads the rewritten small blob (#7) |
| session_choices 4.3 s | journal scan per session | previews served from the catalog index (#8) |
| redb_open 2.9 s | repair-on-open after unclean close + dev build + migration write txn | clean closes + gate (#4), read-only probe (#9), dev opt (#10) |

Expected steady state after one upgrade boot: sub-second cold boot for the
legacy 400 MiB session (dominated by the RW open of the remaining live
journal data, ~0.2 s with the dev profile), near-instant for fresh sessions,
instant quit unless >= 64 MiB and >= 25% of the file is reclaimable.

The first boot after upgrading still pays the legacy 284 MB checkpoint
decode once (~8 s, then rewrites it small), and the first preview
backfill scans journals once (~4 s for the big one). The first quit after
that will likely see a large reclaimable share and run one final multi-second
compaction; subsequent quits are instant.

## Remaining structural limit (not addressed, by choice)

The legacy session's journal still stores ~226 MiB of raw SSE chunk events
(pre-fold format). They are live data: redb compaction cannot shrink them,
so the RW open of that session pays for them forever (~0.2 s dev-opt). The
no-migration policy was deliberate; if that session ever needs to be fast
and small, the options are a one-time journal rewrite (fold chunk events
into constructed responses) or starting a fresh session.

## Second batch (uncommitted, pending verification)

### 11. Quick-repair commits in lam-redb

- Every committing write transaction in lam-redb (append_batch, checkpoint
  writes, initialize, the open-time migration) now sets
  `set_quick_repair(true)` via one shared `begin_write` helper: each commit
  also persists redb's allocator state table, so a file left behind by a
  SIGKILL or panic opens read-write without a full repair.
- Measured (release, 160 MiB file): unclean-snapshot RW open 71-101 ms
  before, ~10 ms after; clean open unchanged (~5 ms). Cost: commit latency
  roughly doubles (4.4-4.6 ms to 9.2-9.4 ms per commit, the second fsync) —
  invisible at journal-event rates (a few commits per model turn).
- Regression test: `mid_session_snapshot_reopens_without_a_full_repair`
  opens a mid-session file copy with a repair-aborting probe (deterministic
  "needed repair" signal) and via RedbStore::open.
- Correction to the earlier correction: **read-only opens of a dirty file
  fail unconditionally** — redb sets the header `recovery_required` flag
  when a read-write handle opens and clears it only on clean close; commits
  never clear it, so quick-repair does not help ReadOnlyDatabase::open.
  (The earlier observation that a mid-session snapshot sometimes opened
  read-only did not reproduce under a controlled retest, 0/8.) Practical
  impact is small: previews are served from the catalog cache, and an
  un-cached preview of a dirty journal shows "Preview unavailable" until
  the session is opened once (which repairs and clean-closes it).

### 12. Teardown checkpoints

- New `checkpoint_projector` / `Runtime::write_teardown_checkpoints` in
  lam-tui: at quit (`shutdown_checkpoints` phase) and session switch
  (`session_switch_checkpoints` phase), every projector is folded to the
  journal head and snapshotted as a checkpoint. Cold boots now bootstrap
  from the head and fold approximately nothing, even for sessions that
  never triggered a compaction.
- Gated on strict revision progress: an unchanged head writes nothing, so
  quitting an idle session does not orphan the stored blob's pages and
  manufacture compaction waste. Best-effort: failures log warnings, never
  fail teardown. Ordered before quit-time compaction so orphaned old blobs
  are reclaimed in the same teardown.

### 13. Checkpoint-size regression guard

- lam-core test `checkpoint_size_is_bounded_by_the_compaction_window`:
  20 message/model/compaction cycles with incompressible ~32 KB payloads;
  asserts the encoded checkpoint stays near the one-window baseline
  (measured: 327-byte baseline, 610-byte final, 41 KB bound) instead of
  scaling with history (a regression would produce ~1 MB). Guards the class
  of bug behind the 284 MB snapshot / 34 s boot.

### 14. Session deletion in the TUI

- In the `/session` palette, Ctrl+D on a highlighted session arms deletion
  ("! Press ctrl+d again to delete session #N"); a second physical press
  confirms and emits Command::DeleteSession. Any other key (Esc included)
  or moving the highlight disarms; key-repeat never arms or confirms
  (mirrors the double-Esc convention). A dim hint line under the palette
  rows advertises the binding ("ctrl+d delete the highlighted session");
  --help mentions it too.
- Deleting the OPEN session replaces it: arming warns "…delete the open
  session #N and start a fresh one"; confirming emits
  Command::ReplaceSession, which creates the successor first (carrying
  model/effort preferences like /new), tears the old runtime down without
  teardown checkpoints or compaction (wasted work on a journal about to be
  removed), swaps the lease, deletes the old session, and opens the fresh
  one — status "Deleted session #N; started #M". A failed create leaves the
  current session untouched; a failed delete is surfaced but never aborts
  the switch.
- SessionCatalog::delete: under the index write lock, validates the record
  and cwd, acquires the session lease (SessionInUse if any TUI — including
  this one, for its own session — holds it), removes the SESSIONS row and
  repoints LATEST_BY_CWD to the newest remaining session (or removes the
  key) in one transaction, then best-effort removes the journal, lock, and
  debug-log files. The index is the source of truth; file-removal errors
  are logged and ignored.
- Deleting never touches the running session or runtime; the palette
  refreshes from the catalog afterward.
- Motivated by the decision to wipe (not migrate) legacy sessions whose
  journals predate the SSE-chunk fold.

### Planned, written down but not executed (docs/TODO.md)

- Lazy full-history transcript loading after cold boot (journal replay on
  demand, display-only, paged).
- Backfill the active session's catalog preview when its first user message
  is admitted.

## Verification status

- Both batches: 317 tests pass across the workspace, 0 failures; clippy 0
  warnings; cargo fmt clean.
- User-verified: cold boot and quit timing, session picker previews, session
  deletion of non-active sessions. Pending user verification:
  replace-and-delete of the open session.

## Suggested manual verification

1. `cargo run` in a workspace with the big legacy session, with
   `--debug-log`: first boot should show one large state_load (the one-time
   checkpoint rewrite), then quit and boot again — boot.complete should be
   sub-second and session_choices ~0 ms.
2. Check the /session picker previews still render (now served from the
   catalog).
3. Quit: should print either nothing (gated) or the reclaimable size and a
   completion line.
4. Confirm you are comfortable with cold boots rendering the transcript
   from the newest compaction window onward.
