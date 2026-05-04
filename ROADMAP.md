# tina-rs Roadmap

A staged plan for porting Tina's discipline to Rust, structured to deliver
value at each phase rather than waiting for a big-bang release.

Phases are named (not numbered) so we can insert phases later without
renumbering. Existing phase names use space missions; new forward phases use
full names of Dutch prime ministers so the roadmap can change direction without
renaming landed history.

Completed work moves to `CHANGELOG.md`. `ROADMAP.md` is for active and future work.

---

## Vision

Bring Tina's three load-bearing ideas — synchronous effect-returning handlers,
isolate-per-entity state machines, and thread-per-core scheduling with bounded
mailboxes — to Rust **without** building a new general-purpose async runtime
from scratch.

The long-term target is a performant, shared-nothing, bounded, deterministic
Rust concurrency framework. Actor/OTP/Akka systems are useful prior art, but
Tina should not chase Akka feature parity for its own sake. Actor-shaped
isolate state machines are the means; the product is safer Rust concurrency
with visible overload, cancellation, restart, and replay behavior.

The near-term deliverable is a small set of crates (`tina`, `tina-runtime`,
`tina-mailbox-spsc`, `tina-supervisor`, and `tina-sim`) that can run real local
server-shaped workloads with stronger boundedness and testability than ordinary
Tokio-shaped code.

## Non-goals

- A new runtime competing with Tokio/monoio. Use what exists.
- Full feature parity with Tina-Odin. We port the *shape*, not every primitive.
- Akka feature parity as a goal. Persistence, remoting, and clustering are
  future capabilities only if they preserve Tina's safety/performance
  direction.
- "Replacing Tokio." Tokio may still matter as a bridge or comparison point,
  but it should not define Tina's core programming model.

## Crate layout (target shape)

Following the abstraction-vs-implementation rule (capability traits live in their own crate; backends are siblings):

- `tina` — trait crate. `Isolate`, `Effect`, `Mailbox`, `Shard`, plus any small policy types that truly belong at the abstraction boundary. **No impls.**
- `tina-mailbox-spsc` — SPSC ring buffer impl
- `tina-mailbox-mpsc` — MPSC fallback impl
- `tina-supervisor` — supervision tree mechanism
- `tina-runtime` — single-shard runtime proving Tina semantics on an
  explicit-step backend, with completion-driven I/O as the intended Mariner
  direction
- `tina-runtime-monoio` — multi-shard runtime on monoio (io_uring)
- `tina-runtime-tokio-bridge` — adapter for adopting tina inside an existing Tokio app
- `tina-sim` — deterministic simulator

End consumers depend on `tina` plus one runtime crate. Dependencies flow concrete → abstract; runtime crates depend on `tina`, never on each other.

## Current evidence snapshot

The current repo has already moved past the original "vocabulary only" state.
Completed work lives in `CHANGELOG.md`; this snapshot is here so future phases
start from an honest baseline rather than from stale roadmap wording.

| Claim | Current evidence | Still missing |
|---|---|---|
| Trait/API discipline | `tina` exposes `Isolate`, closed `Effect`, typed `Address`, `Outbound`, `ChildDefinition`, supervision policy types, and the preferred authoring surface (`tina::prelude::*`, `#[tina::isolate(...)]`, `#[tina_runtime::isolate(...)]`, effect helpers, typed call helpers, `ctx.me()`, and `ctx.send_self(...)`). | Small call-result helper polish remains optional. |
| Bounded mailbox semantics | `tina-mailbox-spsc` proves FIFO, `Full`/`Closed`, no hidden overflow queue, drop accounting, allocation accounting, focused Miri unsafe-memory checks, and selected Loom interleavings. Cross-shard shard-pair queues are bounded and directly proved in Galileo. | This is not a full formal proof for every capacity/interleaving/refactor. Any future MPSC fallback is not implemented. |
| Single-shard runtime delivery | `tina-runtime` has deterministic trace IDs and causal links, registration-order stepping, local send dispatch, local spawn dispatch, typed ingress, stop-and-abandon, panic capture, address generations, runtime-owned parent-child lineage, restartable child records, direct-child `RestartChildren` execution, supervised panic restart with policy/budget config, an assertion-backed task-dispatcher proof package, and generated-history property tests. | Supervision is still narrow: panic-triggered only, runtime-lifetime budget only, and no timed budget windows. The generated-history model is bounded and does not prove arbitrary user programs. |
| Failure isolation | Unwinding handler panics become runtime events; the panicking isolate stops and the same round continues deterministically. | This is not Tina-Odin's OS trap boundary. Rust segfault isolation, shard quarantine, and `panic = "abort"` behavior are out of scope unless a later phase explicitly designs them. |
| Multi-shard runtime/sim | `tina-runtime` and `tina-sim` now expose multi-shard explicit-step runners with root placement, global event/call ids, bounded shard-pair queues, next-step-only remote visibility, deterministic harvest order, source-time versus destination-time delivery stages, simulator replay, user-shaped dispatcher proofs, sealed address-local remote-failure behavior, and shard-local supervision/restart ownership. Huygens added first live worker-per-shard runners with bounded live ingress and bounded live cross-shard transport; 025 renames that live surface to `BetelgeuseRuntime` / `BetelgeuseMultiShardRuntime`. | Thread pinning/topology, peer quarantine, shard-restart propagation, cross-shard child ownership, and live cross-shard isolate-call reply transport remain future work unless 025 explicitly chooses to add them. |
| Replayability | Runtime traces are deterministic across repeated identical single-shard runs, including generated operation histories and small generated dispatcher workloads. Trace replay proofs can reconstruct worker completions and restart outcomes from the runtime event model alone. `tina-sim` adds virtual time, replay records, seeded delays/reordering over timer-wake/local-send/TCP-completion behavior, checker failures, spawn/supervision replay, scripted TCP simulation, multi-shard replay under default and non-default seeded configs, and multi-shard checker failure replay. | Real substrate liveness faults remain future work; current explicit-step shard-liveness non-claims are sealed. |
| Runtime allocation story | The SPSC mailbox hot path is tested for no per-message allocation after warm-up. Ruud Lubbers pins a narrow numerical runtime cost model for selected hot paths: multi-shard send, isolate call, timer, TCP read/write, batch, spawn/restart, trace pressure, live ingress, and high-cardinality idle stepping. Runtime and simulator now reuse per-step scratch and prebuild coordinator storage where tests prove the warmed path. | No broad runtime/simulator allocation-free claim is supported yet; boxed erasure, traces, replay records, completion slots, call translators, and user payloads may still allocate. |
| Reference examples | A Rust task-dispatcher proof package and a TCP echo proof package both exist with matching runnable examples, backed by assertions rather than logs alone. The echo proof now keeps the listener alive across a one-client smoke run, a sequential multi-client run, and a bounded-overlap run, then closes the listener cleanly and exits. | These are still proof workloads, not a broad production-server claim or benchmark story. |
| Runtime-owned I/O | `tina` names a runtime-owned call effect family (`Effect::Call(I::Call)` plus `Isolate::Call`) and an ordered batch effect (`Effect::Batch(Vec<Effect<I>>)`) for closed-set sequencing of existing effects. `tina-runtime` executes the first TCP call family — bind, accept, read, write, close — through Betelgeuse on nightly Rust, with caller-owned typed completion slots, runtime-assigned opaque resource ids, runtime-controlled completion translation back into ordinary `Message` values, honest `local_addr` reporting for `127.0.0.1:0` binds, accepted-stream `peer_addr`, listener re-arm through normal isolate control flow, and clean listener close. It also executes the first time call verb — `Sleep { after }` with `TimerFired` — with runtime-owned monotonic clock sampling once per step, due-timer harvest, deterministic request-order tie-break for equal deadlines, and a crate-private manual clock seam for deterministic timer tests. 025 adds a narrow Betelgeuse simulated TCP backend with seeded completion delay and partial-write pressure, then proves Tina TCP effects through it on explicit and threaded runners. | The 100k-connection benchmark, broader network-server claims, and live-substrate liveness faults remain future work. |

