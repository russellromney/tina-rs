# Phase 138: Storage On Betelgeuse

## Status

- Planned (2026-05-24). Implementation plan; auditing is done (this Status +
  Starting Facts capture it).
- **Verified against `origin/main` a6cbaa9:** `storage.rs` has zero Betelgeuse
  refs, `StorageLane` is still `Inline | Worker(std::fs thread)`, and Betelgeuse
  exposes `open/pread/pwrite/fsync/mkdir/size` on both platforms (`io/darwin.rs`,
  `io/linux.rs`). Premise holds.
- Same anti-pattern as Phase 136 (TLS): a blocking worker lane that bypasses
  Betelgeuse, even though Betelgeuse already exposes the file ops we need.
- Lower urgency than TLS (storage is correct today and cold for most services;
  TLS spawns a thread per op and is on the wrong substrate). Sequence after 136.

## Starting Facts

- Betelgeuse exposes async file I/O on **both** platforms:
  - Linux via io_uring: `opcode::Read` (PRead), `opcode::Write` (PWrite),
    `opcode::Fsync`, `opcode::MkDirAt`, size/statx (`io/linux.rs`).
  - macOS via non-blocking fds + the same event loop:
    `io/darwin.rs:228` `impl IOFile for DarwinFile` with `pread`/`pwrite`/`fsync`;
    `open` at `darwin.rs:499`.
  - Public surface: `open` (`OpenOptions` → `IOFile`), `pread`, `pwrite`,
    `fsync`, `mkdir`, `size`.
- `tina-runtime/src/driver/storage.rs` uses **pure `std::fs` blocking calls on a
  worker thread** and **never references Betelgeuse** (`grep` confirms zero refs;
  `execute_storage_job` is all `std::fs::rename/remove_file/metadata/read_dir`,
  plus temp-write/rename/fsync for snapshot commit and append/fsync for journal).
- So for the **durability hot path** (write bytes + fsync), we built a parallel
  std::fs worker-thread lane next to a Betelgeuse file rail that already exists.
  This is wasted threads + plumbing + core oversubscription under write load. It
  is **not** a correctness bug.
- Betelgeuse has **no** `rename`, `unlink`/`remove`, `readdir`, or `statx`/
  metadata op. Those storage jobs cannot ride Betelgeuse as-is.
- `StorageLane::Inline` (synchronous std::fs) is used by the **explicit-step
  runtime — the deterministic oracle**. `StorageLane::Worker` (thread) is the
  **live `ThreadedRuntime`** path. This phase changes the live path only.
- Durability semantics in place: append-before-apply, snapshot
  `last_journal_index`, journal `record_index`, truncated-tail, checksum,
  `CallError::CommitUncertain` when rename succeeds but final durability is
  unproven.

## Purpose

Move the live runtime's durability writes onto the per-shard Betelgeuse file rail
so they ride the loop the shard already runs — no separate storage worker thread —
while keeping the few ops Betelgeuse lacks on a thin, honest fallback, and keeping
the deterministic oracle's inline path unchanged.

```text
my Tina service's journal appends and snapshot writes go through the same
per-shard completion reactor as its sockets, instead of a side worker thread,
and recovery semantics (append-before-apply, torn tail, commit-uncertain) are
byte-for-byte the same
```

## Hard Constraints

1. **No storage worker thread for the ops Betelgeuse supports.** `pwrite`/`pread`/
   `fsync`/`mkdir`/`size`-shaped work runs through Betelgeuse on the shard thread.
   The `StorageWorkerLane` thread is deleted for these ops.
2. **The explicit-step oracle keeps `StorageLane::Inline`** (synchronous std::fs).
   This phase does not touch the oracle or `tina-sim`'s `DurableImage`. Live path
   only — exactly mirroring the TLS split.
3. **Recovery semantics are preserved exactly.** append-before-apply, torn-tail,
   checksum, duplicate/out-of-order index, and `CommitUncertain` produce the same
   typed outcomes and trace facts as today. Ordering is enforced by **waiting for
   the fsync completion before applying state** — the continuation message arrives
   only after the Betelgeuse fsync completes.
4. **Bounded admission stays visible.** `StorageFull` / `StorageClosed` semantics
   survive; "storage lane capacity = total accepted pending work" still holds,
   now as pending Betelgeuse file ops rather than channel slots.
