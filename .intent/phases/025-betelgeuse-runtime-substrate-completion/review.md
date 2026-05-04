# 025 Review

Session:

- B (review)

## Plan Review 1

Artifact reviewed:

- `.intent/phases/025-betelgeuse-runtime-substrate-completion/plan.md`

Reviewed against `.intent/SYSTEM.md`, the Mercury (024) closeout state,
the existing `ThreadedRuntime` / `ThreadedMultiShardRuntime` surface in
`tina-runtime/src/lib.rs:2256+`, and the `tina-runtime/src/io_backend.rs`
Betelgeuse integration. So 025 starts from a real substrate, not a
green field. That changes the slice's shape from "build a substrate" to
"finish + rename + prove the substrate that already exists." Worth
naming this in the plan.

### What looks strong

- Phase identity is named loudly: "Tina has one real live runtime
  substrate path that preserves the Tina rules." That is the right
  end-state claim for this phase.
- Refusals are sharp:
  - no async user handlers
  - no arbitrary futures
  - no `Arc<Mutex<Runtime>>`
  - no unbounded queues
  - no Tokio bridge or Tower/Axum adapter
  - no broad production claim
  - no zero-allocation claim outside measured paths
  These match the Tina-Odin spirit and the SYSTEM.md committed
  constraints.
- "Live runner interprets Tina, it does not become Tina" is the right
  framing. Tina owns isolate state, one-message-at-a-time execution,
  effect interpretation, bounded admission, trace vocabulary. The
  backend owns time, sockets, completion delivery, worker wakeups.
  That separation is load-bearing and named here.
- Oracle parity is required, not optional. Same isolate workload runs
  under explicit-step, simulator, and Betelgeuse live runner.
  Bytewise differences must be named, not handwaved.
- Cross-shard isolate-call is "implement and prove" or "explicitly
  reject in a typed, tested way." No half-claim. Right discipline.
- Allocation cost is classified into 4 categories rather than a
  vague "low overhead" claim. Plan asks for focused numbers on 5
  named hot paths.
- Pause gates name the right escape valves: async handler creep,
  unbounded queues, shared mutable state, semantic event-model
  changes forced by backend, cross-shard call protocol balloon,
  drift toward Tower/Axum/Tokio bridge/release docs. Substrate
  choice change is itself a pause gate.
- Done Means is explicit and pins the final runner names, the
  oracle/sim/live composed workload, the cross-shard call
  decision, and the SYSTEM/ROADMAP docs to update. No marketing
  language.
- Non-Claims After This Phase is sharp. "Production parity with
  Tokio/monoio/glommio" is explicitly *not* claimed. Worth
  preserving.

### What is weak or missing

1. **Plan does not name the starting baseline.**
   `ThreadedRuntime` and `ThreadedMultiShardRuntime` already exist
   in `tina-runtime`. Betelgeuse-backed I/O already exists in
   `tina-runtime/src/io_backend.rs`. So 025 is a completion +
   rename + parity-proof phase, not a green-field substrate phase.
   The slice size depends on what the audit (build step 1) finds
   missing. Plan should state the starting baseline plainly:
   "ThreadedRuntime, ThreadedMultiShardRuntime, and a Betelgeuse
   io_backend already ship; 025 finishes naming, completes the
   missing live-path proofs, and pins allocation/cost numbers."
   Without this, a reviewer cannot tell whether the plan is small
   (rename + 8 tests) or large (rename + 30 tests + a new
   composed workload + DST bridge audit).

2. **Cross-shard isolate-call expected direction is unpinned.**
   "Implement or explicitly reject" is the right disjunction, but
   the plan does not say which way it expects the decision to go.
   For comparison with prior slices: 020 explicitly deferred
   cross-shard perturbation to a later phase. Mercury's 022 plan
   review made the same finding for liveness-vs-supervision. Pin
   the expected direction. My recommendation: explicitly reject
   in this slice, defer to a later substrate-routing phase. The
   non-claims list already implies this; make it a build-step
   decision rather than an implementation-time choice.

3. **DST hook is conditional on Betelgeuse exposing one.**
   Build step 8 says "if Betelgeuse does not expose the needed
   hook, record the limitation." That is honest but invites a
   closeout where the DST bridge ends up "Betelgeuse does not
   support this; sim DST stays separate" with no further
   evidence. Pin a fallback expectation: best case is one
   composed workload through a real Betelgeuse hook; worst case
   is a recorded limitation plus an explicit decision about
   whether to keep simulator DST as the sole reproducibility
   path or build a Tina-side fault-injection layer over the
   live runner. Don't let the slice close on "we looked, it
   wasn't there, oh well."