## Testing and proof strategy

We should prove the discipline in layers, matching the abstraction-vs-implementation split:

- **Trait crate (`tina`)** proves API shape and compile-time guarantees only. This is where doc tests, compile-fail tests, and downstream-style integration tests belong.
- **Mailbox crates** prove concrete queue semantics. This is where FIFO, boundedness, `Full`/`Closed`, and no hidden buffering get tested against real implementations and under loom.
- **Unsafe mailbox code** should keep both Loom and focused Miri coverage.
  Loom explores selected concurrent schedules; Miri pressures unsafe memory
  validity. Neither is a total formal proof, so future unsafe refactors should
  add targeted models rather than relying on old green runs.
- **Runtime crates** prove delivery semantics. This is where we can assert that accepted sends become handler invocations, that `Stop` actually stops delivery, and that effect dispatch is the only place side effects happen. Generated-history property tests should cover broad bounded invariants; hand-authored tests should still pin exact causal chains for important behaviors.
- Most runtime proofs should stay black-box integration tests, but when a slice
  proves crate-private runtime state that should not become public API, those
  proofs may live in `src/lib.rs` unit tests instead of `tests/*.rs`.
- **Simulator** proves interleavings and replay. This is where we stop trusting timing-sensitive live tests and start proving seeded, reproducible traces.

Live examples matter, but they are smoke tests, not the proof. Every runnable example should be backed by black-box assertions in the crate that owns the implementation being exercised.

Current future proof gaps to keep visible:

- Runtime property tests are bounded generated histories, not a proof over all
  possible user isolate programs.
- SPSC unsafe correctness has Loom and Miri evidence, not a complete formal
  proof across all future refactors.
- Runtime allocation behavior is intentionally not claimed beyond the narrow
  SPSC hot path and the explicit Kepler non-claim.
- Real substrate peer/shard liveness, shard-restart propagation, and
  cross-shard child ownership remain future work.

## Optional post-021 syntax cleanup

021's main ergonomics bar is now met. If real user code still shows repeated
tiny translator friction, a later cleanup slice may add one very small helper
family around typed call result mapping.

Possible candidates:

- `map_ok(...)`
- `map_err(...)`
- one compact paired helper such as `ok_err(...)`

This is optional polish only. It should stay tiny, avoid creating a second
effect DSL, and preserve explicit completion-as-message semantics. Tokio does
not get its readability from helpers like these; it mostly gets it from
`async`, `.await`, `?`, and focused I/O helpers such as `write_all`.

## IDD phase shape

The rows below are planning buckets, not necessarily one `spec-diff.md` each.
IDD execution should still happen in reviewable slices when a bucket contains
several semantic decisions. The lesson from Mariner slices 001-006 is not "make
everything tiny forever" or "fuse everything into one heroic review"; it is
"choose slices that preserve intent and can survive independent review."

Avoid one-helper phases that only move bookkeeping around, but also avoid
bundling independent design questions into one review unit. A bucket like
"supervision and dispatcher proof" should become several IDD slices if address
liveness, restart records, restart execution, supervisor policy, and examples
need separate intent decisions.

When a bucket is approved, implementation may run in an autonomous stacked-slice
loop: propose the slice stack once, then run each slice through spec, plan,
review, implementation, evidence, follow-up review, and commit without stopping
for the human unless an escalation gate trips. Escalate for public API changes,
semantic ambiguity, reviewer disagreement, unsafe/concurrency/allocation-claim
changes, roadmap order changes, or public positioning questions. Tiny review
findings should be folded into the active slice instead of becoming separate
phases.

