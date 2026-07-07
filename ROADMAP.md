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
  `ThreadedRuntime` implementation over the Betelgeuse backend. Betelgeuse is
  Tina's canonical portable live substrate: Linux `io_uring`, macOS `kqueue`,
  and a simulated backend sit behind the same Tina driver contract.
- `tina-runtime-uring` — possible future Tina-owned Linux `io_uring`
  driver/substrate backend only if Betelgeuse cannot satisfy a named Tina
  contract. This is evidence-gated, not the default plan.
- `tina-runtime-monoio` / `tina-runtime-glommio` — possible future adapter
  backends only if they preserve Tina's semantics better than Betelgeuse for a
  named workload
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
| Single-shard runtime delivery | `tina-runtime` has deterministic trace IDs and causal links, registration-order stepping, local send dispatch, local spawn dispatch, typed ingress, stop-and-abandon, panic capture, address generations, runtime-owned parent-child lineage, restartable child records, direct-child `RestartChildren` execution, supervised panic restart and explicit `Effect::Fail` restart with policy/budget config, explicit `Effect::StopChildren`, lifetime and windowed restart budgets, stopped-entry collection after settled restarts, `SupervisorReport`, `FairnessReport`, an assertion-backed task-dispatcher proof package, and generated-history property tests. | Cross-shard child stop/restart/address-change is still future work. The generated-history model is bounded and does not prove arbitrary user programs. |
| Failure isolation | Unwinding handler panics become runtime events; the panicking isolate stops and the same round continues deterministically. | This is not Tina-Odin's OS trap boundary. Rust segfault isolation, shard quarantine, and `panic = "abort"` behavior are out of scope unless a later phase explicitly designs them. |
| Multi-shard runtime/sim | `tina-runtime` and `tina-sim` expose multi-shard explicit-step runners with root placement, global event/call ids, bounded shard-pair queues, reserved terminal reply lanes, next-step-only remote visibility, deterministic harvest order, source-time versus destination-time delivery stages, simulator replay, user-shaped dispatcher proofs, sealed address-local remote-failure behavior, shard-local supervision/restart ownership, and local cross-shard child ownership with bounded remote child-control, lifecycle reports, and stale-address truth. The live Betelgeuse multi-shard runner has bounded ingress, bounded cross-shard transport, live cross-shard isolate-call request/reply transport, first-class live topology reports, visible queue-pressure counters, per-shard `Running`/`Stopped`/`Failed` lifecycle reports, partial trace snapshots after shard failure, terminal shutdown reports with topology/resource/error truth retained together, opt-in hard pinning of a shard worker to an OS CPU id on Linux (`sched_setaffinity` over the allowed affinity mask, reported `Applied`/`Unsupported`/`Failed`, helper lanes left unpinned), and a bounded remote-inbound drain budget. | Peer quarantine, shard-restart propagation, distributed remoting/clustering, and NUMA-aware memory placement remain future work. |
| Replayability | Runtime traces are deterministic across repeated identical single-shard runs, including generated operation histories and small generated dispatcher workloads. Trace replay proofs can reconstruct worker completions and restart outcomes from the runtime event model alone. `tina-sim` adds virtual time, replay records, deterministic per-tag seeded fault streams over timer-wake/local-send/TCP-completion behavior, checker failures, spawn/supervision replay, scripted TCP simulation, multi-shard trace observers, structured replay config/projection checks, multi-shard replay under default and non-default seeded configs, and multi-shard checker failure replay. | Real substrate liveness faults remain future work; current explicit-step shard-liveness non-claims are sealed. |
| Runtime allocation story | The SPSC mailbox hot path is tested for no per-message allocation after warm-up. Ruud Lubbers pins a narrow numerical runtime cost model for selected hot paths: multi-shard send, isolate call, timer, TCP read/write, batch, spawn/restart, trace pressure, live ingress, and high-cardinality idle stepping. Runtime and simulator now reuse per-step scratch and prebuild coordinator storage where tests prove the warmed path. `PreallocationConfig` lets live systems reserve runtime-owned metadata at setup. | No broad runtime/simulator allocation-free claim is supported yet; boxed erasure, traces, replay records, backend-owned completion slots, call translators, and user payloads may still allocate. |
| Reference examples | A Rust task-dispatcher proof package and a TCP echo proof package both exist with matching runnable examples, backed by assertions rather than logs alone. The echo proof now keeps the listener alive across a one-client smoke run, a sequential multi-client run, and a bounded-overlap run, then closes the listener cleanly and exits. | These are still proof workloads, not a broad production-server claim or benchmark story. |
| Runtime-owned I/O | `tina` names a runtime-owned call effect family (`Effect::Io(I::Io)` plus `Isolate::Io`) and an ordered batch effect (`Effect::Batch(Vec<Effect<I>>)`) for closed-set sequencing of existing effects. `tina-runtime` executes time, TCP server/client operations, native TLS client and server lanes, Unix-domain socket bind/accept/connect/read/write/close, local file/path operations, local persistence, UDP, bounded DNS, bounded process runs, runtime shutdown notification, and Unix `SIGINT`/`SIGTERM` capture through Tina-owned driver rails with cancellation, shutdown, trace, and same-resource lane ownership. TCP, TLS, and Unix-domain sockets all ride the per-shard completion substrate on the shard thread; the only deliberately retained off-shard lanes are the DNS resolver, process spawn/wait, and a narrow rename/remove/readdir/metadata storage fallback, each with a written reason. `tina-sim` scripts TCP, file/path, persistence, UDP, DNS, TLS, process, and signal rails for deterministic replay/DST. Every rail self-classifies in the capability report as completion-backed, fallback-worker, justified-blocking-lane, simulator-scripted, or unsupported, and a static guard fails the build if a new runtime rail adds a worker thread or blocking std socket/file call without being inventoried. | Live-substrate liveness faults, remoting, clustering, native database clients, and production-grade streaming remain future work. |
| Local persistence | `tina-runtime` exposes local snapshot/journal helpers with explicit append-before-apply semantics, snapshot `last_journal_index`, journal `record_index`, visible truncated/corrupt/commit-uncertain recovery outcomes, persistence trace events, bounded live storage-lane admission for snapshot/journal work, and `DurableOutbox` for bounded restart-survivable local work with record-before-apply tokens, recovery reports, compaction, commit fences, and resume queues. `tina-sim` captures `DurableImage` path-to-bytes state for replay. | This is not a database, durable mailbox, durable queue with exactly-once semantics, or distributed log. Already-started local filesystem work cannot be preempted; a full nonblocking storage reactor remains future work. |
| Native service protocols | `tina-http` now gives Tina a native HTTP stack: HTTP/1.1 parser/framing, connection/listener isolates, request/response types, routing helpers, bounded limits, visible overload, graceful close paths, native client, native HTTPS, response streaming, response-side and request-side chunked transfer, chunked client decode, keepalive client pool, server-side keepalive, keepalive chunked-response decode with retire-after-chunked safety, strict chunked size-line/accounting checks, protocol-relative target rejection, HTTP/2 server and client with `#[non_exhaustive]` `Http2Outcome` naming the lifecycle categories (`Replied`/`Full`/`Closed`/`FlowControlBlocked`/`Timeout`/`ProtocolError`/`StreamReset`/`LocalCancel`), typed `Http2ProtocolError` (peer-reset, flow-control, malformed-frame, oversized frame/headers/body, GOAWAY), h2c and h2/TLS client paths, streaming request/response bodies, PADDED/PRIORITY frame handling, peer SETTINGS application for stream windows and outbound max frame size, forbidden connection-control header rejection, rapid-reset `ENHANCE_YOUR_CALM` guard, and live tests that assert the wire error codes on every GOAWAY/RST_STREAM (FRAME_SIZE_ERROR, PROTOCOL_ERROR, FLOW_CONTROL_ERROR, REFUSED_STREAM, ENHANCE_YOUR_CALM) plus inbound/outbound DATA caps, stream and connection windows, peer reset, and window-update unblocking; native gRPC client/server over HTTP/2 covering unary, server-streaming, client-streaming, and bidirectional modes with typed `GrpcStatus` trailers, deadline mapping, message caps, compact unary submits, reusable/preframed unary bodies, bounded finite buffered server-streaming, and tonic h2c interop; WebSocket server with browser `ws`/`wss` proof, ping/pong, close handshake, subprotocol selection, strict extended-length parsing, fragmented UTF-8 rejection before app delivery, bounded outbound queue, slow-peer eviction, per-session report, and the `WebSocketMemberTable` admit/broadcast/shutdown helper; native WebSocket client sessions with explicit `ws`/`wss` targets, HTTP/1.1 upgrade, client masking, typed send/receive/report calls, ping/pong, close facts, and live Tina-server proof; runtime/simulator protocol facts for HTTP/2, WebSocket, and gRPC status replay. DST replay covers parse-good/bad, slow body, service-full, shutdown-mid-request lifecycle, and protocol fact projections. | HTTP/2 mTLS, gRPC reflection/interceptors/load balancing, pooled/reconnecting WebSocket client managers, `permessage-deflate`, web-framework ergonomics, full WebSocket-bytes simulator replay, and a production native Tina HTTP/2/gRPC client service whose received gRPC status becomes a real runtime fact remain future work. |
| Ecosystem bridges | `tina-tokio-bridge`, `tina-rpc-tokio`, `tina-tower-bridge`, `tina-reqwest-bridge`, `tina-sqlite-bridge`, `tina-sqlx-bridge`, and `tina-aws-bridge` exist as bounded bridge shapes. The docs name runtime cost, weakened replay boundary, explicit caps, shutdown truth, caller-owned retry, caller-timeout versus external-work capacity truth, typed DB/S3/SQS/SNS/DynamoDB/Secrets outcomes, bridge tracing, async Tokio drain, shared bridge convention table, public extension hooks, and workspace-excluded extension smoke crates that use public APIs only. | smol, common bridge setup extraction, publication/semver proof, and bridge crate/folder layout remain future work. |
| App/service ergonomics | Specimen work has turned repeated example pain into typed result waiters, bounded observation handles, reply aliases, TCP/Unix loop helpers, pressure summaries, HTTP router sugar, sharded placement/table helpers, deferred replies, `CallContext` reply obligation, `RequestContext` multi-turn replies, typed child refs via `spawn_observed`, cancellation handles, deadlines, pending-call sets, bounded pools, host-burst helpers, single-call gates, reqwest/DB classifiers, capacity reports, all-or-nothing shared-capacity reservations, weighted/shared body capacity, timer helper state, `RecurringTick`, `LocalPermitGate`, `DrainState`, `FullHandling`, `BoundedItems` / `BoundedEffects` service-owned caps, register-and-bootstrap helpers, `PendingCancelableCallSet`, bounded first-success `CallGroup` with cancelable branch-start sugar, `BroadcastTargets` / `broadcast_observed` for bounded fanout, threaded shutdown handles, and host `call_blocking` scripts where appropriate. | Cross-isolate paired registration, generic scatter/gather happy-path helpers, host-side scenario/test ergonomics, join-all/stream-select helpers, natural-key keyed-wait helpers, framed writer helpers, mid-run observation/projection helpers, and more real-world specimens remain future work. |

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
  distributed remoting/clustering remain future work.

