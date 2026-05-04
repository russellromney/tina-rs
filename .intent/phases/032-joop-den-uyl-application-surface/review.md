# 032 Joop den Uyl Review

## Plan Review 1

Verdict: correct phase shape, not quite ready until the surface decisions and
proof artifacts are pinned harder.

What looks strong:

- The phase is now big enough. It is about the local application surface, not a
  bag of tiny helpers.
- It correctly comes before Gemini. Docs should explain a settled service
  shape, not discover one.
- It refuses async handlers, Tokio bridge work, Tower/Axum, unbounded queues,
  and macro fog.
- The canonical service target is right: listener, connection, worker pool,
  supervisor, shutdown owner, capacities, timeouts, and trace/backpressure
  assertions.
- The plan requires runnable tests across sim/live layers instead of
  README-only examples.

Load-bearing gaps:

1. Helper surface remains too open. The plan lists many possible helpers and
   says decide during implementation. That can reintroduce the old problem:
   three little APIs for the same service ceremony. The plan should pin the
   first expected helper direction and pause-gate anything bigger.
2. "Less ceremony" needs a concrete proof. Before/after evidence should name
   the files and count repeated setup blocks, message boilerplate, or helper
   call sites. Otherwise closeout can become taste theater.
3. The canonical harness needs exact artifact names. Implementation should not
   get to invent whether it lives in old `local_production_runtime.rs`, a new
   `application_surface.rs`, or only in comparison tests.
4. Runner parity is good but vague. The plan should pin which runner owns which
   proof:
   - `tina-sim` owns deterministic oracle/replay;
   - explicit-step runtime owns semantic event shape without native threads;
   - `BetelgeuseRuntime` owns live native loopback and worker-thread lifecycle.
5. Trace helpers are listed as possible public helpers, but trace assertions
   may be test-only. The plan should default trace query/assertion helpers to
   crate/test support unless proven useful for users.
6. The 031 medium rocks are correctly mentioned, but "likely yes" on trace
   retention policy is too large for this phase unless narrowed. Query helpers
   are Joop-shaped; full trace retention modes are probably Gemini/Cassini
   unless needed by the canonical harness.

Medium tightenings:

- Pin helper crate ownership defaults:
  - `tina` only for generic effect ergonomics;
  - `tina-runtime` for runtime/service capacity and live runner helpers;
  - tests/support modules for trace assertions until promoted.
- Require a "one preferred path" cleanup pass after helpers land.
- Add a negative proof that helper macros do not hide message enums, timeout
  arguments, or capacity settings.
- Require README changes only if the old first-read code contradicts the new
  preferred shape; broad docs remain Gemini.
- Name that existing tests should be migrated to new helpers only when that
  improves the canonical surface, not as churn.

Recommended plan fixes:

- Add an "Expected Helper Direction" section.
- Add exact artifact names:
  - `tina-runtime/tests/application_surface.rs`;
  - `tina-sim/tests/application_surface.rs`;
  - optional updates to `tina-sim/tests/tokio_vs_tina_examples.rs`.
- Add ceremony scorecard:
  - repeated setup blocks removed;
  - capacity magic centralized;
  - shutdown path named once;
  - trace assertion helpers used in at least three assertions;
  - no extra public dialect.
- Narrow 031 rocks in this phase:
  - yes: capacity config and trace query helpers if needed;
  - maybe: batch helper;
  - defer: trace retention modes, typed fast paths, completion slot pooling,
    worker command boxing.

## Plan Review 2

Verdict: ready to hand off to implementation.

What changed after review:

- Added an expected helper direction with pause gates.
- Pinned artifact names:
  - `tina-runtime/tests/application_surface.rs`;
  - `tina-sim/tests/application_surface.rs`;
  - optional targeted updates to `tina-sim/tests/tokio_vs_tina_examples.rs`.
- Pinned runner responsibilities:
  - `tina-sim` owns deterministic oracle/replay proof;
  - explicit-step `Runtime` owns semantic event-shape proof;
  - `BetelgeuseRuntime` owns live native loopback and worker-thread lifecycle.