| Next phase package | Scope |
|---|---|
| **Mariner supervision and dispatcher proof** | Planning bucket covering supervised restart hardening, timed or explicitly deferred restart-budget windows, and a Rust task-dispatcher proof example with trace assertions. Expected to split into multiple IDD slices. |
| ~~**Mariner I/O, current runtime, and echo**~~ | Delivered as one reviewed package with autonomous internal slices (`.intent/phases/012-mariner-io-current-runtime-and-echo/`). Shipped: runtime-owned call effect family at the `tina` boundary, runtime-owned child bootstrap message on `ChildDefinition` / `RestartableChildDefinition`, Betelgeuse-backed TCP call family in `tina-runtime`, focused call-dispatch tests, assertion-backed TCP echo integration test (with partial-write retry coverage) and runnable `tcp_echo` example. Substrate is Betelgeuse on nightly Rust per the human-anchored plan; runtime-owned sleep / timer wake follows in a later slice once the call contract has a verb whose completion the runtime can drive on demand. |
| ~~**Mariner TCP completeness**~~ | Delivered as a reviewed package (`.intent/phases/014-mariner-tcp-completeness/`). Shipped: ordered `Effect::Batch(Vec<Effect<I>>)` at the `tina` boundary, direct batch-semantics proof in `tina-runtime`, listener self-addressing plus re-armed `TcpAccept` through normal isolate control flow, sequential and bounded-overlap TCP echo proofs, graceful listener close/stop, a refreshed assertion-backed `tcp_echo` example that accepts exactly `N` clients and exits, and a crate-local proof that two accepted stream reads can be pending in `IoBackend` at once. |
| ~~**Mariner runtime-owned time and retry**~~ | Delivered as a reviewed package (`.intent/phases/015-mariner-runtime-owned-time-and-retry/`). Shipped: one-shot relative `Sleep` call verb and `TimerFired` result, runtime-owned monotonic clock sampled once per step with due-timer harvest, deterministic request-order tie-break for equal deadlines, crate-private `ManualClock` seam for deterministic timer tests, focused timer semantics proofs (single wake, no early fire, fires once, different-deadline ordering, equal-deadline tie-break, stopped-requester rejection), and a retry/backoff proof package with both crate-local semantics tests and a public-path integration test for delayed retry. |
| **Voyager deterministic simulation** | Planning bucket with reviewed slices delivered in `.intent/phases/016-voyager-virtual-time-and-replay/`, `.intent/phases/017-voyager-seeded-faults-and-checkers/`, `.intent/phases/018-voyager-spawn-and-supervision-simulation/`, and `.intent/phases/019-voyager-single-shard-io-simulation/`. Shipped so far: `tina-sim`, a single-shard virtual-time simulator for the shipped `Sleep { after }` / `TimerFired` contract, deterministic replay artifacts, direct timer-semantics proofs, simulator-backed retry/backoff proof, seeded perturbation over timer-wake and local-send behavior, a small checker surface with replayable failure capture, single-shard spawn/supervision replay covering public spawn payloads, restart policies, stale identity, budget exhaustion, and direct-child scope, and scripted single-shard TCP simulation covering the shipped bind/accept/read/write/close call family plus replayed echo workloads and TCP checker replay. Remaining Voyager work still includes broader PRNG policy, richer faults/checkers, and later multi-slice expansion. |
| ~~**Galileo multi-shard semantics and simulation**~~ | Delivered in `.intent/phases/020-galileo-multi-shard-semantics-and-simulation/`: multi-shard explicit-step runtime/simulator runners, cross-shard delivery, routing/placement, deterministic traces, replay, source-time vs destination-time delivery stages, seeded simulator composition proofs, and user-shaped dispatcher/TCP/supervision proof workloads. |
| ~~**Kepler core primitive completion**~~ | Delivered in `.intent/phases/022-kepler-core-primitive-completion/`: sealed the current explicit-step liveness non-signal, proved address-local remote failures do not poison shards, sealed shard-local supervision/restart ownership, pinned ownership/buffering/allocation non-claims, added multi-shard checker/replay pressure, and added user-shaped runtime/simulator e2e proofs. |
| ~~**Huygens DST harness and runtime substrate**~~ | Delivered in `.intent/phases/023-huygens-dst-runtime-substrate/`: composed-workload DST harnessing, TCP/timer/supervision/cross-shard replay pressure, `ThreadedRuntime`, `ThreadedMultiShardRuntime`, bounded live ingress, bounded live cross-shard transport, and user-shaped live substrate proofs. |
| ~~**Mercury production-shaped runtime contract**~~ | Delivered in `.intent/phases/024-mercury-production-shaped-runtime-contract/`: bounded observed send, isolate-to-isolate call with mandatory timeout, live runner lifecycle, live supervision/restart, allocation probes, macros/devex cleanup, and Tokio-vs-Tina semantic comparisons. Mercury made the primitive sharper; it is not the final substrate story. |
| ~~**Betelgeuse runtime substrate completion**~~ | Delivered in `.intent/phases/025-betelgeuse-runtime-substrate-completion/`: backend-honest `BetelgeuseRuntime` / `BetelgeuseMultiShardRuntime` names, shard-local Betelgeuse ownership, bounded ingress proof, live time/TCP completion semantics, live multi-shard bounded send, typed live cross-shard call rejection, narrow Betelgeuse simulated TCP backend with seeded delay/partial-write pressure, allocation probes, and oracle/sim/live parity tests. Tokio stays comparison/later bridge, not the main runtime story. |
| ~~**Tina TCP driver contract**~~ | Delivered in `.intent/phases/026-tina-driver-contract/`: runtime-owned time/TCP behind a small Tina-owned driver boundary, timers, TCP submissions, completions, cancellation, shutdown, native Betelgeuse adapter, simulated Betelgeuse adapter, same-resource `ResourceBusy` semantics, and direct cancellation/late-completion proofs. This is not a general async runtime and not a Tokio bridge. |
| ~~**Parallel substrate support**~~ | Delivered in `.intent/phases/027-parallel-substrate-support/`: Betelgeuse simulated I/O polish, narrow substrate cost evidence, expanded Tokio-vs-Tina constrained/backpressure comparisons, external review prompts, Tokio current-thread/Monoio/Glommio/Compio adapter research, and brief README/story refinement without changing Tina core semantics. |
| ~~**Ranger core runtime substrate completion**~~ | Delivered in `.intent/phases/028-ranger-substrate-driver-maturity/`: documented the driver capability contract, moved TCP pending ownership to listener/read/write lanes, allowed full-duplex same-stream read/write, kept close and duplicate-lane `ResourceBusy` honest, made per-call cancel tombstone without silently closing unrelated lanes, added live worker TCP shutdown proof, pinned TCP read/write allocation counts, and recorded Betelgeuse as the near-term substrate direction. |
| ~~**Surveyor Betelgeuse adapter ownership**~~ | Delivered in `.intent/phases/029-surveyor-betelgeuse-adapter-ownership/`: Tina now treats its live substrate as a Tina-owned implementation over Betelgeuse, with explicit completion-slot ownership, no-leak shutdown/cancel-drain, controlled simulated/native backend release proofs, and no dependence on upstream Betelgeuse growing Tina-specific guarantees first. |
| ~~**Willem Drees local production runtime**~~ | Delivered in `.intent/phases/030-willem-drees-local-production-runtime/`: Tina now has a composed one-process server-shaped proof covering listener/connection lifecycle, graceful shutdown, bounded overload, supervisor behavior under live TCP pressure, memory ceilings/backpressure guards, and server-shaped assertions rather than demo logs. |
| ~~**Ruud Lubbers performance and memory hardening**~~ | Delivered in `.intent/phases/031-ruud-lubbers-performance-memory-hardening/`: measured and improved runtime/simulator/driver hot paths, pinned narrow allocation counts for send/call/timer/TCP/batch/spawn/restart/trace/live-ingress/high-cardinality idle stepping, reduced multi-shard send hot path to `1 alloc / 0 realloc`, kept SPSC no-allocation proof intact, preserved trace/replay and next-step remote visibility, and deferred medium cost rocks explicitly. |
| ~~**Joop den Uyl application surface**~~ | Delivered in `.intent/phases/032-joop-den-uyl-application-surface/`: migrated the local-production workload into canonical `application_surface` tests, named service capacities in the harness, added trace assertion helpers and terminal-outcome invariants, proved the service shape across `tina-sim`, explicit-step runtime, threaded simulated I/O, and live Betelgeuse loopback, and added bounded-router plus stateful-session porting proofs without adding a premature public app-builder surface. |
| **Gemini release story** | Release-story phase after the local production runtime, performance/memory story, and application surface are real. Supported invariant docs, guides, examples, semver/publication decision, CI/proof gate, public positioning, and a clear adoption story. Gemini should not add new core semantics; it documents a framework that already has real proof and a runtime path. |
| **Apollo Tokio bridge** | Preserved/weakened guarantees table, minimal bridge, and an assertion-backed Axum or similar reference adoption example. Apollo remains an adoption bridge, not the center of Tina's runtime story. |
| **Cassini hardening** | Optional MPSC decision, benchmark suite, memory profile, docs polish, and dogfood report. |
| **Wim Kok persistence** | Future durable-state phase: snapshots, event journal, restart recovery, durable replay artifacts, and explicit non-claims around durable mailboxes until they are designed. Not required for first local-runtime launch. |
| **Jan Peter Balkenende remoting** | Future networked Tina-to-Tina phase: serialization, node identity, remote isolate identity, bounded remote ingress, remote `Full`/`Closed`/`Timeout`/node-down semantics, and cross-node trace causality. Not required for first local-runtime launch. |
| **Mark Rutte clustering** | Future cluster phase after remoting is boring: membership, placement, peer quarantine, node liveness, shard migration/rebalancing if it still fits Tina's safety and performance model. Not required for first local-runtime launch. |

