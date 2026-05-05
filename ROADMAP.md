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
- `tina-mailbox-mpsc` — possible future bounded multi-producer mailbox impl,
  only if a named workload proves the producer model needs it
- `tina-supervisor` — supervision tree mechanism
- `tina-runtime` — current explicit-step, simulated-driver, and
  `ThreadedRuntime` implementation over the Betelgeuse backend
- `tina-runtime-monoio` — possible future multi-shard runtime on monoio
  (io_uring), only if it preserves Tina's semantics better than the current
  driver path
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
| Bounded mailbox semantics | `tina-mailbox-spsc` proves FIFO, `Full`/`Closed`, no hidden overflow queue, drop accounting, allocation accounting, focused Miri unsafe-memory checks, and selected Loom interleavings. Cross-shard shard-pair queues are bounded and directly proved in Galileo. | This is not a full formal proof for every capacity/interleaving/refactor. Any future multi-producer mailbox support must preserve the same bounded contract and is not implemented. |
| Single-shard runtime delivery | `tina-runtime` has deterministic trace IDs and causal links, registration-order stepping, local send dispatch, local spawn dispatch, typed ingress, stop-and-abandon, panic capture, address generations, runtime-owned parent-child lineage, restartable child records, direct-child `RestartChildren` execution, supervised panic restart with policy/budget config, an assertion-backed task-dispatcher proof package, and generated-history property tests. | Supervision is still narrow: panic-triggered only, runtime-lifetime budget only, and no timed budget windows. The generated-history model is bounded and does not prove arbitrary user programs. |
| Failure isolation | Unwinding handler panics become runtime events; the panicking isolate stops and the same round continues deterministically. | This is not Tina-Odin's OS trap boundary. Rust segfault isolation, shard quarantine, and `panic = "abort"` behavior are out of scope unless a later phase explicitly designs them. |
| Multi-shard runtime/sim | `tina-runtime` and `tina-sim` expose multi-shard explicit-step runners with root placement, global event/call ids, bounded shard-pair queues, next-step-only remote visibility, deterministic harvest order, source-time versus destination-time delivery stages, simulator replay, user-shaped dispatcher proofs, sealed address-local remote-failure behavior, and shard-local supervision/restart ownership. The live Betelgeuse multi-shard runner has bounded ingress, bounded cross-shard transport, first-class live topology reports, visible queue-pressure counters, and per-shard `Running`/`Stopped`/`Failed` lifecycle reports. | Thread pinning, peer quarantine, shard-restart propagation, cross-shard child ownership, and live cross-shard isolate-call reply transport remain future work. |
| Replayability | Runtime traces are deterministic across repeated identical single-shard runs, including generated operation histories and small generated dispatcher workloads. Trace replay proofs can reconstruct worker completions and restart outcomes from the runtime event model alone. `tina-sim` adds virtual time, replay records, seeded delays/reordering over timer-wake/local-send/TCP-completion behavior, checker failures, spawn/supervision replay, scripted TCP simulation, multi-shard replay under default and non-default seeded configs, and multi-shard checker failure replay. | Real substrate liveness faults remain future work; current explicit-step shard-liveness non-claims are sealed. |
| Runtime allocation story | The SPSC mailbox hot path is tested for no per-message allocation after warm-up. Ruud Lubbers pins a narrow numerical runtime cost model for selected hot paths: multi-shard send, isolate call, timer, TCP read/write, batch, spawn/restart, trace pressure, live ingress, and high-cardinality idle stepping. Runtime and simulator now reuse per-step scratch and prebuild coordinator storage where tests prove the warmed path. | No broad runtime/simulator allocation-free claim is supported yet; boxed erasure, traces, replay records, completion slots, call translators, and user payloads may still allocate. |
| Reference examples | A Rust task-dispatcher proof package and a TCP echo proof package both exist with matching runnable examples, backed by assertions rather than logs alone. The echo proof now keeps the listener alive across a one-client smoke run, a sequential multi-client run, and a bounded-overlap run, then closes the listener cleanly and exits. | These are still proof workloads, not a broad production-server claim or benchmark story. |
| Runtime-owned I/O | `tina` names a runtime-owned call effect family (`Effect::Call(I::Call)` plus `Isolate::Call`) and an ordered batch effect (`Effect::Batch(Vec<Effect<I>>)`) for closed-set sequencing of existing effects. `tina-runtime` executes time, TCP server/client operations, local file/path operations, local persistence, UDP, bounded DNS, native TLS, bounded process runs, and runtime shutdown notification through Tina-owned driver rails with cancellation, shutdown, trace, and same-resource lane ownership. `tina-sim` scripts TCP, file/path, persistence, UDP, DNS, TLS, process, and signal rails for deterministic replay/DST. Capability reports name lane-backed, poll-backed, completion-backed, tombstoned, drained, and unsupported shapes. | Raw OS signal capture, broad network-server claims, live-substrate liveness faults, remoting, clustering, and general Tower/Axum middleware inside Tina remain future work. |
| Local persistence | `tina-runtime` exposes local snapshot/journal helpers with explicit append-before-apply semantics, snapshot `last_journal_index`, journal `record_index`, visible truncated/corrupt/commit-uncertain recovery outcomes, persistence trace events, and bounded live storage-lane admission for snapshot/journal work. `tina-sim` captures `DurableImage` path-to-bytes state for replay. | This is not a database, durable mailbox, durable work queue, or exactly-once system. Directory fsync and rename-commit support remain platform/backend scoped. Already-started local filesystem work cannot be preempted; a full nonblocking storage reactor remains future work. |

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
- Runtime allocation behavior is intentionally claimed only for the narrow
  measured paths recorded in the current cost model.
