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

## Testing and proof strategy

We should prove the discipline in layers, matching the abstraction-vs-implementation split:

- **Trait crate (`tina`)** proves API shape and compile-time guarantees only. This is where doc tests, compile-fail tests, and downstream-style integration tests belong.
- **Mailbox crates** prove concrete queue semantics. This is where FIFO, boundedness, `Full`/`Closed`, and no hidden buffering get tested against real implementations and under loom.
- **Runtime crates** prove delivery semantics. This is where we can assert that accepted sends become handler invocations, that `Stop` actually stops delivery, and that effect dispatch is the only place side effects happen.
- **Simulator** proves interleavings and replay. This is where we stop trusting timing-sensitive live tests and start proving seeded, reproducible traces.

Live examples matter, but they are smoke tests, not the proof. Every runnable example should be backed by black-box assertions in the crate that owns the implementation being exercised.

## Strategic prerequisites

These should be resolved early enough to avoid rework, but they do not all block implementation at the same phase:

- **Coordinate with Banugo early.** If the upstream author wants to maintain a Rust port, collaborate on design, or avoid ecosystem confusion, the project shape changes. Local design exploration is not blocked on this, but public positioning and any publish decision should not outrun that conversation.
- **Commit to the hot-path allocation story early.** If "zero per-message allocation after warm-up on the hot SPSC path" is a real invariant, Pioneer and Mariner must be designed around it. If that is too strong, narrow the claim before the runtime crates ship.

---

## Phase Mariner
> First single-thread runtime. Effect dispatcher + supervision mechanism.

> After: Completed Sputnik and Pioneer work · Before: Phase Voyager