## Testing infrastructure roadmap

Orthogonal to feature phases but necessary before public release claims.

- **Miri expansion.** Current Miri coverage is narrow: `tina-mailbox-spsc` only.
  Every crate touching `unsafe` should run under Miri in CI. `tina-runtime`
  denies `unsafe_code` at the crate root, which is honest, but bridge crates
  and substrate adapters may add `unsafe`. Miri gating should catch any new
  `unsafe` block, not just the mailbox ring buffer.

- **`tina-test` dev-dependency crate.** Users today hand-roll simulator setup,
  config, and step loops. A `tina-test` crate should provide:
  - `SimulatorBuilder` presets for common test shapes (single isolate,
    pair, service-with-client).
  - Assertion helpers: `assert_effect_eq`, `assert_trace_contains`,
    `assert_mailbox_full`.
  - A `#[tina_sim::test(seed = 42)]` proc macro that sets up a simulator,
    runs the body, and asserts no panics / expected trace fingerprint.
  This is the Tina equivalent of `tokio_test` and `#[tokio::test]`. The
  simulator stays visible; the goal is removing ceremony so every user writes
  DST tests as easily as unit tests.

- **Race-surface honesty beyond mailbox.** Loom covers `tina-mailbox-spsc`.
  The current shared-memory surface is intentionally narrow: cross-shard transport is stdlib
  channels, "task-list/effect-batching atomics" are not real shared structures,
  and `SharedCapacityScope` is the current public cross-thread-capable helper
  that needs a loom/shuttle model. Future shared-memory primitives must be
  added to the allowlist + model before merge. If a bounded multi-producer
  mailbox (`tina-mailbox-mpsc`) ships, it must have loom coverage before the
  crate is usable.

- **Property-based and generative testing.** `tina-sim` generates interleavings
  from a seed, but not random message sequences, fault configurations, or
  topology mutations. `proptest` or `quickcheck` over the simulator would let
  us state invariants like "for any send sequence to a bounded queue, `Full`
  outcomes never exceed capacity" and have the generator hunt for violations.
  Fuzz targets for the HTTP parser and router are worth adding once the wire
  format is stable.

- **Soak and stress testing.** Not urgent pre-release, but needed before
  production claims. A 10M-step simulation stress test hammering
  `send`/`reply`/`call` across many isolates would catch trace hash drift,
  memory leaks in simulator bookkeeping, and counter overflow. Live stress
  tests under `ThreadedRuntime` would catch substrate resource leaks and
  queue-pressure growth that short tests miss.

- **Model checking / exhaustive exploration.** `tina-sim` is randomized
  simulation. A second `tina-model-check` back-end could do bounded exhaustive
  exploration of all message delivery orderings within a step limit. This is
  research, not a release blocker. It would let us check LTL properties on
  small bounded instances. State-space explosion is managed by capping mailbox
  depth, active isolates, and steps.

- **Testing philosophy and user-guide doc.** A `docs/tina-user-guide/16-testing.md`
  should cover: when to use unit tests vs simulator tests vs live tests; how to
  write a loom test for a new concurrent structure; how to choose a DST seed;
  how to debug a failing trace fingerprint; what "same seed same failure"
  means and what it does not mean. Keep it fresh — the philosophy should evolve
  with user experience, not be frozen early.

- **Code coverage reporting.** `cargo-llvm-cov` on the workspace would show
  whether DST paths, fault-injection paths, and error-handling branches are
  actually exercised. Currently we know tests pass but not which branches they
  hit.

- **CI test matrix.** `make verify` runs locally. A public framework needs this
  in CI on at least Linux and macOS, debug and release, with loom and miri
  gates. The matrix should also test each specimen example independently so
  workspace-only changes cannot silently break downstream specimens.

### Other gaps to keep visible

- **Compile-fail / doc tests for `tina` trait crate.** The proc-macro surface
  (`#[tina::isolate]`, `#[tina_runtime::isolate]`) should have tests that
  assert malformed definitions fail at compile time. This is standard for
  macro-heavy crates.

- **Benchmark / performance regression framework.** `make perf` now gives alpha
  users local release-mode performance evidence over runtime cost rows, native
  Tina-vs-bounded-Tokio rows, and one whole-service load row with
  pressure/capacity/leak truth. Native rows now split first-queue host enqueue,
  observed admission, host request/reply, chained service request/reply, and
  HTTP/1 close/keepalive/body cases. Before production claims we still need
  broader native rows, historical tracking, and repeated equivalent-workload
  runs on stable hardware.

- **HTTP/1 parser conformance suite.** Beyond our own DST, we should run an
  established HTTP parser test corpus (e.g., `httparse` test vectors, or a
  subset of `h2spec` for HTTP/1) to catch wire-format edge cases our
  hand-authored tests miss.

- **Bridge convention test harness.** The bridge crates (`tokio`, `tower`,
  `reqwest`, `sqlx`) share setup patterns but have no shared test utilities.
  A `tina-bridge-test` crate with mock substrate adapters would let each
  bridge prove its bounded-outcome contract without spinning up real
  databases or HTTP servers. This is a convenience, not a blocker.

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

- Initial core/runtime slices: trait surface, supervision vocabulary, and bounded SPSC
  mailbox.
- Runtime and simulator slices: single-shard runtime, runtime-owned time/TCP, supervision
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
- Victor Marijnen: live local service semantics with real bounded
  source-destination queues, live cross-shard isolate calls, inbound TLS server
  rail, configured DNS/TLS/process/signal capacities, live resource inventory,
  terminal shutdown accounting, and LocalSystem e2e/DST pressure.
- Sadie's Ward: typed worker-held and pending-driver-call accounting across
  every lane, bounded per-shard shutdown drain with a configurable timeout,
  raw Unix `SIGINT`/`SIGTERM` capture via `signal-hook` (no Tokio, no async
  signal task, no custom unsafe handler), explicit failed-shard ingress
  rejection ahead of the channel-disconnect race, every bounded lane
  capacity surfaced in the topology snapshot, partial `TraceSnapshot`
  observation after shard failure, low-level shutdown reports that retain
  trace/topology/error/resource truth together, and a typed
  `ShutdownUncleanReason` list on the terminal report.
- Blue Whale: opt-in hard pinning of a shard worker to an OS CPU id on Linux
  (`sched_setaffinity`, reported `Applied`/`Unsupported`/`Failed`, helper lanes
  unpinned), runtime-owned metadata
  preallocation knobs, bounded remote-inbound drain budget, fake-substrate
  contract proof, cooperative fairness proof, checked Seastar-principles table,
  and combined e2e coverage tying core ownership, preallocation, remote budget,
  and cross-shard calls together.
- Portable local runtime completion: canonical public-path
  `LocalMultiShardSystem` service harness, runtime-call continuation replies
  through I/O/persistence, executable budget manifest, visible placement and
  backpressure policy proofs, service-level DST with saved seed and shrink,
  portable local cost rows, and focused CI gate.
