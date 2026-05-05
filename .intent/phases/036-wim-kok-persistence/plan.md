# 036 Wim Kok Persistence Plan

## Purpose

Make Tina capable of local durable state without pretending to be a database,
distributed log, or durable mailbox system.

Jelle gave Tina runtime-owned file I/O. That means Tina can now read and write
local files through the same effect/call model as time and TCP. Wim Kok turns
that raw I/O into a persistence story that makes stateful local services
reasonable to port from Tokio-shaped code:

> A Tina service can snapshot its owned state, append durable domain records,
> stop or crash, restart, recover from durable state, and prove the same shape
> through simulator tests.

This phase is core framework work, not docs, release story, remoting,
clustering, or broad database design.

## Starting Baseline

Current Tina has:

- runtime-owned file calls: `mkdir`, `file_open`, `file_create`,
  `file_read`, `file_read_at`, `file_write`, `file_write_at`, `file_fsync`,
  `file_size`, and `file_close`;
- runtime-owned TCP client/server and time calls;
- `LocalApp` as the preferred live owner;
- `tina-sim` deterministic file behavior for local file workloads;
- trace events for runtime-owned call completion/failure;
- supervised restart for direct children after panic;
- no durable recovery contract.

Current missing rock:

- state can be held in isolates, but after process restart the framework has no
  preferred way to restore it;
- file I/O exists, but users still hand-roll snapshot/journal protocols;
- simulator replay can reproduce in-memory runs, but not durable state across
  app incarnations;
- no trace vocabulary distinguishes "durability happened" from ordinary file
  calls;
- no tested crash/partial-write rule protects snapshots from torn writes.

## Accepted Scope

Wim implements the smallest local persistence layer that helps real Tina apps:

1. **Snapshots**
   - Add a runtime-supported or helper-supported snapshot write/read pattern
     over existing file I/O.
   - Snapshot data is user-provided bytes. Tina does not serialize arbitrary
     isolate state.
   - Preferred shape should support temp-file write, fsync, rename/commit, and
     read-back.

2. **Event Journal**
   - Add append/read journal helpers for durable domain records.
   - Records are user-provided bytes with a small Tina framing format:
     `record_index`, payload length, checksum, payload.
   - Journal appends must be ordered, fsync-capable, and replayable.
   - Journaling is opt-in and domain-level. It is not "every mailbox message is
     durable."

3. **Restart Recovery**
   - Add a user-shaped recovery flow: load latest snapshot, replay journal
     records after the snapshot, rebuild isolate state, then resume service.
   - Snapshot metadata stores `last_journal_index`; recovery replays only
     journal records with `record_index > last_journal_index`.
   - Prove recovery after clean shutdown and after simulated crash boundary.
   - Recovery should work through `LocalApp` without bespoke test-only runtime
     wiring.

4. **Simulator Parity**
   - Extend `tina-sim` enough to model snapshot/journal behavior
     deterministically.
   - Replay artifact must include a concrete durable image: a deterministic
     path-to-bytes map, named `DurableImage` unless code audit finds a better
     existing name.
   - Simulator durable behavior may be smaller than real filesystem behavior,
     but it must be explicit and honest.

5. **Crash / Partial-Write Semantics**
   - Snapshot commit protocol is: write temp file, fsync data, rename/commit,
     fsync parent directory where the backend supports it, then expose the
     snapshot as committed.
   - Directory fsync is a platform support table row, not a blocker or hidden
     behavior change.
   - Truncated final journal record replays the valid prefix and returns visible
     `TruncatedTail`.
   - Bad checksum on a complete journal record fails recovery with visible
     `CorruptRecord`.
   - Do not claim arbitrary crash consistency beyond tested protocol.

6. **Trace Integration**
   - Add persistence-specific trace event/kind vocabulary for:
     `SnapshotCommitted`, `JournalAppended`, `RecoveryStarted`,
     `RecoveryFinished`, `RecoveryFailed`, `SnapshotCommitFailed`, and
     `JournalAppendFailed`.
   - If trace sink/counter polish is needed, keep it scoped to persistence
     proof.
   - File-call trace alone is too low-level for this phase's durability proof.

7. **Bridge / LocalApp Proof**
   - Add a stateful service that runs under `LocalApp`, persists state, shuts
     down, starts a fresh app, recovers, and continues.
   - Add a bridge-hosted proof: Tokio/Axum request mutates Tina state, Tina
     persists it, app restarts, later request observes recovered state.
   - Keep bridge proof bounded and semantic; no web-demo theater.

