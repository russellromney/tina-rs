# 028 Ranger Core Runtime Substrate Completion Plan

## Purpose

Finish Tina's core runtime substrate.

026 gave Tina a driver boundary for runtime-owned time and TCP. That was the
right boundary, but the substrate story is not yet strong enough to build
service-shaped framework work on top of it. Ranger should harden the driver
and live runtime substrate before Gemini documents it or a later phase builds
service patterns around it.

This is not a service example phase, not a Tokio bridge phase, not a narrow
polish pass, and not release documentation. It is the core-completion phase
for answering:

> What must be true about the live driver/runtime substrate before Tina can
> honestly support real workloads?

Ranger should be as large as it needs to be to settle Tina core. It should not
close merely because one neat slice landed. It closes when the remaining work
is service patterns, docs, ecosystem adapters, benchmarks, packaging, and
production polish rather than basic runtime/substrate semantics.

## Context

025 made Betelgeuse the honest live substrate.
026 factored time/TCP behind Tina's `RuntimeDriver` boundary.
027 added Betelgeuse simulated-I/O polish, narrow cost evidence,
Tokio-vs-Tina constrained comparisons, and substrate adapter research.

Those phases leave several live-substrate gaps:

- live TCP uses a conservative one-active-operation-per-resource rule;
- full-duplex read/write on one stream is not supported;
- cancellation is honest but still resource-closing for Betelgeuse TCP;
- driver capability and lifecycle requirements are not fully named;
- real per-message/per-call allocation and operation costs are only partly
  pinned;
- the future substrate direction is still a research note, not a decision;
- production-shaped shutdown, draining, and outstanding-operation behavior
  need a clearer contract.

Ranger should close or explicitly pin these gaps before service workload
hardening begins. If implementation exposes another core runtime/substrate gap,
that gap belongs in Ranger unless it is clearly an ecosystem adapter, docs,
benchmarking, or post-core production-operations concern.

## Phase Size

Ranger is intentionally allowed to be large.

Do not split work just to keep the phase small if the split would leave Tina's
core runtime substrate half-settled. Smaller commits and internal review
checkpoints are good; premature phase closeout is not.

The standard is:

> After Ranger, Tina core is done enough that later phases build on it instead
> of reopening runtime/substrate fundamentals.

If that standard requires multiple implementation chunks, keep them inside
Ranger and record progress in `review.md`.

## Expected Direction

Default path: keep hardening the vendored Betelgeuse-backed driver for Ranger.

Ranger should not begin by building a Tokio, Monoio, Glommio, or Compio
adapter. Those remain future substrate options unless Betelgeuse blocks a core
semantic that Ranger must settle. If Betelgeuse does block a required semantic,
pause and record why the project is changing substrate direction before
implementing an adapter or Tina-owned completion substrate.

The expected core outcome is:

- Betelgeuse remains the near-term live substrate;
- Tina's `RuntimeDriver` contract becomes precise enough that another driver
  can implement it later;
- simulator and live behavior stay aligned for modeled time/TCP semantics;
- future adapter work becomes an implementation of a settled contract, not a
  redesign of Tina core.

## Scope

### 1. Driver Capability Contract

Turn the 026 driver boundary into a real capability contract.

Name what a Tina runtime driver must provide for real workloads:

- timer submission, wake, timeout, and cancel;
- TCP bind, accept, read, write, close, and cancel;
- bounded pending-operation admission;
- no hidden unbounded queues;
- explicit progress through runtime-owned steps or worker turns;
- deterministic simulator compatibility where applicable;
- traceable cancellation, shutdown, and substrate failure.

The contract should be written where future driver implementers will see it,
not only in phase notes. `review.md` should record the reasoning and remaining
non-claims.

Record capability gaps in `review.md`. Do not hide a missing capability behind
silent fallback behavior.

### 2. TCP Resource Concurrency

Revisit the 026 `ResourceBusy` rule.

Expected direction: support full-duplex TCP read/write on one stream if it can
be done with honest ownership and cancellation. A likely shape is separate
resource lanes:

