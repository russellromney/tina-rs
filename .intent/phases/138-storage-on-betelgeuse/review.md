# Phase 138 Review (append-only)

## Plan Review 1 — hostile (2026-05-24)

Verdict: premise is solid and verified. Two real correctness holes in the proof,
one internal contradiction, and the "inline fallback" lean is more dangerous than
the plan admits.

### Finding 1 (blocking) — the proof does not actually prove write→fsync ordering

With std::fs, write then fsync are sequential on one thread, so ordering is free.
On io_uring/Betelgeuse you submit `pwrite`, get its completion, **then** submit
`fsync`. The hazard is submitting `fsync` before the `pwrite` completion is
harvested. The plan's Hard Constraint 3 says "wait for the fsync completion before
applying state" — but the real ordering rule is **"wait for the pwrite completion
before submitting fsync."** A happy-path round-trip test PASSES even when this is
racy (bytes are usually there). "How could this be broken while tests pass?" is
trivially answerable → the proof is weak.

**Required plan change:** add a fault-injection proof using the Betelgeuse
simulated backend with delayed/reordered completions, asserting that
`apply` never observes state whose backing `pwrite`+`fsync` have not both
completed in order. Happy-path round-trip is `surrogate proof` here, not direct.

### Finding 2 (blocking) — `CommitUncertain` reproduction has no injection mechanism

On the new path, snapshot commit spans **two mechanisms**: a fallback `rename`
(not Betelgeuse) + a Betelgeuse parent-dir `fsync`. The plan says
`CommitUncertain` is "reproduced when the final durability step cannot be proven"
but never says **how to inject** that failure on the new path. Without an
injection hook it is `missing proof`. **Required plan change:** name the injection
points — Betelgeuse simulated fsync-failure for the fsync leg, and a fault hook on
the fallback rename — and assert `CommitUncertain` arises from each.

### Finding 3 (blocking) — the guard contradicts the open fallback decision

Proof bullet: "no `thread::spawn` for the storage durability path; a thread-count
assertion shows no storage worker." Open Decision: the rename/remove/readdir/
metadata fallback might be "a tiny bounded syscall **worker** thread." These
conflict — if the fallback is a worker, the guard cannot assert "no storage
thread." **Required plan change:** resolve the fallback mechanism *in this plan*,
then make the guard match it. If the fallback stays a worker, the guard must be
"no thread for the Betelgeuse-supported ops," not "no storage thread at all."

### Finding 4 — "inline on shard" for the fallback is a TPC regression in disguise

The plan leans toward running rename/remove/readdir/metadata **inline on the shard
thread** because they are "rare/fast." But `rename` (and especially the snapshot-
commit rename + parent fsync) can block — and blocking the shard thread is the one
thing thread-per-core forbids. "Rare but blocking on the shard" is exactly the
footgun this whole TLS/storage workstream is removing. **Recommendation:** keep
the lacking-op fallback **off the shard** (thin bounded worker) until/unless
`renameat`/`unlinkat` are upstreamed into Betelgeuse. Do not trade a clean
off-shard worker for an on-shard stall to win a "zero threads" headline.

### Finding 5 — clarify the Inline-vs-live boundary

Constraint 2 says the oracle keeps `Inline` and the live path changes. Confirm
`StorageLane::Inline` is **only** ever the explicit-step runtime and never a live
`LocalSystem`/`ThreadedRuntime` config, so "live changes / oracle unchanged" is
unambiguous. One sentence in Starting Facts.

### Keep

The Betelgeuse-op vs fallback-op split, recovery-semantics-preserved constraint,
and the oracle-untouched mirror of the TLS split are all right.

## Plan Review 2 — second reviewer (2026-05-25)

Verdict: the fallback-worker decision is right; the headline proof wording still
needed to match it.

### Finding 1 — do not claim "no storage worker" while keeping a fallback worker

The plan correctly keeps rename/remove/readdir/metadata off the shard on a tiny
bounded fallback worker. One proof bullet still said "no storage worker thread
spawned." Fixed in plan v3: the direct proof is "no worker thread for
Betelgeuse-supported durability ops"; the metadata fallback worker is allowed
and named.

## Implementation Review 1 — hostile self-review (2026-05-25)

