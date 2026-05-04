# 030 Willem Drees Local Production Runtime Plan

## Purpose

Make Tina's one-process runtime story boring enough for real local
server-shaped workloads.

Surveyor gave Tina a cleaner live substrate ownership story over Betelgeuse:
shutdown/cancel-drain has an explicit release contract, completion-slot leaks
are no longer an accepted escape hatch, and CI now exercises Linux and macOS.
Willem Drees should build on that by proving that a local Tina server can run
under ordinary production pressures:

- many connections;
- slow peers;
- bounded overload;
- worker pool pressure;
- restart under live TCP work;
- graceful shutdown with pending accept/read/write/timer/call work;
- memory ceilings narrow enough to catch accidental unbounded buffering.

This phase is not a demo phase. It is core runtime completion work.

## Framing

Tina should not try to win by saying "Akka in Rust" or "Tokio replacement" too
early. The stronger claim is smaller and more useful:

> Tina is a shared-nothing Rust concurrency primitive for local stateful
> services where bounded failure visibility, deterministic testing, and
> runtime-owned I/O matter.

Willem Drees exists to make that claim less theoretical. It should take the
runtime pieces that already exist and pressure them as a user would:

- listener isolate owns accepting;
- connection isolates own socket state;
- worker isolates own bounded stateful work;
- supervisor policy owns restart;
- runtime-owned calls own timers and TCP;
- bounded mailboxes and ingress expose overload instead of hiding it.

The result should be a production-shaped local runtime substrate, not a polished
release story. Gemini can write the friendly docs later. This phase should make
the hard runtime behavior true.

## Starting Baseline

The repo already has useful pieces:

- `ThreadedRuntime` / `BetelgeuseRuntime` run a shard-owned Tina runtime on a
  worker thread.
- `RuntimeDriver` owns time and TCP calls behind a small capability contract.
- Betelgeuse simulated I/O can drive delayed TCP completions and partial writes.
- `tina-sim` has deterministic time/TCP oracle coverage.
- Ranger and Surveyor hardened lane ownership, cancellation, shutdown, and
  completion-slot release.
- Existing TCP echo and dispatcher tests prove important slices, but they are
  still mostly narrow proofs rather than one composed production-shaped local
  server workload.

Willem Drees is therefore a completion and composition phase, not a green-field
runtime phase.

## Expected Direction

Default path: keep the Betelgeuse-backed local runtime and build a composed
local-server workload around it.

Expected workload shape:

- one listener/control isolate;
- many connection isolates;
- a bounded worker pool;
- a supervisor that restarts at least one live child under pressure;
- runtime-owned TCP accept/read/write/close and sleep/timeout effects;
- bounded ingress and bounded isolate mailboxes;
- explicit shutdown choreography.

The same semantic shape should be exercised through:

- live Betelgeuse runtime where native platform behavior matters;
- Betelgeuse simulated I/O where slow/partial/faulted peers matter;
- `tina-sim` where deterministic oracle replay is modeled.

Do not build remoting, clustering, persistence, Tower/Axum integration, or async
handlers here.

## Scope

### 1. Capability Audit

Start by reading the current runtime/test surface and write the work plan into
this phase's `review.md`.

Answer concretely:

- Which server-shaped behaviors are already directly tested?
- Which are only surrogate-tested by small unit/workload slices?
- Which are missing from live Betelgeuse, simulated Betelgeuse, or `tina-sim`?
- Which failures should be same semantics across all three, and which are
  honest live-only substrate differences?

Likely existing anchors:

- `tina-runtime/tests/tcp_echo.rs`;
- `tina-runtime/tests/call_dispatch.rs`;
- `tina-runtime/tests/betelgeuse_substrate.rs`;
- `tina-sim/tests/io_simulation.rs`;
- `tina-sim/tests/multishard_dispatcher.rs`;
- `tina-sim/tests/tokio_vs_tina_examples.rs`.

### 2. Composed Local Server Workload

Add one composed workload rather than many unrelated demos.

It should model a small local service:

- listener accepts connections;
- connection isolate reads requests and writes responses;
- worker pool handles bounded stateful requests;
- connection observes worker `Full` / `Closed` / `Timeout`;
- supervisor restarts a child that panics or stops while work exists;
- shutdown can stop listener, connections, and workers cleanly.

The workload should assert events and outcomes, not print logs.

Do not hide this behind a broad framework abstraction yet. If small helpers are
needed to make the workload readable, add them only when they preserve one
preferred API surface.

### 3. Live Betelgeuse Runtime Proof

Prove the composed workload on the actual live threaded runtime.

