# 025 Betelgeuse Runtime Substrate Completion Plan

Session:

- A

## What We Are Building

025 finishes the core runtime substrate story.

Not docs. Not examples as theater. Not a Tokio bridge. The point is:

> Tina has one real live runtime substrate path that preserves the Tina rules:
> shard-owned execution, synchronous handlers, explicit effects, bounded
> queues, visible backpressure, runtime-owned time and I/O, deterministic proof
> pressure, and no generic async scheduler leaking into user code.

The expected substrate is Betelgeuse.

Betelgeuse matters here because it already points in the right spiritual
direction: completion-driven I/O, shard-local control, and deterministic
simulation/testing hooks. That is closer to Tina than building a thin skin over
arbitrary futures.

The explicit-step runtime and `tina-sim` remain the semantic oracle. A live
Betelgeuse runner must preserve their meaning; it does not get to redefine the
program because wall-clock I/O is now involved.

This is **not** green-field substrate work. The pre-025 starting baseline
already exists:

- `tina-runtime::ThreadedRuntime` exists as the single-shard live runner.
- `tina-runtime::ThreadedMultiShardRuntime` exists as the fixed-shard live
  runner.
- those runners already use the current `tina-runtime` `io_backend`, including
  Betelgeuse-backed TCP paths.
- 025 is rename, completion, hardening, parity proof, DST decision, and cost
  proof on that existing substrate.

Slice size must be judged against that baseline. 025 renames those public live
runner names to `BetelgeuseRuntime` / `BetelgeuseMultiShardRuntime` rather than
keeping them as equal peers. If implementation discovers
that the existing threaded substrate is too small to complete rather than just
rename/harden, pause and record the new scope before building a second
substrate.

End-state claim for this phase:

> Tina can be tried as a shared-nothing, thread-per-core Rust concurrency
> primitive on a real live substrate. The same isolate logic can run under the
> explicit-step oracle, the simulator/DST harness, and the Betelgeuse live
> runner, with bounded ingress, runtime-owned time/TCP completions, visible
> overload, shutdown, trace, and tested failure paths.

## What Will Not Change

- 025 does **not** make handlers async.
- 025 does **not** let users pass arbitrary futures into isolates.
- 025 does **not** expose a raw Betelgeuse, Tokio, or io_uring handle through
  ordinary Tina context.
- 025 does **not** turn `Runtime` internals into `Arc<Mutex<Runtime>>`.
- 025 does **not** add unbounded queues to make the live backend convenient.
- 025 does **not** make the simulator subordinate to the live backend.
- 025 does **not** build a Tower/Axum adapter.
- 025 does **not** build a Tokio adoption bridge.
- 025 does **not** claim broad production readiness.
- 025 does **not** claim zero allocation unless a specific hot path is directly
  measured and pinned.
- 025 does **not** hide I/O in user handlers, helper traits, or test shims.
- 025 does **not** rename explicit-step runtime types only to make names match
  the live substrate. Rename pressure is scoped to live runner names and any
  config/handle names that would otherwise mislead users.
- 025 does **not** expand Loom coverage beyond the SPSC mailbox unless a new
  unsafe/concurrent queue implementation is introduced.

## Core Decisions

### 1. Betelgeuse Is The Primary Runtime Substrate

Expected direction:

- Betelgeuse is the live backend for this phase.
- Tokio stays useful for comparison and later ecosystem adapters.
- monoio/compio stay possible future backend candidates, but they are not the
  first core-substrate blocker.

Reason:

Tina wants a shard-local interpreter riding a completion-driven runtime. It
does not want to become a futures runtime. Betelgeuse is already aligned with
completion ownership and deterministic testing pressure, so it is the right
thing to finish first.

### 2. The Live Runner Interprets Tina, It Does Not Become Tina

The live backend owns time, sockets, completions, and worker-thread wakeups.

Tina still owns:

- isolate state ownership
- one-message-at-a-time handler execution
- effect interpretation
- bounded mailbox admission
- visible `Full` / `Closed` / `Timeout` / requester-stopped outcomes
- trace and causal event vocabulary

