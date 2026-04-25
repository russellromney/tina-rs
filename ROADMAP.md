# tina-rs Roadmap

A staged plan for porting Tina's discipline to Rust, structured to deliver value at each phase rather than waiting for a big-bang release.

Phases are named (not numbered) so we can insert phases later without renumbering. Names are space missions, ordered roughly chronologically by mission complexity.

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

- `tina` — trait crate. `Isolate`, `Effect`, `Mailbox`, `Shard`, `Supervisor`. **No impls.**
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

---

## Phase Sputnik
> First in orbit. Minimum viable types — no executor.

Trait crate with the core abstractions and nothing else.

- `Isolate` trait: typed state machine. `fn handle(&mut self, msg: Self::Message, ctx: &mut Context) -> Effect<Self>`.
- `Effect` enum (closed enum with per-isolate associated payload types): `Noop`, `Reply`, `Send`, `Spawn`, `Stop`, `RestartChildren`.
- `Mailbox<T>` trait: typed bounded inbox. `try_send`, `recv`.
- `Shard` trait: executor-per-core abstraction.
- A handful of small example isolates compile-tested only.

**Done when:** `cargo doc` produces a coherent API surface; nothing runs yet; consumers can write isolates against the traits.

---

## Phase Pioneer
> Pioneering deeper. SPSC mailbox + supervision trees.

> After: Phase Sputnik · Before: Phase Mariner

