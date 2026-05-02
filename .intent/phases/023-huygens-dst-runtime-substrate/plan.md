# 023 Huygens DST Harness And Runtime Substrate Plan

Session:

- A

## What We Are Building

Build the phase that turns the proven explicit-step model into a usable
shared-nothing concurrency framework:

> **prove the primitives under systematic DST pressure, then run them on a real
> shard-owned runtime substrate**

Kepler settled the core semantic model. That is not the same as proving the
model survives real composition or giving users a runtime they can try against
Tokio-shaped workloads.

Huygens closes that gap.

At the end of this phase, the repo should be able to say, honestly:

> tina-rs is a shared-nothing, shard-owned concurrency framework for Rust. Its
> core primitives are hammered by a deterministic simulation/testing harness,
> and the same isolate model can run on an actual shard-owned runtime path you
> can try for selected Tokio-shaped workloads today.

That sentence is the bar. If the implementation cannot support it, narrow the
claim or keep working.

Expected direction:

- build a proper DST-style harness over the current primitives
- compose primitives aggressively instead of testing them only one by one
- add user-shaped workloads that run against both semantic oracle and runtime
  substrate where possible
- add an actual shard-owned runtime path, not just more explicit-step tests
- preserve the explicit-step runtime/simulator as the semantic oracle
- keep release/publication/Gemini work deferred until this proof exists

## What Will Not Change

- This phase does **not** publish crates.
- This phase does **not** write the release contract.
- This phase does **not** build the Tokio bridge as the main answer.
- This phase does **not** change the isolate/effect programming model unless
  the runtime substrate exposes a real semantic hole.
- This phase does **not** introduce unbounded queues.
- This phase does **not** turn handlers async.
- This phase does **not** claim full production readiness or broad workload
  replacement.
- This phase does **not** pretend explicit-step multi-shard execution is real
  parallel execution.

## Why This Comes Before Gemini Or Apollo

Gemini needs a contract worth publishing. Apollo needs a stable set of
guarantees to compare against Tokio.

Right now the semantic model is strong, but the proof regime is still too
piecemeal and the runtime substrate is not yet the thing a user can try as a
concurrency framework.

Huygens should make the next claims true:

- the primitives work together under replayable pressure
- the simulator/DST harness is not just a collection of isolated examples
- there is a real shard-owned runtime path beyond the explicit-step oracle
- Tina can be tried against selected Tokio-shaped workloads without pretending
  to be a Tokio bridge

## Core Questions Huygens Should Close

### 1. DST harness completeness

The simulator already has virtual time, seeded perturbation, replay artifacts,
checkers, scripted TCP, supervision replay, and multi-shard replay. Huygens
should turn that into a systematic harness.

The harness should exercise composed primitive families:

- local send + timer wake + supervision
- spawn + restart + stale address + later send
- TCP completion + stopped requester + mailbox full
- cross-shard send + local mailbox pressure + checker failure
- panic/restart + cross-shard message flow
- multi-shard + timer/TCP fault surfaces
- replay + user-defined checker + app-shaped workload

The goal is not to invent broad random chaos. The goal is to make the existing
seeded surfaces act like one coherent DST regime.

### 2. Semantic oracle versus runtime substrate

The explicit-step runtime remains the semantic oracle. It is how we prove
meaning.

Huygens should add an actual runtime substrate path that preserves that meaning
while running in a shape closer to the intended framework:

- shard-owned execution
- one runtime worker per shard, or a current-thread stepping runner with an
  honest path to thread-per-core
- bounded ingress
- bounded cross-shard transport
- no hidden Tokio task migration as the core model
- no unbounded channels
- deterministic/traceable enough to compare against the oracle

The expected substrate is conservative. If monoio/thread-per-core is too big
for this slice, the phase should still land the smallest real runtime path that
lets users run isolates outside test-only explicit stepping while preserving
Tina's ownership and backpressure rules.

If the only feasible path is "still explicit-step," pause. That means Huygens
has not earned the runtime-framework claim yet.

### 3. User-shaped parity workloads

Huygens should stop proving only helpers.

Add or upgrade workloads that look like things users would build:

