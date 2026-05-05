# Phase 038: Stuga First-Class DST

## Goal

Turn Tina's deterministic simulation testing from good bespoke tests into a
first-class framework capability.

The end state is not "more random tests." The end state is that a Tina user, a
future Codex session, or a skeptical runtime engineer can define a workload as
data, run it under seeded perturbation, get a replay artifact, shrink a failure
to a small script, and apply reusable Tina invariants without re-writing the
same checker glue in every test file.

This phase makes DST a Tina primitive:

> If something can overload, Tina should make it visible.
> If something can fail, Tina should make it traceable.
> If something can race, Tina should make it replayable and shrinkable.

## Why This Is Production Work

Stuga is not next because Barend is next. Barend is not next.

Stuga is next only if the highest-leverage production move is to make Tina's
proof machinery reusable before adding more runtime surface. The current
roadmap still has larger runtime rocks after Thorbecke: live substrate liveness
faults, shard topology/pinning, peer quarantine, shard-restart behavior,
cross-shard isolate-call reply transport, nonblocking storage maturity, and
platform/CI posture.

The reason to do Stuga before those rocks is that every one of those later
runtime decisions needs stronger DST rails. If a future phase changes live
topology, storage behavior, or cross-shard replies, Stuga should make the
failure reproducible, shrinkable, and comparable against simulator semantics.

Flow ergonomics can wait until the runtime core feels boring under these proof
rails.

## Current Baseline

Thorbecke already added serious DST pressure:

- random single-shard and multi-shard histories;
- multi-shard TCP + persistence service replay;
- overlap/partial-I/O seed sweeps;
- persistence append/snapshot/recovery matrix;
- supervision + persistence recovery after panic;
- TCP cancellation/tombstone matrix;
- live-vs-sim differential send/stop proof;
- bridge ingress timeout model;
- one shrinker smoke proof.

This is strong evidence, but it is still scattered test code. Stuga extracts
the reusable shape.

## Non-Goals

- No new scheduler.
- No hidden randomness. Every failing generated run must name the seed and
  history.
- No fuzzing-only proof. Generated tests supplement hand-authored semantic
  proofs; they do not replace them.
- No broad performance benchmark phase.
- No public release story. Gemini still waits until Tina is boring under
  real app pressure.
- No magic "fallback" mode. If a workload cannot run under a harness, the test
  says so explicitly.

## Design Principles

1. **Histories are data.**
   A generated workload should produce a printable, replayable operation list.
   The operation type may be workload-specific, but the harness should own the
   run/replay/shrink loop.

2. **Artifacts are receipts.**
   A failure report should include seed, config, operation history, checker
   failure, event record, final virtual time, peer output, and durable image
   when relevant.

3. **Checkers are reusable.**
   Common invariants should live in one library surface:
   - event IDs are monotonic;
   - causal links point backward and exist;
   - every send attempt settles as accepted or rejected;
   - every runtime call attempt settles as completed, failed, or rejected;
   - no handler runs after isolate stop/panic for that shard-local identity;
   - no untraced message drop;
   - bounded pressure appears as `Full`, `Closed`, `StorageFull`, or a named
     rejection;
   - persistence durable images remain replayable after accepted appends.

4. **Shrinking is part of the contract.**
   A failed generated run should be reducible by deleting irrelevant operations.
   Start with deletion shrinking. Add operation simplification only where it
   pays for itself.

5. **Differential tests compare semantics, not raw traces.**
   Explicit runtime, simulator, and Betelgeuse-backed runner have different
   scheduling details. Differential harnesses compare projected outcomes:
   delivered values, visible `Full`/`Closed`, terminal call counts, recovery
   results, and allowed ordering constraints.

6. **DST must cover user-shaped flows.**
   Tiny models are useful, but the harness must also run app-shaped scenarios:
   TCP ingress, bounded worker pressure, cross-shard sends, persistence,
   cancellation, shutdown, and recovery.

## Public/Internal Surface