- Baobab: executable local-service readiness matrix, Baobab user-service
  gauntlet over TCP/timer/DNS/process/file/persistence/cross-shard call/
  shutdown, live multi-shard sibling-survives-failed-shard proof, selected
  LocalSystem rail/backpressure e2e gate, saved-seed service/persistence/bridge
  DST histories, real Tina local timing rows, all folded into
  `make verify`.
- DST as a first-class dev mode: a "bug in a box" `ReplayCase` /
  `ReplayReport` / `ReplayConfig` shape in `tina_sim::dst` carrying the
  full `SimulatorConfig` and declared per-isolate mailbox capacities,
  `assert_replay_case` / `check_replay_case` failure messages that name
  the next decision and include the case history, debug-asserted
  name/seed/history coherence, deterministic `sweep_seeds`,
  `shrink_replay_case` with refreshed expected count/hash on the
  smaller case, a rewritten user-guide chapter around the
  build/sweep/save/shrink workflow, an upgraded `specimen_replay_dst`
  specimen with load-bearing history ops, one service-shaped saved
  case pinning a real `SendRejected{ Full }` mailbox-pressure fact
  with exact pressure counts, live-capture-to-simulator helpers that
  carry seed/config/history/expected trace shape plus typed
  config/history/event/hash/invariant mismatch output, a tiny saved-case
  reader/writer for materialized histories, and a migrated
  `timmerhus_dst` saved case so the new helpers are the way for new DST
  tests.
- SQLite/Postgres bridge and HTTP maturity tranche: `tina-sqlite-bridge`,
  `tina-sqlx-bridge`, native HTTPS, response streaming, response/request
  chunked transfer, client chunked decode, HTTP keepalive pool, server-side
  keepalive, body pressure metrics, bounded pool vocabulary, database pressure
  reports, deadlines, `PendingCallSet`, child refs, and call-context reply
  obligation. These are recorded in `CHANGELOG.md`; remaining work is now the
  next active roadmap, not a continuation of first form.
- Recent core capability tranche: capacity modeling round 2, bounded
  first-success `CallGroup`, live trace-to-sim replay capture, resource
  lifecycle shutdown matrix/keepalive shutdown helper, AWS S3/SQS bridge
  surfaces, timer helper vocabulary, native HTTP/2 with typed flow-control
  errors and live tests, native gRPC unary/server/client/bidi streaming with
  tonic h2c interop, the WebSocket production replacement story (browser
  `ws`/`wss`, slow-peer eviction, per-session report, and the
  `WebSocketMemberTable` helper), cancelable deferred admission, bridge
  convention audit, production service skeleton refresh, compile-time rails,
  mailbox-first service helpers, host-control ergonomics, production
  client/bridge breadth, request-scoped cancellation, lifecycle/health/topology,
  observability and capacity product, proof harnesses and replay ops, and the
  typed config/protocol state safety split event/request rail. These are
  recorded in `CHANGELOG.md`.
- Core/ecosystem reorg: docs now separate Tina core from official batteries,
  battery-authoring rules are written down, Wave A plans carry the new layering
  language, and the largest core files were split along behavior-preserving
  module boundaries.
- Adversarial hardening: recent broad review fixed HTTP/1 keepalive chunked
  safety, HTTP/WebSocket parser strictness, HTTP/2 peer-setting and rapid-reset
  handling, RPC cancellation accounting, reserved cross-shard terminal reply
  lanes, bridge timeout/capacity truth, process/persistence cleanup, runtime
  trace/restart/cancel hot paths, simulator fault streams, macro crate-path
  overrides, and SQLx ambiguous-commit outcome reporting.
- HTTP/2 and multi-shard fairness hardening: HTTP/2 content-length, known-length
  streaming, duplicate pseudo-header, continuation/priority, and flow-control
  edge cases are pinned with visible wire outcomes, and live multi-shard remote
  inbound drain no longer starves local runtime commands.
- Ecosystem hooks and async boundary: public extension seams — an open
  `SyncCodec` codec trait, an open `ServicePolicy` admission trait, a
  read-shaped `RuntimeCapabilityReport`, and the aligned bridge / capacity /
  event-sink vocabulary — plus five public-API-only extension smoke crates
  (custom capacity surface, custom codec, custom policy, a bounded fake bridge
  with caller-timeout honesty, and a compile-fail proof that extensions cannot
  mint runtime-owned tokens or forge private reports), and docs classifying
  native vs bridge vs unsupported async paths.
- Wave A and post-122 core tranche: native HTTP/2/gRPC client parity,
  local I/O/codec/Unix IPC parity, admission/rate policy, production resource
  lifetime, durable local work/outbox, and supervision/fairness reports are now
  recorded in `CHANGELOG.md`. Their open edges move forward as follow-ups,
  not as "first form still in progress."
- Fairness/load, native session, live-replay, copied-service ergonomics,
  cross-shard child ownership, and trace timeline tranche: Phases 120, 121,
  127, 128, 129, and 130 are now recorded in `CHANGELOG.md`. Their remaining
  edges moved into outbound session managers, protocol chaos/byte replay,
  request-scope propagation, native AWS, and protocol hardening follow-ups;
  the first three of those are now shipped too.
- Substrate alignment and service-owned boundedness tranche: Phases 134 and
  136-143 are now recorded in `CHANGELOG.md`. Config/budget manifests,
  Linux hard shard pinning, TLS/storage/Unix-on-substrate work, race-surface
  honesty, rail-inventory guards, bounded broadcast fanout, service-owned
  effect rails, overload bugbox replay, and the post-review boundedness fixes
  are done. Their remaining edges now mostly feed native AWS, pooled
  HTTP/2/gRPC clients, production soak/benchmark follow-ups, and public
  release cleanup.
- Native performance evidence tranche: completed performance work is recorded in
  `CHANGELOG.md`. Local native-vs-bounded-Tokio rows, hotpath stage probes,
  process allocation rows, owned-buffer TCP/TLS calls, HTTP encoder
  presizing, small-response coalescing, terminal TCP write-close, and an HTTP
  body-pressure perf proof are done. The history work added hotpath rows,
  a manual Linux/x86 perf workflow, direct whole-service notify/outbound-pool
  soak facts, an opt-in long soak command, and one small HTTP/1 buffered
  response allocation cleanup. Later measurement work added sharper hotpath counters,
  warmed keepalive steady-state rows, a narrow terminal completion action for
  successful protocol-local backend completions, and the first HTTP/1 user of
  that action. The scheduler/turn/tail pass added tail-aware rows (p90/p99,
  ratios, scheduler-gap counts, traced/untraced variants), a bounded worker
  hot-drain, a bounded FIFO backend-completion drain, and a host-call fast lane
  (one fewer allocation per `call_blocking`); warmed `call_blocking` p50
  improved on Linux/x86. (A ready-isolate scheduler was prototyped and reverted
  — it assumed all ingress is runtime-mediated, which the explicit runtime's
  direct-mailbox seam breaks; it needs a mailbox-driven ready signal that does
  not punch through the completion/event model.) That pass
  also isolated the dominant HTTP cost: the worker re-polled the I/O loop on a
  timer instead of waking on socket readiness, so the host path slept ~1ms
  between events. The readiness-driven worker-park experiment proved the
  performance upside but added a non-completion wake side channel
  (`step_blocking`/`IOWaker`) to the Tina-facing substrate. The current design
  removes that experiment and restores explicit-step I/O purity: live workers observe
  I/O by calling `step()` after a bounded re-poll sleep, accepting the known
  idle-wakeup and HTTP-latency tradeoff until an efficient path can be modeled
  as ordinary completion/event work. The protocol rows pass then added
  Tina-only HTTP/2 h2c and WebSocket workload rows alongside the HTTP/1 ones,
  split connection-setup rows from steady-state-reuse rows so setup cost is no
  longer mixed into service cost, and started removing protocol-internal copies.
  The real protocol pass then moved inbound HTTP/2 DATA payloads, consumed
  buffered HTTP/2 responses by value, coalesced buffered HTTP/2 responses into
  one queued write, removed duplicate WebSocket app-event delivery, and moved
  gRPC request bodies to an owned/shared cursor path. The gRPC hot-path pass
  added compact unary submits, reusable method-path templates, preframed shared
  unary bodies, shared buffered response bodies, and bounded finite
  server-streaming responses with explicit message-count/body-byte caps. The
  protocol-service hot-path pass then removed public `HttpRequest`/`HeaderMap`
  materialization from compact gRPC dispatch where no generic HTTP policy
  boundary needed it, added compact gRPC response framing, and pinned the
  remaining turn/allocation cost with protocol rows.
- Adversarial review fix wave: the 2026-06-08 review artifacts are now in
  `.intent/review/`, and every live Medium-or-higher finding from that wave has
  landed. The wave fixed RPC macro arg shadowing, split request-call compile
  rails, process drain-handle leaks, HTTP/1 keepalive over-send reuse,
  bridge/self-continuation slot leaks, multishard trace ambiguity, runtime
  stopped/promoted-slot scan cost, and HTTP/2/gRPC length/cap/settings/window
  truth. Future broad reviews should start from those artifacts instead of
  refiling closed findings.