- task dispatcher with worker restart and cross-shard routing
- retry/backoff coordinator
- TCP echo or request/response server with partial I/O
- multi-tenant/session-style state machine with audit/log side traffic
- bad-peer/bad-address path that does not kill good work

Each workload should run through the most relevant pair of engines:

- simulator plus replay/checker
- explicit-step runtime oracle
- actual runtime substrate, once it exists

Not every workload needs every engine, but the phase must state why any gap is
acceptable.

### 4. Runtime-substrate proof against Tokio-shaped workloads

The phase should include at least one reference workload that a Tokio user
would recognize:

- TCP echo / request-response
- timer-backed retry
- dispatcher/worker queue
- many independent sessions/connections

The point is not "beat Tokio." The point is:

- bounded backpressure is visible
- state is isolate-local
- runtime-owned I/O/time stays out of handlers
- failures are traceable/replayable where the simulator is used
- the runtime path can actually run the workload

### 5. Claim boundary

At closeout, be precise:

Good claims:

- shared-nothing isolate model is implemented
- bounded mailbox and shard-pair queues are implemented
- primitives compose under DST/replay pressure
- a shard-owned runtime path exists for selected workloads
- users can try replacing small Tokio-shaped components where Tina's model fits

Bad claims:

- production-ready
- full Tokio replacement
- every async workload can move today
- full peer quarantine / distributed liveness
- broad allocation-free runtime
- complete formal proof

## Pause Gates

Pause before implementation continues if:

- the runtime substrate wants unbounded queues
- handlers need to become async
- the substrate requires a large public `tina` API redesign
- the DST harness needs a brand-new broad fault language before it can prove
  useful composed workloads
- any user-shaped workload cannot be run against either simulator/replay or a
  runtime path
- a claim starts sounding like release/public positioning instead of evidence
- thread-per-core implementation becomes larger than the proof harness itself

## Build Steps

1. Audit existing simulator/test coverage and list composed primitive gaps.
2. Add a DST harness layer or helpers that make composed workloads easy to run
   under seed/replay/checker pressure.
3. Add composed workload tests that combine supervision, timers, TCP, bounded
   backpressure, cross-shard routing, and replay.
4. Design the smallest actual shard-owned runtime substrate for this phase.
5. Implement the runtime substrate without changing isolate handler semantics.
6. Run at least one user-shaped workload on:
   - simulator/replay
   - explicit-step runtime oracle
   - actual runtime substrate
7. Add parity assertions between oracle/substrate where deterministic
   comparison is possible.
8. Document the exact claims Huygens earns and the exact claims it still
   refuses.
9. Update README/ROADMAP only after the evidence lands.
10. Run `make verify`; add any substrate-specific verification command to the
    standard gate if it is required for the claim.

## Proof Plan

Huygens proof should be e2e-first and replay-backed.

Required proof modes:

- simulator replay tests for composed workloads
- checker failures for at least one composed semantic bug
- live/runtime e2e tests for user-shaped workloads
- oracle/substrate parity tests where feasible
- bounded queue/backpressure tests on the runtime substrate
- panic/restart behavior under composed workload pressure
- trace assertions, not logs
- `make verify`

The phase should also add a closeout matrix:

| Workload | Simulator/replay | Explicit-step oracle | Runtime substrate | Checker |
|---|---|---|---|---|

The matrix is not paperwork. It is the thing that tells us whether the claim
"you can try this framework today" is earned.

## What Huygens Explicitly Defers

- public `0.1.0` release story
- Tokio bridge / Axum adapter
- full production hardening
- benchmark suite as marketing
- real peer quarantine / shard-restarted broadcast semantics
- cross-shard child ownership
- broad allocation-free runtime claims
- every possible runtime backend

## Done Means

Huygens is done when:

- the DST harness exercises composed primitive paths, not just isolated unit
  cases
- at least one replayable checker failure targets a composed workload
- at least one Tokio-shaped workload runs on a real shard-owned runtime path
- that workload has oracle/substrate proof or a clearly justified comparison
  boundary
- bounded backpressure remains visible on the runtime substrate
- handler semantics stay synchronous and effect-returning
- README/ROADMAP can honestly say users can try Tina for selected
  shared-nothing workloads today
- Gemini/release work has a real framework contract to document later
- `make verify` passes