- `tina-mailbox-spsc`: lock-free single-producer/single-consumer ring buffer. Bounded. `try_send` with explicit `Full` error. Drop-on-full is the consumer's policy choice, not the mailbox's.
- `tina-supervisor`: supervision tree mechanism. Parent isolates spawn children, observe panics, apply restart policies (`one-for-one`, `one-for-all`, `rest-for-one`) with restart budgets.
- Property tests via [loom](https://github.com/tokio-rs/loom) for the SPSC ring under contention.
- `tina-mailbox-mpsc` sibling impl for cases where SPSC isn't enough (cross-shard fan-in). Default is SPSC; consumers opt into MPSC.
- Keep the abstraction boundary strict: `tina` owns traits and policy types only; mailbox crates own buffer layout, atomics, and queue semantics.

**Proof plan:**

- Unit + integration tests for a real SPSC implementation cover FIFO order, bounded capacity, `TrySendError::Full`, `TrySendError::Closed`, and no hidden fallback queue.
- Loom tests cover concurrent producer/consumer interleavings for the SPSC ring.
- Supervisor tests prove restart policy behavior with runnable toy isolates: `one-for-one`, `one-for-all`, `rest-for-one`, and restart-budget exhaustion.
- A runnable example demonstrates crash/restart behavior, but the assertions live in tests, not just in stdout.

**Done when:** mailbox passes loom under contention; mailbox tests prove FIFO/boundedness/error behavior; supervision tree restarts a panicking isolate without bringing down its siblings; restart budgets actually halt restart loops.

---

## Phase Mariner
> First single-thread runtime. Effect dispatcher + scheduler.

> After: Phase Pioneer · Before: Phase Voyager

- `tina-runtime-current`: single-shard runtime backed by `tokio::runtime::Builder::new_current_thread`. Pin to one core. Run a poll loop: drain mailboxes → run handlers → dispatch effects.
- The effect dispatcher is the **only** place real I/O happens. Handlers return effects; the dispatcher executes them. This is the property that makes deterministic simulation possible later.
- A working TCP echo server isolate (mirroring Tina-Odin's example) — proves the API end-to-end.
- Keep the abstraction boundary strict: `tina-runtime-current` owns scheduling, polling, and effect execution; `tina` must not grow runtime helpers just to make tests easier.

**Proof plan:**

- Black-box runtime tests prove delivery semantics on one shard:
  - accepted `Send` becomes exactly one handler invocation
  - mailbox FIFO order is preserved by the dispatcher
  - `Stop` prevents later delivery to the stopped isolate
  - rejected sends are observable and not silently buffered
  - `Reply`, `Spawn`, and `RestartChildren` are executed only by the dispatcher
- A trace-oriented integration test records handler invocations and proves that two identical seeded runs on the single-shard runtime produce the same event sequence.
- A runnable echo example is used as an end-to-end smoke test and benchmark surface, not as the only evidence of correctness.

**Done when:** runtime tests prove single-shard delivery semantics; a seeded trace test is deterministic on repeated runs; echo server handles 100k connections on a single shard with stable memory and zero work-stealing; hot-path allocations are zero per message after warm-up.

---

## Phase Voyager
> Long-duration deep-space mission. Deterministic simulation.

> After: Phase Mariner · Before: Phase Galileo

- `tina-sim`: deterministic simulator. Time is virtual, I/O is intercepted, mailbox arrival order is reproducible from a seed.
- Failure injection: drop messages, partition shards, simulate crashes, inject slow disk.
- Replay: every test failure produces a seed that reproduces the failure exactly.
- Property tests via [shuttle](https://github.com/awslabs/shuttle) for the runtime; fuzz tests for the mailbox.
- Keep the abstraction boundary strict: runtime crates expose enough hooks for simulation, but the simulator owns virtual time, trace capture, and failure injection.

**Proof plan:**

- Seeded simulator tests prove reproducible delivery traces across repeated runs.
- Failure-injection tests prove behavior under dropped messages, partitions, crashes, and slow resources without relying on wall-clock timing.
- Shuttle/property tests explore interleavings that live examples and runtime integration tests cannot cover exhaustively.

**Done when:** a multi-shard supervision tree under simulated chaos converges to a known good state every run; replay from a saved seed reproduces failures exactly; the simulator catches a deliberately-injected race that production tests miss.

This is the highest-leverage phase. Deterministic simulation is what makes Tina's discipline pay off — failures become reproducible artifacts, not phantoms.

---

## Phase Galileo
> Jupiter mission. Multi-shard runtime + cross-shard mailboxes.

> After: Phase Voyager · Before: Phase Cassini

- `tina-runtime-monoio`: thread-per-core backed by [monoio](https://github.com/bytedance/monoio) (io_uring on Linux). One shard per core, pinned. No work-stealing.
- Cross-shard messaging: when isolate A on shard 0 sends to isolate B on shard 3, the mailbox crosses cores via a separate cross-shard SPSC channel. Per-pair, not global.
- Workload placement: a hash-based router decides which shard owns which isolate. Stable under shard add/remove (consistent hashing).
- A two-shard echo benchmark: half the connections on shard 0, half on shard 1, no migration.

**Done when:** monoio runtime matches `tina-runtime-current` on single-shard echo; sharded workload of 1M small mailbox messages outperforms equivalent Tokio multi-thread code by a measurable margin (target: ≥30% lower p99 from the cache-locality win).

---

## Phase Cassini
> Long mission, sustained operations. Tokio-bridge + production hardening.

> After: Phase Galileo

- `tina-runtime-tokio-bridge`: adapter that lets a tina isolate run inside an existing Tokio app. **The whole point.** Codebases on Tokio adopt the discipline incrementally, one isolate at a time, without a runtime swap.
- Benchmark suite: SPSC throughput, mailbox latency p50/p99, per-core scheduling overhead, isolate spawn cost.
- Memory profile: pre-allocated arenas per isolate, zero per-message allocation in the hot path.
- One real-world consumer migration as a case study. Pick a multi-tenant Rust service that currently uses `Arc<Mutex<Connection>>` per tenant (a common pattern for embedded SQLite/Kuzu/etc.); replace that with a per-tenant isolate that owns the connection and processes a typed mailbox. The migration validates the bridge path and produces a public benchmark write-up.

**Done when:** a Tokio codebase compiles and runs with at least one tina isolate inside it; the bench suite is documented and green vs comparable Tokio code; the case study migration ships and reports its win/loss honestly.

---

## Open questions

These don't block Phase Sputnik, but they need answers as we go.

1. **`Effect` shape.** Resolved in Sputnik: use a closed enum with per-isolate associated payload types for `Reply`, `Send`, and `Spawn`. This keeps the dispatcher contract uniform without erasing the types carried by each isolate.
2. **Cross-shard ownership.** Tina-Odin's mailboxes are SPSC; cross-shard requires copy-or-move. Investigate whether we can use ownership transfer (move + atomic pointer swap) for zero-copy. If not, accept the copy.
3. **Supervisor split.** How much lives in `tina` (policy types: `RestartPolicy`, `RestartBudget`) vs `tina-supervisor` (mechanism: the actual tree, the watcher loop)?
4. **Coordination with Banugo.** File an issue on tina-odin asking if a Rust port is welcome before publishing crates. Polite + may produce useful design feedback.
5. **MSRV.** Pick a Rust version that supports the io_uring story without nightly. Currently this is stable Rust 1.85+ via monoio.
6. **License.** Resolved in Sputnik: dual-license under MIT or Apache-2.0 to match Rust ecosystem norms.

---

## What we're explicitly *not* doing

- **No new scheduler.** Every phase delegates execution to monoio or current-thread Tokio. The scheduler is solved; we're not solving it again.
- **No async/await replacement.** Handlers are synchronous functions returning effects. If you want await, you're in the wrong layer.
- **No global allocator games.** Pre-allocated arenas per isolate, but no `#[global_allocator]` requirements imposed on consumers.
- **No FFI to Tina-Odin.** Two runtimes fighting for cores would be the worst of both worlds.