- listener accept lane;
- stream read lane;
- stream write lane;
- close lane that rejects while any active lane is pending.

If Betelgeuse cannot cancel one lane without closing the underlying stream,
the runtime may still use tombstones internally, but it must not invalidate an
unrelated live operation silently. The public outcome must be typed and
traceable.

Pause before broadening beyond TCP read/write/close lanes.

### 3. Cancellation And Shutdown Semantics

Harden stopped-requester, explicit-close, timeout, and runtime-shutdown
behavior under live and simulated drivers.

Required cases:

- stopped requester with pending accept/read/write/timer;
- explicit stream/listener close while operations are pending;
- runtime shutdown with pending timer and TCP operations;
- late substrate completion after cancel or shutdown;
- requester mailbox full when completion arrives;
- timeout racing with late completion where the call shape supports timeout.

The result must be visible through Tina events and typed outcomes. No pending
operation should keep quiescence false after its requester has been stopped or
after runtime shutdown begins.

### 4. Live / Sim / Oracle Parity

Keep the three layers aligned:

- live Betelgeuse-backed runtime;
- Betelgeuse simulated driver/runtime;
- `tina-sim` deterministic oracle where the behavior is modeled there.

If a behavior belongs only to the live driver, record why. If live and sim
disagree, fix the disagreement or pause and record the design decision.

### 5. Substrate Direction Decision

Make a decision record for the next substrate bet. The decision can be:

- continue hardening the vendored Betelgeuse path for now;
- add a Tokio current-thread driver adapter later;
- investigate Monoio/Glommio/Compio later;
- build a small Tina-owned completion substrate later.

Ranger does not need to implement a new adapter unless the existing substrate
blocks the phase's required semantics. But Ranger must leave the roadmap with
a clear next substrate direction, not another open-ended research note.

If Betelgeuse is selected as the near-term substrate, Ranger must say what
that means concretely: which semantics are now supported, which are explicitly
not, and what would trigger revisiting the choice.

### 6. Cost And Allocation Pressure

Measure enough to keep substrate claims honest:

- per timer call;
- per TCP read/write completion;
- per isolate call;
- per cross-shard send;
- live worker ingress handoff.

Prefer allocation counts, operation counts, and bounded resource counts before
wall-clock benchmarks. If a hot path allocates because of type erasure or
completion boxing, say so plainly and decide whether it is acceptable for now.

### 7. Minimal Service-Shaped Smoke

Use only the smallest service-shaped workload needed to pressure the substrate,
such as framed TCP over one or two clients. This is not Ranger's main output.
It exists only to catch driver/lifecycle bugs that unit tests miss.

Defer real service framework hardening until after the substrate/driver story
is ready.

### 8. Core Boundary Closeout

At the end of Ranger, write down what is now considered Tina core and what is
not.

Core should include at least:

- isolate scheduling and bounded mailboxes;
- local and cross-shard delivery semantics;
- supervision/restart semantics already settled by prior phases;
- runtime-owned time;
- runtime-owned TCP at the capability level Ranger settles;
- driver progress/cancel/shutdown semantics;
- simulator/live parity boundaries;
- allocation and buffering claims that the core is willing to make.

Non-core should include at least:

- Tower/Axum/Hyper integration;
- arbitrary futures inside isolate handlers;
- broad protocol libraries;
- service-framework convenience APIs;
- release docs and packaging;
- throughput benchmarks beyond narrow substrate measurements.

## Build Order

1. **Capability audit.** Read the current live driver, threaded runtime,
   simulated Betelgeuse driver/runtime, and `tina-sim` TCP/time paths. Produce
   the capability contract and a gap list in `review.md` before changing
   behavior.
2. **TCP lanes.** Implement or explicitly defer lane-based TCP concurrency:
   listener accept lane, stream read lane, stream write lane, and close
   rejection while lanes are active. Keep live, simulated driver, and `tina-sim`
   aligned where the behavior is modeled.
3. **Cancellation and shutdown.** Harden stopped-requester, explicit close,
   runtime shutdown, late completion, requester-mailbox-full completion, and
   timeout races against the lane model.