## Non-Goals

- No durable mailboxes.
- No durable work queue. That can be a later phase, and it must be separate
  from durable mailbox semantics.
- No transparent persistence of all messages.
- No distributed persistence.
- No remoting/clustering.
- No RocksDB/SQLite abstraction unless a tiny adapter is required by a pinned
  workload and reviewed separately.
- No arbitrary serde requirement in `tina`.
- No hidden blocking pool.
- No unbounded write-behind queue.
- No "exactly once" marketing claim.
- No broad crash-consistency claim beyond the chosen snapshot/journal protocol.

## Design Rules

- Persistence lives above runtime-owned file calls and below application logic.
- Isolates still own their state and handle one message at a time.
- Handlers still return effects; persistence must not run file I/O directly in
  handlers.
- Durable records are explicit user data. Tina should not inspect arbitrary
  user state.
- Any helper must make durability policy visible: path, sync policy, record
  framing, and recovery behavior.
- Boundedness remains visible. Persistence helpers must not hide backpressure
  behind an internal unbounded queue.
- Recovery must be deterministic for the same durable input.
- Simulator and live behavior must share the same persistence meaning, even if
  live has platform-specific fsync limitations.

## README Rule Check

Wim must preserve the README rule:

> If something can overload, Tina should make it visible.
>
> If something can fail, Tina should make it traceable.
>
> If something can race, Tina should make it replayable.

Persistence-specific interpretation:

1. **Overload visible**
   - Persistence helpers must not add an unbounded internal queue, write-behind
     buffer, retry loop, or spill-to-disk mailbox.
   - Disk/resource exhaustion, capacity-style admission failure, and bridge
     ingress overload must surface as typed outcomes, not background best
     effort.
   - Unsupported durability strength, such as missing directory fsync, must be
     visible in the support table and not silently upgraded in prose.

2. **Failure traceable**
   - Snapshot commit failure, journal append failure, recovery failure,
     truncated tail, corrupt record, missing file, permission error, fsync
     error, and rename error must be visible through typed outcomes and trace.
   - Success-only trace is not enough. The failure path is part of the product.

3. **Race replayable**
   - Recovery from a durable image must be reproducible without wall-clock
     timing.
   - Concurrent bridge requests that append records must produce an observable
     order in Tina trace and durable journal indexes.
   - Simulator proof must include a seeded workload where message ordering,
     persistence ordering, and recovery ordering stay explainable from the
     replay artifact.

## Expected Public Shape

Exact names may change after code audit, but the plan should start with a
concrete target.

Snapshot helpers:

```rust
snapshot_commit(path, bytes, last_journal_index)
    .reply(ServiceMsg::SnapshotCommitted);

snapshot_load(path)
    .reply(ServiceMsg::SnapshotLoaded);
```

Journal helpers:

```rust
journal_append(path, record_index, record_bytes)
    .reply(ServiceMsg::JournalAppended);

journal_replay(path)
    .reply(ServiceMsg::JournalLoaded);
```

Recovery remains application-owned:

```rust
match msg {
    ServiceMsg::Start => snapshot_load(self.snapshot_path.clone()).reply(ServiceMsg::SnapshotLoaded),
    ServiceMsg::SnapshotLoaded(Ok(snapshot)) => {
        self.last_journal_index = snapshot.last_journal_index;
        self.restore_snapshot(snapshot.bytes);
        journal_replay(self.journal_path.clone()).reply(ServiceMsg::JournalLoaded)
    }
    ServiceMsg::JournalLoaded(Ok(records)) => {
        for record in records {
            if record.index > self.last_journal_index {
                self.apply_record(record.bytes);
                self.last_journal_index = record.index;
            }
        }
        noop()
    }
    ServiceMsg::Mutate(command) => {
        let record = self.record_for(command);
        let index = self.last_journal_index + 1;
        journal_append(self.journal_path.clone(), index, record.clone())
            .reply(move |result| ServiceMsg::MutationDurable(result, index, record))
    }
    ServiceMsg::MutationDurable(Ok(()), index, record) => {
        self.apply_record(record);
        self.last_journal_index = index;
        maybe_snapshot(self.snapshot_path.clone(), self.snapshot_bytes(), index)
    }
    _ => stop(),
}
```

