# 023 Review

Session:

- B (review)

## Plan Review 1

Artifact reviewed:

- `.intent/phases/023-huygens-dst-runtime-substrate/plan.md`

Reviewed against `.intent/SYSTEM.md`, the delivered Kepler/Galileo posture, and
the IDD habit used by the prior phase reviews: name the claim, name the
non-claim, pin the proof surface, and do not let examples stand in for proof.

Verdict: right phase, not quite implementation-ready. The plan has the correct
instinct: do not do Gemini release-story work until Tina has both systematic
composed DST proof and an actual runtime path users can try. The weak parts are
mostly pinning problems. A few "or" branches still let implementation close on
the smaller half while the closeout sentence sounds like the bigger half.

### What looks strong

- Phase identity is clear: Huygens exists to prove the framework story before
  Gemini documents it or Apollo bridges it.
- The final claim is honest and useful: shared-nothing, shard-owned,
  deterministic proof pressure, and a selected-runtime path for Tokio-shaped
  workloads.
- The plan refuses the right temptations: no release contract, no bridge-first
  move, no async handlers, no unbounded queues, no production/full-Tokio
  replacement claim.
- It keeps explicit-step runtime/simulator as the semantic oracle instead of
  letting the new substrate become a second meaning model.
- The workload list is user-shaped rather than helper-shaped: dispatcher,
  retry, TCP, sessions, bad-peer survival.
- The proof posture says trace assertions and replay, not logs.

### What is weak or missing

1. **Runtime substrate shape is still too loose.**
   The plan says "one runtime worker per shard, or a current-thread stepping
   runner with an honest path to thread-per-core." That "or" is dangerous. A
   current-thread runner can be useful, but it does not earn the phrase
   "shared-nothing, thread-per-core concurrency framework." Pin the expected
   substrate now: either Huygens lands one OS-thread runtime worker per shard
   with bounded ingress/cross-shard queues, or it explicitly narrows the
   closeout claim. Current-thread may remain a stepping/oracle helper, but it
   should not satisfy the runtime-substrate bar by itself.

2. **The runtime-substrate crate/API boundary is not named.**
   The plan says "add an actual runtime substrate path" but does not say where
   it lives or what the user touches. Before implementation, pin the smallest
   public shape: likely an additive `tina-runtime` runner type or a new runtime
   crate, plus exact names for construct/register/run/shutdown/try_send. This
   matters because SYSTEM.md says `tina` must not grow scheduler helpers. The
   plan should explicitly keep the new substrate out of `tina` unless a real
   semantic hole forces a reviewed boundary change.

3. **Tokio-shaped reference workload is not selected.**
   "At least one" workload from TCP echo, retry, dispatcher, sessions leaves too
   much room. Pick the load-bearing reference now. My read: choose TCP
   echo/request-response if runtime-owned I/O is ready enough, because it proves
   scheduler + bounded queues + runtime-owned I/O + user-recognizable shape. If
   that is too big, choose dispatcher/worker plus timer-backed retry and narrow
   the claim away from I/O. Do not defer this to implementation taste.

4. **Oracle/substrate parity is underspecified.**
   The plan asks for parity "where deterministic comparison is possible." Good
   caution, but too easy to dodge. Pin the required comparable facts for the
   selected workload: final app state/output, accepted/rejected send counts,
   bounded-full behavior, restart count if supervision participates, and a
   trace-event subset that both engines expose. Wall-clock ordering does not
   need to match; semantic outcomes do.

5. **DST harness artifact is not concrete enough.**
   "Add a DST harness layer or helpers" could mean a few test functions hidden
   in one module, or a real reusable harness. Huygens should pin a minimal
   artifact: a small harness module that runs named workloads under seed,
   replay, checker, and explicit-step/runtime engines, and produces the closeout
   matrix. The goal is not a broad random-chaos DSL, but the helper should be
   reusable enough that future phases stop hand-rolling e2e proof setup.

6. **Composed workload minimum is not pinned.**
   The plan lists seven composed primitive families, but build step 3 says
   "tests that combine..." without saying how many must land. Pick a required
   minimum set. Suggested minimum:
   - dispatcher/restart/cross-shard/bad-address survival
   - timer retry plus supervision/restart
   - TCP completion plus bounded mailbox/full or stopped requester
   - one checker-induced replayable composed failure
   That is enough to make "composed DST pressure" real without pretending every
   pairwise combination is done.

7. **Backpressure proof on the live substrate needs exact failure points.**
   Bounded backpressure is a core Tina promise, so Huygens should prove the live
   substrate rejects pressure at the same visible places as the oracle: bounded
   ingress, local mailbox full, and cross-shard transport full. If the first
   runtime substrate only supports one shard, say so and require bounded ingress
   + local mailbox full there, with cross-shard full still proved by the
   explicit-step multi-shard runner.

8. **Shutdown/quiescence semantics are missing.**
   A runtime users can try needs an honest stop condition. The plan should pin
   how the live substrate exits tests: explicit shutdown handle, run-until-idle
   for test only, root-isolate stop, or timeout-backed test driver. Without
   this, e2e tests may become sleeps and joins, which would violate the plan's
   own "trace assertions, not logs" spirit.