The live runner is an interpreter loop around those rules. It should not expose
backend-specific escape hatches as the normal path.

### 3. The Public Live Runner Name Should Say What It Is

`ThreadedRuntime` is mechanically true but semantically bland. If Betelgeuse is
the substrate story, the public live-runner surface should say so.

Expected direction:

- keep the crate name `tina-runtime` for now; it is the semantic runtime crate
- introduce or rename the live runner surface to:
  - `BetelgeuseRuntime`
  - `BetelgeuseMultiShardRuntime`
  - `BetelgeuseRuntimeConfig`
  - `BetelgeuseRuntimeHandle`
- remove or demote `ThreadedRuntime` / `ThreadedMultiShardRuntime` as public
  teaching names during this phase

No compatibility alias is required unless implementation discovers an internal
transition need. This repo is still pre-user. Grug no carry old rocks for
imaginary users.

### 4. Shard-Local Runtime Contract

Each live shard has one owning OS thread.

That thread owns:

- one shard runtime/interpreter
- one Betelgeuse driver/reactor
- runtime-owned sockets and timers for that shard
- the shard's mailbox execution
- the shard's local trace production

External handles communicate with the shard through bounded command queues.

Cross-shard sends move through bounded shard-pair queues or bounded worker
handoff. There must be no hidden overflow queue and no "try" API that can block
after successful bounded admission.

### 5. Explicit-Step Runtime And Simulator Remain The Oracle

The explicit-step runtime remains the clean semantic model:

- collect completed runtime-owned work
- translate completions into messages
- deliver mailbox work
- interpret returned effects
- emit deterministic trace events

The simulator remains the DST/replay model:

- virtual time
- scripted TCP
- seeded perturbation
- checker failures
- replay records

The Betelgeuse runner must be tested against this oracle where the observable
behavior should match. Where wall-clock/live behavior cannot be bytewise
identical, the plan requires a named, narrow difference.

### 6. Runtime-Owned Time And TCP Are The First Complete Backend Surface

025 should complete the runtime-owned time/TCP live path before expanding the
effect language.

Required live backend surface:

- sleep/timer completion
- TCP bind
- TCP accept
- TCP read
- TCP write, including partial write behavior
- TCP close
- resource error and closed-resource behavior
- requester stopped before completion
- requester mailbox full at completion
- late completion after timeout or stop
- graceful runner shutdown with outstanding resources

### 7. DST Must Stay Wired Into The Runtime Story

Betelgeuse is valuable partly because it has deterministic testing machinery.
025 must read and use that machinery deliberately.

Expected direction:

- first audit the actual Betelgeuse APIs already in the repo
- identify the smallest DST hook Tina should use rather than inventing a
  second chaos layer
- if the vendored backend has no concrete hook, add the smallest Betelgeuse
  simulated I/O backend needed for Tina-owned TCP proof instead of closing on
  a fallback
- make at least one composed workload run through:
  - explicit-step runtime or simulator oracle
  - Betelgeuse live runner
  - seeded Betelgeuse simulated backend pressure

If Betelgeuse DST cannot yet drive a specific Tina path, the phase must record
that as an honest limitation rather than pretending simulator replay proves the
live backend.

Hard decision:

- If Betelgeuse exposes a usable deterministic/fault hook for the touched
  paths, use it in 025.
- If Betelgeuse does not expose a usable hook, pause the phase before closeout
  and decide explicitly whether to:
  - add the missing Betelgeuse hook
  - narrow 025 by removing the live-DST claim
  - split the DST hook into a follow-up phase

No fallback closeout. The chosen 025 path is to add the missing narrow
Betelgeuse hook: a deterministic simulated TCP backend with seeded completion
delay and partial-write pressure, then prove Tina runtime-owned TCP through it.

### 8. Cross-Shard Live Semantics Are Core If They Block Thread-Per-Core

Thread-per-core Tina needs multiple live shard workers.

025 should support live worker-per-shard execution for bounded cross-shard
sends if it is not already complete enough. Cross-shard isolate-call reply
transport is out of scope for 025.

