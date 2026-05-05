# 036 Wim Kok Plan Review 1

Verdict: right next phase, not ready to execute until several load-bearing
choices are pinned. The phase aim is good: local durable state is the next
capability that makes Tina feel like a framework for whole stateful services,
not just bounded in-memory workers. But persistence has many old traps. Grug
wants fewer "expected direction" stones and more "we choose this" stones before
implementation starts.

## What Looks Strong

- The phase says "local durable state" instead of pretending to be a database,
  distributed log, or durable mailbox system.
- It correctly builds on Jelle's file I/O instead of inventing a second storage
  path.
- It refuses durable mailboxes, hidden write-behind queues, arbitrary serde in
  core, and exactly-once marketing. Good grug.
- It keeps recovery application-owned: user bytes in, user state rebuilt by user
  logic.
- It names the real hard parts: crash/partial-write behavior, journal
  truncation, snapshot/journal relationship, simulator artifact shape.
- It requires LocalApp recovery proof, not just helper unit tests.

## Load-Bearing Gaps

1. **Snapshot/journal relationship is not pinned enough.**
   The plan says "snapshot has a journal position / generation marker" or maybe
   "smaller single-file log/snapshot sequence." That is too much wiggle. This
   choice determines the whole implementation. Recommendation: choose
   generation-based snapshot metadata now:
   - snapshot stores user bytes plus `last_journal_index`;
   - journal records have monotonic `record_index`;
   - recovery loads snapshot, then replays records with index greater than
     `last_journal_index`.

2. **Helper surface might hide mutation policy.**
   `journal_append(path, record_bytes).reply(MutationDurable)` is good, but the
   example returns only success/failure while the app later applies `record`.
   The plan should require the success continuation to carry the original record
   bytes or an application token, so append-before-apply is ergonomic without
   cloning ad hoc in every app.

3. **"Crash-boundary" needs a concrete test harness.**
   The plan asks for crash-boundary tests, but does not say how grug simulates
   crash without actual process kill. Pin the method:
   - live tests manipulate on-disk temp/current/journal files between fresh
     `LocalApp` incarnations;
   - simulator tests inject durable-state sidecars / truncated bytes;
   - no sleep/process-kill proof.

4. **Directory fsync support can derail the phase.**
   Betelgeuse may not expose directory fsync/rename exactly as needed. The plan
   should pin the first slice to "best available local protocol" and make
   directory fsync a support-table row, not a blocker. Otherwise Wim can get
   stuck adding broad filesystem API before proving recovery.

5. **Journal corruption policy is too broad.**
   "bad checksum is visible and deterministic" leaves two possible policies:
   replay through previous record and report truncation, or fail whole recovery.
   Recommendation: pin two classes:
   - truncated final record: replay valid prefix and return a visible
     `TruncatedTail` warning;
   - bad checksum on a complete record: fail recovery with `CorruptRecord`
     because valid prefix may hide real data loss.

6. **Simulator durable artifact shape needs a first concrete representation.**
   "includes deterministic durable bytes or sidecar value" is right but vague.
   Pin a map: `BTreeMap<PathBuf, Vec<u8>>` or a named `DurableImage` with
   snapshot/journal bytes. No arbitrary filesystem paths during replay.

7. **Trace vocabulary may be under-specified.**
   The plan says maybe persistence-specific events if file-call trace is too
   low-level. For persistence, file calls are too low-level. Pin the minimal
   events now: `SnapshotCommitted`, `JournalAppended`, `RecoveryStarted`,
   `RecoveryFinished`, `RecoveryFailed` or equivalent call kinds/outcomes.

8. **Bridge proof is optional but should probably be required.**
   If the goal is "full Tokio workflow can sensibly move to Tina," then a
   bridge-hosted stateful service is not theater. It proves the adoption edge:
   Tokio request enters Tina, mutation is durable, new app observes recovery.
   Make this required unless implementation discovers bridge lifecycle makes it
   noisy for reasons unrelated to persistence.

## Medium Tightenings

- Add a durable-work-queue note: explicitly deferred, distinct from durable
  mailbox, possibly later if workload needs disk-backed admission after visible
  `Full`.