5. **Metadata ops that Betelgeuse lacks** (`rename`, `remove`, `readdir`,
   `metadata`) keep a thin fallback. Mechanism is an explicit decision (see Open
   Decisions): a tiny bounded syscall path vs inline-on-shard for these rare/fast
   ops. Parent-dir sync is `fsync` on the directory fd → rides Betelgeuse.

## Includes

- A live storage path that, per job, issues Betelgeuse file ops:
  - `JournalAppend` → `open`(append) + `pwrite` + `fsync`.
  - `SnapshotCommit` → temp `open`/`pwrite`/`fsync` + **rename (fallback op)** +
    parent-dir `fsync` (Betelgeuse).
  - `SnapshotLoad` / `JournalReplay` → `open` + `pread` (+ `size`).
  - `SyncParent` → directory `fsync` (Betelgeuse).
- A thin fallback for `RenameReplace`, `RemoveFile`, `ReadDir`, `PathMetadata`
  (and the rename leg of `SnapshotCommit`) — see Open Decisions for the chosen
  mechanism.
- The pending/cancel/timeout/tombstone accounting for storage rebuilt over the
  Betelgeuse completion model, preserving `StorageFull`/`StorageClosed`/cancel.
- Capability truth: storage family for the live runtime reports
  **completion-backed** for the Betelgeuse-supported ops, and names the fallback
  ops explicitly (so we do not imply the whole family is on the reactor).
- Delete the `StorageWorkerLane` thread once all live jobs route through
  Betelgeuse + the thin fallback.

## Does Not Include

- Adding `renameat`/`unlinkat`/`getdents` opcodes to the vendored Betelgeuse
  (an option noted below, but it is substrate work, not this phase).
- Any change to the explicit-step oracle or `tina-sim`.
- Durable mailbox / durable work queue / exactly-once claims (still non-goals).
- A performance claim. Mechanism + honest reporting only.

## How We Prove The New Behavior (direct proof)

- Journal append + replay round-trip on the live runtime, asserting bytes,
  `record_index`, and **no storage worker thread spawned**.
- Append-before-apply ordering: state mutates only after the Betelgeuse fsync
  completion (assert the continuation ordering, not just the final value).
- `CommitUncertain` reproduced when the final durability step cannot be proven.
- Torn-tail / corrupt-checksum / duplicate-index recovery produce the same typed
  outcomes as the std::fs path.
- `StorageFull` / `StorageClosed` still fire under bounded pressure; canceled
  queued work does not start.
- Capability report shows completion-backed for supported ops and names the
  fallback ops.
- Guard: no `thread::spawn` for the storage durability path after deletion; a
  thread-count assertion shows no storage worker.

## How We Prove We Did Not Break Old Intent (blast-radius proof)

- The full snapshot/journal recovery suite and LocalSystem/bridge recovery proofs
  pass on the live path.
- The explicit-step oracle and `tina-sim` durable-image suites are unchanged and
  green (proves the live change did not leak into the oracle).
- Composed live TCP + persistence proof still holds.

## Open Decisions

- **Fallback mechanism for rename/remove/readdir/metadata.** Tiny bounded syscall
  worker (one rare-ops thread) vs inline-on-shard (accept a brief block for rare,
  fast metadata syscalls). Lean inline for `metadata`/`readdir`/`remove`; the
  `rename` leg of snapshot commit is rare (commit-time only) and also a candidate
  for inline. Decide in `Plan Review 1` / `Implementation Review 1`.
- **Whether to upstream `renameat`/`unlinkat` into Betelgeuse later** to remove
  the fallback entirely — defer; only if the fallback proves costly.

## Pointer: removal of the broader old model

Storage is the second instance of the bypass-Betelgeuse lane anti-pattern (TLS,
Phase 136, is the first). After 136 and 138 prove the pattern, **Phase 140
(unplanned): Retire the bypass-Betelgeuse lane model** should remove the generic
worker-lane scaffolding and move/justify the remaining lanes (unix sockets onto
the substrate; DNS resolver and process spawn kept as blocking lanes with written
justification). This phase deletes the storage-specific worker; the broad model
removal is 140.

## IDD Next Step

Plan only (Session A). Next: `Plan Review 1` in
`.intent/phases/138-storage-on-betelgeuse/review.md` (resolve the fallback
mechanism) before any code.