Expected direction:

- live cross-shard sends remain supported and directly proved
- live cross-shard isolate calls reject in 025 with a typed/tested outcome
- cross-shard isolate-call reply transport is named as later work

No accidental half-claim. The live cross-shard call path must not panic,
silently drop, or look like it works without reply transport.

### 9. Allocation And Cost Must Be Measured, Not Waved Away

Tina's first win is semantics, not raw throughput. But performance costs still
matter.

025 must classify runtime allocations into:

- necessary today because Rust erased-message plumbing needs it
- avoidable implementation cost
- trace/replay/test-only cost
- backend cost outside Tina's direct control

The phase should pin focused counts for:

- local send hot path
- same-shard isolate call round trip
- live ingress command handoff
- TCP completion delivery
- cross-shard send if touched

If the path allocates, say how much and why. If the path claims no warm-path
allocation, prove it directly.

Measurement tier:

- use the existing global-allocator probe pattern
- run in the same debug-profile style as existing allocation tests
- count allocations/reallocations, not wall-clock latency
- do not make throughput or latency claims in this phase
- pin exact counts only where the test harness is stable enough to keep them
  meaningful

Wall-clock benchmarking is later work.

### 10. Oracle-Versus-Live Differences Are Named Up Front

Expected live-only differences:

- live timers have wall-clock variance; oracle/sim timers have deterministic
  virtual/manual time
- real TCP can produce OS/backend errors that scripted TCP does not produce
- live cross-shard worker scheduling may interleave work differently than the
  explicit global coordinator
- per-shard trace order should remain deterministic inside one shard; global
  cross-shard trace order may require explicit merge/sort rules rather than
  pretending OS scheduling is deterministic

Those differences are allowed only when named in tests/closeout. They do not
excuse different delivery semantics, unbounded buffering, or hidden failure.

### 11. Worker And Shutdown Semantics Are Pinned

Worker panic visibility:

- worker panic must produce a trace-visible runtime event where possible
- public handle operations after worker death must return a typed runtime error
- no live worker thread may disappear while the handle keeps pretending the
  runtime is healthy

Graceful shutdown rule:

- synchronous already-ready completions may be drained
- outstanding async timers/TCP operations are canceled by shutdown
- canceled requester-facing work surfaces as requester closed / runtime closed
  according to the existing completion vocabulary
- shutdown with outstanding timer and outstanding TCP operation both need
  direct tests

### 12. Live Multi-Shard Coordinator Shape

The live multi-shard runner should preserve Galileo's stable-shard-ownership
rule:

- once an isolate is placed, that incarnation's shard stays stable
- children belong to the parent shard unless a later phase explicitly designs
  cross-shard child ownership
- supervision remains shard-local

Expected delivery shape:

- each receiving shard worker polls inbound remote queues in ascending
  source-shard order
- this mirrors `MultiShardSimulator::step` enough that ordering proofs remain
  meaningful
- if the live backend cannot guarantee global cross-shard event order, closeout
  must say so and tests must assert stable per-shard semantics plus the explicit
  merge rule

### 13. Composed Workload Is One Workload

Use one primary composed workload for oracle/sim/live parity.

Expected direction:

- reuse the existing 020 dispatcher-style workload shape
- extend it only as needed to include timer, observed send/call, restart, and
  bounded cross-shard behavior
- avoid creating separate "almost the same" workloads for the oracle and live
  runner

One workload, many engines. Grug like fewer rocks.

### 14. Ergonomics Stay Single-Path

025 may improve names and small helpers only if the live substrate makes a
current name dishonest.

Allowed:

- backend-honest runner names
- config names that expose bounded capacities clearly
- small test harness helpers used by runtime and sim

Not allowed:

- second effect DSL
- new macro family just for backend setup
- adapter sugar that hides boundedness
- convenience APIs that bypass trace/backpressure semantics

## Pause Gates

Pause and ask before continuing if:

- Betelgeuse integration requires async isolate handlers
- Betelgeuse integration requires unbounded queues
- `Runtime` must become shared mutable state across threads
- backend completion order forces a semantic event-model change
- live time/TCP cannot map to existing `RuntimeCall` without changing user
  handler meaning
- live cross-shard isolate calls cannot reject with a typed/tested outcome
- allocation numbers contradict a claim already written into SYSTEM or README
- the work drifts into Tower/Axum, Tokio bridge, release docs, or benchmark
  marketing
- the backend choice changes away from Betelgeuse
- the existing `ThreadedRuntime` baseline turns out to need replacement rather
  than rename/completion
- Betelgeuse does not expose the DST/fault hook needed for the live-DST claim

## Build Order

1. **Substrate audit**
   - Read the current Betelgeuse integration in `tina-runtime`.
   - Start from existing `ThreadedRuntime` / `ThreadedMultiShardRuntime` and
     identify what is missing for rename/completion/proof.
   - Read the vendored/local Betelgeuse API enough to identify real driver,
     completion, timer, TCP, and DST hooks.
   - Record the exact missing pieces in `review.md`.

2. **Pin the live runner surface**
   - Decide final public names.
   - Expected names: `BetelgeuseRuntime`, `BetelgeuseMultiShardRuntime`,
     `BetelgeuseRuntimeConfig`, and `BetelgeuseRuntimeHandle`.
   - Update tests and examples to use the chosen names.
   - Do not keep silent equal public aliases unless an implementation-only
     transition truly needs them.

3. **Complete bounded ingress semantics**
   - Prove bounded command queues reject immediately when full.
   - Ensure no `try_*` method blocks after successful bounded admission.
   - Separate "queued to worker" from "delivered to mailbox" if necessary.
   - Add direct live tests without sleeps-as-proof, using barriers/channels or
     a deliberately blocked worker to fill the bounded queue deterministically.

4. **Complete shard lifecycle**
   - Start, handle, drain, shutdown, join.
   - Worker panic and backend error must become trace-visible where possible
     and typed on later handle operations.
   - Outstanding timers/resources on shutdown cancel and surface as
     requester-closed/runtime-closed style outcomes.
   - Trace collection must be deterministic enough for assertions.

5. **Complete live time mapping**
   - Sleep completion works on the live Betelgeuse runner.
   - Timeout paths do not rely on wall-clock race in tests.
   - Requester stopped, requester full, and late completion paths are covered.

6. **Complete live TCP mapping**
   - Bind, accept, read, write, close through Betelgeuse live runner.
   - Partial write behavior is pinned.
   - Closed resource and backend error behavior is pinned.
   - TCP echo proof runs without manual stepping.

7. **Oracle parity suite**
   - Reuse one dispatcher-style composed workload and run it under:
     - explicit-step runtime where feasible
     - simulator/DST harness
     - Betelgeuse live runner
   - Assert final outputs and semantic event classes.
   - Name any live-only differences explicitly.

8. **Betelgeuse DST bridge**
   - Add `betelgeuse::io::simulated` if the vendored backend lacks the needed
     deterministic/fault hook.
   - Support deterministic bind, accept, read, write, close, completion delay,
     and partial-write pressure.
   - Add a composed Tina workload that runs runtime-owned TCP through that
     Betelgeuse simulated backend on both explicit `Runtime` and threaded
     `BetelgeuseRuntime`.
   - Keep broader live-substrate liveness faults out of claim unless directly
     implemented and proved.

9. **Live multi-shard worker proof**
   - Prove worker-per-shard execution under the Betelgeuse runner.
   - Prove bounded cross-shard send behavior.
   - Prove source-time and destination-time stages remain visible.
   - Poll inbound remote queues in ascending source-shard order, or document
     the exact equivalent merge rule.
   - Reject cross-shard isolate calls with a typed/tested outcome.

10. **Allocation and cost probes**
    - Add focused allocation probes for each touched hot path.
    - Use the existing global-allocator probe pattern in debug-profile tests.
    - Pin counts where stable.
    - Reduce avoidable allocation only when it does not blur Tina semantics.
    - Keep trace/replay allocation claims honest.