- Pin helper names as provisional but concrete: `snapshot_commit`,
  `snapshot_load`, `journal_append`, `journal_replay` may be clearer than
  `snapshot_write` / `journal_read` because commit/replay are semantic words.
- Require no public API in `tina`; persistence belongs in `tina-runtime` or a
  sibling persistence crate if audit says boundary wants it.
- Add allocation note for journal framing: copying record bytes is acceptable in
  this phase if named; zero-copy journal belongs later.
- Add negative tests: helper does not mutate user state on failed append;
  missing snapshot + missing journal recovers to empty state; durable mailbox
  is not implied by any API.
- Add a support table at closeout for platform durability claims:
  temp-write, rename, file fsync, directory fsync, journal truncation, checksum.

## Recommended Plan Changes Before Execution

1. Replace "expected direction" for snapshot/journal relationship with the
   generation/index design.
2. Pin journal record framing and corruption policy exactly.
3. Pin simulator durable artifact as `DurableImage` / path-to-bytes map.
4. Require bridge recovery e2e, not optional.
5. Pin minimal persistence trace events.
6. Add durable work queue as explicitly deferred, separate from durable
   mailbox.
7. Add support table requirement for filesystem crash-consistency level.

After those edits, grug says Wim can launch.

---

# 036 Wim Kok Implementation Review

Verdict: implementation is on-shape and ready after the hostile fixes below.

What landed:

- `tina-runtime` owns the persistence vocabulary:
  `snapshot_commit`, `snapshot_load`, `journal_append`, `journal_replay`,
  `SnapshotImage`, `JournalRecord`, `JournalReplay`,
  `JournalReplayWarning::TruncatedTail`, and `CallError::CorruptRecord`.
- Snapshot metadata carries `last_journal_index`; journal records carry
  monotonic `record_index`, payload length, checksum, and payload.
- Snapshot commit uses temp-write, file fsync, rename, and parent-directory
  fsync where the platform supports it.
- `LOCAL_PERSISTENCE_SUPPORT` names temp-write, rename, file fsync,
  parent-directory fsync, truncation warning, and checksum validation support.
- `tina-sim` has `DurableImage` path-to-bytes capture/load support, so durable
  recovery is replayable from a deterministic artifact.
- Persistence trace vocabulary is present:
  `SnapshotCommitted`, `SnapshotCommitFailed`, `JournalAppended`,
  `JournalAppendFailed`, `RecoveryStarted`, `RecoveryFinished`, and
  `RecoveryFailed`.
- `LocalApp`, explicit-step `Runtime`, deterministic `Simulator`, and
  Tokio bridge all have user-shaped persistence tests.

Hostile review fixes made during implementation:

- `RecoveryStarted` now emits when the recovery call is dispatched, before the
  load/replay completion, instead of being emitted at completion time.
- Current-directory paths like `state.snapshot` now use `.` as their parent
  instead of failing on an empty parent path.
- Snapshot temp file names are unique per process/call counter rather than one
  fixed `.tmp` path per snapshot target.
- Stale temp snapshots are directly tested and ignored by load/recovery.
- Journal append validates the existing journal before writing and rejects
  duplicate, out-of-order, corrupt, or truncated-tail journals as
  `CorruptRecord`, so success cannot create unreplayable durable state.
- Snapshot commit reports `CallError::CommitUncertain` if rename already
  happened but the final parent-directory sync fails.
- `LOCAL_PERSISTENCE_SUPPORT.rename_commit` and
  `directory_fsync_after_rename` are platform-scoped instead of overclaimed
  unconditionally.
- Bridge persistence proof now covers concurrent Tokio callers. The example
  Tina service serializes durable mutations explicitly in isolate state, so
  append-before-apply stays visible instead of hidden in bridge machinery.
- Bridge lifecycle test drops cloned handles before shutdown, proving the
  existing `StillShared` guard remains real.

Remaining non-claims:

- This is local snapshot/journal persistence, not durable mailbox, durable work
  queue, database, remoting, clustering, distributed log, or exactly-once
  system.
- The persistence helpers are synchronous inside the current driver path. They
  are runtime-owned effects from the user's point of view, but this phase does
  not claim a high-throughput storage reactor.