4. **Allocation probe measurement tier is unstated.**
   Build step 10 says "pin counts where stable." Pick the tier:
   - allocation count via global-allocator probe (the existing
     pattern in `multishard_allocation.rs`)
   - wall-clock latency
   - both
   The 5 named hot paths each need a tier. Pin: "allocation
   counts via the existing global-allocator probe pattern; we do
   not claim wall-clock numbers in 025." Otherwise the
   implementation will probably do counts (because the harness
   exists) but the closeout will be unable to say what was
   actually measured.

5. **Composed workload appears twice without saying whether it
   is the same workload.**
   Build step 7 ("Run the same user-shaped isolate workload under
   oracle/sim/live") and build step 8 ("Add a composed workload
   that pressures timer, TCP, send, and restart or call behavior
   through that hook"). Are these the same workload or two? Pin.
   My recommendation: same workload, runs under oracle/sim/live in
   step 7, pressured under Betelgeuse DST (where available) in
   step 8. Otherwise you end up with two near-identical workloads
   and the closeout has to say which one is the canonical
   reference.

6. **Live-only differences are not enumerated.**
   "Where wall-clock/live behavior cannot be bytewise identical,
   the plan requires a named, narrow difference." Good
   discipline, but the plan does not list which differences are
   expected. At minimum the known ones:
   - timer fires at `Instant::now() + after` vs simulator's
     `virtual_now + after`; the wall-clock variance is non-zero
   - real TCP errors (ECONNRESET, EADDRINUSE) can surface;
     scripted-peer simulator never does
   - peer-side bytes are real network; the simulator side is
     captured `ObservedPeerOutput`
   - shard worker scheduling is OS-controlled; oracle uses a
     deterministic step loop
   List these in the plan so the implementation review knows
   which differences are "expected and named" vs "unexpected
   and a bug."

7. **Worker panic shape is not pinned.**
   "Worker panic and backend error must become visible
   outcomes." But what shape? Trace event (e.g.,
   `WorkerPanicked`), typed result on the runner handle
   (`ThreadedControlError::WorkerPanicked`), or both? The shape
   determines whether tests can deterministically assert on it.
   Pin. My read: both — trace event for downstream consumers,
   typed result on `join` / `shutdown` for the harness side.

8. **Graceful-shutdown semantics are unspecified.**
   "Graceful runner shutdown with outstanding resources" is in
   the required surface, and "graceful shutdown with outstanding
   timer/TCP" are required tests. But the *behavior* is not
   pinned:
   - do outstanding timers/TCP ops cancel and surface as
     `RequesterClosed`?
   - do they drain to completion before the runner stops?
   - hybrid (cancel async ops, finish sync ops)?
   Pin a rule before testing it. Without this, the test will
   inevitably encode whatever the implementation defaulted to,
   and "shutdown semantics" stays inferred-from-tests rather
   than written down.