4. **Cost pressure.** Add or update allocation/operation probes for the named
   hot paths. Record which costs come from core semantics and which are current
   implementation overhead.
5. **Minimal service smoke.** Add only enough service-shaped workload to catch
   lifecycle/substrate bugs missed by focused tests.
6. **Substrate decision and core boundary.** Record the near-term substrate
   decision, core/non-core boundary, and any remaining non-claims in
   `review.md` and the roadmap.

Each chunk should leave `make verify` green before moving to the next chunk
unless a pause gate is active.

## Production-ish Next Layer

Ranger should close Tina's core runtime/substrate semantics, not every
production concern. The next production-ish layer after Ranger should focus on
hardening the implementation around the settled contract:

- reduce or justify hot-path allocation sources such as boxed completions,
  erased translators, trace growth, and replay artifacts;
- add stronger shutdown/drop safety probes around backends that hold external
  completion ownership;
- decide whether requester-stop should cancel pending isolate-call waits the
  same way it cancels driver calls, or keep isolate-call settlement as
  reply/timeout-driven;
- turn the driver contract into adapter guidance for a future Tokio
  current-thread, Monoio, Glommio, Compio, or Tina-owned substrate.

Those are production-hardening and adapter-readiness tasks. They should not
reopen the core lane/cancel/shutdown semantics Ranger settles.

## Refusals

- Do not build `tina-runtime-tokio-bridge` in Ranger unless the phase pauses
  and records that Betelgeuse cannot support the required driver semantics.
- Do not add Tower, Axum, Hyper, or arbitrary futures integration.
- Do not make isolate handlers async.
- Do not expose driver/backend handles to user isolates.
- Do not add unbounded queues for convenience.
- Do not build a broad service example suite here.
- Do not claim production readiness.
- Do not claim broad Tokio replacement.
- Do not start Gemini release docs until the substrate direction is settled.

## Pause Gates

Pause and record a design decision if:

- full-duplex TCP requires changing public resource identity;
- per-operation cancellation cannot be made honest without a new driver shape;
- Betelgeuse blocks required semantics rather than merely lacking polish;
- allocation costs are large enough to undermine the intended claim;
- implementing a Tokio/Monoio/Glommio/Compio adapter becomes necessary rather
  than future work;
- service-shaped work starts dominating the substrate phase.
- a discovered issue appears to require a new core primitive rather than a
  driver/runtime implementation change.

## Review Prompts

Ask reviewers to focus on:

- whether the driver contract is sufficient for real workloads;
- whether TCP read/write/close concurrency is honest and typed;
- whether cancellation and late-completion behavior are directly tested;
- whether live, simulated-driver, and `tina-sim` semantics stay aligned;
- whether the substrate direction decision is concrete enough for the roadmap;
- whether cost evidence is narrow, measured, and not overclaimed.

## Done Means

- `RuntimeDriver` has a documented capability contract for time/TCP, progress,
  cancellation, shutdown, and bounded pending work.
- Full-duplex TCP read/write on one stream is either supported with direct
  tests or explicitly deferred with the exact blocker recorded.
- Explicit close, stopped-requester cancellation, timeout, runtime shutdown,
  and late completion are tested across live and simulated drivers where
  applicable.
- Live Betelgeuse, Betelgeuse simulated driver/runtime, and `tina-sim` agree on
  the modeled TCP/time semantics or `review.md` records concrete non-overlap.
- Allocation and operation costs are pinned for the named hot paths.
- The roadmap names the next substrate direction after Ranger.
- Any service-shaped workload in Ranger is minimal and exists only to pressure
  the substrate.
- No Tokio bridge or ecosystem integration appears unless a pause gate records
  a deliberate change.
- `review.md` records capability gaps, design decisions, cost evidence, and
  remaining non-claims.
- `review.md` records the core boundary: what Tina core now includes, what is
  deliberately outside core, and why later phases should not need to reopen
  runtime/substrate fundamentals.
- Later service/docs/adapter phases can proceed without depending on unresolved
  core runtime substrate questions.
- `make verify` passes.