Important grug rule: persistence helper may shrink file ceremony, but user
still decides what state means and when mutation becomes visible. The helper
must not hide append-before-apply: the success continuation must carry the
original record bytes, app command, or app token needed to mutate state only
after the durable append succeeds.

## Pinned Design Decisions

These are decisions, not implementation taste:

1. **Apply-before-append or append-before-apply.**
   Append durable record first, then mutate in-memory state on append success.
   This gives simple recovery semantics and makes failed appends non-mutating.

2. **Snapshot format.**
   Snapshot payload is opaque user bytes. Tina metadata sits beside the payload:
   at minimum `last_journal_index`.

3. **Journal framing.**
   Journal records are `record_index`, payload length, checksum, payload.
   Replay returns records in increasing `record_index` order.

4. **Snapshot and journal relationship.**
   Snapshot stores `last_journal_index`; recovery replays journal records with
   `record_index > last_journal_index`.

5. **Directory fsync support.**
   Support when the driver exposes it. If unsupported on a platform/backend,
   document it in a support table and do not claim parent-directory crash
   durability there.

6. **Simulator durable-state artifact.**
   Simulator replay artifact includes a deterministic durable image:
   `DurableImage`, shaped as a path-to-bytes map unless code audit finds an
   existing better local type.

7. **Serialization boundary.**
   No `serde` in core. Tests can use simple byte encodings.

8. **Corruption policy.**
   A truncated final record returns valid prefix plus visible `TruncatedTail`.
   A complete record with bad checksum fails recovery with `CorruptRecord`.

## Build Steps

1. Audit current file I/O, simulator file storage, trace kinds, LocalApp
   lifecycle, and bridge tests. Update this plan if the code says the first
   persistence slice should be smaller.
2. Verify the pinned design decisions above in `review.md` before adding public
   helper surface.
3. Add persistence vocabulary in the owning crate. Expected home:
   `tina-runtime` for runtime helpers and trace kinds; `tina-sim` for oracle
   durable state. Do not add public persistence API to `tina` unless the audit
   proves the prelude needs a tiny re-export.
4. Implement `snapshot_commit` / `snapshot_load` helpers over runtime-owned
   file calls.
5. Implement snapshot commit protocol with direct tests for successful commit,
   failed write, missing file, stale temp file, read-back, and
   `last_journal_index` preservation.
6. Implement `journal_append` / `journal_replay` helper with record framing and
   direct tests for multiple records, empty journal, truncated tail,
   complete-record bad checksum, and replay after snapshot index.
7. Add simulator durable file/persistence support and `DurableImage` replay
   artifact handling.
8. Add recovery helper or canonical recovery pattern; prefer small helpers over
   a broad persistence manager unless repeated code proves the need.
9. Add LocalApp recovery e2e: mutate state, persist, shut down, create fresh
   app, recover, continue, and assert final state and trace.
10. Add bridge recovery e2e: Tokio caller mutates state through bridge, app
    restarts, later Tokio caller observes recovered state.
11. Add crash-boundary tests:
    - committed snapshot survives;
    - uncommitted temp snapshot does not replace previous committed snapshot;
    - truncated journal tail returns valid prefix plus `TruncatedTail`;
    - complete journal record with bad checksum fails with `CorruptRecord`;
    - recovery remains deterministic.
12. Add live crash-boundary tests by manipulating on-disk temp/current/journal
    files between fresh `LocalApp` incarnations. Do not use sleep/process-kill
    proof.
13. Add simulator crash-boundary tests by injecting durable image sidecars and
    truncated/corrupt bytes.
14. Add trace/replay assertions for `SnapshotCommitted`, `JournalAppended`,
    `RecoveryStarted`, `RecoveryFinished`, `RecoveryFailed`,
    `SnapshotCommitFailed`, and `JournalAppendFailed`.
15. Add a filesystem support table covering temp-write, rename, file fsync,
    directory fsync, journal truncation, and checksum validation.
16. Add visible-overload/failure tests for disk/resource exhaustion where the
    backend can simulate it, permission/unsupported-operation errors, and
    bridge ingress overload while persistence is active.
17. Add a concurrent bridge append/recovery e2e where multiple Tokio requests
    enter Tina, Tina assigns durable journal indexes, fresh app recovery sees
    exactly that order, and the trace explains the order.
18. Add a seeded simulator persistence workload where message ordering,
    journal ordering, and recovery ordering replay from `DurableImage`.