- Remaining edges are still real and named, not hidden: after the protocol
  turn/header pass and the return to explicit-step I/O,
  wider tails under one single-shard worker, modeled efficient waiting, native
  AWS, broader equivalent-workload comparisons, and production-performance
  claims remain blocked until evidence earns them.

These are recorded in `CHANGELOG.md`; the remaining near-term roadmap now
starts with modeled readiness/completion work and native AWS. Public release
cleanup waits until Tina stops apologizing for the core HTTP/runtime hot path.

## Near-term roadmap

These phases are about finishing Tina as a local bounded, shared-nothing
framework before public release-story work.

| Phase | Purpose |
|---|---|
| **Modeled readiness completion adapter** | Recover kernel-efficient idle waiting without bringing back a wake side-channel. Readiness may be used only if Tina observes it as runtime-owned completion/event work with a deterministic simulator model, bounded ordering rules, explicit cancellation/shutdown truth, and measured idle-CPU/HTTP-latency evidence against the current bounded re-poll baseline. |
| **Native AWS first form** | Add a native Tina AWS battery for the smallest honest production shape: static SigV4 with explicit signing time, native S3 put/get/head/delete, native SQS send/receive/delete, native HTTP keepalive under endpoint/connect policy, bounded bodies/in-flight work, typed pressure/lifecycle reports, hermetic fake-AWS tests, and clear native-vs-SDK-bridge docs. Plan: `.intent/phases/135-native-aws-first-form/plan.md`. |
| **Alpaca rename** | Before public launch, rename the project/crates/docs away from Tina to Alpaca so the lineage is respectful and clear: independently maintained Rust framework, inspired by Peter Mbanugo's Tina/Odin and Seastar, not an official Tina port. |
| **Barend Biesheuvel visible flow ergonomics** | Optional high-level ergonomics only after the local runtime core feels boring: a `flow!`-style authoring surface that preserves named suspension points, visible failure policy, trace step names, and ordinary Tina message/effect expansion. No fake async, no hidden retries, no hidden queues. |

### Post-109 capability backlog

These are not IDD phases yet. They are capability clusters to design from the
evidence produced by phases 103-109 and the systems specimens. Future IDD plans
should turn them into implementation slices only after the repeated shapes are
clear.

The north star is not merely "actor-style services." It is bounded services
with deterministic simulation and replay of logical interleavings as a
first-class design constraint. Physical memory ordering is the honest
exception: it is loom-checked on a small enumerated shared-memory surface, not
replayed (`.intent/SYSTEM.md`). New capabilities must preserve:

- bounded admission or explicit bounded-exception policy
- typed `Full` / `Closed` / `Timeout` / cancel outcomes
- trace and capacity facts that can be summarized
- simulator/replay support, or an honest unsupported fact
- user-proof through specimens/systems, not only unit tests

| Cluster | Gap | Future IDD shape |
|---|---|---|
| Ecosystem extension hooks | Shipped: bridge-author vocabulary, public codec/policy/capacity/event hooks, runtime capability reports, and public-API-only extension smoke crates. Remaining: publication/semver proof and stronger templates once third-party-shaped crates grow. | Keep batteries replaceable without creating a dynamic plugin ABI, unbounded extension queues, or hooks that bypass trace/cancel/capacity truth. |
| Whole-framework ergonomics | Shipped: the copied-service path gives a real skeleton for prelude choice, config/budget manifest, public requests/internal events, defer/cancel/drain/report/shutdown, capacity assertions, and replay hooks. Remaining: keep migrating systems/specimens when they reveal repeated ceremony, especially around join/select and long-lived sessions. | New service code should start from the skeleton instead of stitching ten specimens together; future helpers should be extracted only when the copied path repeats. |
| Core ecosystem parity | Local IPC, file streaming, codec helpers, admission/rate limits, resource lifecycle, and async-boundary hooks now exist. Remaining core gaps are native AWS, native database wire clients, pooled HTTP/2/gRPC clients, saga/compensation patterns, broader soak proof, and performance that is good enough to stop apologizing. | Close these as boring capability slices with user-proof systems: native AWS, checkout saga, Redis-ish keyspace, native DB wire path, pooled gRPC client service, and long soak proof. |

Ecosystem hooks phase seed:

- **Batteries and sockets.** Blessed Tina crates remain the default batteries
  (`tina-http`, bridges, test harnesses), but each battery should plug into
  public sockets where replacement is useful. Users can adopt the battery,
  replace one socket, or use an explicit escape hatch.
- **Capacity surface hook.** Custom services expose validated surface names,
  current/high-water/cap/full/released counts, mode, discovery lines, and test
  assertions that join the normal capacity summary.
- **Bounded event sink hook.** Custom log/metric/event sinks must have caps,
  full/drop/closed outcomes, high-water/dropped counts, and drain/shutdown
  reports. No hidden unbounded observability queue.
- **Bridge author kit.** Standardize install result, closer, metrics/pressure
  report, config validation, tracing fields, late-result vocabulary, supplied
  client ownership, and worker-terminal vs caller-observed outcome language.
- **Sync codec adapter pattern.** A codec socket feeds bounded bytes, emits
  typed frames/messages, returns `NeedMore`/`Full`/`Malformed`, and keeps parser
  state replayable. Tina owns I/O and pressure; codecs own bytes.
- **Runtime capability reports.** Runtime/rail crates report supported,
  unsupported, poll/completion backed, cancel semantics, drain semantics, and
  simulator support before any pluggable-rail work.
- **Extension smoke proof.** Future IDD work should add external-looking crates
  that use only public hooks: fake bridge, custom codec, custom capacity
  surface, and bounded event sink.

Whole-framework ergonomics phase seed:

- **Blessed service skeleton.** One copied app shape for config manifest,
  runtime setup, listener, bridge/pool install, request-scoped cancellation,
  health/readiness, shutdown, topology/capacity summary, and replay seed/facts.
- **Prelude tiers.** Keep `tina::prelude::*` boring for app authors; move raw
  escape hatches into explicit advanced imports.
- **Noun guide.** One short map for `Effect`, request/event, `RequestContext`,
  `CallOutcome`, pending helpers, pool leases, capacity reports, and trace
  reports. The point is "which noun do I use right now?"
- **Fluent but honest workflows.** Defer, defer-cancelable, race/join,
  recurring tick, pressure policy, and shutdown helpers may compress ceremony
  only when the suspension point, failure policy, capacity, and trace step stay
  visible.
- **Specimen rewrite.** Rewrite a small fixed set of systems on the blessed
  path, then delete stale README guidance and move solved pain to history.
- **Cheap-model proof.** Give the docs/skeleton to a fresh model, record what it
  wires wrong, and either make the mistake a compile error or fix the copied
  path.

### Mailbox-first devex polish sketch

The goal is to remove accidental ceremony from the code users and LLM coding
agents will write most often while preserving Tina's load-bearing weirdness:
handlers stay synchronous, all delayed work returns through a mailbox, caller
authority is explicit, and pressure remains a typed service fact.

The current raw shape is honest but easy to make visually noisy:

```rust
let request = ctx.take_request_context().unwrap();
sleep(self.work).reply_with_request(request, move |request, result| {
    LaneMsg::PutFinished { request, key, result }
})
```

This future note should look for a blessed spelling closer to:

```rust
ctx.defer_reply(sleep(self.work))
    .to_self(move |reply, result| LaneMsg::PutFinished { reply, key, result })
```

or, after 086 gives call handlers explicit reply authority:

```rust
call.reply_after(sleep(self.work))
    .to_self(move |reply, result| LaneMsg::PutFinished { reply, key, result })
```

This helper must not run user state mutation in a hidden callback. The
continuation still produces an ordinary message to self; the final state change
and final reply happen in the isolate handler.

The same phase should harden two other repeated footguns found by the
system-shaped examples:

- Bridge calls should become harder to misuse by construction. A health check
  should not require remembering whether `SELECT 1` is query-shaped or
  execute-shaped:

  ```rust
  db.probe().call(self.db_timeout)
  db.query("SELECT 1").limit(1).call(self.db_timeout)
  db.execute("INSERT INTO items ...").call(self.db_timeout)
  ```

- Bounded lanes should have a tiny isolate-local admission vocabulary. It
  should be mechanism, not policy: admit, release, snapshot, reply busy. Pool
  phases and capacity phases already cover richer reports and shared/weighted
  capacity; this note is the small ergonomic surface for the common "cap
  in-flight work and shed visibly" case:

  ```rust
  if let Some(permit) = self.in_flight.try_admit() {
      call.reply_after(work).to_self(move |reply, result| {
          LaneMsg::Finished { permit, reply, result }
      })
  } else {
      call.reply(LaneReply::Busy(self.in_flight.snapshot()))
  }
  ```

Keep the hidden-bug list explicit during review: permits must release exactly
once on timeout, cancellation, shutdown, and late completion; reply authority
must be impossible to reply twice or silently drop; bridge DSLs should stay
shallow enough that compile errors do not become type-state soup; pressure
helpers must not smuggle in retries, fairness, priority, or unbounded queues.