- Added crate ownership defaults for helper placement.
- Added a ceremony scorecard requirement so "better ergonomics" has concrete
  closeout evidence.
- Narrowed 031 medium rocks: capacity config and test trace query helpers are
  in-scope if needed; trace retention modes, typed fast paths, completion-slot
  pooling, and worker command boxing are deferred by default.
- Updated `ROADMAP.md` so Joop is now framed as application surface, not a
  small porting-helper phase.
- Added an explicit intensive proof matrix so implementation must cover
  deterministic replay with non-default seeds, delayed completions,
  no-pending-work shutdown, live loopback pressure, trace terminal outcomes,
  stale-address/requester-stopped regressions, and helper boundedness.

Remaining implementation risks:

1. The phase can still grow large if every possible helper gets promoted.
   Follow the audit table and pause gates.
2. A worker-pool helper can easily become a router/registry. Keep it tied to
   the canonical tests or do not ship it.
3. Trace helpers should start as test support. Public trace query APIs need a
   stronger user-facing reason.
4. The canonical service should not erase the older Willem Drees tests unless
   replacement proof is strictly stronger.
5. Macros are allowed but not expected. If added, they need negative proof that
   they do not hide message, timeout, capacity, or failure semantics.

Implementation should start with the audit and canonical service contract, not
with helper design.

## Implementation Audit 1

Friction table from `local_production_runtime` and the comparison tests:

| Area | Friction | Decision |
| --- | --- | --- |
| Canonical artifact | The service-shaped proof exists, but under `local_production_runtime.rs`, so future work has to know old phase lore. | **must fix now**: migrate the proof shape into `application_surface.rs` artifacts. |
| Capacity setup | Mailbox, worker, listener, connection, command, backlog, and pending-completion capacities are repeated as magic numbers. | **must fix now** in the canonical harness with one local `ServiceCapacities`; public runtime config waits until more than tests need it. |
| Trace assertions | Tests repeatedly hand-match `RuntimeEventKind` for `Full`, `Timeout`, accept/write completions, stopped isolates, and stale rejection. | **must fix now** as test-support helpers used across at least three assertions. Public trace query API deferred. |
| Sim/live parity | Sim and live tests prove similar behavior, but the parity contract is spread across separate files and names. | **must fix now**: new application-surface tests name which runner owns which proof. |
| Shutdown choreography | Shutdown cancellation is explicit and semantically important. Hiding it would make the safety claim worse. | **maybe fix now** only as named test harness helpers; no public shutdown DSL. |
| Worker pool | Current service has one bounded worker behind a supervised parent, enough to prove backpressure/timeouts/restart. | **defer** a public pool helper until multiple production-shaped tests need routing. |
| `batch(vec![...])` | Existing service uses `vec!` even though `batch([...])` is already the preferred small batch shape. | **must fix now** in canonical code; no new batch helper. |
| Macros | `#[tina_runtime::isolate]` already removes the six-associated-type slab. | **refuse** new macros in Joop unless later proof shows remaining boilerplate is Rust-only ceremony. |
| README/docs | Broad language polish belongs to Gemini. | **defer**, except correcting contradiction if the canonical surface changes public usage. |

Contract for implementation:

- canonical runtime artifact: `tina-runtime/tests/application_surface.rs`;
- canonical simulator artifact: `tina-sim/tests/application_surface.rs`;
- old Willem Drees tests may be migrated into those names, preserving their
  proof strength;
- no public helper is promoted until the canonical tests still repeat it after
  local test-support helpers exist.

## Implementation Review 1

What landed:

- Migrated the Willem Drees service proof into canonical Joop artifacts:
  - `tina-runtime/tests/application_surface.rs`;
  - `tina-sim/tests/application_surface.rs`.
- Added local `ServiceCapacities` in both canonical tests so mailbox,
  command-queue, backlog, worker, connection, and pending-completion capacities
  are named once instead of scattered as magic numbers.
- Replaced canonical `batch(vec![...])` use with `batch([...])`. No new batch
  helper was needed.