Real concurrent shard execution is a substrate story around Huygens, Mercury,
Betelgeuse substrate completion, the Tina driver contract, and later runtime
work, not something Galileo or Kepler quietly smuggled in.

027 is a parallel/support lane. It should not block 026 unless it finds a real
contradiction in the substrate contract.
Galileo and Kepler proved the multi-shard contract under one explicit global
coordinator thread first. Huygens added the first worker-owned runtime
substrate around that contract. Mercury sharpened the overload/call contract.
025 makes Betelgeuse the honest tryable runtime substrate instead of letting a
Tokio bridge become the center of gravity by accident. 026 and 027 make the
driver boundary and support evidence real enough to inspect. Ranger settles the
live substrate/driver semantics: full-duplex TCP, cancellation, shutdown,
driver capabilities, cost, the next substrate direction, and the core/non-core
boundary. Surveyor follows because the Betelgeuse implementation should now be
treated as Tina-owned code over Betelgeuse primitives, with its own
completion-ownership and no-leak shutdown contract.

After Willem Drees, the roadmap deliberately stays local before release polish:
Ruud Lubbers keeps performance and allocation claims honest; Joop den Uyl makes
ordinary server-shaped Tina applications less fragile to structure and port.
Gemini only freezes and publishes the story after those rocks are real.
Persistence, remoting, and clustering remain later Tina capabilities, not
Akka-parity launch blockers.

## Strategic prerequisites

These should be resolved early enough to avoid rework, but they do not all block implementation at the same phase:

- **Decide the Peter Mbanugo / Tina-Odin public-positioning question early.**
  Preferred path: reach out before public publish and coordinate if practical.
  If that does not happen, docs must be explicit that `tina-rs` is an
  independently maintained Rust project inspired by Tina-Odin, not an official
  project or implied endorsement. Local design exploration is not blocked on
  this, but public positioning and any publish decision should not outrun an
  explicit decision.
- **Commit to the hot-path allocation story early.** If "zero per-message allocation after warm-up on the hot SPSC path" is a real invariant, Pioneer and Mariner must be designed around it. If that is too strong, narrow the claim before the runtime crates ship.
- **Decide the address liveness story before supervision.** Tina-Odin's
  examples rely on stale handles failing safely after restart. `tina-rs` needs
  an explicit generation/stale-address design, or a documented alternative,
  before the dispatcher proof example can honestly mirror the reference.
- **Design runtime-owned I/O before echo.** The current `Effect` vocabulary has
  no Rust equivalent of Tina-Odin's I/O, timer, or call effects. TCP echo should
  not arrive before the boundary between handler descriptions and runtime I/O is
  written down.

---

## Phase Mariner
> First single-thread runtime. Effect dispatcher + supervision mechanism.

> After: Completed Sputnik and Pioneer work · Before: Phase Voyager

- `tina-runtime`: single-shard runtime with explicit-step execution.
  Mariner should prefer a completion-driven backend that keeps progression
  visible and DST-compatible rather than quietly centering a futures executor.
  Pin to one core. Run a poll loop: drain mailboxes → run handlers → dispatch
  effects.
