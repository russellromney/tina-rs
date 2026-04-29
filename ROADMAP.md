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
- "Replacing Tokio." This is a discipline layer that rides on top of any thread-per-core runtime.

## Crate layout (target shape)

Following the abstraction-vs-implementation rule (capability traits live in their own crate; backends are siblings):

- `tina` — trait crate. `Isolate`, `Effect`, `Mailbox`, `Shard`, plus any small policy types that truly belong at the abstraction boundary. **No impls.**
- `tina-mailbox-spsc` — SPSC ring buffer impl
- `tina-mailbox-mpsc` — MPSC fallback impl
- `tina-supervisor` — supervision tree mechanism
- `tina-runtime-current` — single-shard runtime on `tokio::runtime::Builder::new_current_thread`
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
| Trait/API discipline | `tina` exposes `Isolate`, closed `Effect`, typed `Address`, `SendMessage`, `SpawnSpec`, and supervision policy types with doc/compile-fail/downstream-style tests. | Request/reply ergonomics and I/O/timer/call effect vocabulary are not designed yet. |
| Bounded mailbox semantics | `tina-mailbox-spsc` proves FIFO, `Full`/`Closed`, no hidden overflow queue, drop accounting, allocation accounting, focused Miri unsafe-memory checks, and selected Loom interleavings. | This is not a full formal proof for every capacity/interleaving/refactor. Cross-shard channel semantics and any future MPSC fallback are not implemented. |
| Single-shard runtime delivery | `tina-runtime-current` has deterministic trace IDs and causal links, registration-order stepping, local send dispatch, local spawn dispatch, typed ingress, stop-and-abandon, panic capture, address generations, runtime-owned parent-child lineage, restartable child records, direct-child `RestartChildren` execution, and generated-history property tests. | No actual supervisor policy mechanism exists yet. Restart strategies, restart budgets, dispatcher proof examples, and user-facing supervision docs are still missing. The generated-history model is bounded and does not prove arbitrary user programs. |
| Failure isolation | Unwinding handler panics become runtime events; the panicking isolate stops and the same round continues deterministically. | This is not Tina-Odin's OS trap boundary. Rust segfault isolation, shard quarantine, and `panic = "abort"` behavior are out of scope unless a later phase explicitly designs them. |
| Replayability | Runtime traces are deterministic across repeated identical single-shard runs, including generated operation histories. | There is no seed-driven simulator, virtual time, fault injection, structural checker, or replay artifact yet. |
| Runtime allocation story | The SPSC mailbox hot path is tested for no per-message allocation after warm-up. | The runtime currently uses boxed erasure and per-round collection; no broad runtime allocation claim is supported yet. |
| Reference examples | None in Rust yet. | The Odin task dispatcher and TCP echo examples still need Rust equivalents backed by assertions, not logs alone. |

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

- Product-level end-to-end tests do not exist yet because there is no driver,
  I/O effect contract, TCP echo, task-dispatcher example, or simulator.
- Runtime property tests are bounded generated histories, not a proof over all
  possible user isolate programs.
- SPSC unsafe correctness has Loom and Miri evidence, not a complete formal
  proof across all future refactors.
- Runtime allocation behavior is not proven beyond the narrow SPSC hot path.

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
| **Mariner supervision and dispatcher proof** | Planning bucket covering restartable child records, address liveness/stale-handle semantics, real `RestartChildren`, `tina-supervisor`, restart strategies, restart budgets, and a Rust task-dispatcher proof example with trace assertions. Expected to split into multiple IDD slices. |
| **Mariner I/O, current runtime, and echo** | Planning bucket covering Rust I/O/timer/call effect contract, current-thread Tokio driver, runtime allocation audit or narrowed claims, and TCP echo smoke/benchmark backed by assertions. Expected to split at least into contract, driver, and echo proof slices. |
| **Voyager deterministic simulation** | Planning bucket covering `tina-sim` with virtual time, domain-isolated PRNG, integer-ratio faults, test-driver isolates, structural/user checkers, failure injection, replay artifacts, and injected-bug proof. Expected to split into multiple IDD slices. |
| **Gemini single-shard release story** | Supported invariant docs, guides, examples, semver/publication decision, CI/proof gate, and a clear single-shard adoption story. |
| **Galileo multi-shard runtime** | Cross-shard semantics, cross-shard channel, routing/placement, monoio runtime, multi-shard simulation coverage, and honest benchmarks. |
| **Apollo Tokio bridge** | Preserved/weakened guarantees table, minimal bridge, and an assertion-backed Axum or similar reference adoption example. |
| **Cassini hardening** | Optional MPSC decision, benchmark suite, memory profile, docs polish, and dogfood report. |

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