- Added test-support trace helpers for:
  - event existence;
  - exact event counts;
  - at-least event counts;
  - observation checks;
  - listener-stopped-and-idle checks;
  - terminal-outcome invariants for send and call dispatch attempts.
- Added an explicit-step `Runtime` application-surface proof using simulated
  Betelgeuse I/O. This fills the runner gap between `tina-sim` oracle and
  threaded `BetelgeuseRuntime`.
- Added non-default-seed replay proof for the simulator application oracle.
- Added the two non-TCP porting proofs promised by the plan:
  - bounded worker/router pressure without TCP;
  - stateful session/control-plane shape with local audit send and snapshot.

Runner coverage now:

- `tina-sim/tests/application_surface.rs`
  - deterministic oracle/replay;
  - default-seed operation-count proof;
  - non-default-seed replay proof;
  - bounded worker `Full`;
  - mandatory timeout;
  - partial write proof;
  - terminal dispatch outcome invariant.
- `tina-runtime/tests/application_surface.rs`
  - live native loopback with three concurrent clients;
  - explicit-step runtime over simulated I/O;
  - threaded runtime over simulated I/O with partial writes;
  - bounded worker/router proof without TCP;
  - stateful session/control-plane proof;
  - supervised worker restart and stale-address rejection;
  - shutdown cancellation for pending accept/read/timer/isolate-call work;
  - shutdown cancellation for pending write work, including safe external
    simulated-I/O steps after shutdown;
  - terminal dispatch outcome invariant on every canonical trace.

Ceremony scorecard:

- canonical artifact names: 2 new test artifacts replace lore-bound
  `local_production_runtime` names;
- capacity magic centralized: yes, local `ServiceCapacities` owns the repeated
  service numbers;
- trace assertion simplification: yes, helpers are used across success,
  overload, timeout, partial-write, restart/stale, and shutdown assertions;
- runnable porting proofs: TCP service, bounded worker/router, and stateful
  session/control-plane are all direct tests;
- shutdown path reuse: named wait/trace helpers, no public shutdown DSL;
- public helpers added: none. The audit did not justify a stable public helper
  yet, so Joop keeps the public surface smaller.

Focused verification:

- `cargo +nightly test -p tina-runtime --test application_surface`
- `cargo +nightly test -p tina-sim --test application_surface`
- `cargo +nightly test -p tina-runtime -p tina-sim`
- `make verify`

Verdict after Implementation Review 1: Joop's first implementation slice is
complete enough to close unless external review finds a bug. No public helper
surface was added because the canonical harness got materially clearer with
local capacity and trace helpers alone; grug prefers no new public rock when
test-support rock does job.

## Implementation Review 2

Verdict: no code findings.

Review focus:

- The canonical service migration preserves the old Willem Drees proof strength
  while moving it into the planned `application_surface` artifacts.
- `ServiceCapacities` stays test-local, so it does not create a premature
  public runtime config surface.
- The bounded router proof uses the real mailbox-full path: one worker mailbox
  slot, two same-turn calls, one accepted reply and one `CallOutcome::Full`.
- The stateful session proof exercises ordinary local state plus local send,
  without TCP hiding the basic application shape.
- The terminal-outcome invariant is useful here: every send/call dispatch
  attempt in the canonical traces has one visible accepted/rejected/completed/
  failed/rejected terminal event.
- The explicit-step runtime proof, threaded simulated-I/O proof, live native
  loopback proof, and `tina-sim` replay proof cover the runner split promised
  by the plan.

Residual risk:

- Joop intentionally did not add public application helpers. If the next real
  service-shaped workload repeats the same local `ServiceCapacities` or trace
  assertion helpers, that is the right moment to promote a small public/test
  support API. Promoting now would be guessing.

Verification already run after this slice:

- `cargo +nightly test -p tina-runtime --test application_surface`
- `cargo +nightly test -p tina-sim --test application_surface`
- `cargo +nightly test -p tina-runtime -p tina-sim`
- `make verify`