19. Add allocation/cost notes only for new hot paths touched by persistence,
    especially journal framing.
20. Add negative proof that persistence helpers do not imply durable mailbox or
    durable work queue behavior.
21. Update `SYSTEM.md`, `ROADMAP.md`, and `CHANGELOG.md` if public persistence
    semantics land.
22. Run `make verify`.

## Required Proof Set

Snapshot:

- write/read round trip through live runtime;
- write/read round trip through simulator;
- commit replaces old snapshot atomically at Tina's claimed level;
- stale temp file does not become current snapshot;
- failed write surfaces typed failure and does not mutate app state;
- fsync, rename, permission, missing-file, and unsupported-operation failures
  surface as typed outcomes where the backend can produce them;
- recovery from snapshot restores user state in a fresh `LocalApp`.

Journal:

- append/read multiple records in order;
- empty/missing journal is a typed empty state, not panic;
- truncated final record returns valid prefix plus `TruncatedTail`;
- complete record with bad checksum fails recovery with `CorruptRecord`;
- journal replay after snapshot rebuilds state.

Recovery:

- clean shutdown + restart recovers;
- crash-boundary restart recovers from last committed durable state;
- append-before-apply semantics are proved with a mutation that fails to append;
- replay produces same state across two fresh app starts from same durable
  bytes.

Simulator:

- same durable workload replays deterministically;
- replay artifact includes enough durable state to reproduce recovery;
- seeded perturbation does not change committed durable ordering unless the
  record order itself changes visibly;
- seeded persistence workload proves message order, journal order, and recovery
  order are replayable from `DurableImage`.

Trace:

- `SnapshotCommitted` and `JournalAppended` are visible at the persistence
  layer;
- `SnapshotCommitFailed` and `JournalAppendFailed` are visible at the
  persistence layer;
- `RecoveryStarted`, `RecoveryFinished`, and `RecoveryFailed` are visible where
  helper surface owns recovery;
- corrupt/truncated journal handling emits a visible event or typed outcome.

App/bridge:

- `LocalApp` stateful service persists, restarts fresh, recovers, and continues;
- bridge-hosted stateful service proves the same path;
- concurrent bridge mutations assign visible durable journal indexes and recover
  in exactly that order;
- bridge ingress overload while persistence is active remains visible;
- no helper hides timeout, `Full`, `Closed`, or persistence failure;
- no helper implies durable mailbox or durable work queue behavior.

## Pause Gates

Pause and amend the plan if:

- snapshot commit requires a filesystem operation Betelgeuse cannot expose
  safely enough;
- directory fsync support changes the claimed crash-consistency level;
- journal framing wants a dependency or serialization crate;
- persistence starts turning into durable mailbox semantics;
- persistence starts turning into durable work queue semantics;
- recovery wants to deserialize arbitrary isolate state;
- simulator durable artifact design starts becoming a database;
- bridge proof requires flow ergonomics or unrelated adapter work;
- a persistence helper needs an internal queue, retry loop, or hidden worker to
  look ergonomic;
- a live-only race cannot be reduced to a simulator durable image or trace
  ordering proof;
- performance cost is dominated by an avoidable extra copy or allocation and
  changes public semantics.

## Done Means

- Tina has a small local persistence vocabulary for snapshots and journals.
- A stateful service can persist, restart fresh, recover, and continue under
  `LocalApp`.
- Simulator can replay the same durable recovery semantics.
- Crash/partial-write behavior is pinned and tested.
- Durable mailbox semantics are explicitly not claimed.
- Durable work queue semantics are explicitly not claimed.
- Trace/replay evidence is strong enough to debug recovery behavior.
- Public helper surface is small and does not hide state mutation policy.
- The README rule is preserved: overload visible, failure traceable, race
  replayable.
- `make verify` passes.

## Non-Claims After This Phase

Even if Wim succeeds:

- Tina is not a database.
- Tina does not persist every mailbox message.
- Tina does not spill overloaded mailboxes to disk.
- Tina does not provide a durable work queue.
- Tina does not provide exactly-once delivery.
- Tina does not provide remoting, clustering, distributed consensus, or durable
  distributed logs.
- Tina does not serialize arbitrary isolate state.
- Tina does not claim crash consistency beyond the tested snapshot/journal
  protocol and platform support.
- `flow!` ergonomics remain Barend Biesheuvel.