- `tina-runtime-current`: single-shard runtime backed by `tokio::runtime::Builder::new_current_thread`. Pin to one core. Run a poll loop: drain mailboxes → run handlers → dispatch effects.
- `tina-supervisor`: actual supervision mechanism, now that there is a runtime capable of catching failures, restarting children, and applying policy.
- The effect dispatcher is the **only** place real I/O happens. Handlers return effects; the dispatcher executes them. This is the property that makes deterministic simulation possible later.
- Introduce a deterministic runtime event trace for tests and later simulation handoff. The trace records mailbox accept/reject, handler invocation start/end, effect dispatch, stop, spawn, and restart events, with causal linkage so tests and replay can reason about provenance rather than only timeline order.
- A working TCP echo server isolate (mirroring Tina-Odin's example) — proves the API end-to-end.
- Keep the abstraction boundary strict: `tina-runtime-current` owns scheduling, polling, and effect execution; `tina` must not grow runtime helpers just to make tests easier.
- Runtime tests should inject a deterministic test mailbox through `Mailbox<T>` where possible. Benchmarks and smoke examples can use the real SPSC crate, but correctness tests should avoid coupling two fresh implementations unless that coupling is the point of the test.

**Proof plan:**

- Black-box runtime tests prove delivery semantics on one shard:
  - accepted `Send` becomes exactly one handler invocation
  - mailbox FIFO order is preserved by the dispatcher
  - `Stop` prevents later delivery to the stopped isolate
  - rejected sends are observable and not silently buffered
  - `Reply`, `Spawn`, and `RestartChildren` are executed only by the dispatcher
- Black-box supervisor tests prove actual runtime behavior:
  - a panicking child is restarted according to policy
  - sibling survival matches `one-for-one`, `one-for-all`, and `rest-for-one`
  - restart-budget exhaustion halts restart loops predictably
- Trace-oriented integration tests assert on the runtime event trace and prove that two identical seeded runs on the single-shard runtime produce the same event sequence with the same causal chains.
- A runnable echo example is used as an end-to-end smoke test and benchmark surface, not as the only evidence of correctness.

**Done when:** runtime tests prove single-shard delivery semantics; supervisor tests prove actual restart behavior on the single-shard runtime; trace tests are deterministic on repeated runs with stable causal linkage; echo server handles 100k connections on a single shard with stable memory; hot-path allocations are zero per message after warm-up on the SPSC single-shard path, or the claim is explicitly revised before publish.

---

## Phase Voyager
> Long-duration deep-space mission. Deterministic simulation for the single-shard runtime.

> After: Phase Mariner · Before: Phase Gemini

- `tina-sim`: deterministic simulator for the single-shard runtime. Time is virtual, I/O is intercepted, mailbox arrival order is reproducible from a seed.
- `tina-sim` consumes Mariner's event trace as its semantic model. The simulator does not invent a second observable surface; it provides a different execution and I/O substrate against the same event vocabulary.
- Failure injection: drop messages, simulate crashes, inject slow disk, and perturb delivery order within the single-shard model.
- Replay: every test failure produces a seed that reproduces the failure exactly.
- Keep the abstraction boundary strict: runtime crates expose enough hooks for simulation, but the simulator owns virtual time, trace capture, and failure injection.
- Voyager runs before Gemini deliberately: building the simulator first surfaces which runtime hooks the simulator actually needs, so Gemini can stabilize an API that already accommodates them.

**Proof plan:**

- Seeded simulator tests prove reproducible delivery traces across repeated runs of the single-shard runtime.
- Failure-injection tests prove behavior under dropped messages, crashes, and slow resources without relying on wall-clock timing.
- Replay tests prove that a saved seed reproduces a prior failure exactly.

**Done when:** single-shard simulated workloads converge to a known good state every run; replay from a saved seed reproduces failures exactly; the simulator catches a deliberately-injected single-shard ordering bug that production tests miss.

This is the highest-leverage phase. Deterministic simulation is what makes Tina's discipline pay off — failures become reproducible artifacts, not phantoms.

---

## Phase Gemini
> First crewed flight. Stabilize and publish the single-shard story.

> After: Phase Voyager · Before: Phase Galileo

- Publish a coherent `0.1.0` story for `tina`, `tina-mailbox-spsc`, `tina-supervisor`, `tina-runtime-current`, and `tina-sim`, or explicitly decide that the APIs are still private and not ready for semver promises. By this point Voyager has surfaced which runtime hooks the simulator needs to be part of the stable surface and which can stay private.
- Write the first user-facing guide set: architecture overview, getting-started guide, isolate authoring guide, simulation guide, and at least one example beyond TCP echo.
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
- Benchmarks measure the monoio runtime honestly against the single-shard runtime and comparable Tokio baselines, with documentation about where each approach wins or loses.

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

1. **`Effect` shape.** Resolved in Sputnik: use a closed enum with per-isolate associated payload types for `Reply`, `Send`, and `Spawn`. This keeps the dispatcher contract uniform without erasing the types carried by each isolate.
2. **Cross-shard ownership.** Tina-Odin's mailboxes are SPSC; cross-shard requires copy-or-move. Investigate whether we can use ownership transfer (move + atomic pointer swap) for zero-copy. If not, accept the copy.
3. **Supervisor split.** Current plan: policy types may live in `tina` if they are truly shared abstractions; mechanism lives in `tina-supervisor` and does not ship before Mariner.
4. **Coordination with Banugo.** Resolve before public positioning or publish (Gemini at the latest). Local design exploration is not blocked on this.
5. **MSRV.** Pick a Rust version that supports the io_uring story without nightly. Currently this is stable Rust 1.85+ via monoio.
6. **License.** Resolved in Sputnik: dual-license under MIT or Apache-2.0 to match Rust ecosystem norms.

---

## What we're explicitly *not* doing

- **No new scheduler.** Every phase delegates execution to monoio or current-thread Tokio. The scheduler is solved; we're not solving it again.
- **No async/await replacement.** Handlers are synchronous functions returning effects. If you want await, you're in the wrong layer.
- **No global allocator games.** Pre-allocated arenas per isolate, but no `#[global_allocator]` requirements imposed on consumers.
- **No FFI to Tina-Odin.** Two runtimes fighting for cores would be the worst of both worlds.
