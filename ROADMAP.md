# tina-rs Roadmap

A staged plan for porting Tina's discipline to Rust, structured to deliver value at each phase rather than waiting for a big-bang release.

Phases are named (not numbered) so we can insert phases later without renumbering. Names are space missions, ordered roughly chronologically by mission complexity.

Completed work moves to `CHANGELOG.md`. `ROADMAP.md` is for active and future work.

---

## Vision

Bring Tina's three load-bearing ideas — synchronous effect-returning handlers, isolate-per-entity state machines, and thread-per-core scheduling with bounded mailboxes — to Rust **without** building a new runtime from scratch.

The deliverable is a small set of crates (`tina`, `tina-runtime-*`, `tina-sim`) that any existing Rust async codebase can adopt incrementally.

## Non-goals

- A new runtime competing with Tokio/monoio. Use what exists.
- Full feature parity with Tina-Odin. We port the *shape*, not every primitive.
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
| Trait/API discipline | `tina` exposes `Isolate`, closed `Effect`, typed `Address`, `Outbound`, `ChildDefinition`, supervision policy types, and the preferred 021 authoring surface (`tina::prelude::*`, effect helpers, typed call helpers, `ctx.me()`, `ctx.send_self(...)`, and `tina::isolate_types!`). | Small call-result helper polish remains optional. |
| Bounded mailbox semantics | `tina-mailbox-spsc` proves FIFO, `Full`/`Closed`, no hidden overflow queue, drop accounting, allocation accounting, focused Miri unsafe-memory checks, and selected Loom interleavings. Cross-shard shard-pair queues are bounded and directly proved in Galileo. | This is not a full formal proof for every capacity/interleaving/refactor. Any future MPSC fallback is not implemented. |
| Single-shard runtime delivery | `tina-runtime` has deterministic trace IDs and causal links, registration-order stepping, local send dispatch, local spawn dispatch, typed ingress, stop-and-abandon, panic capture, address generations, runtime-owned parent-child lineage, restartable child records, direct-child `RestartChildren` execution, supervised panic restart with policy/budget config, an assertion-backed task-dispatcher proof package, and generated-history property tests. | Supervision is still narrow: panic-triggered only, runtime-lifetime budget only, and no timed budget windows. The generated-history model is bounded and does not prove arbitrary user programs. |
| Failure isolation | Unwinding handler panics become runtime events; the panicking isolate stops and the same round continues deterministically. | This is not Tina-Odin's OS trap boundary. Rust segfault isolation, shard quarantine, and `panic = "abort"` behavior are out of scope unless a later phase explicitly designs them. |
| Multi-shard runtime/sim | `tina-runtime` and `tina-sim` now expose multi-shard explicit-step runners with root placement, global event/call ids, bounded shard-pair queues, next-step-only remote visibility, deterministic harvest order, source-time versus destination-time delivery stages, simulator replay, user-shaped dispatcher proofs, sealed address-local remote-failure behavior, and shard-local supervision/restart ownership. Huygens added first live worker-per-shard runners with bounded live ingress and bounded live cross-shard transport. | Production-shaped runtime backend, thread pinning/topology, peer quarantine, shard-restart propagation, and cross-shard child ownership remain future work. |
| Replayability | Runtime traces are deterministic across repeated identical single-shard runs, including generated operation histories and small generated dispatcher workloads. Trace replay proofs can reconstruct worker completions and restart outcomes from the runtime event model alone. `tina-sim` adds virtual time, replay records, seeded delays/reordering over timer-wake/local-send/TCP-completion behavior, checker failures, spawn/supervision replay, scripted TCP simulation, multi-shard replay under default and non-default seeded configs, and multi-shard checker failure replay. | Real substrate liveness faults remain future work; current explicit-step shard-liveness non-claims are sealed. |
| Runtime allocation story | The SPSC mailbox hot path is tested for no per-message allocation after warm-up. Kepler pins the runtime/simulator allocation story narrowly: boxed erasure, traces, replay records, and coordinator storage may allocate, and a focused multi-shard allocation probe guards against pretending otherwise. | No broad runtime/simulator allocation-free claim is supported yet. |
| Reference examples | A Rust task-dispatcher proof package and a TCP echo proof package both exist with matching runnable examples, backed by assertions rather than logs alone. The echo proof now keeps the listener alive across a one-client smoke run, a sequential multi-client run, and a bounded-overlap run, then closes the listener cleanly and exits. | These are still proof workloads, not a broad production-server claim or benchmark story. |
| Runtime-owned I/O | `tina` names a runtime-owned call effect family (`Effect::Call(I::Call)` plus `Isolate::Call`) and an ordered batch effect (`Effect::Batch(Vec<Effect<I>>)`) for closed-set sequencing of existing effects. `tina-runtime` executes the first TCP call family — bind, accept, read, write, close — through Betelgeuse on nightly Rust, with caller-owned typed completion slots, runtime-assigned opaque resource ids, runtime-controlled completion translation back into ordinary `Message` values, honest `local_addr` reporting for `127.0.0.1:0` binds, accepted-stream `peer_addr`, listener re-arm through normal isolate control flow, and clean listener close. It also executes the first time call verb — `Sleep { after }` with `TimerFired` — with runtime-owned monotonic clock sampling once per step, due-timer harvest, deterministic request-order tie-break for equal deadlines, and a crate-private manual clock seam for deterministic timer tests. | A production-shaped I/O substrate decision remains: likely `monoio`/io_uring, or an explicit decision that the current threaded+Betelgeuse substrate is the tryable backend for now. The 100k-connection benchmark and broader network-server claims remain future work. |

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
| **Mercury overload lab and runtime contract** | Next. Make the Huygens substrate story concrete with an overload proof app: bounded observed send, isolate-to-isolate call with mandatory timeout, live runner lifecycle, spawned-child cross-shard proof, live supervision/restart, simulator replay, and a narrow Tokio current-thread backend for TCP/time so the same Tina-shaped workload can be compared against naive and hardened Tokio. Monoio remains the native thread-per-core backend candidate, but not the first blocker. |
| **Gemini release story** | Deferred until Mercury makes the tryable runtime contract true. Supported invariant docs, guides, examples, semver/publication decision, CI/proof gate, public positioning, and a clear adoption story. Gemini should not add new core semantics; it documents a framework that already has real proof and a runtime path. |
| **Apollo Tokio bridge** | Preserved/weakened guarantees table, minimal bridge, and an assertion-backed Axum or similar reference adoption example. |
| **Cassini hardening** | Optional MPSC decision, benchmark suite, memory profile, docs polish, and dogfood report. |

