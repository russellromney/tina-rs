# 032 Joop den Uyl Application Surface Plan

## Purpose

Make Tina feel like a usable local application framework, not a box of good
concurrency parts.

Willem Drees proved Tina can run a production-shaped local service. Ruud
Lubbers made the cost model honest and cheaper. Joop den Uyl should now make
the preferred Tina application shape obvious enough that a human or Codex can
port a small Tokio-shaped TCP/control-plane service without inventing local
architecture.

This is still core framework work. It is not a docs polish phase, not a demo
phase, and not a Tokio bridge. The proof must be runnable code that exercises
the same service shape through `tina-sim`, the explicit-step runtime, and the
Betelgeuse-backed live runtime where each layer is supposed to apply.

## Why This Comes Before Gemini

Gemini should document a framework whose local application shape is already
settled. If Joop is skipped, Gemini has to teach raw building blocks:
listener isolate, connection isolate, worker pool, supervisor, shutdown owner,
capacity choices, call timeouts, and trace assertions. That is too much
ceremony and too much room for Codex to invent dialects.

Joop should leave Gemini with one preferred service shape to explain.

## Starting Baseline

Known evidence existed at Joop start:

- `tina-runtime/tests/local_production_runtime.rs` has a live server-shaped
  workload with listener, connection, worker, supervisor, shutdown pressure,
  bounded overload, runtime-owned TCP, and runtime-owned time.
- `tina-sim/tests/local_production_runtime.rs` has an oracle version of the
  same workload with replay and exact event/output assertions.
- `tina-sim/tests/tokio_vs_tina_examples.rs` has many runnable semantic
  comparisons, but they are comparison slices, not one canonical application
  architecture.
- `README.md` has improved language, but broad guide polish belongs to Gemini.
- 031 recorded medium cost rocks:
  - batch small path;
  - live worker command boxing;
  - runtime sizing/preallocation knobs;
  - trace retention policy;
  - typed fast paths;
  - completion-slot pooling/slabbing.

Joop may take only the 031 rocks that directly improve application authoring
without changing Tina's semantics or adding multiple ways to do one thing.

## Target User Shape

By the end of Joop, this shape should be boring:

- one owner/root isolate starts the service;
- one listener isolate owns the listener id and accepts connections;
- one connection isolate owns each stream id and retries partial writes;
- one bounded worker pool handles request/reply work with mandatory timeouts;
- one supervisor config owns worker restart behavior;
- one shutdown path stops listener, connections, worker children, and pending
  runtime calls without leaked work;
- one capacity/config object or pattern names mailbox, worker, ingress, and
  shard-pair capacities;
- tests assert backpressure and trace behavior directly.

The resulting Tina code can still look like Tina: explicit messages, explicit
effects, bounded queues, synchronous handlers, and runtime-owned calls. The goal
is not to hide the model. The goal is to remove accidental ceremony and make
the right structure obvious.

## Expected Helper Direction

Implementation should start with the conservative helper direction below.
Anything larger is a pause gate and must be justified in `review.md` before it
lands.

- **Capacity/config:** expected yes. Add one small service-capacity config or
  pattern in `tina-runtime` if the canonical harness repeats mailbox, worker,
  ingress, or shard-pair capacities.
- **Startup/registration:** expected yes only if it preserves typed addresses
  and explicit capacities. Prefer small runtime helpers or test-support
  builders over a public "app builder" DSL.
- **Worker pool:** expected maybe. Add only if the canonical service and
  router proof share the same worker-spawn/request/reply ceremony.
- **Shutdown:** expected maybe. Add a helper or canonical pattern only if it
  preserves visible cancellation/rejection events.
- **Trace assertions:** expected test-support first. Do not make public trace
  query helpers unless the application surface clearly needs user-facing trace
  inspection.
- **Batch:** expected maybe. A small public helper is allowed only if it keeps
  `Effect` as the one sequencing model and avoids recursive boxing or another
  DSL.
- **Macros:** expected no new macro by default. Add one only if repeated Rust
  boilerplate survives after helper work, and it must not hide messages,
  addresses, capacities, timeouts, or effects.

Default crate ownership:

- `tina` only gets backend-independent effect ergonomics.
- `tina-runtime` owns service/runtime capacity and live-runner helpers.
- `tina-sim` owns simulator harness helpers.
- Trace assertion helpers begin as test support unless promoted by a reviewed
  user-facing need.

