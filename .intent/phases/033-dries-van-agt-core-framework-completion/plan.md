# 033 Dries van Agt Core Framework Completion Plan

## Purpose

Finish Tina's local framework story before Gemini. This phase deliberately
absorbs the old Dries/Apollo/Cassini roadmap rocks into one ordered phase:

1. finish the public local-framework surface;
2. build the narrowest useful Tokio/Tower/Axum bridge;
3. harden the result with CI, cost evidence, and dogfood-style proof.

The goal is not release polish. The goal is to reach the point where Tina is a
complete, bounded, shared-nothing local concurrency framework with a credible
adoption path for existing Tokio applications.

## Why This Is One Phase

The old roadmap split these concerns into Dries, Apollo, and Cassini. That made
the sequence tidy on paper but wrong in practice. The bridge cannot be designed
honestly until the local framework invariants are pinned, and hardening cannot
wait until after the bridge if the bridge changes the guarantee surface.

So Dries is one large phase with ordered internal rocks:

- **Core first.** Public local lifecycle/test/simulation helpers, trace
  retention, runtime lifecycle, I/O breadth, remaining cost cleanup.
- **Bridge second.** Tokio/Tower/Axum only after the local invariant list is
  sharp enough to compare against.
- **Hardening third.** CI, benchmarks, memory profile, optional MPSC decision,
  and dogfood proof after the real surface exists.

Implementation should still land in reviewable commits. One phase does not
mean one giant patch.

## Starting Baseline

At Dries start:

- `tina`, `tina-runtime`, `tina-sim`, `tina-mailbox-spsc`, and
  `tina-supervisor` exist and are tested.
- Joop left canonical application-surface tests but no broad public app builder
  or public test harness.
- Ruud left a narrow cost model plus deferred medium rocks.
- Live Betelgeuse-backed Tina runtime exists, but the stable public substrate
  story and CI story are not release-ready.
- No Tokio/Tower/Axum bridge exists.
- Gemini is intentionally blocked until this phase says the core is ready to
  document instead of still being completed.

## Rock 1: Local Framework Surface

Decide and implement the small public/test-support surface needed for normal
Tina services:

- app/root startup pattern;
- lifecycle handle for start, observe, shutdown, drain/cancel, and terminal
  trace/sink inspection;
- capacity/config types for service, runtime, and bridge-relevant queues;
- public or test-support trace predicates and replay helpers;
- simulation scenario helpers that let users prove replayable failures without
  raw `RuntimeEventKind` wall climbing;
- helper/macro polish that removes repeated ceremony while preserving one
  preferred path.

Refusals:

- no second app DSL;
- no hidden unbounded queues;
- no async handlers;
- no helper that hides capacity, timeout, address, message, or effect
  semantics.

Proof:

- canonical application tests use the new helpers;
- negative tests prove helpers do not hide boundedness or timeout semantics;
- simulator and live runtime tests both use the same user-facing structure
  where possible.

## Rock 2: Bounded Observability

Full trace forever is not a production-bounded story. Define and implement the
supported trace retention modes:

- full trace for tests;
- bounded ring;
- streaming sink;
- off/debug mode if safe;
- preserved/weakened guarantee table for each mode.

Proof:

- direct tests for each mode;
- bounded mode cannot grow without bound under a fixed long workload;
- replay/test mode keeps the existing deterministic proof behavior;
- bridge/hardening docs and tests use the same guarantee vocabulary.

## Rock 3: Runtime Lifecycle And Topology

Make the normal live-app path boring:

- build runtime;
- register roots;
- start service;
- observe worker failure;
- graceful shutdown;
- drain/cancel pending work;
- inspect terminal state and trace/sink.

Decide local thread-per-core polish:

- named shard topology/config;
- worker lifecycle visibility;
- backend-honest live runner names: use `BetelgeuseBackedRuntime` /
  `BetelgeuseBackedMultiShardRuntime`, so Tina does not imply Betelgeuse itself
  is an actor/runtime framework;
- optional thread/core placement policy if it is small and portable enough;
- final live cross-shard isolate-call decision: implement reply transport if
  required for local framework completeness, otherwise keep a sharper typed
  rejection contract.

Proof:

- startup/failure/shutdown tests from a user perspective;
- multi-shard local service proof if topology config changes;
- no leaked pending driver calls, worker commands, or trace buffers after
  shutdown.

## Rock 4: Runtime-Owned I/O Breadth

Decide the first local framework I/O claim.

Expected default: Tina supports runtime-owned time and TCP as the first local
framework surface. DNS, TLS, file, process, signal, and UDP are either added
only if one is required for the bridge/reference app, or explicitly deferred.

Proof:

- if TCP/time only, add refusal tests/docs so later code does not imply more;
- if one more I/O family is added, prove it in explicit runtime, simulated
  driver, and live runtime where applicable.

## Rock 5: Tokio / Tower / Axum Bridge

Build the narrowest useful bridge for incremental adoption inside an existing
Tokio application. Starting target should be Tower/Axum if it can stay small:
host one Tina service/isolate tree inside a Tokio HTTP application and route a
bounded request into Tina.

Bridge design must write down:

- where the Tina isolate actually runs;
- where effect dispatch happens;
- which thread owns Tina state and I/O;
- which queues exist and their capacities;
- how shutdown flows from Tokio into Tina;
- what guarantees are preserved and weakened.

Expected shape:

- Tokio app calls a bridge handle;
- bridge ingress is bounded;
- Tina isolate code stays synchronous and unchanged;
- bridge returns typed `Full` / `Closed` / `Timeout`-like outcomes where they
  exist;
- replayability is preserved in `tina-sim`, not claimed for the Tokio runtime
  itself.

Proof:

- one Axum or Tower reference integration test with assertions, not logs;
- overload test proves bounded bridge ingress;
- shutdown test proves no hidden task survives;
- preserved/weakened-guarantees table is asserted by tests where possible.

Refusals:

- do not make isolate handlers async;
- do not run arbitrary futures inside isolates;
- do not pretend Tokio work-stealing is deterministic replay;
- do not hide unbounded Tokio queues behind Tina APIs.

## Rock 6: Hardening And Cost Closeout

Pull in the old Cassini bar after the real surface exists:

- CI gate for workspace verification;
- platform-specific substrate CI decision;
- benchmark/memory profile for the supported local framework paths;
- optional MPSC fallback decision;
- sizing/preallocation knobs if they matter for users;
- small `Effect::Batch` path if it remains a meaningful cost;
- live worker command boxing cleanup if it remains hot;
- typed fast paths / completion-slot pooling only if mechanically safe.

Proof:

- CI config exists and runs the intended gate;
- benchmark/memory numbers are reproducible enough for engineering decisions;
- optional MPSC is either implemented with tradeoffs or explicitly deferred;
- hardening does not weaken trace/replay/boundedness without a table saying so.

## Pause Gates

Pause for human review if:

- the bridge needs async handlers;
- Tower/Axum requires unbounded hidden queues;
- trace retention weakens replay more than expected;
- runtime-owned I/O breadth expands beyond TCP/time plus one tightly justified
  family;
- MPSC seems required for the core local framework;
- public API shape grows into multiple ways to do the same thing;
- cost cleanup wants unsafe pooling/slabbing.

## Done Means

- Tina has a coherent public local-framework surface.
- Canonical app tests use that surface instead of private harness dialects.
- Trace retention is bounded where production needs it and full where tests
  need replay.
- Live lifecycle and worker topology are boring to use and directly tested.
- The Tokio/Tower/Axum bridge exists in its narrow first form, with explicit
  preserved/weakened guarantees.
- CI, benchmarks, memory profile, and optional MPSC decision are real enough to
  support or narrow the public claim.
- Gemini can document the framework instead of discovering missing core work.

## Implementation Notes

- Live runner names are `BetelgeuseBackedRuntime` and
  `BetelgeuseBackedMultiShardRuntime`.
- Trace retention is implemented as full, bounded recent retention, or off.
- The first bridge lives in `tina-tokio-bridge`; it preserves bounded Tina
  ingress and surfaces `Full`, `Closed`, and `Timeout` without making isolate
  handlers async.
- The bridge has Axum/Tower proof, bounded ingress-full proof, target
  mailbox-full proof, explicit timeout proof, and a runnable llama example.