11. **Docs that protect semantics**
    - Update `.intent/SYSTEM.md` with the exact live substrate claim.
    - Update `ROADMAP.md` so the next roadmap does not keep sending us toward
      a premature Tokio bridge.
    - Keep README changes brief unless the public surface changed.

12. **Closeout**
    - Update `review.md`, `.intent/SYSTEM.md`, and `ROADMAP.md` with:
      - final runner names
      - engine matrix / proof matrix status
      - Betelgeuse simulated I/O proof
      - cross-shard call rejection status
      - allocation numbers
      - tests run
      - remaining non-claims

## Required Proof Set

Direct tests should include:

- live bounded ingress returns `IngressFull` without blocking
- accepted live ingress eventually reaches target mailbox or reports a typed
  delivery failure
- live sleep fires once
- live timeout produces exactly one completion outcome
- late completion after timeout is traced and not delivered as success
- requester stopped before completion is traced
- requester mailbox full at completion is visible
- TCP echo works on the Betelgeuse runner
- TCP partial write retry works on the Betelgeuse runner
- TCP close rejects or completes outstanding work in the pinned way
- graceful shutdown with outstanding timer
- graceful shutdown with outstanding TCP operation
- worker panic does not silently disappear
- later handle operations after worker panic return a typed error
- live supervision/restart still works if touched by the runner
- live cross-shard send respects bounded queue capacity
- live cross-shard unknown/stale/full target behavior matches the explicit
  source-time/destination-time model
- live cross-shard isolate call rejects with the pinned typed outcome
- same workload final output matches oracle/sim/live where intended
- Betelgeuse DST or fault hook drives at least one composed live workload, or
  the phase pauses and changes scope explicitly before closeout
- allocation probes pin the touched hot paths

Tests should prefer synchronization barriers, bounded channels, explicit
completion notifications, and trace assertions over sleeps and hope.

## Proof Matrix

| Capability | Explicit-step oracle | `tina-sim` / DST | Betelgeuse live runner |
|---|---|---|---|
| local send boundedness | required | required | required |
| same-shard isolate call | required | required | required |
| timeout / late reply | required | required | required |
| sleep / timer | required | required | required |
| TCP echo | existing/proof helper | scripted TCP | required |
| TCP partial write | required | scripted TCP | required |
| supervision/restart | required | required | required if runner touches it |
| cross-shard send | required | required | required |
| cross-shard call | existing oracle semantics | existing sim semantics | reject typed/tested in 025 |
| replay / DST pressure | deterministic trace | required | Betelgeuse hook required for live-DST claim |
| allocation budget | focused debug probes | focused debug probes | focused debug probes |

## Done Means

- The repo has a named Betelgeuse live runtime substrate surface.
- The live runner owns shard-local time/TCP/message execution without async
  user handlers.
- Bounded ingress and bounded cross-shard handoff are directly proved.
- Same user-shaped isolate logic can run under oracle/sim/live for at least one
  composed workload.
- Betelgeuse DST/fault support is used directly for at least one composed live
  workload, or 025 has explicitly changed scope before closeout.
- Cross-shard call reply transport is explicitly rejected by the live runner
  with a typed/tested outcome and named as later work.
- Runtime allocation/cost claims are pinned to measured paths, not vibes.
- `.intent/SYSTEM.md` and `ROADMAP.md` describe Betelgeuse as the current core
  substrate story and do not accidentally center a Tokio bridge.
- `make verify` passes.

## Non-Claims After This Phase

Even if 025 succeeds, Tina still should not claim:

- general replacement for Tokio HTTP services
- Tower/Axum integration
- arbitrary async ecosystem compatibility
- zero allocation across the whole runtime
- full Erlang/OTP maturity
- crash/segfault isolation beyond Rust panic capture
- production performance parity with Tokio/monoio/glommio
- mature multi-backend runtime abstraction

The right claim is narrower and stronger:

> Tina has a real Betelgeuse-backed live substrate for its bounded,
> shared-nothing concurrency model, plus deterministic proof machinery around
> the same semantics.