## Scope

### 1. Application Surface Audit

Append an implementation audit to `review.md`.

Audit the current server-shaped and comparison code for repeated friction:

- listener setup and listener self-addressing;
- connection setup and stream ownership;
- worker-pool spawn, address capture, and request/reply routing;
- mandatory call timeouts;
- overload handling for `Full`, `Closed`, `Timeout`, and requester stopped;
- supervisor config and restart bootstrap;
- shutdown choreography;
- capacity setup and magic numbers;
- trace assertions that require too much event matching boilerplate;
- repeated `batch(vec![...])` two-effect handlers;
- repeated root registration and startup sequences;
- repeated sim/live parity harness code.

Each friction point must be classified:

- **must fix now**: repeated, user-facing, and likely to cause dialects;
- **maybe fix now**: useful, but only if it preserves one preferred API;
- **defer**: performance/internal/bridge/docs work that does not block the
  application surface;
- **refuse**: would hide Tina's semantics or add another DSL.

### 2. Canonical Service Harness

Build one canonical local service workload that becomes the reference
application shape for future work.

Expected artifact names:

- `tina-runtime/tests/application_surface.rs`
- `tina-sim/tests/application_surface.rs`
- optional targeted updates to `tina-sim/tests/tokio_vs_tina_examples.rs`

The old `local_production_runtime.rs` tests may remain as regression tests or
be partially migrated, but the canonical application surface should live in the
new `application_surface` tests.

Implementation note: Joop migrated the Willem Drees service-shaped tests into
the canonical `application_surface` artifacts instead of keeping the older file
names as parallel regression tests.

It should cover:

- TCP accept/read/write lifecycle;
- bounded worker request/reply;
- success response;
- worker mailbox `Full`;
- worker call `Timeout`;
- worker restart under supervisor;
- stale address rejection after restart;
- graceful shutdown with pending accept/read/write/timer/isolate-call work;
- trace assertions for the above, not only user-output assertions.

Required runners:

- `tina-sim` owns deterministic oracle/replay proof.
- explicit-step `Runtime` owns semantic event-shape proof without native
  worker threads.
- `BetelgeuseRuntime` owns live native loopback and worker-thread lifecycle
  proof.

The three runners do not need byte-for-byte identical traces, because live
native scheduling can differ. They do need a named parity contract:

- same user-visible response set/order where the service protocol requires it;
- same visible failure classes;
- same shutdown/restart guarantees;
- same bounded-pressure visibility;
- documented live-only differences such as OS scheduling and real TCP errors.

### 3. Helper Surface

Add helpers only after the audit and canonical workload show the repeated pain.

Allowed helper categories:

- service capacity/config helpers;
- registration/startup helpers that keep typed addresses visible;
- worker-pool helpers if the pattern repeats across canonical tests;
- shutdown helpers that preserve visible cancellation/rejection semantics;
- trace assertion helpers for tests;
- tiny effect helpers where they preserve the existing `Effect` model;
- macros only when they remove Rust boilerplate without hiding messages,
  addresses, capacities, effects, or runtime calls.

Possible helpers to decide during implementation:

- a small service capacity struct in `tina-runtime`;
- a worker-pool spawn/config helper;
- a shutdown-owner helper or pattern;
- a trace query/assertion helper;
- a two-effect batch helper if it remains the obvious surface and does not
  create recursive boxing or a second sequencing DSL;
- small result/outcome helpers for the existing `SendOutcome` and
  `CallOutcome` paths if tests show readability wins.

Pause gates:

- public app-builder DSL;
- public router/registry/locator;
- new macro;
- new public trace-retention mode;
- helper that changes public crate boundaries;
- helper that cannot be explained as the single preferred path in one
  paragraph.

Helper rules:

- one preferred public path only;
- no silent compatibility aliases;
- no helper that hides `Full`, `Closed`, `Timeout`, stale generation, or
  requester-stopped outcomes;
- no helper that creates unbounded storage;
- no helper that moves I/O into handlers;
- no helper that makes handlers async;
- no helper that makes `tina` depend on `tina-runtime`.

### 4. Porting Proofs

Add runnable porting tests, not example-only demos.

At minimum:

- one TCP service porting proof;
- one bounded worker/router proof;
- one stateful control-plane/session proof.