- `tina-supervisor`: supervisor configuration vocabulary exists; broader reusable supervision mechanism should grow only when multiple runtime crates need it.
- The effect dispatcher is the **only** place real I/O happens. Handlers return effects; the dispatcher executes them. This is the property that makes deterministic simulation possible later.
- Continue using the deterministic runtime event trace as the semantic proof surface. The trace records mailbox accept/reject, handler invocation start/end, effect dispatch, stop, spawn, panic, abandonment, and restart events with causal linkage so tests and replay can reason about provenance rather than only timeline order.
- Build supervision on stored runtime state, not trace reconstruction. Parent-child lineage, restartable child records, address liveness, direct-child restart execution, and supervised panic restart already exist; the remaining supervision work needs hardening and proof examples.
- A task-dispatcher proof example should land before TCP echo. It mirrors Tina-Odin's "dead worker is not a dead system" example without needing runtime-owned network I/O first.
- A working TCP echo server isolate (mirroring Tina-Odin's example) lands after
  the Rust I/O/timer effect contract and completion-driven current-thread
  driver exist.
- Keep the abstraction boundary strict: `tina-runtime` owns scheduling, polling, and effect execution; `tina` must not grow runtime helpers just to make tests easier.
- Runtime tests should inject a deterministic test mailbox through `Mailbox<T>` where possible. Benchmarks and smoke examples can use the real SPSC crate, but correctness tests should avoid coupling two fresh implementations unless that coupling is the point of the test.

**Proof plan:**

- Existing black-box runtime tests already prove delivery semantics on one shard for local sends, FIFO-preserving stepping, `Stop`, rejected sends, spawn, typed ingress, panic capture, and deterministic traces.
- The remaining supervisor tests should prove actual runtime behavior:
  - a panicking child is restarted according to policy
  - stale targets fail safely after restart, or the documented Rust alternative is proven
  - sibling survival matches `one-for-one`, `one-for-all`, and `rest-for-one`
  - restart-budget exhaustion halts restart loops predictably
  - supervisor mechanisms consume stored lineage/restart records instead of reconstructing parenthood from the trace
- Trace-oriented integration tests assert on the runtime event trace and prove that two identical seeded runs on the single-shard runtime produce the same event sequence with the same causal chains.
- The task-dispatcher and echo examples are used as end-to-end smoke/benchmark surfaces, not as the only evidence of correctness.

**Done when:** supervisor tests prove actual restart behavior on the single-shard runtime; task-dispatcher proves stale worker identity/restart behavior with trace assertions; trace tests remain deterministic on repeated runs with stable causal linkage; the Rust I/O/timer contract is written down; echo server handles 100k connections on a single shard with stable memory; runtime allocation claims are either proven narrowly or explicitly revised before publish.

---

## Phase Voyager
> Long-duration deep-space mission. Deterministic simulation for the single-shard runtime.

> After: Phase Mariner · Before: Phase Galileo / Huygens proof work

- `tina-sim`: deterministic simulator for the single-shard runtime. Time is virtual, I/O is intercepted, mailbox arrival order is reproducible from a seed.
- `tina-sim` consumes Mariner's event trace as its semantic model. The simulator does not invent a second observable surface; it provides a different execution and I/O substrate against the same event vocabulary.
- Reviewed Voyager footholds are now landed: `tina-sim` exists as a
  single-shard virtual-time simulator for the shipped timer contract
  (`Sleep { after }` / `TimerFired`), captures replay artifacts, proves a
  retry/backoff workload under virtual time, makes the seed semantically real
  for narrow local-send/timer perturbation, adds a small checker surface with
  replayable failure capture, replays the shipped single-shard
  spawn/supervision surface, and now simulates the shipped single-shard TCP
  bind/accept/read/write/close call family with scripted listeners/peers,
  replayed peer output, and checker-backed TCP fault replay. Broader
  simulation surfaces still follow in later Voyager slices.
- Use Tina-Odin's DST shape as the conceptual bar: a domain-isolated PRNG tree, integer-ratio fault probabilities, ordinary test-driver isolates rather than privileged injection, structural/user checkers, and replay artifacts that include seed/config/trace.
- Failure injection: drop messages, simulate crashes, inject slow disk or I/O resources, delay completions, and perturb delivery order within the single-shard model.
- Replay: every test failure produces a seed that reproduces the failure exactly.
- Keep the abstraction boundary strict: runtime crates expose enough hooks for simulation, but the simulator owns virtual time, trace capture, and failure injection.
- Voyager and Huygens run before Gemini deliberately: the simulator surfaces
  runtime hooks, and the runtime-substrate phase proves those hooks under
  composed workloads before Gemini stabilizes a public story around them.

**Proof plan:**

- Seeded simulator tests prove reproducible delivery traces across repeated runs of the single-shard runtime.
- Different-seed tests under faults diverge in observable, intentional ways.
- Failure-injection tests prove behavior under dropped messages, crashes, and slow resources without relying on wall-clock timing.
- Checker tests prove framework invariants and at least one user-defined invariant can halt the run with a reproducible seed.
- Replay tests prove that a saved seed/config reproduces a prior failure exactly.

**Done when:** single-shard simulated workloads converge to a known good state every run; replay from a saved seed/config reproduces failures exactly; the simulator catches a deliberately-injected single-shard ordering bug that production tests miss; simulation docs explain test drivers, checkers, faults, and replay without relying on logs as proof.

This is the highest-leverage phase. Deterministic simulation is what makes Tina's discipline pay off — failures become reproducible artifacts, not phantoms.

---

## Phase Huygens
> Probe deployment. Prove composed workloads and land the first real runtime substrate.

> After: Phase Kepler · Before: Phase Mercury

Delivered in `.intent/phases/023-huygens-dst-runtime-substrate/`.

- Added a composed DST-style proof harness around timers, local sends, TCP
  completions, supervision/restart, stale addresses, bounded backpressure,
  cross-shard routing, replay, and checkers.
- Added `ThreadedRuntime`, a one-worker-thread live substrate for one shard.
- Added `ThreadedMultiShardRuntime`, a fixed worker-per-shard live substrate
  with bounded cross-shard transport for `Send + 'static` payloads.
- Proved TCP echo / request-response on simulator/replay, explicit-step
  runtime oracle, and live threaded runtime substrate.
- Proved live bounded ingress, local mailbox `Full`, live cross-shard
  request/reply, live remote queue `Full`, and stale remote address survival.

**Done:** the repo can honestly say the primitives survive composed DST
pressure and a real shard-owned runtime path can run selected user-shaped
workloads with bounded backpressure and synchronous effect-returning handlers.

---

## Phase Mercury
> Overload lab and runtime contract. Prove Tina's primitive under constrained memory.

> After: Phase Huygens · Before: Phase Betelgeuse

Mercury exists because Huygens proved the model and first live substrate, but
not the production-shaped overload/call contract that makes Tina's value
obvious. Do Mercury before release/docs polish.

- Add user-visible send backpressure. App code must be able to react to
  `Accepted`, `Full`, and `Closed`, not only inspect trace after the fact.
- Add isolate-to-isolate call with mandatory timeout for request/reply work.
- Harden live runner lifecycle: start roots, run, drain/shutdown, worker error
  reporting, trace inspection, and bounded config.
- Prove live supervision/restart on the Betelgeuse substrate: a worker panics,
  supervisor restarts it, and later work succeeds.
- Run the same core Tina workload through deterministic simulator replay and
  the live runner.
- Compare against naive Tokio and hardened Tokio versions where useful, but do
  not make Tokio the substrate story.
- Pin capacity/allocation claims: either prove stronger runtime bounds or keep
  the claim explicitly narrower than Tina-Odin's no-hidden-allocation story.

**Done when:** the repo can honestly say: "you can try replacing selected
Tokio-shaped workloads with Tina when you want bounded queues, shard-owned
state, timeout-based request/reply, deterministic testing, and a live
thread-per-shard runtime path." Betelgeuse substrate completion must not start
from a fuzzy runtime claim.

---

## Phase Betelgeuse
> Complete the real runtime substrate story.

> After: Phase Mercury · Before: Phase Tina Driver Contract

Betelgeuse exists because the core primitive should ride a real shard-local
completion runtime before we talk about bridges, release polish, or broad
adoption. The plan lives in
`.intent/phases/025-betelgeuse-runtime-substrate-completion/plan.md`.

- Treat Betelgeuse as the primary live substrate for this slice.
- Keep the explicit-step runtime and `tina-sim` as the semantic oracle.
- Rename or introduce backend-honest live runner names such as
  `BetelgeuseRuntime` and `BetelgeuseMultiShardRuntime`.
- Complete bounded ingress, shutdown, worker error, live time, live TCP, and
  trace behavior on the Betelgeuse runner.
- Add/use the narrow Betelgeuse simulated TCP backend for seeded substrate
  pressure instead of inventing a second live chaos layer.
- Prove live multi-shard bounded sends; implement or explicitly reject
  cross-shard isolate calls on the live runner.
- Pin allocation and cost numbers for the touched hot paths.
- Do not build Tower/Axum, a Tokio bridge, arbitrary async handlers, or release
  docs in this phase.

**Done when:** Tina has a named Betelgeuse-backed live runtime substrate that
runs the same synchronous-effect isolate code as the oracle/simulator, with
bounded queues, runtime-owned time/TCP completions, seeded Betelgeuse simulated
TCP proof for touched I/O paths, and honest non-claims where not.

---

## Phase Tina TCP Driver Contract
> Own the small time/TCP substrate boundary without becoming an async runtime.

> After: Phase Betelgeuse · Before: Phase Ranger

This phase exists because Tina should not be accidentally coupled to one
backend implementation. Betelgeuse is the best current substrate, but the
Tina-owned runtime contract should be small enough that native Betelgeuse,
simulated Betelgeuse, and later adapters can all plug in without changing
isolate semantics.

The plan lives in `.intent/phases/026-tina-driver-contract/plan.md`.

- Define a narrow `Driver`-shaped boundary inside `tina-runtime` for timers,
  TCP operations, completions, cancellation, shutdown, and wakeups.
- Keep isolate handlers synchronous. Do not add futures, wakers, async
  handlers, or arbitrary task spawning.
- Preserve bounded ingress and bounded cross-shard transport as Tina semantics,
  not backend conveniences.
- Keep Betelgeuse as the first native adapter and Betelgeuse simulated I/O as
  the deterministic adapter.
- Prove the same user-shaped workloads on explicit runtime, native
  Betelgeuse-backed runtime, and simulated-driver runtime.
- Measure touched hot paths enough to know whether the abstraction added
  meaningful allocation or dispatch cost.
- Decide whether any actor-framework substrate can be reused as prior art only;
  do not build Tina on Actix/Ractor/etc. unless they preserve explicit step,
  bounded queues, and replay semantics.
- Leave Tokio current-thread / Tower / Axum bridge work for Apollo unless this
  phase discovers a tiny adapter seam that does not weaken the core contract.

**Done when:** Tina has a backend-neutral runtime driver contract that is
small, synchronous, bounded, testable with deterministic simulated I/O, and
proved against the existing Betelgeuse path. The project can then explain its
production path without saying "trust this one backend forever."

---

## Phase Parallel Substrate Support
> Do safe support work beside the driver-contract phase.

> Parallel With: Phase Tina TCP Driver Contract

The plan lives in `.intent/phases/027-parallel-substrate-support/plan.md`.

- Polish `betelgeuse::io::simulated` as generic Betelgeuse substrate code.
- Add narrow allocation/performance probes for current hot paths.
- Expand runnable Tokio-vs-Tina comparisons around constrained capacity,
  backpressure, timeout, shutdown, and overload behavior.
- Add only tiny API helpers/macros that preserve one preferred surface.
- Prepare external review prompts and record review results.
- Research Tokio current-thread, Monoio, Glommio, and Compio as possible future
  adapters without implementing them.
- Refine README/story language lightly while leaving full release docs for
  Gemini.

**Done when:** the support evidence helps 026 and later Apollo/Gemini without
changing Tina core semantics or creating a second substrate direction.

---

## Phase Ranger
> Finish Tina's core runtime substrate before service-framework work.

> After: Phase Tina Driver Contract + Phase Parallel Substrate Support · Before: Phase Surveyor

This phase exists because the next question is not "can we build service
examples?" It is:

> What must be true about the live driver/runtime substrate before Tina can
> honestly support real workloads?

Ranger is not a Tokio bridge, not a service demo phase, not a narrow polish
pass, and not release docs. It finishes the load-bearing substrate story
beneath future service work. It is allowed to be as large as needed to settle
Tina core; it should not close while later phases would still have to reopen
runtime/substrate fundamentals.

The delivered plan and review live in
`.intent/phases/028-ranger-substrate-driver-maturity/`.

### Driver Capability Contract

Turn the 026 driver boundary into a capability contract:

- timer submission, wake, timeout, and cancel;
- TCP bind, accept, read, write, close, and cancel;
- bounded pending-operation admission;
- no hidden unbounded queues;
- explicit runtime-owned progress;
- deterministic simulator compatibility where applicable;
- traceable cancellation, shutdown, and substrate failure.

### TCP Resource Concurrency

Revisit the conservative 026 `ResourceBusy` rule. Expected direction:
support full-duplex TCP read/write on one stream if it can be done with honest
ownership and cancellation. A likely shape is separate resource lanes:
listener accept, stream read, stream write, and close rejection while any lane
is active.

If Betelgeuse cannot cancel one lane without closing the underlying stream,
the runtime may still use tombstones internally, but it must not invalidate an
unrelated live operation silently.

### Cancellation, Shutdown, And Late Completion

Harden stopped-requester, explicit-close, timeout, and runtime-shutdown
behavior under live and simulated drivers:

- stopped requester with pending accept/read/write/timer;
- explicit stream/listener close while operations are pending;
- runtime shutdown with pending timer and TCP operations;
- late substrate completion after cancel or shutdown;
- requester mailbox full when completion arrives;
- timeout racing with late completion where the call shape supports timeout.

### Live / Sim / Oracle Parity

Keep the three layers aligned: live Betelgeuse-backed runtime, Betelgeuse
simulated driver/runtime, and `tina-sim` deterministic oracle where the
behavior is modeled there. If a behavior belongs only to the live driver,
record why.

### Substrate Direction Decision

Leave the roadmap with a real next substrate decision: continue hardening
vendored Betelgeuse for now, add a Tokio current-thread driver later,
investigate Monoio/Glommio/Compio later, or build a small Tina-owned
completion substrate later. Ranger does not need to implement a new adapter
unless the existing substrate blocks required semantics.

### Cost And Allocation Pressure

Measure enough to keep substrate claims honest: per timer call, per TCP
read/write completion, per isolate call, per cross-shard send, and live worker
ingress handoff. Prefer allocation counts, operation counts, and bounded
resource counts before wall-clock benchmarks.

### Core Boundary Closeout

Record what Tina core now includes and what remains deliberately outside core.
The closeout should make clear why later service/docs/adapter phases can build
on Ranger rather than reopen core runtime/substrate semantics.

### Refusals

- Do not build `tina-runtime-tokio-bridge` unless a pause gate records that
  Betelgeuse cannot support required semantics.
- Do not add Tower, Axum, Hyper, or arbitrary futures integration.
- Do not make isolate handlers async.
- Do not expose driver/backend handles to isolate code.
- Do not add unbounded queues for convenience.
- Do not build a broad service example suite here.
- Do not claim production readiness or broad Tokio replacement.
- Do not start Gemini release docs until the substrate direction is settled.

### Done Means

- `RuntimeDriver` has a documented capability contract for time/TCP, progress,
  cancellation, shutdown, and bounded pending work.
- Full-duplex TCP read/write on one stream is supported with direct tests or
  explicitly deferred with the blocker recorded.
- Explicit close, stopped-requester cancellation, timeout, runtime shutdown,
  and late completion are tested across live and simulated drivers where
  applicable.
- Live Betelgeuse, Betelgeuse simulated driver/runtime, and `tina-sim` agree on
  modeled TCP/time semantics or concrete non-overlap is recorded.
- Allocation and operation costs are pinned for named hot paths.
- The roadmap names the next substrate direction after Ranger.
- Any service-shaped workload is minimal and exists only to pressure the
  substrate.
- The core/non-core boundary is recorded so later phases can build on Tina
  core instead of relitigating it.

---

## Phase Willem Drees
> Local production runtime. Make one-process Tina servers boring before release polish.

> After: Phase Surveyor · Before: Phase Ruud Lubbers

Delivered in `.intent/phases/030-willem-drees-local-production-runtime/`.

Willem Drees exists because Tina should not launch as "Akka in Rust" or as a
demo framework. It should first be a safe local concurrency runtime that can
run server-shaped workloads under pressure without hiding overload or lifecycle
bugs.

- Prove listener, connection, worker, supervisor, and shutdown lifecycles under
  live Betelgeuse runtime, Betelgeuse simulated runtime, and `tina-sim` where
  the behavior is modeled there.
- Build server-shaped regression workloads with assertions:
  - many short connections
  - slow readers and slow writers
  - bounded overload
  - connection isolate restart
  - listener shutdown while accepts are pending
  - worker pool full/closed behavior
  - timeout-driven cleanup
- Make graceful shutdown boring: no leaked completion slots, no hidden pending
  calls, no late delivery after requester stop, and typed lifecycle failure
  when the substrate cannot prove release.
- Strengthen CI as proof, not ceremony: Linux exercises io_uring, macOS
  exercises kqueue, and both run the same workspace verification gate.
- Keep the phase local. Do not add remoting, clustering, persistence,
  Tower/Axum, or arbitrary async handlers.

**Done when:** a production-shaped local Tina TCP/control-plane workload can be
run under constrained memory and bounded queues, with direct tests proving
overload, cancellation, restart, shutdown, and replay behavior. The outcome
should support the honest claim: "try porting a local stateful server component
to Tina when bounded failure visibility matters."

---

## Phase Ruud Lubbers
> Performance and memory hardening. Make the safety story cheap enough to use.

> After: Phase Willem Drees · Before: Phase Joop den Uyl

Ruud Lubbers keeps Tina from becoming a safe but slow actor toy. The target is
not benchmark theater; the target is knowing where Tina pays and removing costs
that fight the framework's own concurrency model.

- Measure hot paths with fixed, repeatable workloads:
  - mailbox send/recv
  - local send
  - isolate call/reply/timeout
  - TCP read/write completion
  - spawn/restart
  - trace/event recording
  - live ingress and cross-shard bounded transport
- Keep the SPSC hot-path no-allocation claim protected.
- Decide whether boxed erasure, call translators, trace storage, or completion
  slots need arenas/pools before release.
- Add allocation and latency counters only where they inform design decisions.
- Do not hide costs by disabling trace/replay semantics unless a preserved-vs-
  weakened-guarantees table says exactly what changed.
- Carry the medium cost rocks that are real but not "easy now":
  - design an `Effect::Batch` small path that avoids a heap `Vec` for common
    two-effect handlers without adding recursive boxes or a second public DSL;
  - replace generic live worker command boxing on common paths with concrete
    commands where it preserves the single runtime API;
  - add runtime sizing/preallocation knobs for expected isolates, calls, trace
    volume, TCP resources, and cross-shard queues;
  - define a production trace retention policy: full trace, bounded ring,
    streaming sink, and off/debug modes, with explicit guarantee differences;
  - explore typed fast paths that reduce erased `Box<dyn Any>` use for
    same-shard send/call while keeping the current safe generic boundary;
  - pool or slab driver completion slots only after backend pointer ownership
    and cancel/drain semantics are mechanically safe.

**Done when:** the roadmap can state the current hot-path cost model with
numbers, the worst avoidable costs are either fixed or explicitly deferred, and
the local production runtime remains safe under the same tests after
performance work.

---

## Phase Joop den Uyl
> Application surface. Make local Tina services boring to structure.

> After: Phase Ruud Lubbers · Before: Phase Gemini

Joop den Uyl is not a docs polish phase. It is the phase that makes Tina's
local application structure obvious enough that a human or Codex can port a
small Tokio-shaped TCP/control-plane service without inventing five local
dialects.

- Establish one canonical service shape:
  listener isolate, connection isolate, bounded worker pool, supervisor,
  shutdown owner, capacity config, mandatory call timeouts, and trace
  assertions.
- Prove that shape through `tina-sim`, explicit-step runtime, and
  Betelgeuse-backed live runtime tests where each layer applies.
- Audit existing server/comparison code for repeated ceremony, then add only
  helpers/macros justified by that audit and used by the canonical tests.
- Keep `Effect`, message enums, addresses, capacities, timeouts, bounded
  failure outcomes, and runtime-owned calls explicit.
- Add runnable porting proofs for TCP service, bounded router/worker, and
  stateful control-plane/session shapes.
- Pull in only the 031 medium rocks that directly help application structure,
  such as capacity config or test trace query helpers. Defer performance-only
  rocks.

**Done when:** Tina has one obvious local-service structure, proved in tests
across the relevant runners; the helper surface is small enough that there is
still one preferred path; and Gemini can document the service shape without
reopening core semantics.

---

## Phase Gemini
> First crewed flight. Stabilize and publish the settled framework story.

> After: Phase Joop den Uyl · Before: Phase Apollo

- Publish a coherent `0.1.0` story for `tina`, `tina-mailbox-spsc`,
  `tina-supervisor`, `tina-runtime`, and `tina-sim`, or explicitly decide that
  the APIs are still private and not ready for semver promises. Kepler settled
  the core multi-shard primitive; Huygens proved the composed framework and
  first runtime-substrate story; Mercury sharpened the overload/call contract;
  Betelgeuse and the Tina driver contract made the first tryable runtime
  substrate true; Ranger and Surveyor matured the substrate/driver ownership
  story; Willem Drees, Ruud Lubbers, and Joop den Uyl make the local
  production/runtime/application story real enough for Gemini to document instead
  of reopening core semantics.
- Write the first user-facing guide set: architecture overview, getting-started guide, isolate authoring guide, simulation guide, task-dispatcher walkthrough, and TCP echo walkthrough.
- Document the supported invariants for the core runtime/simulator model:
  delivery behavior, mailbox guarantees, supervision behavior, replayability,
  shard behavior, and the current allocation story.
- Publishing is gated on reviewed code, docs, and proofs all existing together. "The code works locally" is not enough for `0.1.0`.

**Done when:** there is either a published `0.1.0` with semver intent or an
explicit decision not to publish yet; the core crates have user-facing guides
covering both runtime and simulator usage; the supported invariants are
documented and reviewed; a developer outside the project can build a
non-trivial isolate from the docs alone.

---

## Phase Kepler
> Telescope mission. Finish the primitive before we build bridges around it.

> Delivered after Phase Galileo · Before: Phase Huygens

- Kepler was a core-completion phase, not an outward adoption phase.
- It closed or sealed the remaining semantic gaps that still sat too close to
  the primitive itself:
  - peer / shard liveness semantics
  - the multi-shard supervision boundary
  - cross-shard ownership / buffering / allocation honesty
  - stronger replay/checker pressure on those semantics
- Kepler preferred runtime + simulator proof work over docs/examples and did
  not add a `tina` boundary change.

What Kepler explicitly did **not** include:

- Tokio bridge work
- polished adoption examples
- guide-writing
- publication/semver positioning
- benchmark theater beyond what is needed to make allocation claims honest

**Delivered:** the remaining core semantic gaps after Galileo were either
closed and directly proved or deliberately sealed as long-lived boundaries;
runtime-level buffering/allocation claims are honest; and Huygens/Gemini/Apollo
can compare against a settled primitive instead of a still-moving one.

---

## Phase Apollo
> Moonshot. Tokio bridge design and implementation.

> After: Phase Gemini · Before: Phase Cassini

- `tina-runtime-tokio-bridge`: adapter for adopting tina inside an existing Tokio app.
- `tina-runtime-tokio-bridge` v1 must enable incremental adoption inside an existing Tokio app: drop one isolate in, not rearchitect the whole application.
- Write the bridge design down before treating it as an implementation task:
  - where the isolate actually runs
  - where effect dispatch happens
  - which thread owns I/O
  - what guarantees the bridge preserves and which ones it necessarily weakens
- Start with the narrowest bridge that is still useful, rather than trying to make every Tokio pattern look like native tina.
- Gemini's published single-shard invariant list is the source of truth for bridge comparisons. Apollo must ship a preserved/weakened-guarantees table against that list rather than inventing a second guarantee vocabulary.

**Proof plan:**

- Bridge integration tests prove that a Tokio application can host at least one tina isolate without changing the isolate trait surface.
- The bridge design ships a preserved/weakened-guarantees table covering at least: thread affinity, mailbox FIFO order, backpressure observability, supervision restart semantics, effect dispatch atomicity, and replayability under simulation.
- Tests document preserved semantics versus weakened semantics against Gemini's invariant list, especially around thread affinity, delivery, and backpressure.
- A small reference example demonstrates incremental adoption inside a Tokio app, backed by assertions rather than logs.

**Done when:** a tina isolate runs inside a reference Tokio HTTP server example (axum or similar) without modifications to the isolate trait surface; preserved-vs-weakened semantics are documented against Gemini's invariant list in both tests and prose.

---

## Phase Cassini
> Long mission, sustained operations. Production hardening, docs, and optional fallback primitives.

> After: Phase Apollo · Before: Phase Wim Kok

- `tina-mailbox-mpsc`: optional fallback for workloads where SPSC is not enough and the tradeoffs are acceptable.
- Benchmark suite: SPSC throughput, mailbox latency p50/p99, per-core scheduling overhead, isolate spawn cost.
- User-facing docs set: architecture guide, runtime selection guide, simulation guide, migration-patterns guide, and multiple worked examples.
- Ship one or two reference integration examples as examples or companion crates to anchor the I/O-isolate pattern without committing to a broad official adapter ecosystem.
- Memory profile and benchmark documentation: report where the current design allocates, where it does not, and what remains to improve.

**Done when:** the optional MPSC fallback is either shipped with clear tradeoffs or explicitly deferred; the bench suite is documented; at least one developer outside the project successfully ships a non-trivial isolate using only the published docs and reports their experience back; hardening work reports wins and losses honestly without requiring a case-study migration to another codebase.

---

## Phase Wim Kok
> Persistence. Durable state only after local runtime behavior is settled.

> After: Phase Cassini · Before: Phase Jan Peter Balkenende

Wim Kok is the first Akka/OTP-adjacent long-arc capability, but it should be
designed for Tina's own model rather than copied from Akka.

- Decide whether persistence means snapshots, event journals, durable replay
  artifacts, durable mailboxes, or some deliberately smaller first slice.
- Preserve boundedness: no hidden unbounded durable queue that bypasses mailbox
  backpressure.
- Make recovery semantics explicit for isolate state, address generations,
  restartable children, and pending runtime-owned calls.
- Keep simulator/replay compatibility as a design constraint.
- Do not add remoting or clustering in this phase.

**Done when:** Tina has a directly tested durable-state story for local
isolates, and the roadmap can honestly say which data survives process restart
and which data does not.

---

## Phase Jan Peter Balkenende
> Remoting. Tina runtime to Tina runtime over a network.

> After: Phase Wim Kok · Before: Phase Mark Rutte

Remoting means one Tina isolate sends or calls another Tina isolate in another
process or machine. This is where network lies become Tina semantics rather
than hidden library behavior.

- Define node identity, remote isolate identity, and serialization boundaries.
- Preserve boundedness across the network: local outbound queue, network
  transport, remote inbound queue, and remote mailbox all need explicit full or
  closed outcomes.
- Distinguish "accepted locally", "sent on wire", "accepted remotely", and
  "delivered to target mailbox" in trace semantics.
- Define remote call outcomes: reply, timeout, remote full, remote closed, node
  down, and requester stopped.
- Keep replay/simulation in view. If real network behavior cannot replay
  exactly, model the semantic envelope in `tina-sim`.
- Do not add clustering/membership until point-to-point remote semantics are
  boring.

**Done when:** two Tina runtimes can communicate over a network with typed,
bounded, traceable outcomes and tests prove failure cases without pretending
that network send means remote delivery.

---

## Phase Mark Rutte
> Clustering. Membership and placement only after remoting is boring.

> After: Phase Jan Peter Balkenende

Clustering is not an Akka checklist item. It is a later capability only if it
preserves Tina's safety and performance model.

- Define membership, node liveness, peer quarantine, and placement.
- Decide whether shard migration/rebalancing belongs in Tina at all, or whether
  static placement plus explicit restart is the safer first answer.
- Keep backpressure explicit across node boundaries.
- Preserve address-generation and stale-address semantics under node restart.
- Make operational failure visible: node down, peer slow, remote queue full,
  partitioned peer, and recovered peer should not collapse into one generic
  error.

**Done when:** a small Tina cluster can route bounded messages and calls under
node failure with explicit semantics, and the implementation does not weaken
the local runtime guarantees that made Tina worth building.

---

## Open questions

These still need answers, but a couple now have an explicit phase boundary.

1. **`Effect` shape.** Resolved in Sputnik for the current verbs: use a closed enum with per-isolate associated payload types for `Reply`, `Send`, and `Spawn`. The next design question is how I/O, timers, calls, yields, and crash/restart requests should enter that closed vocabulary without turning handlers into async functions.
2. **Supervisor execution semantics.** Direct `RestartChildren` execution and
   panic-triggered supervised restart now exist. The next supervision design
   question is how far runtime-lifetime budgets can go before timed windows or
   explicit deferral are required, and how to prove the task-dispatcher example
   without reconstructing supervision state from the trace.
3. **Runtime allocation boundary.** Resolved narrowly by Kepler: the SPSC mailbox hot path is proven narrowly, while the broader runtime/simulator path remains an explicit non-claim because boxed erasure, traces, replay records, and coordinator storage may allocate.
4. **Cross-shard ownership.** Resolved narrowly by Kepler for the explicit-step model: user payloads move into erased runtime storage, then through bounded shard-pair queues, then into destination mailboxes; core transport does not require user-message cloning. Zero-copy production transport remains a later backend question.
5. **Supervisor split.** Resolved for the current shape: policy types live in
   `tina`, supervisor configuration lives in `tina-supervisor`, and mutable
   runtime supervision state/execution lives in runtime crates. Future reusable
   supervisor mechanisms can move into `tina-supervisor` once multiple runtime
   crates need them.
6. **Peter Mbanugo / Tina-Odin public positioning.** Resolve before public positioning or publish (Gemini at the latest). Local design exploration is not blocked on this.
7. **MSRV.** Pick a Rust version that supports the io_uring story without nightly. Currently this is stable Rust 1.85+ via monoio.
8. **License.** Resolved in Sputnik: dual-license under MIT or Apache-2.0 to match Rust ecosystem norms.

---

## What we're explicitly *not* doing

- **No new scheduler.** Tina should ride on existing substrates where
  practical, but the core programming model should stay explicit-step and
  completion-driven where that best preserves the design. We are not building a
  new general-purpose async ecosystem.
- **No async/await replacement.** Handlers are synchronous functions returning effects. If you want await, you're in the wrong layer.
- **No global allocator games.** Pre-allocated arenas per isolate, but no `#[global_allocator]` requirements imposed on consumers.
- **No FFI to Tina-Odin.** Two runtimes fighting for cores would be the worst of both worlds.