Bridge-backed bounded lanes add one more concrete checklist. The
`system_bounded_object_lane` specimen deliberately stays hermetic because the
real-S3 temptation exposed the production contract a real AWS bridge must own:

- completion delivery must be observed (`Full` / `Closed` / worker stopped), not
  best-effort `try_send`, so a dropped completion cannot leak in-flight
  accounting;
- caller timeout, operation timeout, and late completion must be distinct typed
  outcomes, with abandoned replies and tombstoned completions visible in trace
  and terminal reports;
- bridge shutdown needs explicit close/cancel/drain budget semantics, not a
  specimen-local thread join;
- bridge-job capacity, completion mailbox capacity, and in-flight admission cap
  should be configured as one inspectable budget surface;
- any SDK-backed bridge must name the weakened boundary honestly: Tina can bound
  admission into the SDK, but SDK-internal queues/threads are not Tina-owned
  unless the bridge proves and reports them.

### Visible race and call-capture ergonomics

The rule for race/capture helpers is:

```text
smooth mechanical repetition
do not flatten semantic truth
```

`examples/systems/ergonomics_playground` is the evidence. It now has five small
probes:

- first-success quote race: reply to the original caller once, cancel the loser,
  and keep loser-cancel settlement visible
- no-winner quote race: both providers reply unavailable, so the gateway waits
  for all branch outcomes before answering `Unavailable`
- late cancelled reply: the cancelled loser eventually replies and the trace
  records `CallerCancelled` rejected truth instead of delivering a normal
  gateway message
- debounced batch drain: callers parked in `PendingReplies` are all replied
  `Closed` when admission is drained
- single-flight cache fill: five callers miss the same key, one upstream fill
  runs, three admitted callers share the result, and two overflow callers get
  `Full`

These cases show what must remain visible in any helper:

- caller authority: the original `RequestContext` is replied exactly once
- bounded storage: pending waiters live in a named fixed-capacity container
- branch identity: each race branch has a key/token and a terminal outcome
- cancellation truth: loser waits are explicitly cancelled and cancel outcomes
  are recorded
- late-result truth: cancellation means "Tina stopped waiting," not "external
  work stopped"; late replies still become rejected trace facts
- aggregate failure: no-winner races wait for all relevant branch outcomes
  instead of treating first reply as success
- overload: rejected admission is `Full`/`Closed`/`Timeout` vocabulary, not
  hidden buffering

The first-success `CallGroup` helper now covers the core race shape. Remaining
polish should stay modest. A future start helper may replace the repeated:

```rust
let token = group.reserve_token()?;
let (effect, handle) = call_cancelable(addr, msg, timeout).then(...);
group.insert_reserved(key, token, handle)?;
```

with a start helper such as:

```rust
self.race.start(key, addr, msg, timeout, Msg::Returned)?;
```

but the later handler must still explicitly call `record_reply`, return
`cancel_call(...)` effects for losers, feed continuations into `record_cancel`,
check `report_ready`, and answer the parked `RequestContext`.

Likewise, a `PendingReplies::try_capture_call(...)` helper may replace manual
capacity pre-check plus:

```rust
pending.try_insert(qid, call.into_request_context().into_deferred())?;
```

but failed admission must return the unconsumed `CallContext` so the service can
reply or reject deliberately:

```rust
match pending.try_capture_call(call, qid) {
    Ok(()) => { /* start or join visible work */ }
    Err(CaptureCallError::Full(call)) => call.reply(Reply::Full),
    Err(CaptureCallError::DuplicateKey(call, _)) => call.reject(...),
}
```

Non-goals stay explicit: no `select!` clone, no race DSL, no hidden scheduler,
no fake async surface, no hidden retry, no auto-cancel that erases terminal
facts, and no helper that implies cancellation stopped work outside Tina's owned
wait.

### Natural-key admission for cancelable pending sets

`system_job_queue` exposed a generally-applicable mismatch between Tina's
two pending-set helpers and the kinds of keys real services actually use.

**The shape today.** Tina ships two bounded "I have outstanding work to
remember" structures:

- `PendingReplies<K, R>` — used by `system_cache_with_fill`. Allows
  multiple parked callers per key (multiple browser tabs hitting the
  same cold cache key). Removal is by `(K, slot_id)`. ABA-safe by
  construction because each insert mints a fresh slot.