Real concurrent shard execution is a substrate story around Huygens, Mercury,
and later runtime work, not something Galileo or Kepler quietly smuggled in.
Galileo and Kepler proved the multi-shard contract under one explicit global
coordinator thread first. Huygens added the first worker-owned runtime
substrate around that contract. Mercury decides and hardens the runtime
contract enough that "try this for selected Tokio-shaped workloads" is a
technical claim, not a docs claim.

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

> After: Phase Huygens · Before: Phase Gemini

Mercury exists because Huygens proved the model and first live substrate, but
not the proof app that makes Tina's value obvious. Do Mercury before
release/docs polish.

- Build the overload lab: one stateful TCP-shaped service under tiny bounded
  capacities, with a traffic spike, slow worker mode, recovery after overload,
  and asserted latency/memory/backpressure outcomes.
- Add user-visible send backpressure. App code must be able to react to
  `Accepted`, `Full`, and `Closed`, not only inspect trace after the fact.
- Add isolate-to-isolate call with mandatory timeout for request/reply work.
- Harden live runner lifecycle: start roots, run, drain/shutdown, worker error
  reporting, trace inspection, and bounded config.
- Prove spawned-child cross-shard behavior on the live substrate, or add a
  clear guard that prevents unsupported behavior.
- Prove live supervision/restart on the threaded substrate: a worker panics,
  supervisor restarts it, and later work succeeds.
- Run the same core Tina workload through deterministic simulator replay and
  the live runner.
- Add a narrow Tokio current-thread backend for TCP/time only, so the overload
  lab can run on a known, cross-platform Rust substrate without async handlers.
- Compare against naive Tokio and hardened Tokio versions of the same workload:
  the naive version should show the overload failure mode; the hardened version
  should survive with explicit user discipline; Tina should survive by default
  framework shape.
- Keep `monoio` as the likely native thread-per-core backend candidate after
  this proof. Do not block Mercury on a full monoio backend.
- Pin capacity/allocation claims: either prove stronger runtime bounds or keep
  the claim explicitly narrower than Tina-Odin's no-hidden-allocation story.
- Expose a small public-ish DST harness surface so users can test their own
  isolate workloads under replay/checker pressure without depending on
  crate-private test helpers.

**Done when:** the repo can honestly say: "you can try replacing selected
Tokio-shaped workloads with Tina when you want bounded queues, shard-owned
state, timeout-based request/reply, deterministic testing, and a live
thread-per-shard runtime path." Gemini must not start before this is true or
explicitly narrowed.

---

## Phase Gemini
> First crewed flight. Stabilize and publish the settled framework story.

> After: Phase Mercury · Before: Phase Apollo

- Publish a coherent `0.1.0` story for `tina`, `tina-mailbox-spsc`,
  `tina-supervisor`, `tina-runtime`, and `tina-sim`, or explicitly decide that
  the APIs are still private and not ready for semver promises. Kepler settled
  the core multi-shard primitive; Huygens proved the composed framework and
  first runtime-substrate story; Mercury must make the tryable runtime contract
  true before Gemini publishes or freezes it.
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

> After: Phase Apollo

- `tina-mailbox-mpsc`: optional fallback for workloads where SPSC is not enough and the tradeoffs are acceptable.
- Benchmark suite: SPSC throughput, mailbox latency p50/p99, per-core scheduling overhead, isolate spawn cost.
- User-facing docs set: architecture guide, runtime selection guide, simulation guide, migration-patterns guide, and multiple worked examples.
- Ship one or two reference integration examples as examples or companion crates to anchor the I/O-isolate pattern without committing to a broad official adapter ecosystem.
- Memory profile and benchmark documentation: report where the current design allocates, where it does not, and what remains to improve.

**Done when:** the optional MPSC fallback is either shipped with clear tradeoffs or explicitly deferred; the bench suite is documented; at least one developer outside the project successfully ships a non-trivial isolate using only the published docs and reports their experience back; hardening work reports wins and losses honestly without requiring a case-study migration to another codebase.

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