Built on current origin/main. Live durability now rides the per-shard Betelgeuse
rail; the explicit-step oracle keeps `StorageLane::Inline` (`execute_storage_job`
unchanged). All findings below were resolved before PR.

### Resolved as planned

- **Plan Review 1, Finding 1 (pwrite→fsync ordering).** A fault-injecting
  Betelgeuse-trait backend (real file I/O, completion-based delivery with
  deterministic delay, plus a per-fd detector that flags an fsync submitted while
  a pwrite on the same file is still in flight) proves the rule directly:
  `pwrite_completion_harvested_before_fsync_submitted`. Happy-path round-trip is
  kept as a separate real-rail test, not the ordering proof.
- **Plan Review 1, Finding 2 (CommitUncertain injection).** Named and exercised:
  `parent_fsync_failure_is_commit_uncertain` injects an fsync failure on the
  parent-directory fd through the fault backend.
- **Plan Review 1/2, Finding 3 (guard vs fallback worker).** The fallback worker
  is lazily spawned on first fallback op. `pure_betelgeuse_ops_use_no_fallback_worker`
  drives a replay and a parent-sync (open/size/pread, open/fsync) and asserts the
  worker thread is never spawned — the honest "no worker for the supported ops"
  guard. The fallback worker is named in the capability report as
  `storage_metadata_fallback`.
- **Finding 4 (fallback off-shard).** rename/remove/readdir/metadata, plus the
  internal create-dir-all and torn-tail truncation, run on the bounded off-shard
  worker, never inline on the shard.
- **Finding 5 (Inline boundary).** `Runtime::new`/explicit-step uses
  `StorageLane::inline`; only the live `ThreadedRuntime`/`LocalSystem` path
  (`with_io_loop_and_capacities`) uses `StorageLane::reactor`. Confirmed by
  grep and by tina-sim staying green.

### Self-found issues

- **F1 (corrected the task wording, kept the truth).** The task brief said
  "fsync failure and fallback rename failure both produce `CommitUncertain`." The
  inline path returns `CommitUncertain` only for a parent-dir fsync failure
  *after* a successful rename; a rename failure returns `Io` because the durable
  state is known (the original is intact, the temp is removed). Hard Constraint 3
  ("same typed outcomes as today") binds the reactor path to that, so rename
  failure stays `Io`. Both are proven:
  `reactor_rename_failure_is_io_not_uncertain` (Io) and
  `parent_fsync_failure_is_commit_uncertain` (Uncertain). Implementing
  rename→Uncertain would have *violated* Constraint 3.
- **F2 (mid-job cancellation is crash-equivalent).** A reactor durability job is
  multi-step, so cancel/shutdown can stop it between syscalls — unlike the old
  worker, which ran a whole `commit_snapshot`/`append_journal_record` as one
  blocking unit. This is safe: the on-disk format is crash-consistent between
  every syscall (temp-write→rename→parent-fsync; data-append→fsync→sidecar swap;
  sidecar is revalidated against the journal length on the next append), so
  stopping between steps is indistinguishable from a crash there, which recovery
  already handles. Cancelled jobs stop issuing steps and are dropped once no
  Betelgeuse completion slot is outstanding; a stuck completion stays visible in
  `physical_pending_count` and bounded shutdown still returns
  (`shutdown_returns_within_budget_with_stuck_completion`).
- **F3 (concurrent appends to one journal).** The reactor admits up to
  `storage_lane_capacity` concurrent jobs, so two appends to the *same* journal
  path issued concurrently could both compute an end offset from a stale size.
  This is a caller error: append-before-apply means a service awaits
  `JournalAppended` before the next append, and `validate_next_journal_index`
  rejects a non-monotonic index. The old single-worker lane serialized all
  storage globally; the reactor overlaps independent files instead. Documented as
  a caller responsibility, not silently relied upon.
- **F4 (directory fsync via F_FULLFSYNC).** Betelgeuse's `Fsync` uses
  `F_FULLFSYNC`, which returns `ENOTSUP` on a directory fd and falls back to
  `fsync(2)` — matching the inline `sync_parent_directory`. Exercised live by the
  snapshot-commit and `pure_betelgeuse_ops_use_no_fallback_worker` (sync-parent)
  tests on macOS.