- `PendingCancelableCallSet<K, Q, R>` (PR #92) — used by
  `system_job_queue` v2. One entry per key. Bundles caller authority
  (`RequestContext<Q>`) and the cancel handle (`CallHandle<R>`) so
  cancel-while-running can answer the original caller atomically.
  Removal is by `(K, ticket)` to defeat ABA on completion paths, but
  *insertion* still rejects with `DuplicateKey` if the key is already
  present.

**The trap.** `try_insert`'s `DuplicateKey` is correct as a loud version
of an ABA bug, but it forces the user to pick keys that never collide.
`system_job_queue` works around this by minting monotonic `JobId`s
internally — fine when the service owns id assignment. Many real
services do not:

- a session manager keyed by external `session_id`
- a worker pool keyed by `worker_index`
- a tenant rate limiter keyed by `tenant_id`
- a webhook relay keyed by `subscriber_id`

For those, the second concurrent operation on the same natural key
either returns `DuplicateKey` (and the user has to invent a queueing or
versioning layer outside the helper), or the user falls back to
`PendingReplies` and loses the cancelable-handle pairing.

**Why it matters.** This is the kind of choice that should be made by the
type system, not by the user discovering it the hard way. The current
shape teaches "keep your keys monotonic or fall off a cliff," which is
exactly the kind of hidden contract Tina aims to remove.

**Fix.** Use `CancelableWork<K, Q, R>`: a bounded helper
that admits the same caller-authority + cancel-handle bundle but allows
multiple entries per natural key. Internal identity is by `WorkTicket`; natural
key is metadata attached to each entry. The shape:

```rust
let ticket = self.work.admit(natural_key, token)?;
```

Lookup returns an iterator (or "latest") for natural keys; removal is
strictly by `(natural_key, ticket)` like the existing set. ABA is
impossible because nothing is keyed solely by natural key. The cost is
that the user maintains the "natural-key → live tickets" mapping
externally, but that mapping is what they were going to write anyway
once they hit `DuplicateKey`.

**Decision rule for users.** Document the choice:

- one outstanding op per key, key chosen by the service →
  `PendingCancelableCallSet`
- one outstanding op per key, key chosen externally →
  `CancelableWork` with a per-key cap of 1
- multiple parked callers per key, no cancel handle needed →
  `SharedWork`
- multiple parked callers per key WITH cancel handles → we need to
  use `CancelableWork`

**Non-goals.** No silent ABA dedup on insert. No "automatic
key-versioning" that hides whether a stale completion replaced a fresh
one. The slab's value comes from the explicit (natural_key, ticket)
discipline, not from removing it.

**Surfaces this would unblock.** `system_session_auth`,
`system_tenant_rate_limiter`, `system_webhook_relay`, `system_lock_manager`,
and the worker-index variant of `system_job_queue` are all natural-key
shaped and currently have to invent the workaround themselves. This is now
strong enough for the repeated natural-key workload.

### Keyed wait lists over bounded pending replies

`system_cache_with_fill` and `system_lock_manager` both wrote the same
waiter shape:

- global `PendingReplies` cap owns parked caller authority;
- each key/bucket owns a FIFO of waiter ids;
- handoff loops pop ids until one still has a live deferred reply;
- per-key caps and global caps are reported separately by handwritten code.

`SharedWork<K, R>` owns this shape. The helper owns both caps, returns
typed global `Full` vs per-key `KeyFull`, recover the caller on failed
admission, and make skip-reclaimed-slot behavior explicit. No hidden per-key
unbounded queues.

## Capability layers still needed

These are not planning phases. They are capability gaps to close as real
implementation slices pull on them. Each entry should land as boring code,
tests, specimens, and replay/unsupported truth when the adjacent work proves the
shape.

| Capability | Build | User outcome |
|---|---|---|
| Child lifecycle and join | Typed child-start observation, typed child address/result, join-many result collection, parent-stop child cleanup, and replacement-address refresh without trace spelunking. | A service can spawn workers/sessions, wait for them, stop them, and survive restarts without inventing a side registry. |
| Stronger owned-work cancellation | Uniform cancel/close/drain rules for Tina-owned timer/TCP/TLS/process/file/DNS rails, with tombstoned late completions and prompt resource release where the backend can support it. | `cancel` stops waiting everywhere, and for Tina-owned work it also releases the owned resource as strongly as the rail allows. |
| Capacity scopes | Extend the shipped weighted/shared capacity tools beyond HTTP body bytes into shard-local budgets for pools, pending calls, bridge in-flight work, and service-owned buffers. | Users tune from evidence instead of vibes, and related surfaces can share one visible budget. |
| Live trace to sim replay expansion | Shipped: live-capture workflow, saved-case round trips, shrink, and overload bugbox helpers for bounded capacity facts. Remaining: keep extending supported facts as new runtime rails, bridges, and protocol specimens need them. | More production bugs can become deterministic "bug in a box" cases instead of log-reading séances. |
| Race/join helpers | First-success `CallGroup` exists. Add join-all, stream select, and child-ref sugar only when real specimens pull on them. | Common `select!` / `join!` Tokio shapes can be written in Tina without hiding which branch won or what got cancelled. |
| Timer vocabulary | Interval, backoff, retry-delay, debounce, and throttle helper state exists. Future work is periodic service patterns, jitter policy, and deadline propagation polish where real services need it. | Periodic and retrying services stop hand-rolling sleep loops and still replay deterministically. |
| Production service skeleton | Shipped: the canonical copied-service path ties HTTP/HTTPS-style ingress, routing, DB/outbound pressure, graceful shutdown, tracing, capacity assertions, and live replay capture together. Remaining: add variants as native AWS, pooled HTTP/2/gRPC, and larger long-lived session managers mature. | An LLM can copy one real-service shape and replace a moderate Tokio app without stitching ten specimens by hand. |
| Protocol and client breadth | Shipped: HTTP/2 server/client, gRPC server/client streaming modes, WebSocket server/client sessions, AWS bridge breadth, and protocol facts. Remaining: HTTP/2 mTLS, gRPC reflection/interceptors/load balancing, pooled/reconnecting client managers, client-side session lifecycle polish, and protocol-byte replay beyond the shipped WebSocket bad-frame workflow. | Common Tokio protocol workloads have native Tina paths instead of falling back to Tokio bridges at the first real boundary, without hiding stream pressure. |
| Compile-time diagnostics | `#[diagnostic::on_unimplemented]` and macro error polish for `Isolate`, message/send/call bounds, non-`Send`, non-`'static`, wrong shard, and wrong reply shapes. | Humans and LLMs see "this message is not Send" instead of trait soup. |
| Resource lifecycle unification | One boring vocabulary across runtime resources and bridges: open/start, ready, use, close, cancel, drain, terminal report, pressure report. | No stream/file/process/bridge worker can be stranded invisibly; every resource has the same mental model. |
| Fairness under load | Prove and expose fairness for actor/session scheduling under hot keys, slow sessions, remote inbound drain, timers, and protocol sessions. Add reports for starvation-ish lag where a bounded runtime can observe it honestly. | One hot actor or WebSocket session should not quietly starve unrelated work; when it can, the report says so. |
| Runtime/service observability | Queue depth, lag-ish counters, drops/full counts, shutdown state, task/session counts, pool state, bridge in-flight state, and per-surface pressure reports in one copied reporting shape. | Operators can see why a service is unhealthy without decoding raw traces first. |
| Health and readiness | Shipped. `tina_runtime::lifecycle` names `Lifecycle` (Starting/Ready/Degraded/Draining/NotReady/Stopped), typed `ReadinessReason` variants with stable wire tokens, a `Readiness` verdict with legacy HTTP body rendering, and a `Health` snapshot that pairs the lifecycle state with a `ServicePressureReport`. `mini_saas_api` uses them on `/ready`; `system_metrics_shipper` uses the same vocabulary for a non-HTTP shape. | Deployments can stop sending traffic before shutdown and can report "not ready because DB pool closed" honestly. |
| Shutdown orchestration graph | Shipped. `ShutdownChoreography` records ordered steps (`StopIngress`, `CancelSessions`, `DrainInFlight`, `FlushBatchers`, `CloseResource`, `EmitReport`, `StopOwner`) with elapsed and outcome, folds resource-specific reports into a shared `ResourceCloseReport` vocabulary, and flags backwards recordings as `StepOutcome::OrderingViolation`. Used by `mini_saas_api` (HTTP) and `system_metrics_shipper` (non-HTTP). | Graceful shutdown becomes a copied Tina program instead of bespoke stop-message choreography. |
| Backpressure policies | Shipped: admission/rate/concurrency policies, `FullHandling`, typed pressure actions, guarded pending replies, and all-or-nothing shared-capacity reservations. Remaining: park-friendly local concurrency permits and more retry/degrade policy proof in full-service specimens. | Services choose pressure behavior at call sites without losing `Full`/`Closed`/`Timeout` truth. |
| Runtime-owned recurring work | Local cron/periodic task patterns for compaction, health checks, token refresh, session expiry, and metric flush, with missed-tick policy. | Long-lived services get boring recurring work that is bounded and replayable. |
| Config and budget manifest | Shipped. `ServiceBudgetManifest` declares mailbox caps, pool caps, body/body-weight caps, rail/lane caps, pending call/reply caps, request-scope caps, explicit unbounded policies, replay impact, validation, live-pressure joins, and replay-export hashes. Remaining: add adapters as new batteries land. | Operators and coding agents can see all knobs before the service runs and can include them in replay cases. |
| Service topology report | Shipped. `ServiceTopology` plus `TopologyComponent` build one greppable report naming every started isolate, bridge, pool, listener, address, the shard label, the current `Lifecycle`, and the backing `ServicePressureReport`. No global registry; services thread it explicitly. | Users can ask "what is running?" and get a useful answer without scraping raw traces. |
| Bounded event/log sink | Runtime-owned or specimen-proven bounded log/metric/event sinks with visible overflow/drop policy. | Observability does not become the first hidden unbounded queue in an otherwise bounded Tina service. |
| State snapshot and restore | Blessed snapshot/journal/restore patterns for shard-owned state, including append-before-apply proofs and torn-write recovery specimens. | Ordinary services can restart with state safely, not only demos with in-memory maps. |
| Saga / compensation pattern | Typed multi-step workflow pattern over DB, HTTP, pools, and services, with explicit compensation, timeout, cancellation, and partial-failure reports. | Multi-resource business workflows become readable Tina state machines instead of ad hoc async control flow. |
| Load and soak harness | A reusable harness for long runs that records capacity high-water, full counts, latency-ish summaries, resource leaks, and trace fingerprints. | Teams can prove a Tina service stays bounded for an hour before claiming it is production-shaped. |
| Chaos / bad-peer harness | Shipped: transport bad-peer probes (half-close, reset, slowloris, stalled writers, reconnect storms, TLS failure, malformed frames), one typed `ProtocolChaosReport` per story, a hermetic WebSocket compliance corpus, WebSocket byte-replay save/shrink, and HTTP/2 and gRPC bad-peer probes that map malformed framing to typed facts. Remaining: browser/client interop and more real-service probe coverage. | Protocol work stops passing only happy-path unit tests and starts proving the failure shapes real services meet. |
| DNS/connect policy | Shipped: unresolved endpoints, bounded DNS/connect policy, Happy Eyeballs/address-family ordering, typed DNS/connect partial failures, and session-manager pressure/lifecycle reports. Remaining: extend the same policy into native AWS and pooled HTTP/2/gRPC clients where those batteries need it. | Outbound clients do not hang or hide which name/address path failed. |
| Unix sockets and local IPC | Shipped: Unix-domain listener/client rails in live runtime and simulator, plus local IPC specimens. Remaining: Unix loop helpers (`write_all`, `read_to_eof`) and the same ergonomic companions TCP/file already earned. | Tina can own local admin/sidecar protocols without jumping back to Tokio for one socket kind. |
| File streaming and codec helpers | Shipped: bounded file streaming/copy helpers, line and length-delimited codecs, and open `SyncCodec`. Remaining: framed writers and a less clunky `FileCopyBounded` drive loop. | Services can serve files, ingest files, and parse framed protocols without hand-rolling the same loops. |
| Request-scoped cancellation | Shipped: `ScopedRequestReport`, bounded request-scope sets, tombstoned scoped timers, HTTP/WebSocket/gRPC/body adapters, and system proof that client disconnect cancels the request tree and reclaims capacity. Remaining: add adapters as new batteries land and keep bridge/external-work truth honest. | "Client went away" becomes one typed request shutdown path instead of scattered domain Stop messages. |
| Admission, rate, and concurrency limits | Shipped: concurrency, keyed, rate, shared-capacity, pressure-action, and report vocabulary. Remaining: easier multi-scope all-or-nothing admission and park-friendly charge helpers. | Edge services can say "too busy" in a controlled way instead of hoping each mailbox cap is enough. |
| Broadcast/fanout primitive | Shipped: `BroadcastTargets`, `BroadcastTracker`, `BroadcastReport`, and `broadcast_observed` give room/session services a service-owned target cap, per-target `Accepted`/`Full`/`Closed` accounting, and an ordinary continuation-message path. Remaining: richer per-peer slow policy and room-manager helpers when another real service pulls on them. | Chat/WebSocket/realtime services get a copied Tina shape for broadcast without losing per-peer pressure truth. |
| Pool maturity | Shipped: idle eviction, max lifetime, health checks, retire/reuse policy, shutdown reports, DB pressure alignment, and HTTP/1 keepalive retirement. Remaining: pooled HTTP/2/gRPC clients and cross-protocol session lifecycle polish. | Pools become production resources, not just first-form acquire/release examples. |
| Async ecosystem boundary | Shipped: native-first capability reports, bridge-author vocabulary, extension smoke crates, open sync codec hooks, and docs separating native, bridge, and unsupported async paths. Remaining: decide whether a bounded Future/Stream bridge is worth building once a real workload proves it. | Users know which Tokio apps Tina can replace natively and where a bridge is the honest boundary. |
| Benchmarks with humility | Shipped: local release-mode native performance rows, bounded Tokio comparison rows, hot-path stage reports, perf history/check scripts, process allocation rows, HTTP body-pressure perf proof, manual Linux/x86 perf workflow, opt-in long soak, a now-removed readiness-driven worker-park experiment, HTTP/2/WebSocket/gRPC protocol rows, structural HTTP/2 byte-path reductions, duplicate WebSocket event removal, gRPC compact/preframed/buffered hot paths, compact gRPC service dispatch/response framing, and the return to explicit-step I/O. Remaining: reduce deeper protocol/runtime turn count, reduce inbound HPACK/header allocation beyond compact gRPC, collect repeated Linux/x86 rows, broaden equivalent-workload comparisons, and resist production-performance claims until repeated evidence earns them. | Performance claims do not outrun Tina's boundedness and correctness story. |
| Ecosystem extension hooks | Shipped: bridge-author vocabulary, open sync codec and service-policy hooks, capacity surface/event-sink vocabulary, runtime capability report, and public-API-only extension smoke crates. Remaining: publication/semver proof and stronger author templates once third-party-shaped crates grow. | Tina can grow an ecosystem without every new capability landing in core or weakening bounded/DST truth. |
| Whole-framework ergonomics | Shipped: one coherent copied path for a real service: prelude, config/budget manifest, public requests/internal events, defer/cancel/drain/report/shutdown, and replay hooks. Remaining: keep the skeleton current as AWS, pooled HTTP/2/gRPC, native database, and saga-shaped systems land. | A new developer or cheap model can build a correct bounded + replay-aware service without stitching ten specimens together. |

Closed follow-ups from HTTP body streaming and native HTTP/1:

The 074 slice shipped server-side body streaming with pressure metrics,
`IterBodySource`, chunked transfer-encoding emit, and a specimen. The
follow-ups that were intentionally deferred from that PR have now mostly
landed in their owning phases.

| Group | Lands in | Items |
|---|---|---|
| Cancel surface | **079 cancellation round 2** | Landed: connection-to-source cancel signal on wire failure (`ResponseChunkMsg::Cancel`) for known-length and chunked streaming responses. |
| Capacity report | **082 capacity modeling round 2** | Landed: weighted body-byte capacity, shared HTTP body scope, explicit unbounded-for-now policies, and better discovery/assertions. Periodic / live-tick metric emit remains future observability polish. |
| Chunked symmetric | **080 HTTP body chunked symmetric** | Landed: client-side chunked decoding and server-side chunked request bodies through the streaming pull model. |

Do not resurrect the old "chunked is deferred" wording. The remaining body
work is WebSocket/gRPC/protocol breadth and broader capacity-report polish,
not basic HTTP/1 chunked semantics.

Bridge crate layout note: the bridge crates are still small enough at the
repo root, but the next bridge audit should revisit grouping them under a
shared bridge namespace or folder. Do not abstract setup/config/metrics early;
three repeated shapes is evidence, two is coincidence.

Adopt-don't-rebuild discipline for native protocol/database phases: Tina owns sockets,
buffers, state machines, backpressure, shutdown, and trace. Tina borrows
*codecs*, never *runtimes*. The public sync ecosystem already covers most
of the bytes-shaped work:

- HTTP/1 wire — `httparse` (zero-copy parser, sync)
- HTTP types — the `http` crate (`Request`, `Response`, `Method`, etc.)
- HPACK / HTTP/2 headers — `hpack` or `hpack_codec` (sync)
- HTTP/2 framing — vendor or write; pure bytes, no async required
- Protobuf — `prost` (sync codec, runtime-agnostic)
- Postgres wire — `postgres-protocol` (sync, just bytes-in/bytes-out)
- SQLite — `rusqlite` (sync, blocking C wrapper; fits a bounded blocking
  bridge)
- TLS — `rustls` (sync state machine; not `tokio-rustls`)
- WebSocket framing — `tungstenite` core (sync, not `tokio-tungstenite`)

The pattern is always the same: codec is sync, Tina drives the I/O via
runtime calls, the handler is a state machine that calls the codec on
each `tcp_read` reply. No vendored async runtimes; no hidden Tokio under
Tina services.

Specimen comparison backlog:

| Comparison | Pressure shape | Learning goal |
|---|---|---|
| Chat / slow-consumer fanout | Real TCP, burst fanout, slow or non-reading clients | Learn whether Tina's visible `Full` pressure is worth the extra ceremony versus easy Tokio buffering. |
| Mini-redis-style keyspace | Hot keys, many clients, slow replies, persistence later | Test isolate-per-key/session ergonomics, request/reply repetition, and runtime-owned TCP framing. |
| Axum/Tower stateful service | Tokio HTTP edge, Tina stateful core, overload at ingress | Test bridge ergonomics and whether visible Tina failures map cleanly to HTTP/Tower behavior. |
| Supervised worker | Job worker that panics on poison messages, parent observes the failure | Test whether Tina's supervisor + restart budget reads more honestly than hand-rolled Tokio `catch_unwind`/respawn loops. |
| Persistent counter | Process restart over snapshot + journal | Test runtime-owned local persistence ergonomics against Tokio + a hand-rolled file/sled story. |
| Replay / DST comparison | Same workload run twice under `tina-sim` with one seed plus a Tokio reference run | Demonstrate deterministic replay as a real Tina capability that the Tokio shape cannot offer. |
| Outbound fetch / Tina-as-client | DNS + outbound TCP + read/aggregate | Test whether Tina is honest as a client library, not just a server, and surface DNS/connect/read timeout shapes. |
| Graceful shutdown | Long-lived service with in-flight work receiving SIGINT | Compare `tokio::signal` + manual drain against Tina signal capture, bounded shutdown drain, and terminal `ShutdownReport`. |
| WebSocket room | Bidirectional read/write, ping/pong, reconnects, slow readers | Test whether explicit connection/session isolates make liveness clearer or too verbose. |
| Multiplexed client subset | Many in-flight requests on one connection, timeout/cancel/reply matching | Test whether Tina can model client libraries naturally, not only servers. |
| CPU contention run | Same service load while CPU is quota-limited or contended | See whether Tina keeps shedding visibly under scheduler pressure. |
| Memory-tier run | 32/64/128 MB process limits, same load profile | See whether Tina plateaus while Tokio-shaped buffering grows or fails less visibly. |

Specimen backlog that still matters after the round-2/round-4 harvests:

Many original round-2 ideas have now become code, phases, or closed
findings. Keep the remaining list as a pressure menu, not a fixed phase.
When one of these repeats a product pain, promote that pain into a named
phase and move the closed specimen result to `CHANGELOG.md`.

| Comparison | Pressure shape | Learning goal |
|---|---|---|
| Mini-redis-style keyspace | Hot keys, many clients, slow replies, persistence later | Test owned sharded/session state in a production-shaped service, not only counter specimens. |
| Periodic batcher | Producer pushes items, batcher accumulates until 100 ms timer or 1000 items, flushes durably | Common production pattern (Kafka producer, log shipper, metrics aggregator). Tests timer + state + bounded buffer + persistence in one isolate. Also validates the "cancellation as message arm beats `select!`" claim under real timer load. |
| Stateful HTTP session | `POST /login` issues cookie, `GET /me` reads per-session state | Per-session-isolate ergonomics through the bridge with a real web shape: cookie/header round-tripping, session lifecycle, GC. Tests the "isolate per session" pitch with a non-contrived scenario. |
| Heterogeneous workload | TCP listener + timer-driven flusher + bridge ingress + persistence in one runtime | Tests interactions between subsystems that single-protocol probes can't see. Closer to what real services actually look like. |
| Durability-misorder attempt (adversarial) | Try to update `self.value` before journal commit completes | Tests whether "append-before-apply enforced by message shape" is a real type-system property or just a discipline. |
| Non-determinism-in-isolate (adversarial) | Inject `SystemTime::now()`, `HashMap` iteration, `thread_rng()` inside a handler under tina-sim | Tests whether the replay claim has teeth. If the simulator can't detect the non-determinism, "deterministic replay" is weaker than asserted. |
| Seeded-fault crash recovery | `specimen_persistent_counter` shape under tina-sim FaultConfig with mid-write process death (mid-len, mid-payload, mid-checksum) | Turns the deterministic-replay property into a deterministic-crash-recovery property. The hard test of persistence — torn-tail handling, journal-replay correctness, snapshot atomicity. The thing operators actually want. |
| Live trace ↔ sim trace identity | Record typed live replay facts under `ThreadedRuntime`, replay against `Simulator` with the same seed/config/inputs, and reject unsupported live facts | The direct end-to-end DST workflow: production weirdness can become a deterministic simulator case when the needed facts are captured. |
| Session fanout (1000 sessions) | 1000 concurrent session isolates, each with mailbox, lifecycle, periodic flush | Stress-tests the "isolate-per-session" pitch. Reveals per-isolate memory cost, mailbox-creation throughput, address-allocation cost, what happens when many isolates compete for one shard's processing slot. |
| Real load driver on chat | The driver promised in `specimen_real_io_chat`'s README, never delivered: 1000 clients × 50 000 messages | Finally measures "visible Full" under sustained burst rather than a single hardcoded scripted output. Required to make `specimen_cpu_run` and `specimen_mem_run` reports useful — they currently only answer "did it pass," not "did Tina shed visibly while Tokio buffered silently." |

Specimen rule: when a specimen closes a claim, move it to
`CHANGELOG.md`. When it finds repeated pain, promote that pain to a
phase. Do not leave solved pain in `examples/FINDINGS.md` as if it is
still current.

Parallel-safe side work: CI matrix planning, formatting of existing
performance reports, README wording that adds no new claims, external review
prompts, and research notes for future remoting/clustering. Do not parallelize
changes to driver semantics, call vocabulary, runtime capabilities, simulator
resource semantics, or DST resource-history core.

### Learning from Glommio

Glommio is the closest Rust neighbor to Tina's live-runtime direction:
thread-per-core, shard-local execution, `io_uring`, per-executor placement,
task queues, scheduler shares, latency-sensitive work, direct-I/O emphasis, and
stall detection. Tina should learn from those operator-facing surfaces without
turning into a generic async task runtime. The Tina-shaped translation is
"make shard CPU/I/O policy visible and configurable in terms of shards, lanes,
effects, completions, pressure, and replay truth."

This is a roadmap note, not an IDD plan. Future IDD phases should turn pieces
of it into implementation work only after choosing a concrete slice. Existing
controls and reports already point this way:

- `ThreadedRuntimeConfig` already has `remote_inbound_drain_budget`,
  `driver_completion_drain_budget`, `hot_drain_max_rounds`,
  `hot_drain_max_elapsed`, and `idle_repoll_interval`.
- `LiveTopologyReport` already names shard workers, configured/observed cores,
  affinity outcome, ingress queues, remote queues, and resource counts.
- `FairnessReport` already folds trace progress counts, but does not yet expose
  live ready-turn lag, handler wall-time stalls, or driver/completion stall
  counters.
- Reactor storage already moved the durability read/write/fsync/size path onto
  the Betelgeuse file rail, leaving only named fallback ops on a bounded worker.
- `make perf`, perf history, native rows, hot-path rows, allocation evidence,
  and soak hooks already form the humble performance-proof spine.

Possible implementation slices, in likely order:

0. **Runtime-ops truth cleanup.** Fix stale substrate docs and reports before
   adding new surfaces. Current code has places that still describe TLS as
   worker-thread-backed even though native TLS now rides the Betelgeuse TCP rail
   on the shard thread. Those comments make the thread-per-core story look worse
   than the runtime is. Keep capability docs, live reports, and user-guide text
   aligned with the real rail shape.
1. **SchedulerPolicy / SchedulerReport.** Wrap the existing live scheduling
   knobs in one public policy/report vocabulary instead of scattering them
   across config docs. Report, per shard, the configured local/remote/completion
   drain budgets, hot-drain caps, idle repoll policy, and observed park wakeups.
   Do not add service-class weights yet; first make the current policy legible.
2. **Shard-locality ergonomics.** Extend topology/reporting so a live system can
   answer: which named services or isolate groups are on which shard, which
   remote edges exist, which edges are seeing `Full`/`Closed`, and which
   resources each shard owns. Service/group names should be explicit labels at
   registration or `LocalSystem` builder time, not guessed Rust type names. Add
   assertion helpers only after specimens need colocated or separated placement.
   Do not expose arbitrary executor handles as the teaching path.
3. **Storage visibility.** Split reports for completion-backed storage work and
   fallback-worker work. The capability report already distinguishes
   `storage_lane` from `storage_metadata_fallback`; live reports should do the
   same instead of reporting only unmeasured capacities. Surface storage
   capacity, depth where honest, accepted/full/closed counts, active write-path
   locks, fallback-worker usage, durability capability, and direct-I/O/DMA
   alignment support as `NotClaimed` until real support lands. Add storage perf
   rows for append/replay/snapshot and file streaming before making
   storage-performance claims.
4. **Stall detection.** Keep this separate from `FairnessReport`.
   `FairnessReport` is trace-folded progress counts, not ready-turn lag, timer
   lateness, or wall-clock stalls. Add an opt-in live `StallPolicy` for long
   handler turns,
   long driver advances, and long completion drains. Emit/report typed stall
   facts such as handler-stalled, driver-advance-stalled, and
   completion-drain-stalled. This is live observability, not preemption and not
   simulator replay truth. The simulator may record an unsupported/live-only
   fact, but should not pretend wall-clock stalls replay deterministically.
   This is the Tina equivalent of Glommio's stall posture: one bad cooperative
   turn should be visible fast.
5. **Boring performance evidence.** Expand perf rows so every claimed
   thread-per-core/completion benefit carries backend, platform/kernel,
   core-placement, trace mode, p50/p90/p99, allocation, timeout/full, resource
   leak, and queue-pressure evidence. Keep "no production performance claim"
   until repeated Linux/macOS rows on stable machines justify changing it.

Future IDD plans from this note should include user-shaped proof: a live service
with multiple explicitly labeled services on multiple shards, real
topology/report assertions, a forced stall, a storage-lane report, stale-TLS-doc
cleanup proof, and before/after perf rows. Unit tests alone are not enough for
this direction.

Explicit non-goals for this phase:

- no generic `Future`/`Stream` runtime surface;
- no Glommio-style task queues exposed directly to Tina application code;
- no weighted scheduling before current local/remote/completion policy is
  observable;
- no Direct I/O claim without capability reporting, alignment rules, tests, and
  perf rows;
- no performance positioning that outruns measured rows.

## Later capability roadmap

These are real Tina directions, but they should not be treated as launch
blockers for the first local-runtime story.

| Phase | Purpose |
|---|---|
| **054 userspace TCP research door** | Name the future kernel-bypass/userspace TCP contract and the measurements that would justify it. This is deliberately not an implementation phase: no DPDK, no packet parser, no NIC driver, no launch promise. Kernel TCP plus Betelgeuse's Linux `io_uring` backend stays the real path until evidence says otherwise. |
| **Learning from Glommio** | Turn Tina's existing thread-per-core/completion-runtime ingredients into explicit operator-facing policy and evidence: scheduler policy/reporting, shard-locality topology, storage visibility, stall detection, and boring perf rows, without adopting a generic async task runtime surface. |
| **Jan Peter Balkenende remoting** | Tina runtime to Tina runtime over a network with typed, bounded, traceable remote outcomes. |
| **Mark Rutte clustering** | Membership and placement after remoting is boring, without weakening local boundedness or stale-address semantics. |
| **Gemini release story** | Prime-time readiness only after Tina is reasonably complete: guides, invariant docs, semver/publication decision, CI/proof gate, public positioning, and adoption story. |

## Strategic gates

These should be resolved before public release or broad adoption claims:

- **Decide the Peter Mbanugo / Tina-Odin public-positioning question early.**
  Preferred path: rename public project identity to Alpaca before launch, then
  reach out before public publish and coordinate if practical. Docs must be
  explicit that Alpaca is an independently maintained Rust project inspired by
  Tina-Odin, not an official port or implied endorsement. Local design
  exploration is not blocked on this, but public positioning and any publish
  decision should not outrun an explicit decision.
- **Set the MSRV/runtime-substrate policy.** The current implementation uses
  nightly-facing Betelgeuse pieces. Public release needs an explicit stable
  story or an honest nightly-only claim. Linux uses Betelgeuse `io_uring`,
  macOS uses Betelgeuse `kqueue`, and other platforms must report their actual
  backend capability instead of pretending to have `io_uring`.
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
   process execution, raw Unix signal capture, and runtime shutdown
   notification are implemented on the live local runtime. Broader substrate
   liveness faults, remoting, clustering, and middleware-inside-Tina remain
   future work.
4. **Live cross-shard isolate calls.** Local live cross-shard call/reply
   transport exists. Network remoting remains Jan Peter Balkenende.
5. **Mailbox producer model.** Current decision: one mailbox contract, no
   alternate escape path. Add bounded multi-producer mailbox support only if a
   named workload proves the current producer model is too narrow, and only
   with the same visible `Full`/`Closed`, FIFO rules, no hidden blocking, and
   no unbounded internal queue.
6. **Zero-copy / lower-allocation transport.** The current cost model is
   honest but not final. Phases 144-155 removed the first obvious runtime,
   socket, HTTP/2, WebSocket, and gRPC waste, including compact gRPC request
   and response paths. The next home is a deeper performance slice that reduces
   remaining protocol/runtime turn count and inbound HPACK/header churn, then
   proves the result on Linux and macOS without hiding Tina's
   effect/cancel/capacity truth.
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