Each proof should have a short Tokio-shaped sketch or existing comparison
where useful, but the assertion target is Tina behavior:

- success;
- overload;
- timeout;
- restart/stale identity;
- shutdown.

The point is not to show Tina and Tokio are identical. The point is to show
that a service normally written with Tokio tasks/channels can be expressed in
Tina with clearer boundedness and failure visibility, without absurd ceremony.

### 5. Selected 031 Medium Rocks

Joop may pull in these 031 medium rocks if they are needed for application
surface:

- **yes if needed:** runtime sizing/preallocation knobs or a capacity config,
  if capacity setup remains magic and repeated;
- **yes if needed:** trace query/assertion helpers for tests, if trace
  assertions stay too noisy;
- **maybe:** `Effect::Batch` small path, if two-effect handlers dominate and a
  single preferred helper can stay clean;
- **defer by default:** full trace retention modes;
- **defer by default:** typed fast paths and completion-slot pooling/slabbing;
- **defer by default:** live worker command boxing.

Any pulled-in rock must keep its own proof. Do not smuggle performance work in
without tests.

## Build Order

1. **Audit current application-shaped code.** Append "Implementation Audit 1"
   to `review.md` with a friction table and fix/defer/refuse decisions.
2. **Extract the canonical service contract.** Name the exact user-visible
   behavior, failure classes, trace assertions, and runner parity expectations.
3. **Build the canonical service harness first.** Prefer moving existing
   Willem Drees workload code into a clearer test shape before adding helpers.
4. **Add only helper surface justified by the harness.** Keep helper ownership
   in the right crate and update tests to use the preferred path.
5. **Add porting proofs.** TCP service, bounded router, and stateful
   control-plane/session tests must use the new preferred surface.
6. **Score ceremony before/after.** Record repeated setup blocks removed,
   centralized capacity choices, shutdown paths removed, trace assertion
   simplification, and helper call sites in `review.md`.
7. **Re-run comparison examples only after helpers land.** Update
   `tokio_vs_tina_examples` only where the new surface materially improves the
   Tina side.
8. **Review API shape.** Ask whether each new helper would survive a public
   crate release. Remove any that merely create a second way to write old code.
9. **Run the intensive proof matrix.** The canonical service must be shaken
   under deterministic replay, overload, late completions, restart, shutdown,
   and live loopback pressure before closeout.
10. **Verify.** Run focused tests and `make verify`.

## Proof Bar

This phase closes only with:

- an audit table in `review.md`;
- one named canonical service harness;
- simulator and live-runtime proof for the canonical service;
- at least three runnable porting proofs;
- direct assertions for success, overload, timeout, restart/stale identity, and
  shutdown;
- an intensive test matrix in code:
  - `tina-sim` deterministic replay under default and non-default seeds;
  - explicit-step runtime proof for delayed completions and no pending calls
    after stopped requesters;
  - live `BetelgeuseRuntime` loopback proof with multiple clients, bounded
    overload, timeout, restart, and shutdown pressure;
  - trace invariant checks that every accepted send/call reaches exactly one
    visible terminal outcome;
  - stale-address and requester-stopped regression tests;
  - proof that helpers do not introduce unbounded queues or hidden async
    handler work;
- before/after evidence that new helper surface reduces repeated ceremony in
  named tests;
- ceremony scorecard in `review.md` covering repeated setup blocks, capacity
  centralization, shutdown path reuse, trace assertion simplification, and
  helper call-site count;
- no weakened trace/replay, boundedness, shutdown, or runtime-owned I/O
  semantics;
- `make verify` passing.

## Done Means

- Tina has one obvious local-service structure.
- Codex can port a small Tokio-shaped TCP/control-plane service to Tina without
  inventing new architecture.
- The preferred structure is proved through tests, not only documented.
- Any new helper surface is small, crate-correct, and used by the canonical
  tests.
- Gemini can explain the service shape without reopening core semantics.

## Refusals

- No async handlers.
- No Tower/Axum bridge.
- No Tokio runtime adapter.
- No broad docs polish as the main deliverable.
- No unbounded queues.
- No macros that hide message, address, capacity, effect, timeout, or failure
  semantics.
- No new router/registry/locator framework unless the audit proves the existing
  model cannot support the canonical service shape.
- No helper added only because one test is annoying.
- No performance-only work unless it directly supports the application surface
  and carries its own proof.