Prefer an internal-but-stable test API first. Public docs can come later.

Expected module shape:

```rust
use tina_sim::dst::{
    DstRun, History, HistoryRunner, InvariantSuite, ShrinkConfig,
    SemanticProjection,
};
```

Possible types:

- `History<Op>`: owned list of workload-specific operations plus seed/config.
- `HistoryRunner<Op, Output>`: trait or closure wrapper that executes one
  history and returns `DstRun<Output>`.
- `DstRun<Output>`: output projection plus replay artifact.
- `InvariantSuite`: reusable checker collection over runtime events.
- `ShrinkConfig`: maximum attempts, deletion-only for first slice.
- `ShrunkFailure<Op>`: original history, shrunk history, failure reason,
  artifact.
- `SemanticProjection`: trait for comparing live/sim outcomes without requiring
  byte-identical traces.

Keep this small. Do not invent a property-testing framework bigger than Tina.

## Build Steps

### 1. Audit Existing DST Tests

Inventory all generated/replay/checker tests across `tina-sim`, `tina-runtime`,
and `tina-tokio-bridge`.

Classify each as:

- scenario proof;
- generated history;
- replay artifact proof;
- checker failure proof;
- live-vs-sim differential;
- model-only bridge/resource proof.

Done means Stuga starts by deleting duplication only after we know what each
test currently proves.

### 2. Add `tina_sim::dst` Test Harness Core

Implement the smallest shared harness:

- `History<Op>`;
- `DstRun<Output>`;
- `run_twice_same_history`;
- `assert_replays`;
- deletion shrinker;
- failure report formatting.

Keep generic bounds simple: `Op: Clone + Debug`, `Output: PartialEq + Debug`.

Do not require `serde` unless a later review proves persisted artifacts are
needed now.

### 3. Move Common Invariants Into Library Helpers

Add reusable invariants:

- `events_are_monotonic`;
- `causes_point_backward`;
- `send_attempts_settle`;
- `call_attempts_settle`;
- `no_handler_after_stop`;
- `no_untraced_abandonment`;
- `persistence_image_replays`.

Use them from existing tests. Leave custom checkers where domain-specific
ordering matters, such as "TCP write after journal append."

### 4. Replace Bespoke Random Tests With Harness Calls

Refactor `dst_randomized.rs` first:

- random single-shard histories use `History`;
- random multi-shard histories use `History`;
- shrinker smoke uses shared shrinker;
- failure messages print seed and minimal operation list.

This proves the harness is useful without changing runtime semantics.

### 5. Persistence Fault Injection In Simulator

Add explicit simulator-only storage fault knobs, narrowly scoped:

- fail next/selected `JournalAppend`;
- fail next/selected `SnapshotCommit`;
- truncate committed journal tail;
- corrupt committed journal record;
- produce commit-uncertain snapshot result if the simulator supports that shape
  honestly.

These are not live filesystem claims. They are deterministic durable-image
faults for recovery semantics.

### 6. Persistence Crash Matrix As History

Rewrite the persistence matrix as a `History<PersistenceOp>` with seeded
operations:

- mutate;
- bad append;
- snapshot;
- recover;
- injected truncate/corrupt;
- panic after append;
- stop/restart service.

The proof must assert:

- accepted appends are replayable;
- rejected appends do not mutate durable state;
- corrupt durable images produce visible recovery failure;
- truncated tails produce visible warning and recover the prefix;
- supervision recovery after panic is replayable.

### 7. TCP/Resource Cancellation Matrix As History

Rewrite accept/read/write cancellation stress as `History<IoOp>`:

- bind;
- accept;
- read;
- write;
- close listener;
- close stream;
- stop requester;
- advance time/step;
- inject TCP completion delay/reorder.

The proof must assert:

- no pending in-flight call remains after requester stop;
- late tombstoned completions do not deliver user messages;
- resource lanes stay separate where intended;
- same-lane duplicate ops fail `ResourceBusy`;
- invalid/closed resources fail visibly.