9. **Multi-shard live coordinator is unspecified.**
   Each shard is a thread. Cross-shard sends go through bounded
   handoff queues. Who drives the harvest on the destination
   shard? Receiving thread polls its inbound queues at the start
   of each step? A central coordinator? Pin one model and cite
   the matching simulator coordinator (the plan-text mentions
   ascending source-shard order from 020's semantics). My read:
   each shard worker polls its inbound queues at the start of
   each interpret loop, in ascending source-shard order, exactly
   like the simulator. State this so the trace shape matches by
   construction.

10. **`BetelgeuseRuntime` rename — what about internal callers
    and the multi-shard sibling?**
    The plan says "no compatibility alias is required." Fair.
    But internal callers in tests, examples, README, and the
    multi-shard simulator's `register_with_capacity_on` etc. all
    reference `ThreadedRuntime`. The rename is additive in code
    if you do it carefully (rename + update callers in the same
    diff), but the plan should say "rename in place; update all
    in-repo callers; no aliases survive" so a future closeout
    review has a sharp gate.

11. **`make verify` includes loom; loom does not reach the live
    multi-thread path.**
    Loom interleaves `loom::sync` primitives in a single-process
    model checker. Live Betelgeuse is real OS threads with real
    parking. Loom won't reach the cross-shard handoff under
    `BetelgeuseMultiShardRuntime`. Plan should say "loom remains
    scoped to SPSC mailbox semantics; live multi-thread proof
    comes from the Betelgeuse runner under direct tests, not
    from loom." Otherwise a reader will assume loom covers more
    than it does.

12. **No-blocking proof technique is unstated.**
    "No `try_*` method blocks after successful bounded
    admission." Good rule. How is it tested? You cannot
    trivially prove "this method does not block" with a normal
    Rust test. Options:
    - timing assertion under a tight deadline (flaky)
    - structural proof via reading the code (not a test)
    - a probe that fills the queue and asserts a single
      `try_send` returns within N microseconds without sleeping
    Pin a technique. My recommendation: structural review +
    one probe per `try_*` method that asserts immediate return
    under saturation; document that this is "tight wall-clock
    proof, not formal."

13. **Substrate choice is a strategic bet; pause-gate is in but
    rationale could be stronger.**
    "If the backend choice changes away from Betelgeuse, pause."
    Good. But the plan should also say plainly: "Betelgeuse is
    a small ecosystem; we accept that lock-in for one slice.
    monoio/compio are still future-substrate candidates; we are
    not building a backend abstraction in 025 because we have
    only one live backend." That makes the bet explicit rather
    than implicit.

14. **The proof set lists 18 categories, plus the matrix lists
    cells across 3 engines.**
    That is 30+ tests of the size 016-024 produced. This is a
    large slice. Plan should acknowledge the size: "025 is the
    largest test-surface slice to date because it must prove
    the same semantics on three engines." Otherwise the
    closeout reviewer applies a 016-sized bar to a much larger
    slice.

15. **Trace deterministic-enough-for-assertions claim.**
    Build step 4 says "Trace collection must be deterministic
    enough for assertions." On a multi-thread live runner, event
    ordering across shards is OS-scheduled. Within a shard,
    ordering is deterministic. Across shards, you get whatever
    the OS scheduler did. Pin: "per-shard trace ordering is
    deterministic; cross-shard ordering is asserted at the level
    of paired source/destination event presence and per-shard
    causality, not as a fully sorted global event sequence."
    Otherwise oracle-parity assertions will be flaky on the
    live runner.

### How this could still be broken while the listed tests pass

- **Audit closes "Betelgeuse DST hook does not exist for our
  paths"; the slice ships without any Betelgeuse-side DST proof.**
  The simulator-only DST proof from earlier phases stays. The
  plan technically allows this. Closeout reads "DST hook
  status: unavailable for current paths" with no further
  consequence. Mitigation: finding 3 — pin a fallback decision.
- **Cross-shard call ends up "implemented" with a partial
  reply-transport that works for the dispatcher workload but
  not under load or with timeout interleavings.** The proof
  matrix says "implement or reject clearly," but "implement"
  with weak proof passes the listed tests. Mitigation: finding
  2 — pin the expected direction (probably reject in 025) so
  "implement" is a deliberate add, not a default.
- **Allocation probes assert non-zero counts everywhere.** Same
  pattern as `multishard_allocation.rs` today — allocations
  exist, the probe asserts that they exist. A future allocation
  reduction breaks the test in the right direction (probe asks
  to update the claim). But a future allocation *increase*
  passes silently. Mitigation: pin counts where stable, not
  just the lower bound. The plan says this; the implementation
  needs to honor it.
- **Oracle-parity proof passes because the tests assert only
  final outputs and per-shard event classes, not full event
  records.** A live-only ordering bug between shards would
  pass. Mitigation: finding 6 — name the differences in
  advance so the test surface knows what to compare.
- **Worker panic produces a trace event but the multi-shard
  runtime continues with the remaining shards** (or vice versa),
  and no test pins which is intended. Mitigation: finding 7 +
  a test for "panic on shard A; assert shard B continues" or
  "panic on shard A; assert the runtime tears down."
- **Graceful shutdown with outstanding timer is tested via
  "drive shutdown; assert no panic; assert trace shows
  IsolateStopped events."** That is a smoke test, not a
  semantic check. The behavior — cancel vs drain vs hybrid —
  is implicit. Mitigation: finding 8 — pin the rule in plan.
- **Live ingress under bounded `command_capacity` rejects with
  `IngressFull`, but the test sleeps to allow the worker to
  drain.** Sleeps as proof. Plan explicitly forbids "sleeps as
  proof." Need a synchronization-barrier-based test pattern.
  Mitigation: build step 3 says it; the implementation must
  honor it.
- **Loom continues to pass on SPSC; the live multi-thread path
  has a race that loom does not reach.** Mitigation: finding
  11 — say plainly that loom does not cover this.
- **Allocation probe baseline drifts because allocator behavior
  differs between debug and release builds.** Mitigation: pin
  the build profile under which probes run. The existing
  `multishard_allocation.rs` runs under the default profile;
  worth saying the same.

### What old behavior is still at risk

- 016-020 explicit-step suites: the rename pass touches every
  test that types `ThreadedRuntime`. If the rename is sloppy,
  some tests may end up using the new name on the runner and
  the old name in trait bounds, leading to confusing errors.
  Implementation review should treat compile errors during the
  rename pass as a non-issue (they're correctness gates), but
  treat any test rewrite that drifts behavior as a regression.
- Mercury (024) `tokio_vs_tina_examples.rs` and
  `consumer_api.rs`: both reference current-thread `Runtime`
  and `MultiShardRuntime` for the explicit-step paths. The
  rename should not touch those types — the rename is about the
  *live* runner, not the explicit-step semantic runtime. Pin
  this in the plan.
- The new `CallReplyRejected { NoPendingCall }` trace event
  from 024 needs to behave the same way under the live
  Betelgeuse runner. Worth a focused test under the live
  runner that exercises a late reply after timeout. The proof
  set lists "late completion after timeout is traced and not
  delivered as success" — make sure that includes
  `CallReplyRejected`, not just timeout.
- The 020 stable-shard-ownership rule under multi-shard live
  execution. Live worker-per-shard cannot move isolates
  between shards mid-flight. The plan says cross-shard live
  semantics are core if they block thread-per-core, which they
  do, but the stable-ownership invariant is not mentioned.
  Pin: same rule, stable across the live runner's lifetime.

### What needs a human decision

- **Cross-shard isolate-call: implement in 025, or reject in
  025?** My recommendation: reject in 025; the live coordinator
  is already a big enough surface, and cross-shard call needs
  its own protocol slice once the substrate is stable. The
  rejection should be typed and tested (e.g.,
  `BetelgeuseRuntime::call(...)` returns `CallOutcome::Closed`
  with a docstring saying "cross-shard isolate calls are not
  supported by the live runner in this phase").
- **Betelgeuse DST hook usage: target one composed workload, or
  accept "unavailable" as the outcome?** My recommendation:
  audit first; if a hook exists, target one workload; if not,
  ship simulator-DST-only with an explicit closeout note that
  Tina-side fault injection over the live runner is a later
  phase.
- **Multi-shard live coordinator shape: receiving-shard-worker
  polls vs central coordinator?** My recommendation:
  receiving-shard-worker polls, mirroring the simulator's
  `MultiShardSimulator::step()` loop in the per-shard
  interpret loop.
- **Worker-panic propagation: tear down the multi-shard runner
  or continue with surviving shards?** My recommendation: tear
  down. Tina-Odin treats shard-level panic as catastrophic.
  Test both the panic event and the shutdown.

### Recommendation

Plan is structurally on-shape. The phase identity, fences, oracle
parity discipline, allocation discipline, and pause gates are all
right. The non-claims list is sharp. The slice is large but
deliberately so: substrate completion plus oracle parity plus
allocation probes is genuinely the work of one big phase.

Not yet ready to hand off to implementation. The structural
findings (load-bearing) are 1, 2, 3, 4, 7, 8, 9 — all "pin a
decision" rather than "rethink the slice." None require new
substrate work or new boundary changes.

Amend the plan before implementation begins to:

1. Name the starting baseline: `ThreadedRuntime` /
   `ThreadedMultiShardRuntime` and Betelgeuse io_backend already
   ship.
2. Pin the expected direction for cross-shard isolate-call
   (recommended: reject in 025 with a typed/tested outcome).
3. Pin a fallback for Betelgeuse DST hook unavailability
   (recommended: ship simulator-DST-only with an explicit
   closeout note).
4. Pick the allocation-probe measurement tier (recommended:
   allocation counts via the existing global-allocator probe
   pattern, debug profile, no wall-clock claim).
5. State whether the composed workload in steps 7 and 8 is one
   workload (recommended) or two; cite by name if reusing 020's
   dispatcher.
6. Enumerate the expected oracle-vs-live differences (timer
   wall-clock variance, real TCP errors vs scripted peer,
   captured peer output vs real bytes, OS-scheduled cross-shard
   ordering).
7. Pin the worker-panic visibility shape (recommended: trace
   event + typed result on the handle).
8. Pin the graceful-shutdown rule for outstanding timers / TCP
   ops (recommended: cancel async ops, surface as
   `RequesterClosed`-shaped events; finish sync ops inline).
9. Pin the multi-shard live coordinator shape (recommended:
   receiving-shard-worker polls inbound queues at start of each
   interpret loop in ascending source-shard order).
10. Confirm the rename touches only the *live* runner names, not
    the explicit-step `Runtime` / `MultiShardRuntime`.
11. State plainly that loom remains scoped to SPSC; live
    multi-thread proof does not come from loom.
12. Pin the no-blocking proof technique (recommended: structural
    review + a saturation probe per `try_*` method asserting
    immediate return; documented as wall-clock-tight, not
    formal).
13. Acknowledge the substrate-choice bet (recommended: one
    backend, no abstraction in 025; monoio/compio defer).
14. Acknowledge the slice size: largest test-surface slice to
    date because the same semantics ride three engines.
15. Pin per-shard trace determinism vs cross-shard event-presence
    semantics for oracle-parity assertions.
16. Carry forward 020's stable-shard-ownership rule under the
    live runner.
17. Pin the build profile for allocation probes (recommended:
    debug, matching the existing `multishard_allocation.rs`).

Items 1, 2, 3 are the load-bearing structural pins. 4-9 are the
shape-pinning decisions that protect the closeout review. 10-17
are tightenings.

After those amendments, 025 is reviewable as a real substrate-
completion phase rather than as an open-ended substrate
discussion. None of the findings imply a `tina` boundary change
or a substrate redesign.

## Implementation Evidence

025 started from an existing live substrate, not empty ground:

- `ThreadedRuntime` / `ThreadedMultiShardRuntime` existed as live runner shells.
- the runners owned `Runtime` on worker threads.
- ingress already used bounded `sync_channel` command queues.
- runtime-owned TCP already flowed through the Betelgeuse-backed `IoBackend`.

The live runner surface was renamed without compatibility aliases:

- `ThreadedRuntime` -> `BetelgeuseRuntime`
- `ThreadedMultiShardRuntime` -> `BetelgeuseMultiShardRuntime`
- `ThreadedRuntimeConfig` -> `BetelgeuseRuntimeConfig`
- `ThreadedTrySendError` -> `BetelgeuseTrySendError`
- `ThreadedSendObservedError` -> `BetelgeuseSendObservedError`
- `ThreadedControlError` -> `BetelgeuseControlError`

The Betelgeuse pause gate was real: vendored Betelgeuse had the right
completion-loop shape, but no concrete simulation/fault backend. 025 chose the
no-fallback path and added the missing narrow hook:

- `betelgeuse::io::simulated::SimulatedIO`
- deterministic TCP bind, accept, read, write, close
- deterministic peer handles
- seeded completion delay
- deterministic partial-write pressure
- backend tests for round-trip, partial write, and delayed completion
- Tina TCP echo tests over the simulated backend on both explicit `Runtime` and
  threaded `BetelgeuseRuntime`

Final engine matrix:

| Engine | Purpose | Direct Proof |
|---|---|---|
| `Runtime` | explicit-step semantic oracle | runtime unit/integration suites, TCP echo, call, timer, supervision |
| `tina-sim` | deterministic replay/DST oracle | seeded timer/send/TCP/supervision/multi-shard replay suites |
| `BetelgeuseRuntime` | live one-shard worker substrate | live timer retry, TCP echo, ingress full, shutdown, worker panic |
| `BetelgeuseMultiShardRuntime` | live worker-per-shard substrate | dispatcher round-trip, remote queue full, bad remote survival |
| `SimulatedIO` | deterministic substrate I/O backend | direct backend tests plus Tina TCP echo over explicit and threaded runtime |

Cross-shard isolate calls are rejected on the live runner with a typed/tested
outcome. Live cross-shard isolate-call reply transport remains later work.

Allocation pins:

- multi-shard runtime path: `15` allocations, `2` reallocations
- isolate-call hot path: `9` allocations, `1` reallocation
- Betelgeuse ingress handoff on caller thread: `1` allocation, `0` reallocations

Final verification:

- `cargo +nightly test -p betelgeuse --test simulated_io`
- `cargo +nightly test -p betelgeuse`
- `cargo +nightly test -p tina-runtime --lib`
- `cargo +nightly test -p tina-runtime --test tcp_echo simulated -- --nocapture`
- `cargo +nightly test -p tina-runtime --test tcp_echo`
- `cargo +nightly test -p tina-runtime --test betelgeuse_substrate`
- `cargo +nightly test -p tina-sim --test betelgeuse_parity -- --nocapture`
- `make verify`

Remaining non-claims: general Tokio replacement, Tower/Axum integration,
arbitrary async ecosystem compatibility, broad live-substrate liveness fault
simulation, thread pinning/topology, peer quarantine, shard-restart
propagation, cross-shard child ownership, live cross-shard isolate-call reply
transport, production performance parity, and broad zero-allocation runtime.