- `tina-runtime-current`: single-shard runtime backed by `tokio::runtime::Builder::new_current_thread`. Pin to one core. Run a poll loop: drain mailboxes → run handlers → dispatch effects.
- `tina-supervisor`: actual supervision mechanism, now that there is a runtime capable of catching failures, restarting children, and applying policy.
- The effect dispatcher is the **only** place real I/O happens. Handlers return effects; the dispatcher executes them. This is the property that makes deterministic simulation possible later.
- Continue using the deterministic runtime event trace as the semantic proof surface. The trace records mailbox accept/reject, handler invocation start/end, effect dispatch, stop, spawn, panic, abandonment, and restart events with causal linkage so tests and replay can reason about provenance rather than only timeline order.
- Build supervision on stored runtime state, not trace reconstruction. Parent-child lineage, restartable child records, address liveness, and direct-child restart execution already exist; the remaining supervision work needs policy/budget application and proof examples.
- A task-dispatcher proof example should land before TCP echo. It mirrors Tina-Odin's "dead worker is not a dead system" example without needing runtime-owned network I/O first.
- A working TCP echo server isolate (mirroring Tina-Odin's example) lands after the Rust I/O/timer effect contract and current-thread driver exist.
- Keep the abstraction boundary strict: `tina-runtime-current` owns scheduling, polling, and effect execution; `tina` must not grow runtime helpers just to make tests easier.
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

> After: Phase Mariner · Before: Phase Gemini

- `tina-sim`: deterministic simulator for the single-shard runtime. Time is virtual, I/O is intercepted, mailbox arrival order is reproducible from a seed.
- `tina-sim` consumes Mariner's event trace as its semantic model. The simulator does not invent a second observable surface; it provides a different execution and I/O substrate against the same event vocabulary.
- Use Tina-Odin's DST shape as the conceptual bar: a domain-isolated PRNG tree, integer-ratio fault probabilities, ordinary test-driver isolates rather than privileged injection, structural/user checkers, and replay artifacts that include seed/config/trace.
- Failure injection: drop messages, simulate crashes, inject slow disk or I/O resources, delay completions, and perturb delivery order within the single-shard model.
- Replay: every test failure produces a seed that reproduces the failure exactly.
- Keep the abstraction boundary strict: runtime crates expose enough hooks for simulation, but the simulator owns virtual time, trace capture, and failure injection.
- Voyager runs before Gemini deliberately: building the simulator first surfaces which runtime hooks the simulator actually needs, so Gemini can stabilize an API that already accommodates them.

**Proof plan:**

- Seeded simulator tests prove reproducible delivery traces across repeated runs of the single-shard runtime.
- Different-seed tests under faults diverge in observable, intentional ways.
- Failure-injection tests prove behavior under dropped messages, crashes, and slow resources without relying on wall-clock timing.
- Checker tests prove framework invariants and at least one user-defined invariant can halt the run with a reproducible seed.
- Replay tests prove that a saved seed/config reproduces a prior failure exactly.

**Done when:** single-shard simulated workloads converge to a known good state every run; replay from a saved seed/config reproduces failures exactly; the simulator catches a deliberately-injected single-shard ordering bug that production tests miss; simulation docs explain test drivers, checkers, faults, and replay without relying on logs as proof.

This is the highest-leverage phase. Deterministic simulation is what makes Tina's discipline pay off — failures become reproducible artifacts, not phantoms.

---

## Phase Gemini
> First crewed flight. Stabilize and publish the single-shard story.

> After: Phase Voyager · Before: Phase Galileo

- Publish a coherent `0.1.0` story for `tina`, `tina-mailbox-spsc`, `tina-supervisor`, `tina-runtime-current`, and `tina-sim`, or explicitly decide that the APIs are still private and not ready for semver promises. By this point Voyager has surfaced which runtime hooks the simulator needs to be part of the stable surface and which can stay private.
- Write the first user-facing guide set: architecture overview, getting-started guide, isolate authoring guide, simulation guide, task-dispatcher walkthrough, and TCP echo walkthrough.
- Document the supported invariants for the single-shard runtime: delivery semantics, mailbox guarantees, supervision behavior, replayability, and the current allocation story.
- Publishing is gated on reviewed code, docs, and proofs all existing together. "The code works locally" is not enough for `0.1.0`.

**Done when:** there is either a published `0.1.0` with semver intent or an explicit decision not to publish yet; the single-shard crates have user-facing guides covering both runtime and simulator usage; the supported invariants are documented and reviewed; a developer outside the project can build a non-trivial isolate from the docs alone.

---

## Phase Galileo
> Jupiter mission. Multi-shard runtime + cross-shard semantics.

> After: Phase Gemini · Before: Phase Apollo

- `tina-runtime-monoio`: thread-per-core backed by [monoio](https://github.com/bytedance/monoio) (io_uring on Linux). One shard per core, pinned. No work-stealing.
- Before implementation, ship a cross-shard semantics note as a phase deliverable, not informal scaffolding. It must define:
  - ordering within one mailbox
  - ordering for cross-shard messages from one source to one target
  - interleaving expectations for multiple sources targeting the same destination
  - causal ordering expectations across request/reply pairs
  - what is intentionally unspecified
- Cross-shard messaging: when isolate A on shard 0 sends to isolate B on shard 3, the mailbox crosses cores via a separate cross-shard SPSC channel. Per-pair, not global.
- Cross-shard SPSC has different concerns from within-shard SPSC: cross-core memory ordering, cache-line ping-pong, NUMA effects. Decide whether the cross-shard channel reuses `tina-mailbox-spsc` or ships as a sibling impl crate, and document the choice before benchmarks are written.
- Workload placement: a hash-based router decides which shard owns which isolate. Stable under shard add/remove (consistent hashing).
- Extend `tina-sim` to cover the newly-defined cross-shard semantics rather than inventing them ahead of the runtime.
- A two-shard echo benchmark: half the connections on shard 0, half on shard 1, no migration.

**Proof plan:**

- Multi-shard runtime tests prove cross-shard delivery, ownership, and routing semantics against the actual runtime.
- Extended simulator tests prove reproducible cross-shard traces now that the semantics exist.
- Benchmarks measure the monoio runtime honestly against the single-shard runtime and comparable Tokio baselines, including both naive Tokio and disciplined bounded-channel Tokio where useful, with documentation about where each approach wins or loses.

**Done when:** monoio runtime matches `tina-runtime-current` on single-shard echo; multi-shard tests prove cross-shard delivery and routing semantics; simulator coverage includes the defined cross-shard model; benchmark results are documented honestly rather than tied to a speculative numeric target.

---

## Phase Apollo
> Moonshot. Tokio bridge design and implementation.

> After: Phase Galileo · Before: Phase Cassini

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
2. **Supervisor execution semantics.** Direct `RestartChildren` execution now
   exists, including non-restartable skip behavior and stable child ordinals.
   The next supervision design question is how policy, budgets, and child
   failure detection enter the runtime without making handlers async or
   reconstructing supervision state from the trace.
3. **Runtime allocation boundary.** The SPSC mailbox hot path is proven narrowly. The runtime itself still needs an allocation audit before the project repeats "no hidden allocations" as a runtime-level claim.
4. **Cross-shard ownership.** Tina-Odin's mailboxes are SPSC; cross-shard requires copy-or-move. Investigate whether we can use ownership transfer (move + atomic pointer swap) for zero-copy. If not, accept the copy.
5. **Supervisor split.** Current plan: policy types may live in `tina` if they are truly shared abstractions; mechanism lives in `tina-supervisor` and does not ship before Mariner.
6. **Peter Mbanugo / Tina-Odin public positioning.** Resolve before public positioning or publish (Gemini at the latest). Local design exploration is not blocked on this.
7. **MSRV.** Pick a Rust version that supports the io_uring story without nightly. Currently this is stable Rust 1.85+ via monoio.
8. **License.** Resolved in Sputnik: dual-license under MIT or Apache-2.0 to match Rust ecosystem norms.

---

## What we're explicitly *not* doing

- **No new scheduler.** Every phase delegates execution to monoio or current-thread Tokio. The scheduler is solved; we're not solving it again.
- **No async/await replacement.** Handlers are synchronous functions returning effects. If you want await, you're in the wrong layer.
- **No global allocator games.** Pre-allocated arenas per isolate, but no `#[global_allocator]` requirements imposed on consumers.
- **No FFI to Tina-Odin.** Two runtimes fighting for cores would be the worst of both worlds.