### 8. Live-vs-Sim Differential Harness

Create a small differential helper:

```rust
assert_live_sim_equivalent(
    sim_runner,
    explicit_runner,
    betelgeuse_runner,
    projection,
);
```

Start with two workloads:

- retry timer workload;
- send/stop/closed-rejection workload.

Then add one richer workload if feasible:

- bounded worker pressure without TCP; or
- LocalApp-style service projection.

Do not compare raw traces. Compare semantic projections.

### 9. Bridge Model DST

Keep bridge model DST in `tina-tokio-bridge`, but use the shared history and
shrinker shape where possible.

Prove:

- bounded ingress full is visible;
- caller timeout cancels caller wait;
- queued cancelled work is skipped before user state mutation;
- retry policy remains bounded;
- close makes future calls closed.

This is model DST, not a claim that Tokio itself is deterministic.

### 10. Failure Artifact UX

Make failed DST output useful to grug:

- print seed;
- print original history length;
- print shrunk history length;
- print minimal operation list;
- print checker failure reason;
- include event IDs around failure.

This can be plain `Debug` output in panic messages. No fancy reporter.

### 11. CI/Long-Run Rails

Add two test tiers:

- normal tests: fixed short seed set, runs in `make verify`;
- optional long DST sweep behind env var, for local/manual CI:
  `TINA_DST_LONG=1 cargo test -p tina-sim dst_long`.

Normal verify must stay boring. Long sweep may be slower but must still be
deterministic.

### 12. Docs/Closeout Notes

Update `SYSTEM.md` only for committed semantic rules:

- generated histories must be replayable by seed/history;
- DST failures should shrink or explain why they cannot;
- simulator storage faults are simulator-only claims;
- differential tests compare projections, not raw trace identity.

Move completed roadmap notes to `CHANGELOG.md` only when implemented.

## Proof Set

Minimum proof set before closeout:

- `cargo test -p tina-sim --test dst_randomized`
- `cargo test -p tina-sim --test persistence_simulation`
- `cargo test -p tina-sim --test io_simulation`
- `cargo test -p tina-sim --test betelgeuse_parity`
- `cargo test -p tina-tokio-bridge --test bridge_model_dst`
- `make verify`

New/updated named tests expected:

- `dst_harness_replays_same_history_and_shrinks_failure`
- `common_invariants_catch_broken_causality_fixture`
- `persistence_history_replays_and_recovers_after_fault_injection`
- `io_history_cancels_tombstones_and_preserves_resource_lanes`
- `bridge_history_skips_cancelled_queued_work`
- `live_sim_projection_matches_for_send_stop_and_timer_workloads`

## Done Means

- DST harness exists as reusable `tina_sim::dst` surface.
- Existing random DST tests use the harness.
- Common invariants are not copy-pasted across test files.
- At least one failure shrink proof exists and produces a smaller history.
- Persistence/storage fault injection exists in simulator with clear non-live
  claims.
- TCP/resource cancellation history uses harness and proves no hidden pending
  work.
- Bridge model DST uses same history/shrink discipline.
- Live-vs-sim differential harness compares semantic projections for at least
  two workloads.
- `make verify` passes.

## Pause Gates

Pause and discuss if:

- `tina_sim::dst` wants to become public API rather than test-support API;
- adding `serde` or external property-testing crates looks tempting;
- storage fault injection would require changing persistence semantics;
- live-vs-sim projection hides a real semantic mismatch;
- shrinker complexity grows beyond deletion shrinking;
- normal `make verify` runtime grows noticeably.

## Non-Claims After This Phase

- Tina still does not prove arbitrary user programs correct.
- DST still explores bounded generated histories, not every possible
  interleaving.
- Simulator storage faults are not real filesystem crash consistency proofs.
- Bridge model DST is not Tokio determinism.
- Gemini remains blocked until the local core and ergonomics are ready enough
  for real migration attempts.