Required live assertions:

- many short connections complete without hidden unbounded buffering;
- ingress full is reachable and typed, not sleep-proved;
- worker mailbox full is visible to the connection/control path;
- a stopped/restarted child rejects stale addresses;
- graceful shutdown with pending accept/read/write/call/timer work returns a
  typed success or typed lifecycle failure;
- no late requester delivery after requester stop;
- shutdown does not leave driver in-flight calls behind.

Use native live TCP where useful, but keep the test deterministic enough for CI.
If an assertion can only be stable against simulated I/O, put it there and name
the reason.

### 4. Betelgeuse Simulated-I/O Proof

Use Betelgeuse simulated I/O for production-shaped peer behavior that native CI
cannot make deterministic:

- slow reader;
- slow writer;
- partial writes;
- delayed accept/read/write completions;
- peer closes during pending work;
- shutdown while simulated completions are still scheduled.

These tests should be e2e through the live runtime worker loop, not only direct
driver calls.

### 5. `tina-sim` Oracle Proof

Where `tina-sim` models the same semantic behavior, add or update oracle tests
for the composed workload.

Required oracle pressure:

- seeded replay gives bytewise-equal event records;
- perturbation changes ordering only within allowed bounds;
- bounded overload remains visible;
- restart/shutdown behavior matches the live runtime's semantic contract.

Do not force `tina-sim` to model OS-specific native TCP errors. Name those as
live substrate differences.

### 6. Graceful Shutdown Contract

Make local runtime shutdown one of the phase's main proofs.

Required cases:

- pending accept;
- pending read;
- pending write;
- pending timer;
- pending isolate call;
- requester stopped before completion;
- worker thread shutdown while queues are not empty;
- driver shutdown release failure path preserved from Surveyor.

The runtime should never close "successfully" while it still has hidden pending
runtime work. If it cannot prove release, it must return a typed/tested error.

### 7. Bounded Memory And Backpressure

Add narrow memory/backpressure pressure, not benchmark theater.

Required evidence:

- configured mailbox and ingress capacities are small enough that `Full` paths
  are exercised directly;
- no server-shaped workload uses unbounded queues as an escape hatch;
- allocation probes protect at least the obvious hot path touched by this phase;
- any new buffering has an explicit capacity and a direct test that reaches it.

If deeper hot-path performance work is needed, record it for Ruud Lubbers
instead of growing this phase sideways.

### 8. CI Proof

Make sure the new proof is covered by the workspace verification gate.

Required evidence:

- `make verify` passes locally;
- the GitHub workflow added by Surveyor runs the relevant tests on Linux and
  macOS;
- no test is only meaningful on the developer's machine;
- platform-specific differences are named in `review.md`.

## What Will Not Change

- No async user handlers.
- No arbitrary future execution inside isolates.
- No Tower/Axum adapter.
- No remoting, clustering, or persistence.
- No new general-purpose runtime.
- No "demo app" as proof.
- No multiple competing ergonomic surfaces.
- No unbounded internal queues to make tests pass.

## Pause Gates

Pause and update the plan/review if:

- the composed workload needs a driver capability Ranger/Surveyor did not
  settle;
- native Linux or macOS cannot satisfy shutdown/release semantics in CI;
- useful local-server ergonomics require more than tiny helpers;
- performance/allocation work becomes the main issue;
- a required behavior belongs to remoting, persistence, clustering, or a future
  service adapter rather than local runtime core;
- the live runtime and `tina-sim` oracle disagree on a core semantic rather than
  an honest substrate difference.

## Proof Bar

This phase should close only with direct evidence.

Required proof set:

- composed local-server workload under live Betelgeuse runtime;
- same or equivalent workload under Betelgeuse simulated I/O for slow/faulted
  peers;
- `tina-sim` oracle replay for modeled semantics;
- direct graceful-shutdown tests for pending accept/read/write/timer/call;
- direct bounded overload tests for ingress and worker/mailbox pressure;
- restart/stale-address test under server-shaped work;
- allocation/backpressure probe for new buffering;
- `make verify` pass.

## Done Means

- Tina has a directly tested one-process server-shaped runtime story.
- Listener, connection, worker, supervisor, overload, restart, timeout, and
  shutdown behavior are all exercised through user-shaped isolate code.
- Live Betelgeuse, Betelgeuse simulated I/O, and `tina-sim` agree where they
  claim the same semantics.
- Platform-specific live substrate differences are documented in `review.md`,
  not papered over.
- No hidden unbounded queue was introduced.
- No release/docs/examples phase is needed to explain away missing core runtime
  behavior.