- Real substrate peer/shard liveness, shard-restart propagation, and
  cross-shard child ownership remain future work.

## Roadmap discipline

Completed work belongs in `CHANGELOG.md`, not as long phase bodies here.
`ROADMAP.md` should name the next design decisions, the intended order, and
the boundaries between near-term core work and later capabilities.

IDD execution still happens in reviewable slices. A future phase may be large
conceptually, but implementation should split when it contains independent
semantic decisions. Escalate for public API changes, semantic ambiguity,
reviewer disagreement, unsafe/concurrency/allocation-claim changes, roadmap
order changes, or public positioning questions.

## Completed phase index

Detailed completed work is recorded in `CHANGELOG.md`. The completed IDD plans
and reviews live under `.intent/phases/`.

- Sputnik / Pioneer: trait surface, supervision vocabulary, and bounded SPSC
  mailbox.
- Mariner / Voyager: single-shard runtime, runtime-owned time/TCP, supervision
  proof workloads, and deterministic simulation.
- Galileo / 021 / Kepler: multi-shard explicit-step semantics, devex/call
  ergonomics, and core primitive completion.
- Huygens / Mercury / Betelgeuse / Tina TCP Driver Contract: live threaded
  substrate, observed backpressure, isolate calls, ThreadedRuntime, and
  Tina-owned driver boundary.
- Parallel Substrate Support / Ranger / Surveyor: substrate research/support,
  mature TCP driver ownership/cancellation/shutdown, and Tina-owned
  Betelgeuse-adapter ownership.
- Willem Drees / Ruud Lubbers / Joop den Uyl: local production-shaped runtime
  proof, performance/memory hardening, and canonical application-surface tests.
- Dries van Agt: backend-honest live names, bounded trace retention, narrow
  Tokio/Tower/Axum bridge, bridge production-shape fixes, bridge metrics,
  cancellation, retry semantics, and the named Tina driver-runtime contract.
- Piet de Jong: canonical `LocalSystem` owner, bridge-from-app path, lifecycle
  terminal reports, CI/performance posture, and production-shaped local service
  proofs.
- Jelle Zijlstra: runtime-owned outbound TCP connect, runtime-owned local file
  I/O, simulator file oracle, LocalSystem/bridge file-service proofs, and exact
  deferrals for DNS/TLS/UDP/process/signal.
- Wim Kok: local snapshot/journal persistence, restart recovery, durable
  simulator image, LocalSystem/bridge recovery proof, and explicit durable mailbox
  non-claims.
- Johan Rudolph Thorbecke: bounded live storage lane, live storage overload and
  cancellation visibility, multi-shard/thread-per-core service proof, composed
  live TCP plus persistence proof, terminal trace summaries, and expanded DST
  pressure over persistence, TCP cancellation, bridge ingress, and live-vs-sim
  parity.
- Stuga: reusable deterministic-simulation-testing harness, history-as-data
  replay checks, deletion shrinking, common trace invariants, simulator storage
  fault injection, bridge model DST, long-run seed rails, and live-vs-sim
  semantic projection helpers.
- Timmerhus: first-class live topology and failure-domain reporting, honest
  queue-pressure reports, per-shard live lifecycle states, terminal topology
  snapshots, remote-queue pressure metrics, worker-failure visibility, and
  live-vs-simulator topology/failure DST with shrinking.
- Funkishus: runtime capability reporting, live and simulated UDP, simulator
  DNS with typed live unsupported, bounded live and simulated process runs,
  simulator-first signal injection with typed live unsupported, adapter-only
  TLS status, composed UDP/process/persistence proof, and resource-rail DST.
- Jan de Quay: native bounded live DNS, native rustls-backed TLS over
  `TlsStreamId`, richer runtime-owned path operations, runtime shutdown
  notification, updated capability truth, LocalSystem DNS/TLS/file/signal e2e,
  and expanded ResourceRail DST over DNS/TLS/path/signal/process/UDP.

## Near-term roadmap

These phases are about finishing Tina as a local bounded, shared-nothing
framework before public release-story work.