- **F5 (no simulator/substrate change).** The fault backend lives entirely in
  tina-runtime test code over the public Betelgeuse traits; vendor-betelgeuse and
  tina-sim's `DurableImage`/oracle are untouched.

### Blast radius

Green on the live path and the unchanged oracle: tina-runtime lib (incl. the new
reactor proofs), `persistence`, `durable_outbox`, `local_system`,
`readiness_matrix`, `admission_proofs`, `betelgeuse_substrate`; tina-tokio-bridge
`persistence_bridge`; tina-sim suite.

## Implementation Review 2 — deep hostile pass + fixes (2026-05-25)

Re-attacked the first cut from the angle of "what does a service author actually
hit, and where does the completion substrate differ from the old single worker
thread." Two real correctness bugs found and fixed, plus three coverage gaps
closed. All fixes have regression tests.

### Finding A (correctness, fixed) — concurrent same-path appends could corrupt

The old storage worker was one thread, so it serialized *all* storage; two
appends to the same journal never overlapped. The reactor admits up to
`storage_lane_capacity` concurrent jobs, so a `batch([append(1), append(2)])` to
one journal at capacity ≥ 2 had both jobs do `open→size→pwrite-at-end` from the
same stale length and clobber each other (no `O_APPEND` on the rail).
**Fix:** per-path write serialization. A write-family job (`JournalAppend` /
`SnapshotCommit`) takes a lock on its target path before issuing any op; a
same-path peer waits FIFO. Different paths still overlap (the reactor's point).
Proof: `concurrent_same_path_appends_serialize_and_do_not_corrupt`.

### Finding B (correctness, fixed) — cancelled in-flight job leaked + peer race

A user-cancelled job is no longer polled, so its armed completion slot is never
harvested. The first cut retained a cancelled job while its slot was merely
*present*, so the slot stayed non-idle forever — the job never dropped, leaking
it and inflating `physical_pending_count`. Worse, it released the path write
lock immediately on cancel, so a same-path peer could start writing while the
cancelled job's `pwrite` was still in flight. **Fix:** retain a cancelled job
(and its write lock) only while the **backend still owns** the slot (armed and
*no result yet*), exactly the TCP lane's `has_result`-drop rule; drop it and free
the lock once the result lands or no slot is armed. Proof:
`cancelled_job_drops_once_its_in_flight_completion_lands`. Residual fallback-leg
races (stale sidecar rename) are self-healing via the sidecar length check.

### Finding C (coverage, closed) — partial pwrite/pread

The lane loops on short transfers, but nothing exercised it. Added a fault-backend
`partial_io` mode that completes one byte per op; `partial_pwrite_and_pread_transfer_fully`
proves a multi-byte record still lands whole and replays byte-for-byte.

### Finding D (coverage, closed) — corrupt snapshot load on the reactor

`reactor_snapshot_load_corrupt_is_corrupt_record` asserts a present-but-corrupt
snapshot reads as `CorruptRecord` on the rail, matching inline `load_snapshot`.

### Finding E (coverage, closed) — live torn-tail recovery from the user's seat

The torn-tail repair test used the explicit-step `Runtime` (inline). Added
`local_system_recovers_torn_journal_then_repairs_on_next_append`: a `LocalSystem`
service recovers a torn journal's prefix and repairs the tail on the next append,
entirely over the live reactor — the user-facing proof.

### Second review (post-fix) — no new blocking findings

- Reads (`SnapshotLoad`/`JournalReplay`/`SyncParent`) hold no write lock by
  design; a replay concurrent with a same-path append reads a consistent prefix
  or a torn tail (handled), never corruption. Recovery and steady-state appends
  do not overlap in normal use.
- Per-path lock cannot deadlock (FIFO, capacity-bounded; an over-capacity submit
  is `StorageFull`); the lock is released on every terminal outcome and on
  cancelled-job drop.
- `HashMap`/`HashSet` are used only for membership/point lookups, never
  outcome-affecting ordered iteration; this is the live path, not the oracle.
- Path lock keys on the path as given (no canonicalization) — aliased paths to
  one file are the caller's responsibility, same as the app picking one journal
  path. The realistic per-service-fixed-path usage is safe.