9. **SYSTEM.md update should be mandatory.**
   A new actual runtime substrate changes the shipped-system description. Even
   if `tina` APIs stay unchanged, SYSTEM.md must gain the substrate boundary,
   the claim boundary, and the explicit statement that explicit-step remains
   semantic oracle while the live substrate is an execution path, not a second
   model.

10. **Done Means should require a closeout artifact.**
    The plan mentions the matrix, but Done Means does not require it as a file.
    Add `.intent/phases/023-huygens-dst-runtime-substrate/closeout.md` with the
    workload x engine matrix, exact test names, substrate claim, non-claims, and
    `make verify` result. That keeps this from closing on vibe.

### Suggested plan edits before implementation

- Replace the runtime-substrate "or" with a clear expected direction:
  OS-thread worker per shard is the target; current-thread runner is only an
  implementation stepping stone or a narrowed-claim fallback.
- Name the substrate boundary and user-facing runner shape.
- Pick the reference workload now.
- Pin the minimum composed workload proof set.
- Require exact parity facts, exact backpressure failure points, mandatory
  SYSTEM.md update, and a closeout matrix artifact.

After those edits, Huygens is ready to implement.

## Implementation Review 1

Artifact reviewed:

- `tina-runtime-current/src/lib.rs`
- `tina-runtime-current/tests/tcp_echo.rs`
- `tina-sim/tests/huygens_dst_harness.rs`
- `.intent/SYSTEM.md`
- `.intent/phases/023-huygens-dst-runtime-substrate/audit.md`

Reviewed against the amended Huygens plan: one worker-owned shard runtime,
bounded ingress, user-shaped TCP reference workload, DST/replay/checker
composition, explicit-step oracle remains semantic source of truth, and no
live cross-shard claim until implemented.

Session C follow-up: live fixed-shard transport is now implemented through
`ThreadedMultiShardRuntime`. The older single-shard-only note below is kept as
review history, not current phase state.

Verdict: strong first implementation chunk, but not yet Huygens-complete. The
shape is right: `ThreadedRuntime` is additive, lives in `tina-runtime`, starts a
real OS thread that owns its shard runtime, and the TCP echo test now proves a
real user-shaped workload without manual stepping. The DST harness tests also
prove composed simulator pressure instead of isolated helper proof.

### What looks good

- The substrate boundary is honest. `ThreadedRuntime` does not move an existing
  `Runtime` across threads; the worker constructs and owns it on the shard
  thread. That fits the existing `Rc`/`RefCell` runtime internals instead of
  fighting them.
- The implementation does not touch `tina` or handler semantics. Handlers stay
  synchronous and effect-returning.
- `ThreadedRuntime` is additive and narrow: construct, register root,
  supervise, typed ingress, trace snapshot, in-flight query, shutdown.
- The live TCP echo test is the right kind of proof: real socket I/O,
  runtime-owned TCP calls, spawned connection children, trace assertions, and
  final bytes.
- The DST harness test composes supervision restart, timer wake, local send
  perturbation, checker failure, replay, and app observations in one place.
- SYSTEM.md and the audit artifact state the important non-claim: live
  cross-shard transport is not implemented yet; explicit-step multi-shard
  runtime/simulator remain the oracle there.
- `make verify` passes.

### Findings

1. **[P1] `ThreadedRuntime::try_send` can block after bounded admission.**
   `ThreadedRuntime::try_send` uses a bounded `try_send` into the worker command
   queue, but then immediately waits on `reply_rx.recv()` until the worker
   processes that command. If the worker is in a long synchronous handler turn
   or otherwise not returning to command processing, this "try" API can block
   indefinitely after admission. That weakens the bounded-ingress story: queue
   full is visible, but accepted ingress is not a non-blocking handoff. Either
   rename/scope the method as synchronous ingress, or split the surface so the
   bounded handoff returns immediately and mailbox `Full` / `Closed` is observed
   through trace or a separate request/ack path.

   **Status:** fixed in Session C. `try_send` now means bounded handoff only and
   does not wait for the worker to observe the target mailbox. The synchronous
   observed path is named `send_and_observe`.

2. **[P1] Live ingress `Full` is not directly proved.**
   The new tests prove live TCP echo and live closed-mailbox rejection, but none
   forces `ThreadedTrySendError::IngressFull`. Huygens' plan now explicitly
   requires bounded ingress proof on the live substrate. Add a focused e2e that
   fills the bounded command queue while the worker is unable to drain it, then
   asserts `IngressFull` without relying on sleeps.

   **Status:** fixed in Session C by
   `threaded_runtime_try_send_surfaces_ingress_full_without_blocking_on_worker`.

### Non-blocking notes

- The first live substrate being single-shard is acceptable because the audit
  and SYSTEM.md say so plainly. It should not close the whole phase until either
  live cross-shard transport exists or the closeout claim remains explicitly
  single-shard-live / multi-shard-oracle.
  **Status:** fixed in Session C follow-up. Live cross-shard request/reply,
  remote queue `Full`, and stale-remote/non-poisoning are now directly tested.
- `DstHarness` currently lives in a test file as a small reusable helper. Good
  enough for the first chunk. If more Huygens tests start duplicating setup,
  promote it into a small test support module.
- The TCP threaded test uses timeout guards, but the success path exits through
  actual app/client completion and trace state. That matches the plan.