| Phase | Purpose |
|---|---|
| **Victor Marijnen live service semantics** | Make Tina's live local runtime behave like a real service substrate under overload, shutdown, shard failure, resource pressure, and cross-shard request/reply work. Scope: runtime config manifest, health snapshot, resource ownership cleanup, graceful drain, live cross-shard isolate calls, inbound TLS/server-side TLS, shard failure/quarantine semantics, typed acceptor/service pattern, call/reply type-safety tightening, and e2e/DST service lifecycle pressure. |
| **Barend Biesheuvel visible flow ergonomics** | Optional high-level ergonomics only after the local runtime core feels boring: design a `flow!`-style authoring surface that makes common workflows read top-to-bottom while preserving named suspension points, mandatory visible failure policy, generated trace step names, and ordinary Tina message/effect expansion. No `await` cosplay, no hidden retries, no hidden `?`, no unbounded queues, and the raw `match msg` form remains the semantic truth. |

Parallel-safe side work: CI matrix planning, formatting of existing
performance reports, README wording that adds no new claims, external review
prompts, and research notes for future remoting/clustering. Do not parallelize
changes to driver semantics, call vocabulary, runtime capabilities, simulator
resource semantics, or DST resource-history core.

## Later capability roadmap

These are real Tina directions, but they should not be treated as launch
blockers for the first local-runtime story.

| Phase | Purpose |
|---|---|
| **Jan Peter Balkenende remoting** | Tina runtime to Tina runtime over a network with typed, bounded, traceable remote outcomes. |
| **Mark Rutte clustering** | Membership and placement after remoting is boring, without weakening local boundedness or stale-address semantics. |
| **Gemini release story** | Prime-time readiness only after Tina is reasonably complete: guides, invariant docs, semver/publication decision, CI/proof gate, public positioning, and adoption story. |

## Strategic gates

These should be resolved before public release or broad adoption claims:

- **Decide the Peter Mbanugo / Tina-Odin public-positioning question early.**
  Preferred path: reach out before public publish and coordinate if practical.
  If that does not happen, docs must be explicit that `tina-rs` is an
  independently maintained Rust project inspired by Tina-Odin, not an official
  project or implied endorsement. Local design exploration is not blocked on
  this, but public positioning and any publish decision should not outrun an
  explicit decision.
- **Set the MSRV/runtime-substrate policy.** The current implementation uses
  nightly-facing Betelgeuse pieces; public release needs an explicit stable
  story or an honest nightly-only claim.
- **Strengthen CI before release.** Local `make verify` is not enough for a
  public framework claim. CI should exercise the workspace gate and the
  platform-specific substrate paths we intend to support.

---

## Open questions

These still need answers, but each now has an intended phase home.

1. **Supervisor budget windows.** Direct `RestartChildren` execution and
   panic-triggered supervised restart now exist. Runtime-lifetime budgets are
   enough for the current local-service claim; timed windows remain a later
   supervision polish item if real workloads need them.
2. **Trace retention.** Bounded/off modes exist now and Piet kept lifecycle
   facts trace-observable. Sink/counter polish is a later observability phase,
   not a blocker for Jelle.
3. **Runtime-owned I/O breadth.** Time, TCP server/client operations, local file
   and path I/O, local persistence, UDP, bounded DNS, native TLS, bounded
   process execution, and runtime shutdown notification are implemented on the
   live local runtime. Raw OS signal capture, broader substrate liveness faults,
   remoting, clustering, and middleware-inside-Tina remain future work.
4. **Live cross-shard isolate calls.** Local live cross-shard call/reply
   transport is Victor Marijnen work. Network remoting remains Jan Peter
   Balkenende.
5. **Mailbox producer model.** Current decision: one mailbox contract, no
   alternate escape path. Add bounded multi-producer mailbox support only if a
   named workload proves the current producer model is too narrow, and only
   with the same visible `Full`/`Closed`, FIFO rules, no hidden blocking, and
   no unbounded internal queue.
6. **Zero-copy / lower-allocation transport.** The current cost model is
   honest but not final. Home: later performance phase after Thorbecke's
   storage/live-service pressure exposes real costs.
7. **Sequential-looking workflow ergonomics.** The raw Tina state-machine form
   is honest but verbose for long I/O workflows. Home: Barend Biesheuvel. Any
   macro must compress ceremony only: each runtime-owned suspension point stays
   named in source and trace, failure policy is mandatory and visible, and the
   generated code remains ordinary Tina messages and effects.

---

## What we're explicitly *not* doing

- **No new scheduler.** Tina should ride on existing substrates where
  practical, but the core programming model should stay explicit-step and
  completion-driven where that best preserves the design. We are not building a
  new general-purpose async ecosystem.
- **No async/await replacement.** Handlers are synchronous functions returning effects. If you want await, you're in the wrong layer.
- **No global allocator games.** Pre-allocated arenas per isolate, but no `#[global_allocator]` requirements imposed on consumers.
- **No FFI to Tina-Odin.** Two runtimes fighting for cores would be the worst of both worlds.
